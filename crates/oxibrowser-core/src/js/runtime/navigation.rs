//! Native History/Location authority and anchor activation. Public page globals
//! and bootstrap compatibility arrays never determine automation history.
use super::*;
use boa_engine::JsData;
use boa_gc::{Finalize, Gc, GcRefCell, Trace, WeakGc};

#[derive(Clone, Trace, Finalize)]
struct Entry {
    id: u64,
    url: String,
    #[unsafe_ignore_trace]
    state: Value,
    document: u64,
}
#[derive(Trace, Finalize)]
struct History {
    entries: Vec<Entry>,
    index: usize,
    next_id: u64,
    revision: u64,
    pending: Option<usize>,
    replacement: Option<String>,
    owner: JsObject,
    realm: boa_engine::realm::Realm,
    context_id: u32,
}
#[derive(Clone, Trace, Finalize, JsData)]
struct Access {
    data: Gc<GcRefCell<History>>,
    location: JsObject,
}
#[derive(Clone, Trace, Finalize, JsData)]
struct NativeFetch {
    function: JsObject,
}
#[derive(Clone, Trace, Finalize, JsData)]
struct DocumentOwner(u32);
pub(super) fn set_document_owner(ctx: &mut Context, owner: u32) {
    ctx.insert_data(DocumentOwner(owner));
}
pub(super) fn document_owner(ctx: &Context) -> u32 {
    ctx.get_data::<DocumentOwner>().map_or(1, |owner| owner.0)
}
pub(super) fn require_document_navigation(ctx: &mut Context) -> JsResult<()> {
    if document_owner(ctx) == 1 {
        return Ok(());
    }
    let error: boa_engine::JsError = JsNativeError::typ()
        .with_message("physical child-frame navigation is unsupported")
        .into();
    let value = error.into_opaque(ctx)?;
    if let Some(object) = value.as_object() {
        object.create_data_property_or_throw(
            js_string!("name"),
            js_string!("NotSupportedError"),
            ctx,
        )?;
        object.create_data_property_or_throw(js_string!("code"), 9, ctx)?;
    }
    Err(boa_engine::JsError::from_opaque(value))
}
pub(super) fn capture_fetch(ctx: &mut Context) {
    if let Ok(value) = ctx.global_object().get(js_string!("fetch"), ctx)
        && let Some(function) = value.as_callable()
    {
        ctx.realm()
            .host_defined_mut()
            .insert(NativeFetch { function });
    }
}
thread_local! { static DOCUMENTS:RefCell<HashMap<u64,WeakGc<GcRefCell<History>>>>=RefCell::new(HashMap::new()); }
fn access(ctx: &Context) -> Option<Access> {
    ctx.realm().host_defined().get::<Access>().cloned()
}
pub(super) fn current_url(ctx: &Context) -> Option<String> {
    let access = access(ctx)?;
    let data = access.data.borrow();
    Some(data.entries[data.index].url.clone())
}
fn resolve(ctx: &Context, value: &str) -> JsResult<url::Url> {
    let base = current_url(ctx).unwrap_or_else(|| "about:blank".into());
    url::Url::parse(&base)
        .and_then(|base| base.join(value))
        .map_err(|e| JsNativeError::typ().with_message(e.to_string()).into())
}
fn journal(ctx: &mut Context, mutation: DomMutation) -> JsResult<()> {
    require_document_navigation(ctx)?;
    if let Some(journal) = ctx.get_data::<NavigationJournal>() {
        journal.0.write().push(mutation);
    }
    Ok(())
}
fn emit(ctx: &mut Context, name: &str, old: &str, new: &str, state: Value) -> JsResult<()> {
    let Some(access) = access(ctx) else {
        return Ok(());
    };
    let (owner, realm) = {
        let data = access.data.borrow();
        (data.owner.clone(), data.realm.clone())
    };
    let previous = ctx.enter_realm(realm);
    let event = dom_realm::synthetic_event(ctx, name, false, false);
    if let Some(event) = event.as_object() {
        if name == "popstate" {
            event.create_data_property_or_throw(
                js_string!("state"),
                JsValue::from_json(&state, ctx)?,
                ctx,
            )?;
        } else {
            event.create_data_property_or_throw(js_string!("oldURL"), JsString::from(old), ctx)?;
            event.create_data_property_or_throw(js_string!("newURL"), JsString::from(new), ctx)?;
        }
    }
    let result =
        dispatch_bubbling_event(ctx, &event, &[(owner.into(), EventListenerStore::Window)])
            .map(|_| ());
    ctx.enter_realm(previous);
    result
}
fn clone_state(ctx: &mut Context, value: &JsValue) -> JsResult<Value> {
    fn error(ctx: &mut Context) -> boa_engine::JsError {
        let native: boa_engine::JsError = JsNativeError::typ()
            .with_message("history state supports bounded JSON-compatible values")
            .into();
        match native.into_opaque(ctx) {
            Ok(value) => {
                if let Some(object) = value.as_object() {
                    let _ = object.create_data_property_or_throw(
                        js_string!("name"),
                        js_string!("DataCloneError"),
                        ctx,
                    );
                    let _ = object.create_data_property_or_throw(js_string!("code"), 25, ctx);
                }
                boa_engine::JsError::from_opaque(value)
            }
            Err(error) => error,
        }
    }
    fn copy(
        ctx: &mut Context,
        value: &JsValue,
        seen: &mut std::collections::HashSet<JsObject>,
        depth: usize,
        budget: &mut usize,
    ) -> JsResult<Value> {
        if depth > 128 || *budget == 0 {
            return Err(error(ctx));
        }
        *budget -= 1;
        if value.is_null() {
            return Ok(Value::Null);
        }
        if let Some(value) = value.as_boolean() {
            return Ok(Value::Bool(value));
        }
        if let Some(value) = value.as_number() {
            return serde_json::Number::from_f64(value)
                .map(Value::Number)
                .ok_or_else(|| error(ctx));
        }
        if let Some(value) = value.as_string() {
            if value.len() > 1024 * 1024 {
                return Err(error(ctx));
            }
            return Ok(Value::String(value.to_std_string_escaped()));
        }
        let Some(object) = value.as_object() else {
            return Err(error(ctx));
        };
        if value.is_callable()
            || dom_realm::node_id(ctx, &object).is_some()
            || !seen.insert(object.clone())
        {
            return Err(error(ctx));
        }
        let result = if let Ok(array) = JsArray::from_object(object.clone()) {
            let length = array.length(ctx)?;
            if length > 100_000 {
                return Err(error(ctx));
            }
            let mut values = Vec::with_capacity(length as usize);
            for index in 0..length {
                let value = array.at(index as i64, ctx)?;
                values.push(copy(ctx, &value, seen, depth + 1, budget)?);
            }
            Value::Array(values)
        } else if object.is_ordinary() {
            let mut values = serde_json::Map::new();
            for key in object.own_property_keys(ctx)? {
                let name = match &key {
                    boa_engine::property::PropertyKey::String(name) => name.to_std_string_escaped(),
                    boa_engine::property::PropertyKey::Index(index) => index.get().to_string(),
                    boa_engine::property::PropertyKey::Symbol(_) => continue,
                };
                let descriptor =
                    boa_engine::builtins::object::OrdinaryObject::get_own_property_descriptor(
                        &JsValue::undefined(),
                        &[object.clone().into(), (&key).into()],
                        ctx,
                    )?;
                if descriptor.as_object().is_some_and(|descriptor| {
                    descriptor
                        .get(js_string!("enumerable"), ctx)
                        .ok()
                        .is_some_and(|value| value.to_boolean())
                }) {
                    let value = object.get(key.clone(), ctx)?;
                    values.insert(name, copy(ctx, &value, seen, depth + 1, budget)?);
                }
            }
            Value::Object(values)
        } else {
            return Err(error(ctx));
        };
        seen.remove(&object);
        Ok(result)
    }
    copy(
        ctx,
        value,
        &mut std::collections::HashSet::new(),
        0,
        &mut 100_000,
    )
}
fn state_change(
    ctx: &mut Context,
    state: JsValue,
    url: Option<String>,
    replace: bool,
) -> JsResult<()> {
    let state = clone_state(ctx, &state)?;
    let Some(access) = access(ctx) else {
        return Ok(());
    };
    let current = current_url(ctx).unwrap();
    let destination = if let Some(value) = url {
        resolve(ctx, &value)?.to_string()
    } else {
        current.clone()
    };
    let old =
        url::Url::parse(&current).map_err(|e| JsNativeError::typ().with_message(e.to_string()))?;
    let next = url::Url::parse(&destination)
        .map_err(|e| JsNativeError::typ().with_message(e.to_string()))?;
    if old.origin() != next.origin() {
        return Err(JsNativeError::typ()
            .with_message("SecurityError: history URL must have the document origin")
            .into());
    }
    let document = dom_realm::document_id(ctx);
    let mut data = access.data.borrow_mut();
    let index = data.index;
    data.revision += 1;
    data.pending = None;
    data.replacement = None;
    if replace {
        data.entries[index].url = destination;
        data.entries[index].state = state;
        data.entries[index].document = document;
    } else {
        data.entries.truncate(index + 1);
        let id = data.next_id;
        data.next_id += 1;
        data.entries.push(Entry {
            id,
            url: destination,
            state,
            document,
        });
        data.index = data.entries.len() - 1;
    }
    Ok(())
}
fn hash(ctx: &mut Context, value: &str) -> JsResult<()> {
    let old = current_url(ctx).unwrap_or_else(|| "about:blank".into());
    let mut destination =
        url::Url::parse(&old).map_err(|e| JsNativeError::typ().with_message(e.to_string()))?;
    destination.set_fragment(if value.is_empty() {
        None
    } else {
        Some(value.strip_prefix('#').unwrap_or(value))
    });
    let new = destination.to_string();
    if new == old {
        return Ok(());
    }
    state_change(ctx, JsValue::null(), Some(new.clone()), false)?;
    emit(ctx, "hashchange", &old, &new, Value::Null)
}
pub(super) fn snapshot(ctx: &Context) -> Value {
    let Some(access) = access(ctx) else {
        return serde_json::json!({"entries":[],"index":0,"url":"about:blank","documentId":dom_realm::document_id(ctx),"revision":0});
    };
    let data = access.data.borrow();
    serde_json::json!({"entries":data.entries.iter().map(|entry|serde_json::json!({"id":entry.id,"url":entry.url,"documentId":entry.document})).collect::<Vec<_>>(),"index":data.index,"url":data.entries[data.index].url,"documentId":dom_realm::document_id(ctx),"revision":data.revision,"pendingIndex":data.pending,"pendingReplace":data.replacement})
}
pub(super) fn go_to(ctx: &mut Context, index: usize) -> JsResult<()> {
    let Some(access) = access(ctx) else {
        return Ok(());
    };
    let (old, entry) = {
        let data = access.data.borrow();
        if index >= data.entries.len() {
            return Err(JsNativeError::range()
                .with_message("history entry index is out of range")
                .into());
        }
        if index == data.index {
            return Ok(());
        }
        (
            data.entries[data.index].url.clone(),
            data.entries[index].clone(),
        )
    };
    if entry.document != dom_realm::document_id(ctx) {
        require_document_navigation(ctx)?;
        let mut data = access.data.borrow_mut();
        data.pending = Some(index);
        data.replacement = None;
        drop(data);
        journal(
            ctx,
            DomMutation::Navigate {
                url: entry.url.clone(),
            },
        )?;
        return Ok(());
    }
    {
        let mut data = access.data.borrow_mut();
        data.index = index;
        data.pending = None;
        data.replacement = None;
        data.revision += 1;
    }
    emit(ctx, "popstate", &old, &entry.url, entry.state.clone())?;
    let old_hash = url::Url::parse(&old)
        .ok()
        .and_then(|url| url.fragment().map(str::to_owned));
    let new_hash = url::Url::parse(&entry.url)
        .ok()
        .and_then(|url| url.fragment().map(str::to_owned));
    if old_hash != new_hash {
        emit(ctx, "hashchange", &old, &entry.url, Value::Null)?;
    }
    Ok(())
}
fn go(ctx: &mut Context, delta: i64) -> JsResult<()> {
    if delta == 0 {
        journal(ctx, DomMutation::Reload)?;
        return Ok(());
    }
    let Some(access) = access(ctx) else {
        return Ok(());
    };
    let index = {
        let data = access.data.borrow();
        let index = data.index as i64 + delta;
        if index < 0 || index >= data.entries.len() as i64 {
            return Ok(());
        }
        index as usize
    };
    go_to(ctx, index)
}
fn navigate(ctx: &mut Context, value: &str, replace: bool) -> JsResult<()> {
    let destination = resolve(ctx, value)?;
    let old = current_url(ctx).unwrap_or_else(|| "about:blank".into());
    let mut a =
        url::Url::parse(&old).map_err(|e| JsNativeError::typ().with_message(e.to_string()))?;
    let mut b = destination.clone();
    a.set_fragment(None);
    b.set_fragment(None);
    if a == b
        && (destination.fragment().is_some()
            || url::Url::parse(&old)
                .ok()
                .is_some_and(|url| url.fragment().is_some()))
    {
        if replace {
            state_change(ctx, JsValue::null(), Some(destination.to_string()), true)?;
            emit(ctx, "hashchange", &old, destination.as_str(), Value::Null)
        } else {
            hash(
                ctx,
                &destination
                    .fragment()
                    .map(|value| format!("#{value}"))
                    .unwrap_or_default(),
            )
        }
    } else {
        require_document_navigation(ctx)?;
        if replace && let Some(access) = access(ctx) {
            let mut data = access.data.borrow_mut();
            data.replacement = Some(destination.to_string());
            data.pending = None;
        }
        journal(
            ctx,
            DomMutation::Navigate {
                url: destination.to_string(),
            },
        )?;
        Ok(())
    }
}

