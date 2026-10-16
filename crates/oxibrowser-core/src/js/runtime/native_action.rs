//! Host-only commands. No page global, prototype, selector function, or callback
//! return value is used to resolve a target or construct an action receipt.
use super::*;
use std::cell::Cell;

#[derive(Debug, Clone)]
pub struct NativeTarget {
    document_id: u64,
    tree_revision: u64,
    node_id: u32,
    incarnation: u64,
    attachment_revision: u64,
    fingerprint: String,
    attributes: HashMap<String, String>,
}

pub struct NativeObservation {
    pub snapshot: DomSnapshot,
    document_id: u64,
    tree_revision: u64,
    incarnations: HashMap<u32, (u64, u64)>,
}

impl NativeObservation {
    pub(super) fn empty(document_id: u64) -> Self {
        Self {
            snapshot: DomSnapshot::empty(),
            document_id,
            tree_revision: 0,
            incarnations: HashMap::new(),
        }
    }
    /// Only nodes connected to the physical main document can become targets.
    pub fn target(&self, node_id: u32) -> Option<NativeTarget> {
        Some(NativeTarget {
            document_id: self.document_id,
            tree_revision: self.tree_revision,
            node_id,
            incarnation: self.incarnations.get(&node_id)?.0,
            attachment_revision: self.incarnations.get(&node_id)?.1,
            fingerprint: self.snapshot.fingerprint(node_id),
            attributes: self.snapshot.nodes.get(&node_id)?.attributes.clone(),
        })
    }
}

#[derive(Debug, Clone)]
pub enum NativeInput {
    Click,
    Fill(String),
}

#[derive(Debug, Clone)]
pub struct NativeReceipt {
    pub callbacks: usize,
    pub navigation_requested: bool,
}

pub(super) struct Failure {
    pub message: &'static str,
    pub may_have_acted: bool,
}

fn stale() -> Failure {
    Failure {
        message: "stale ref — re-observe",
        may_have_acted: false,
    }
}

fn failed(message: &'static str, may_have_acted: bool) -> Failure {
    Failure {
        message,
        may_have_acted,
    }
}

fn connected(doc: &RenderDocument, node: usize) -> bool {
    let mut cursor = Some(node);
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = cursor {
        if !seen.insert(id) {
            return false;
        }
        if id == doc.root_element_id() {
            return true;
        }
        let Some(parent) = doc.document().get_node(id).and_then(|n| n.parent) else {
            return false;
        };
        if !doc
            .document()
            .get_node(parent)
            .is_some_and(|p| p.children.contains(&id))
        {
            return false;
        }
        cursor = Some(parent);
    }
    false
}

pub(super) fn observe(doc: &mut RenderDocument, document_id: u64, url: &str) -> NativeObservation {
    let title = doc
        .query_selector("title")
        .map(|id| doc.node_text(id))
        .unwrap_or_default();
    let snapshot = DomSnapshot::from_render_document(doc, url, &title);
    let incarnations = snapshot
        .nodes
        .keys()
        .copied()
        .filter_map(|id| {
            if !connected(doc, id as usize) {
                return None;
            }
            doc.node_incarnation(id as usize)
                .map(|token| (id, (token, doc.node_attachment_revision(id as usize))))
        })
        .collect();
    NativeObservation {
        snapshot,
        document_id,
        tree_revision: doc.tree_revision(),
        incarnations,
    }
}

pub(super) fn pending_navigation(mutations: &Arc<RwLock<Vec<DomMutation>>>) -> bool {
    mutations
        .read()
        .iter()
        .any(|m| matches!(m, DomMutation::Navigate { .. } | DomMutation::Reload))
}

/// A bounded pump for the native path. Does not use page eval or inspect thenables.
/// Network/WebSocket/IDB completion pumping retains the existing engine path.
pub(super) fn pump(
    ctx: &mut Context,
    queue: &Rc<TokioJobQueue>,
) -> std::result::Result<(), Failure> {
    let start = Instant::now();
    let mut budget = 256usize;
    loop {
        let jobs = queue
            .run_bounded_jobs(ctx, budget)
            .map_err(|_| failed("NATIVE_JOB_FAILED_OR_LIMIT", true))?;
        budget -= jobs;
        let due = queue.pop_due_timers();
        if due.is_empty() {
            return Ok(());
        }
        for timer in due {
            if budget == 0 || start.elapsed() > Duration::from_millis(500) {
                return Err(failed("NATIVE_JOB_FAILED_OR_LIMIT", true));
            }
            budget -= 1;
            timer
                .callback
                .call(&JsValue::undefined(), &timer.args, ctx)
                .map_err(|_| failed("NATIVE_JOB_FAILED_OR_LIMIT", true))?;
            if timer.is_interval {
                let ms = timer.interval_ms.unwrap_or(1).max(1);
                queue.schedule_timer(
                    Instant::now() + Duration::from_millis(ms),
                    timer.callback,
                    timer.args,
                    true,
                    Some(ms),
                );
            }
        }
    }
}

