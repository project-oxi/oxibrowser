//! Document-owned dynamic stylesheet requests. Only the existing vetted fetch
//! bridge performs I/O; renderer state and events are committed on the JS thread.
use super::*;
use boa_engine::JsData;
use boa_gc::{Finalize, Trace};
use std::collections::HashSet;

type NodeKey = (usize, usize, u64);

#[derive(Clone)]
struct RequestState {
    href: String,
    attachment: u64,
    id: u64,
}

#[derive(Default, Trace, Finalize, JsData)]
struct Resources {
    #[unsafe_ignore_trace]
    nodes: HashMap<NodeKey, RequestState>,
    #[unsafe_ignore_trace]
    seeded: HashSet<usize>,
}

pub(super) struct PendingStylesheet {
    document: Rc<RefCell<Option<RenderDocument>>>,
    dirty: Arc<AtomicBool>,
    key: NodeKey,
    href: String,
    attachment: u64,
    id: u64,
}

fn stylesheet(doc: &RenderDocument, node: usize) -> Option<String> {
    if doc.tag_name(node).as_deref() != Some("link")
        || !doc
            .node_attr(node, "rel")?
            .split_ascii_whitespace()
            .any(|token| token.eq_ignore_ascii_case("stylesheet"))
    {
        return None;
    }
    let href = doc.node_attr(node, "href")?;
    if href.trim().is_empty() {
        return None;
    }
    let mut cursor = Some(node);
    let mut seen = HashSet::new();
    while let Some(id) = cursor {
        if !seen.insert(id) {
            return None;
        }
        if id == doc.root_element_id() {
            return Some(href);
        }
        let parent = doc.parent_id(id)?;
        if !doc.children_ids(parent).contains(&id) {
            return None;
        }
        cursor = Some(parent);
    }
    None
}

pub(super) fn reset(ctx: &mut Context) {
    ctx.realm().host_defined_mut().remove::<Resources>();
}

/// Mark only the initial, already fetched link nodes as complete. Call after
/// installing a document and before its scripts can create further nodes.
pub(super) fn seed(ctx: &mut Context, document: &Rc<RefCell<Option<RenderDocument>>>) {
    let document_key = Rc::as_ptr(document) as usize;
    if ctx
        .realm()
        .host_defined()
        .get::<Resources>()
        .is_some_and(|state| state.seeded.contains(&document_key))
    {
        return;
    }
    let mut document = document.borrow_mut();
    let Some(doc) = document.as_mut() else {
        return;
    };
    let loaded: HashSet<String> = doc
        .preloaded_stylesheets()
        .iter()
        .map(|sheet| sheet.request_url.clone())
        .collect();
    let base = url::Url::parse(doc.base_url()).ok();
    let mut initial = Vec::new();
    for node in doc.query_selector_all("link") {
        let Some(href) = stylesheet(doc, node) else {
            continue;
        };
        let resolved = base
            .as_ref()
            .and_then(|base| base.join(&href).ok())
            .or_else(|| url::Url::parse(&href).ok());
        if resolved
            .as_ref()
            .is_some_and(|url| loaded.contains(url.as_str()))
            && let Some(token) = doc.node_incarnation(node)
        {
            initial.push((
                (document_key, node, token),
                RequestState {
                    href,
                    attachment: doc.node_attachment_revision(node),
                    id: 0,
                },
            ));
        }
    }
    let realm = ctx.realm().clone();
    let mut host = realm.host_defined_mut();
    if host.get::<Resources>().is_none() {
        host.insert(Resources::default());
    }
    let state = host.get_mut::<Resources>().unwrap();
    if state.seeded.insert(document_key) {
        state.nodes.extend(initial);
    }
}

pub(super) fn poll(
    ctx: &mut Context,
    document: Rc<RefCell<Option<RenderDocument>>>,
    dirty: Arc<AtomicBool>,
    fetch_tx: Arc<RwLock<Option<Sender<FetchRequestMsg>>>>,
) {
    seed(ctx, &document);
    let document_key = Rc::as_ptr(&document) as usize;
    let candidates = {
        let mut cell = document.borrow_mut();
        let Some(doc) = cell.as_mut() else {
            return;
        };
        let base = url::Url::parse(doc.base_url()).ok();
        doc.query_selector_all("link")
            .into_iter()
            .filter_map(|node| {
                let href = stylesheet(doc, node)?;
                let token = doc.node_incarnation(node)?;
                let resolved = base
                    .as_ref()
                    .and_then(|base| base.join(&href).ok())
                    .or_else(|| url::Url::parse(&href).ok())
                    .filter(|url| matches!(url.scheme(), "http" | "https"));
                Some((
                    (document_key, node, token),
                    href,
                    doc.node_attachment_revision(node),
                    resolved,
                ))
            })
            .collect::<Vec<_>>()
    };
    let mut requests = Vec::new();
    {
        let realm = ctx.realm().clone();
        let mut host = realm.host_defined_mut();
        let state = host.get_mut::<Resources>().unwrap();
        let live: HashSet<NodeKey> = candidates.iter().map(|entry| entry.0).collect();
        state
            .nodes
            .retain(|key, _| key.0 != document_key || live.contains(key));
        for (key, href, attachment, resolved) in candidates {
            if state
                .nodes
                .get(&key)
                .is_some_and(|old| old.href == href && old.attachment == attachment)
            {
                continue;
            }
            let id = next_fetch_id();
            state.nodes.insert(
                key,
                RequestState {
                    href: href.clone(),
                    attachment,
                    id,
                },
            );
            requests.push((
                PendingStylesheet {
                    document: document.clone(),
                    dirty: dirty.clone(),
                    key,
                    href,
                    attachment,
                    id,
                },
                resolved,
            ));
        }
    }
    for (resource, resolved) in requests {
        let Some(url) = resolved else {
            resource.emit(ctx, "error");
            continue;
        };
        let url = url.to_string();
        let headers = vec![("accept".into(), "text/css,*/*;q=0.1".into())];
        if event_sink_attached() {
            push_event(CoreEvent::FetchRequest {
                request_id: cdp_request_id(resource.id),
                url: url.clone(),
                method: "GET".into(),
                headers: headers.clone(),
                post_data: None,
                timestamp: now_ms(),
            });
        }
        let request = FetchRequestMsg {
            id: resource.id,
            url,
            method: "GET".into(),
            headers,
            body: None,
            origin: current_origin(),
        };
        let sent = fetch_tx
            .read()
            .as_ref()
            .is_some_and(|tx| tx.send(request).is_ok());
        if sent {
            PENDING_FETCH.with(|pending| {
                pending.borrow_mut().insert(
                    resource.id,
                    PendingFetch::Stylesheet {
                        resource,
                        context_id: active_context_id(),
                    },
                );
            });
        } else {
            resource.emit(ctx, "error");
        }
    }
}