pub(super) struct Transfer {
    entries: Vec<(u64, String, Value, u64)>,
    index: usize,
    next_id: u64,
    revision: u64,
    pending: Option<usize>,
    replacement: Option<String>,
}
pub(super) fn export(ctx: &Context) -> Option<Transfer> {
    let access = access(ctx)?;
    let data = access.data.borrow();
    Some(Transfer {
        entries: data
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.id,
                    entry.url.clone(),
                    entry.state.clone(),
                    entry.document,
                )
            })
            .collect(),
        index: data.index,
        next_id: data.next_id,
        revision: data.revision,
        pending: data.pending,
        replacement: data.replacement.clone(),
    })
}
pub(super) fn restore(ctx: &mut Context, transfer: Transfer, page_url: &str) {
    let location = access(ctx)
        .map(|access| access.location.clone())
        .unwrap_or_else(|| JsObject::with_object_proto(ctx.intrinsics()));
    let data = Gc::new(GcRefCell::new(History {
        entries: transfer
            .entries
            .into_iter()
            .map(|(id, url, state, document)| Entry {
                id,
                url,
                state,
                document,
            })
            .collect(),
        index: transfer.index,
        next_id: transfer.next_id,
        revision: transfer.revision,
        pending: transfer.pending,
        replacement: transfer.replacement,
        owner: ctx.global_object(),
        realm: ctx.realm().clone(),
        context_id: dom_realm::context_id(ctx),
    }));
    ctx.realm()
        .host_defined_mut()
        .insert(Access { data, location });
    bind(ctx, page_url);
}