fn readonly(ctx: &mut Context, obj: &JsObject, key: &'static str, value: JsValue) {
    obj.define_property_or_throw(
        JsString::from(key),
        boa_engine::property::PropertyDescriptor::builder()
            .value(value)
            .writable(false)
            .enumerable(true)
            .configurable(false),
        ctx,
    )
    .expect("fresh host event property");
}

fn function(ctx: &mut Context, obj: &JsObject, key: &'static str, native: NativeFunction) {
    let function = FunctionObjectBuilder::new(ctx.realm(), native)
        .name(JsString::from(key))
        .build();
    readonly(ctx, obj, key, function.into());
}

fn event(
    ctx: &mut Context,
    render_doc: &Rc<RefCell<Option<RenderDocument>>>,
    dirty: &Arc<AtomicBool>,
    target: usize,
    current: usize,
    kind: &'static str,
    stopped: &Rc<Cell<bool>>,
    immediate: &Rc<Cell<bool>>,
    prevented: &Rc<Cell<bool>>,
) -> (JsObject, JsValue) {
    let target_object =
        create_render_element_object(ctx, render_doc.clone(), target, dirty.clone());
    let current_object =
        create_render_element_object(ctx, render_doc.clone(), current, dirty.clone());
    let obj = JsObject::with_null_proto();
    readonly(ctx, &obj, "type", JsString::from(kind).into());
    readonly(ctx, &obj, "target", target_object);
    readonly(ctx, &obj, "currentTarget", current_object.clone());
    readonly(ctx, &obj, "bubbles", true.into());
    readonly(ctx, &obj, "cancelable", (kind == "click").into());
    // Automation is never presented as trusted human input.
    readonly(ctx, &obj, "isTrusted", false.into());
    let state = prevented.clone();
    let getter = FunctionObjectBuilder::new(ctx.realm(), unsafe {
        NativeFunction::from_closure(move |_, _, _| Ok(state.get().into()))
    })
    .build();
    obj.define_property_or_throw(
        js_string!("defaultPrevented"),
        boa_engine::property::PropertyDescriptor::builder()
            .get(getter)
            .enumerable(true)
            .configurable(false),
        ctx,
    )
    .expect("fresh host event accessor");
    let stop = stopped.clone();
    function(ctx, &obj, "stopPropagation", unsafe {
        NativeFunction::from_closure(move |_, _, _| {
            stop.set(true);
            Ok(JsValue::undefined())
        })
    });
    let stop = stopped.clone();
    let now = immediate.clone();
    function(ctx, &obj, "stopImmediatePropagation", unsafe {
        NativeFunction::from_closure(move |_, _, _| {
            stop.set(true);
            now.set(true);
            Ok(JsValue::undefined())
        })
    });
    let cancel = prevented.clone();
    function(ctx, &obj, "preventDefault", unsafe {
        NativeFunction::from_closure(move |_, _, _| {
            if kind == "click" {
                cancel.set(true);
            }
            Ok(JsValue::undefined())
        })
    });
    (obj, current_object)
}

fn still_same(doc: &mut RenderDocument, node: usize, token: u64, attachment: u64) -> bool {
    connected(doc, node)
        && doc.node_incarnation(node) == Some(token)
        && doc.node_attachment_revision(node) == attachment
}

#[allow(clippy::too_many_arguments)]
fn deliver(
    ctx: &mut Context,
    render_doc: &Rc<RefCell<Option<RenderDocument>>>,
    dirty: &Arc<AtomicBool>,
    mutations: &Arc<RwLock<Vec<DomMutation>>>,
    target: &NativeTarget,
    kind: &'static str,
    callbacks: &mut usize,
) -> std::result::Result<(), Failure> {
    let route = {
        let mut cell = render_doc.borrow_mut();
        let doc = cell.as_mut().ok_or_else(stale)?;
        let mut id = Some(target.node_id as usize);
        let mut route = Vec::new();
        while let Some(node) = id {
            if route.len() >= 1024 {
                return Err(failed("NATIVE_EVENT_ROUTE_LIMIT", true));
            }
            if doc.tag_name(node).is_none() {
                break;
            }
            route.push((
                node,
                doc.node_incarnation(node).ok_or_else(stale)?,
                doc.node_attachment_revision(node),
            ));
            id = doc.document().get_node(node).and_then(|n| n.parent);
        }
        route
    };
    let stopped = Rc::new(Cell::new(false));
    let immediate = Rc::new(Cell::new(false));
    let prevented = Rc::new(Cell::new(false));
    for (node, token, attachment) in route {
        if stopped.get() || pending_navigation(mutations) {
            break;
        }
        let handlers = registry_get_native(node as u32, token, kind);
        if handlers.is_empty() {
            continue;
        }
        let (event, this) = event(
            ctx,
            render_doc,
            dirty,
            target.node_id as usize,
            node,
            kind,
            &stopped,
            &immediate,
            &prevented,
        );
        for callback in handlers {
            if immediate.get() || pending_navigation(mutations) {
                break;
            }
            {
                let mut cell = render_doc.borrow_mut();
                let doc = cell
                    .as_mut()
                    .ok_or_else(|| failed("NATIVE_TARGET_DETACHED_DURING_ACTION", true))?;
                if !still_same(
                    doc,
                    target.node_id as usize,
                    target.incarnation,
                    target.attachment_revision,
                ) || !still_same(doc, node, token, attachment)
                {
                    return Err(failed("NATIVE_TARGET_DETACHED_DURING_ACTION", true));
                }
            }
            if *callbacks >= 256 {
                return Err(failed("NATIVE_LISTENER_LIMIT", true));
            }
            *callbacks += 1;
            // Call the stored function directly. Never inspect/serialize its return value.
            callback
                .call(&this, &[event.clone().into()], ctx)
                .map_err(|_| failed("NATIVE_LISTENER_ERROR", true))?;
        }
    }
    Ok(())
}