impl PendingStylesheet {
    fn current(&self, ctx: &Context) -> bool {
        let state_matches = ctx
            .realm()
            .host_defined()
            .get::<Resources>()
            .and_then(|state| state.nodes.get(&self.key))
            .is_some_and(|state| {
                state.id == self.id
                    && state.href == self.href
                    && state.attachment == self.attachment
            });
        if !state_matches {
            return false;
        }
        let mut cell = self.document.borrow_mut();
        let Some(doc) = cell.as_mut() else {
            return false;
        };
        doc.node_incarnation(self.key.1) == Some(self.key.2)
            && doc.node_attachment_revision(self.key.1) == self.attachment
            && stylesheet(doc, self.key.1).as_deref() == Some(self.href.as_str())
    }

    pub(super) fn settle(self, response: FetchResponseMsg, ctx: &mut Context) {
        if !self.current(ctx) {
            return;
        }
        let success = response.error.is_none() && (200..300).contains(&response.status);
        // Keep the same charset policy as initial core stylesheet loading,
        // using authoritative bytes when present rather than a JS text view.
        let css = if let Some(bytes) = &response.body_bytes {
            let content_type = response
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
                .map(|(_, value)| value.as_str());
            crate::encoding::decode_html(bytes, content_type)
        } else {
            response.body.clone()
        };
        let applied = success
            && self.document.borrow_mut().as_mut().is_some_and(|doc| {
                doc.attach_stylesheet(self.key.1, &response.url, &css)
                    .is_ok()
            });
        if applied {
            self.dirty.store(true, Ordering::Relaxed);
        }
        self.emit(ctx, if applied { "load" } else { "error" });
    }

    fn emit(&self, ctx: &mut Context, kind: &str) {
        if !self.current(ctx) {
            return;
        }
        let target = create_render_element_object(
            ctx,
            self.document.clone(),
            self.key.1,
            self.dirty.clone(),
        );
        let event = boa_engine::object::ObjectInitializer::new(ctx)
            .property(js_string!("type"), JsString::from(kind), Attribute::all())
            .property(js_string!("bubbles"), false, Attribute::all())
            .property(js_string!("cancelable"), false, Attribute::all())
            .property(js_string!("defaultPrevented"), false, Attribute::all())
            .property(js_string!("target"), JsValue::null(), Attribute::all())
            .property(
                js_string!("currentTarget"),
                JsValue::null(),
                Attribute::all(),
            )
            .property(js_string!("isTrusted"), true, Attribute::all())
            .function(
                NativeFunction::from_fn_ptr(|_, _, _| Ok(JsValue::undefined())),
                js_string!("preventDefault"),
                0,
            )
            .function(
                NativeFunction::from_fn_ptr(|this, _, ctx| {
                    if let Some(event) = this.as_object() {
                        event.set(js_string!("__stopPropagation"), true, true, ctx)?;
                    }
                    Ok(JsValue::undefined())
                }),
                js_string!("stopPropagation"),
                0,
            )
            .function(
                NativeFunction::from_fn_ptr(|this, _, ctx| {
                    if let Some(event) = this.as_object() {
                        event.set(js_string!("__stopPropagation"), true, true, ctx)?;
                        event.set(js_string!("__stopImmediatePropagation"), true, true, ctx)?;
                    }
                    Ok(JsValue::undefined())
                }),
                js_string!("stopImmediatePropagation"),
                0,
            )
            .build();
        let ancestors = {
            let cell = self.document.borrow();
            let mut ancestors = Vec::new();
            let mut cursor = cell.as_ref().and_then(|doc| doc.parent_id(self.key.1));
            while let Some(id) = cursor {
                let doc = cell.as_ref().unwrap();
                if doc.tag_name(id).is_none() {
                    break;
                }
                ancestors.push(id);
                cursor = doc.parent_id(id);
            }
            ancestors
        };
        let mut path = vec![(target, EventListenerStore::Element(self.key.1 as u32))];
        for node in ancestors {
            let wrapper =
                create_render_element_object(ctx, self.document.clone(), node, self.dirty.clone());
            path.push((wrapper, EventListenerStore::Element(node as u32)));
        }
        if append_document_event_targets(ctx, &mut path).is_ok() {
            let _ = dispatch_bubbling_event(ctx, &event.into(), &path);
        }
    }
}