pub(super) fn bind(ctx: &mut Context, page_url: &str) {
    let document = dom_realm::document_id(ctx);
    let context_id = dom_realm::context_id(ctx);
    let entry_document = if dom_realm::has_document(ctx) {
        document
    } else {
        0
    };
    let existing = access(ctx);
    let shared =
        DOCUMENTS.with(|documents| documents.borrow().get(&document).and_then(WeakGc::upgrade));
    let data = existing
        .as_ref()
        .map(|access| access.data.clone())
        .or(shared)
        .unwrap_or_else(|| {
            Gc::new(GcRefCell::new(History {
                entries: vec![Entry {
                    id: 1,
                    url: if page_url.is_empty() {
                        "about:blank".into()
                    } else {
                        page_url.into()
                    },
                    state: Value::Null,
                    document: entry_document,
                }],
                index: 0,
                next_id: 2,
                revision: 0,
                pending: None,
                replacement: None,
                owner: ctx.global_object(),
                realm: ctx.realm().clone(),
                context_id,
            }))
        });
    {
        let mut data = data.borrow_mut();
        let pending_wait = (data.replacement.is_some()
            || data.pending.is_some_and(|pending| {
                data.entries
                    .get(pending)
                    .is_some_and(|entry| entry.url == page_url)
            }))
            && data.entries[data.index].document == document
            && dom_realm::has_document(ctx);
        if data.context_id == context_id && !pending_wait {
            if let Some(pending) = data.pending {
                if data
                    .entries
                    .get(pending)
                    .is_some_and(|entry| entry.url == page_url)
                {
                    data.index = pending;
                }
                data.pending = None;
            }
            if data.replacement.take().is_some() {
                let index = data.index;
                data.entries[index].url = page_url.into();
                data.entries[index].state = Value::Null;
                data.entries[index].document = 0;
            }
            let url = if page_url.is_empty() {
                "about:blank"
            } else {
                page_url
            };
            let index = data.index;
            let changed_url = data.entries[index].url != url;
            let previous_document = data.entries[index].document;
            if data.entries[index].url != url
                && data.entries.len() == 1
                && data.entries[0].document == 0
            {
                data.entries[0].url = url.into();
                data.entries[0].state = Value::Null;
            } else if data.entries[index].url != url {
                data.entries.truncate(index + 1);
                let id = data.next_id;
                data.next_id += 1;
                data.entries.push(Entry {
                    id,
                    url: url.into(),
                    state: Value::Null,
                    document: 0,
                });
                data.index = data.entries.len() - 1;
            }
            let index = data.index;
            let replaced_document = data.entries[index].document;
            if replaced_document != 0 && replaced_document != entry_document {
                for entry in &mut data.entries {
                    if entry.document == replaced_document {
                        entry.document = entry_document;
                    }
                }
            } else {
                data.entries[index].document = entry_document;
            }
            if changed_url || previous_document != entry_document {
                data.revision += 1;
            }
        }
    }
    DOCUMENTS.with(|documents| {
        let mut documents = documents.borrow_mut();
        documents.retain(|_, data| data.upgrade().is_some());
        documents.insert(document, WeakGc::new(&data));
    });
    let location = JsObject::with_object_proto(ctx.intrinsics());
    ctx.realm().host_defined_mut().insert(Access {
        data,
        location: location.clone(),
    });
    for name in [
        "href", "hash", "search", "pathname", "origin", "protocol", "host", "hostname", "port",
    ] {
        let getter = NativeFunction::from_copy_closure_with_captures(
            |_, _, name, ctx| {
                let value = current_url(ctx).unwrap_or_else(|| "about:blank".into());
                let Ok(url) = url::Url::parse(&value) else {
                    return Ok(JsString::from(value).into());
                };
                let value = match name.as_str() {
                    "href" => url.to_string(),
                    "hash" => url
                        .fragment()
                        .map(|value| format!("#{value}"))
                        .unwrap_or_default(),
                    "search" => url
                        .query()
                        .map(|value| format!("?{value}"))
                        .unwrap_or_default(),
                    "pathname" => url.path().into(),
                    "origin" => url.origin().ascii_serialization(),
                    "protocol" => format!("{}:", url.scheme()),
                    "host" => url[url::Position::BeforeHost..url::Position::AfterPort].into(),
                    "hostname" => url.host_str().unwrap_or("").into(),
                    "port" => url
                        .port()
                        .map(|value| value.to_string())
                        .unwrap_or_default(),
                    _ => String::new(),
                };
                Ok(JsString::from(value).into())
            },
            name.to_owned(),
        )
        .to_js_function(ctx.realm());
        let setter = NativeFunction::from_copy_closure_with_captures(
            |_, args, name, ctx| {
                let value = args
                    .first()
                    .unwrap_or(&JsValue::undefined())
                    .to_string(ctx)?
                    .to_std_string_escaped();
                if name == "hash" {
                    hash(ctx, &value)?;
                } else if name == "href" {
                    navigate(ctx, &value, false)?;
                } else {
                    let mut url = resolve(ctx, "")?;
                    match name.as_str() {
                        "pathname" => url.set_path(&value),
                        "search" => url.set_query(if value.is_empty() {
                            None
                        } else {
                            Some(value.strip_prefix('?').unwrap_or(&value))
                        }),
                        _ => {
                            return Err(JsNativeError::typ()
                                .with_message("this Location component setter is unsupported")
                                .into());
                        }
                    }
                    navigate(ctx, url.as_str(), false)?;
                }
                Ok(JsValue::undefined())
            },
            name.to_owned(),
        )
        .to_js_function(ctx.realm());
        let _ = location.define_property_or_throw(
            JsString::from(name),
            boa_engine::property::PropertyDescriptor::builder()
                .get(getter)
                .set(setter)
                .enumerable(true)
                .configurable(false),
            ctx,
        );
    }
    for (name, replace) in [("assign", false), ("replace", true)] {
        let function = NativeFunction::from_copy_closure_with_captures(
            |_, args, replace, ctx| {
                let value = args
                    .first()
                    .unwrap_or(&JsValue::undefined())
                    .to_string(ctx)?
                    .to_std_string_escaped();
                navigate(ctx, &value, *replace)?;
                Ok(JsValue::undefined())
            },
            replace,
        )
        .to_js_function(ctx.realm());
        let _ = location.create_data_property_or_throw(JsString::from(name), function, ctx);
    }
    let reload = NativeFunction::from_fn_ptr(|_, _, ctx| {
        journal(ctx, DomMutation::Reload)?;
        Ok(JsValue::undefined())
    })
    .to_js_function(ctx.realm());
    let _ = location.create_data_property_or_throw(js_string!("reload"), reload, ctx);
    let string = NativeFunction::from_fn_ptr(|_, _, ctx| {
        Ok(JsString::from(current_url(ctx).unwrap_or_default()).into())
    })
    .to_js_function(ctx.realm());
    let _ = location.create_data_property_or_throw(js_string!("toString"), string, ctx);
    let history = JsObject::with_object_proto(ctx.intrinsics());
    for (name, replace) in [("pushState", false), ("replaceState", true)] {
        let function = NativeFunction::from_copy_closure_with_captures(
            |_, args, replace, ctx| {
                let state = args.first().cloned().unwrap_or(JsValue::null());
                let url = args
                    .get(2)
                    .filter(|value| !value.is_null() && !value.is_undefined())
                    .map(|value| {
                        value
                            .to_string(ctx)
                            .map(|value| value.to_std_string_escaped())
                    })
                    .transpose()?;
                state_change(ctx, state, url, *replace)?;
                Ok(JsValue::undefined())
            },
            replace,
        )
        .to_js_function(ctx.realm());
        let _ = history.create_data_property_or_throw(JsString::from(name), function, ctx);
    }
    for (name, delta) in [("back", -1), ("forward", 1)] {
        let function = NativeFunction::from_copy_closure_with_captures(
            |_, _, delta, ctx| {
                go(ctx, *delta)?;
                Ok(JsValue::undefined())
            },
            delta,
        )
        .to_js_function(ctx.realm());
        let _ = history.create_data_property_or_throw(JsString::from(name), function, ctx);
    }
    let go_method = NativeFunction::from_fn_ptr(|_, args, ctx| {
        let delta = args
            .first()
            .map(|value| value.to_i32(ctx))
            .transpose()?
            .unwrap_or(0);
        go(ctx, delta as i64)?;
        Ok(JsValue::undefined())
    })
    .to_js_function(ctx.realm());
    let _ = history.create_data_property_or_throw(js_string!("go"), go_method, ctx);
    for name in ["state", "length"] {
        let getter = NativeFunction::from_copy_closure_with_captures(
            |_, _, name, ctx| {
                let Some(access) = access(ctx) else {
                    return Ok(JsValue::null());
                };
                let data = access.data.borrow();
                if *name {
                    let state = data.entries[data.index].state.clone();
                    drop(data);
                    JsValue::from_json(&state, ctx)
                } else {
                    Ok(JsValue::from(data.entries.len()))
                }
            },
            name == "state",
        )
        .to_js_function(ctx.realm());
        let _ = history.define_property_or_throw(
            JsString::from(name),
            boa_engine::property::PropertyDescriptor::builder()
                .get(getter)
                .enumerable(true)
                .configurable(true),
            ctx,
        );
    }
    let _ = history.create_data_property_or_throw(
        js_string!("scrollRestoration"),
        js_string!("auto"),
        ctx,
    );
    let _ = ctx.register_global_property(js_string!("history"), history, Attribute::all());
    let location_get = NativeFunction::from_fn_ptr(|_, _, ctx| {
        Ok(access(ctx)
            .map(|access| access.location.clone().into())
            .unwrap_or(JsValue::null()))
    })
    .to_js_function(ctx.realm());
    let location_set = NativeFunction::from_fn_ptr(|_, args, ctx| {
        let value = args
            .first()
            .unwrap_or(&JsValue::undefined())
            .to_string(ctx)?
            .to_std_string_escaped();
        navigate(ctx, &value, false)?;
        Ok(JsValue::undefined())
    })
    .to_js_function(ctx.realm());
    let _ = ctx.global_object().define_property_or_throw(
        js_string!("location"),
        boa_engine::property::PropertyDescriptor::builder()
            .get(location_get.clone())
            .set(location_set.clone())
            .enumerable(true)
            .configurable(true),
        ctx,
    );
    if let Ok(document) = ctx.global_object().get(js_string!("document"), ctx)
        && let Some(document) = document.as_object()
    {
        let _ = document.define_property_or_throw(
            js_string!("location"),
            boa_engine::property::PropertyDescriptor::builder()
                .get(location_get)
                .set(location_set)
                .configurable(true),
            ctx,
        );
        for name in ["URL", "documentURI", "baseURI"] {
            let getter = NativeFunction::from_fn_ptr(|_, _, ctx| {
                Ok(JsString::from(current_url(ctx).unwrap_or_default()).into())
            })
            .to_js_function(ctx.realm());
            let _ = document.define_property_or_throw(
                JsString::from(name),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(getter)
                    .enumerable(true)
                    .configurable(true),
                ctx,
            );
        }
    }
    let native = ctx.realm().host_defined().get::<NativeFetch>().cloned();
    if !frames::is_opaque(ctx)
        && let Some(native) = native
    {
        let fetch = NativeFunction::from_copy_closure_with_captures(
            |_, args, function, ctx| {
                let mut args = args.to_vec();
                if let Some(value) = args.first().and_then(JsValue::as_string) {
                    let input = value.to_std_string_escaped();
                    let url = resolve(ctx, &input)?;
                    args[0] = JsString::from(url.to_string()).into();
                }
                function.call(&JsValue::undefined(), &args, ctx)
            },
            native.function.clone(),
        )
        .to_js_function(ctx.realm());
        let _ = ctx.register_global_property(js_string!("fetch"), fetch, Attribute::all());
    }
}