pub(super) fn act(
    ctx: &mut Context,
    render_doc: &Rc<RefCell<Option<RenderDocument>>>,
    dirty: &Arc<AtomicBool>,
    mutations: &Arc<RwLock<Vec<DomMutation>>>,
    document_id: u64,
    target: &NativeTarget,
    input: NativeInput,
) -> std::result::Result<NativeReceipt, Failure> {
    if pending_navigation(mutations) {
        return Err(stale());
    }
    {
        let mut cell = render_doc.borrow_mut();
        let doc = cell.as_mut().ok_or_else(stale)?;
        if document_id != target.document_id
            || doc.tree_revision() != target.tree_revision
            || !still_same(
                doc,
                target.node_id as usize,
                target.incarnation,
                target.attachment_revision,
            )
        {
            return Err(stale());
        }
        let snapshot = DomSnapshot::from_render_document(doc, "", "");
        if snapshot.fingerprint(target.node_id) != target.fingerprint
            || !snapshot
                .nodes
                .get(&target.node_id)
                .is_some_and(|n| n.attributes == target.attributes)
        {
            return Err(stale());
        }
        let id = target.node_id as usize;
        let tag = doc.tag_name(id).unwrap_or_default();
        let ty = doc
            .node_attr(id, "type")
            .unwrap_or_default()
            .to_ascii_lowercase();
        if doc.node_attr(id, "disabled").is_some()
            || matches!(ty.as_str(), "password" | "file" | "hidden")
        {
            return Err(failed("NATIVE_CONTROL_DENIED", false));
        }
        if let NativeInput::Fill(value) = &input {
            if doc.node_attr(id, "readonly").is_some() {
                return Err(failed("TARGET_NOT_FILLABLE", false));
            }
            if tag == "textarea"
                || (tag == "input"
                    && !matches!(
                        ty.as_str(),
                        "checkbox" | "radio" | "button" | "submit" | "reset" | "image"
                    ))
            {
                doc.set_attribute(id, "value", value);
            } else if doc
                .node_attr(id, "contenteditable")
                .is_some_and(|v| v.is_empty() || v.eq_ignore_ascii_case("true"))
            {
                doc.set_text(id, value);
            } else {
                return Err(failed("TARGET_NOT_FILLABLE", false));
            }
        }
    }
    dirty.store(true, Ordering::Relaxed);
    let mut callbacks = 0;
    match input {
        NativeInput::Click => deliver(
            ctx,
            render_doc,
            dirty,
            mutations,
            target,
            "click",
            &mut callbacks,
        )?,
        NativeInput::Fill(_) => {
            deliver(
                ctx,
                render_doc,
                dirty,
                mutations,
                target,
                "input",
                &mut callbacks,
            )?;
            if pending_navigation(mutations) {
                return Err(failed("NATIVE_NAVIGATION_DURING_FILL", true));
            }
            {
                let mut cell = render_doc.borrow_mut();
                if !cell.as_mut().is_some_and(|doc| {
                    still_same(
                        doc,
                        target.node_id as usize,
                        target.incarnation,
                        target.attachment_revision,
                    )
                }) {
                    return Err(failed("NATIVE_TARGET_DETACHED_DURING_ACTION", true));
                }
            }
            deliver(
                ctx,
                render_doc,
                dirty,
                mutations,
                target,
                "change",
                &mut callbacks,
            )?;
        }
    }
    Ok(NativeReceipt {
        callbacks,
        navigation_requested: pending_navigation(mutations),
    })
}