pub(super) fn anchor(
    ctx: &mut Context,
    rd: &Rc<RefCell<Option<RenderDocument>>>,
    mutations: &Arc<RwLock<Vec<DomMutation>>>,
    node: usize,
) -> JsResult<()> {
    if !dom_realm::is_current(ctx, rd) {
        return Ok(());
    }
    let link = {
        let cell = rd.borrow();
        let Some(doc) = cell.as_ref() else {
            return Ok(());
        };
        let mut id = Some(node);
        let mut found = None;
        while let Some(node) = id {
            if doc.tag_name(node).as_deref() == Some("a")
                && let Some(href) = doc.node_attr(node, "href")
            {
                found = Some((
                    href,
                    doc.node_attr(node, "target").unwrap_or_default(),
                    doc.node_attr(node, "download").is_some(),
                ));
                break;
            }
            id = doc.parent_id(node);
        }
        found
    };
    let Some((href, target, download)) = link else {
        return Ok(());
    };
    if download {
        return Err(JsNativeError::typ()
            .with_message("download link activation is unsupported")
            .into());
    }
    let destination = resolve(ctx, &href)?;
    if target.eq_ignore_ascii_case("_blank") {
        require_document_navigation(ctx)?;
        mutations.write().push(DomMutation::OpenTab {
            url: destination.to_string(),
            target,
        });
        return Ok(());
    }
    navigate(ctx, destination.as_str(), false)
}
