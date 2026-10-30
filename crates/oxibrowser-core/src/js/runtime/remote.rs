//! Native CDP references. The registry is a rooted, traced host value and is
//! inaccessible to page scripts, including scripts replacing built-in globals.
#[path = "remote_input.rs"]
mod input;
use super::{Context, JsValue, Source, TokioJobQueue};
use boa_engine::builtins::{object::OrdinaryObject, promise::PromiseState};
use boa_engine::object::builtins::JsPromise;
use boa_engine::property::PropertyKey;
use boa_engine::{JsBigInt, JsError, JsNativeError, js_string};
use boa_gc::{Finalize, Gc, GcRefCell, Trace};
use oxibrowser_render::RenderDocument;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT_REFERENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Trace, Finalize)]
struct Entry {
    value: JsValue,
    #[unsafe_ignore_trace]
    group: Option<String>,
}

#[derive(Default, Trace, Finalize)]
struct Registry {
    entries: HashMap<String, Entry>,
}

type Root = Gc<GcRefCell<Registry>>;

fn registry(ctx: &mut Context) -> Root {
    if let Some(root) = ctx.get_data::<Root>() {
        return root.clone();
    }
    let root = Gc::new(GcRefCell::new(Registry::default()));
    ctx.insert_data(root.clone());
    root
}

pub(super) fn clear(ctx: &mut Context) {
    ctx.remove_data::<Root>();
}

fn reference(value: JsValue, group: Option<&str>, ctx: &mut Context) -> String {
    let id = format!(
        "oxi-remote-{}-{}",
        super::active_context_id(),
        NEXT_REFERENCE.fetch_add(1, Ordering::Relaxed)
    );
    registry(ctx).borrow_mut().entries.insert(
        id.clone(),
        Entry {
            value,
            group: group.map(str::to_owned),
        },
    );
    id
}

fn resolve(id: &str, ctx: &mut Context) -> Result<(JsValue, Option<String>), String> {
    registry(ctx)
        .borrow()
        .entries
        .get(id)
        .map(|entry| (entry.value.clone(), entry.group.clone()))
        .ok_or_else(|| format!("Could not find object with given id: {id}"))
}

fn describe(
    value: JsValue,
    by_value: bool,
    group: Option<&str>,
    ctx: &mut Context,
) -> Result<Value, String> {
    if value.is_undefined() {
        return Ok(json!({"type":"undefined"}));
    }
    if value.is_null() {
        return Ok(json!({"type":"object", "subtype":"null", "value":null}));
    }
    if let Some(v) = value.as_boolean() {
        return Ok(json!({"type":"boolean", "value":v}));
    }
    if let Some(v) = value.as_string() {
        return Ok(json!({"type":"string", "value":v.to_std_string_escaped()}));
    }
    if let Some(v) = value.as_number() {
        let special = if v.is_nan() {
            Some("NaN")
        } else if v == f64::INFINITY {
            Some("Infinity")
        } else if v == f64::NEG_INFINITY {
            Some("-Infinity")
        } else if v == 0.0 && v.is_sign_negative() {
            Some("-0")
        } else {
            None
        };
        return Ok(match special {
            Some(v) => json!({"type":"number", "unserializableValue":v, "description":v}),
            None => {
                json!({"type":"number", "value":value.to_json(ctx).map_err(|error| error.to_string())?, "description":v.to_string()})
            }
        });
    }
    if let Some(v) = value.as_bigint() {
        let text = format!("{v}n");
        return Ok(json!({"type":"bigint", "unserializableValue":text, "description":text}));
    }
    if let Some(v) = value.as_symbol() {
        let description = v.descriptive_string().to_std_string_escaped();
        let id = reference(value, group, ctx);
        return Ok(json!({"type":"symbol", "description":description, "objectId":id}));
    }
    let object = value.as_object().ok_or("Unsupported remote value")?;
    let callable = object.is_callable();
    let array = object.is_array();
    let promise = JsPromise::from_object(object.clone()).is_ok();
    let mut result = json!({"type":if callable {"function"} else {"object"},
        "className":if callable {"Function"} else if array {"Array"} else if promise {"Promise"} else {"Object"},
        "description":if callable {"function"} else if array {"Array"} else if promise {"Promise"} else {"Object"}});
    if array {
        result["subtype"] = json!("array");
    }
    if promise {
        result["subtype"] = json!("promise");
    }
    if super::dom_realm::node_id(ctx, &object).is_some() {
        result["subtype"] = json!("node");
        result["className"] = json!("HTMLElement");
        result["description"] = json!("HTMLElement");
    }
    if by_value {
        if let Some(json) = value.to_json(ctx).map_err(|e| e.to_string())? {
            result["value"] = json;
        }
    } else {
        result["objectId"] = json!(reference(value, group, ctx));
    }
    Ok(result)
}

fn argument(arg: &Value, ctx: &mut Context) -> Result<JsValue, String> {
    if let Some(id) = arg.get("objectId").and_then(Value::as_str) {
        return resolve(id, ctx).map(|v| v.0);
    }
    if let Some(value) = arg.get("value") {
        return JsValue::from_json(value, ctx).map_err(|e| e.to_string());
    }
    if let Some(value) = arg.get("unserializableValue").and_then(Value::as_str) {
        return match value {
            "NaN" => Ok(JsValue::new(f64::NAN)),
            "Infinity" => Ok(JsValue::new(f64::INFINITY)),
            "-Infinity" => Ok(JsValue::new(f64::NEG_INFINITY)),
            "-0" => Ok(JsValue::new(-0.0)),
            _ => value
                .strip_suffix('n')
                .and_then(JsBigInt::from_string)
                .map(JsValue::from)
                .ok_or_else(|| format!("Invalid unserializableValue: {value}")),
        };
    }
    Ok(JsValue::undefined())
}

fn await_value(
    value: JsValue,
    params: &Value,
    ctx: &mut Context,
    queue: &Rc<TokioJobQueue>,
) -> Result<JsValue, JsError> {
    if !params
        .get("awaitPromise")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(value);
    }
    // Promise.resolve is called through the intrinsic API, never a page global.
    let promise = JsPromise::resolve(value, ctx)?;
    let budget = Duration::from_millis(
        params
            .get("timeout")
            .and_then(Value::as_u64)
            .unwrap_or(5000)
            .clamp(1, 30_000),
    );
    let deadline = Instant::now() + budget;
    loop {
        match promise.state() {
            PromiseState::Fulfilled(value) => return Ok(value),
            PromiseState::Rejected(reason) => return Err(JsError::from_opaque(reason)),
            PromiseState::Pending => {}
        }
        if Instant::now() >= deadline {
            return Err(JsNativeError::error()
                .with_message("Promise evaluation timed out")
                .into());
        }
        let _ = queue.run_background_jobs(ctx, 256);
        super::drain_pending_fetch_responses_bounded(ctx, 256, false);
        super::drain_ws_events(ctx);
        crate::js::idb::drain_idb_events_bounded(ctx, 64);
        // A finite due batch prevents self-scheduling timers monopolizing CDP.
        for timer in queue.pop_due_timers_bounded(64) {
            if queue.timer_is_active(timer.id) {
                let _ = timer.callback.call(&JsValue::undefined(), &timer.args, ctx);
                queue.complete_timer(timer, true);
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn exception(error: JsError, group: Option<&str>, ctx: &mut Context) -> Value {
    let text = error.to_string();
    let mut remote = describe(
        error
            .into_opaque(ctx)
            .unwrap_or_else(|_| JsValue::from(boa_engine::JsString::from(text.as_str()))),
        false,
        group,
        ctx,
    )
    .unwrap_or_else(|_| json!({"type":"string", "value":text}));
    if remote.get("type").and_then(Value::as_str) == Some("object") {
        remote["description"] = json!(text);
    }
    json!({"result":{"type":"undefined"}, "exceptionDetails":{
        "exceptionId":1, "text":text, "lineNumber":0, "columnNumber":0, "exception":remote
    }})
}

fn properties(params: &Value, ctx: &mut Context) -> Result<Value, String> {
    let id = params
        .get("objectId")
        .and_then(Value::as_str)
        .ok_or("Missing objectId")?;
    let (value, group) = resolve(id, ctx)?;
    let mut object = value.as_object().ok_or("Object is not an object")?;
    let own_only = params
        .get("ownProperties")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let accessors_only = params
        .get("accessorPropertiesOnly")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let non_indexed = params
        .get("nonIndexedPropertiesOnly")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut seen = HashSet::new();
    let mut visited = HashSet::new();
    let mut result = Vec::new();
    let mut own = true;
    loop {
        if !visited.insert(object.clone()) {
            break;
        }
        for key in object.own_property_keys(ctx).map_err(|e| e.to_string())? {
            if !seen.insert(key.clone()) || (non_indexed && matches!(key, PropertyKey::Index(_))) {
                continue;
            }
            // Calling the engine intrinsic avoids page-overridden Object methods
            // and reads accessor descriptors without invoking their getters.
            let descriptor = OrdinaryObject::get_own_property_descriptor(
                &JsValue::undefined(),
                &[object.clone().into(), (&key).into()],
                ctx,
            )
            .map_err(|e| e.to_string())?;
            let Some(desc) = descriptor.as_object() else {
                continue;
            };
            let has_value = desc
                .has_own_property(js_string!("value"), ctx)
                .map_err(|e| e.to_string())?;
            if accessors_only && has_value {
                continue;
            }
            let mut item = json!({"name":key.to_string(), "isOwn":own});
            for flag in ["enumerable", "configurable"] {
                item[flag] = json!(
                    desc.get(boa_engine::JsString::from(flag), ctx)
                        .map_err(|e| e.to_string())?
                        .to_boolean()
                );
            }
            if has_value {
                item["writable"] = json!(
                    desc.get(js_string!("writable"), ctx)
                        .map_err(|e| e.to_string())?
                        .to_boolean()
                );
                item["value"] = describe(
                    desc.get(js_string!("value"), ctx)
                        .map_err(|e| e.to_string())?,
                    false,
                    group.as_deref(),
                    ctx,
                )?;
            } else {
                for field in ["get", "set"] {
                    item[field] = describe(
                        desc.get(boa_engine::JsString::from(field), ctx)
                            .map_err(|e| e.to_string())?,
                        false,
                        group.as_deref(),
                        ctx,
                    )?;
                }
            }
            if let PropertyKey::Symbol(symbol) = key {
                item["symbol"] = describe(symbol.into(), false, group.as_deref(), ctx)?;
            }
            result.push(item);
        }
        if own_only {
            break;
        }
        let Some(prototype) = object.prototype() else {
            break;
        };
        object = prototype;
        own = false;
    }
    Ok(json!({"result":result, "internalProperties":[]}))
}

#[derive(Default)]
struct BindingNames(HashSet<String>);

pub(super) fn binding_names(ctx: &Context) -> Vec<String> {
    ctx.get_data::<BindingNames>()
        .map(|names| names.0.iter().cloned().collect())
        .unwrap_or_default()
}

pub(super) fn install_bindings(ctx: &mut Context, names: Vec<String>) -> Result<(), String> {
    for name in names {
        let callable = boa_engine::NativeFunction::from_copy_closure_with_captures(
            |_, args, name, _ctx| {
                if args.len() != 1 || args[0].as_string().is_none() {
                    return Err(JsNativeError::typ()
                        .with_message("Binding requires exactly one string argument")
                        .into());
                }
                super::push_event(super::CoreEvent::BindingCalled {
                    name: name.to_std_string_escaped(),
                    payload: args[0].as_string().unwrap().to_std_string_escaped(),
                    execution_context_id: super::active_context_id(),
                    timestamp: super::now_ms(),
                });
                Ok(JsValue::undefined())
            },
            boa_engine::JsString::from(name.as_str()),
        );
        ctx.register_global_callable(boa_engine::JsString::from(name.as_str()), 1, callable)
            .map_err(|error| error.to_string())?;
        if ctx.get_data::<BindingNames>().is_none() {
            ctx.insert_data(BindingNames::default());
        }
        ctx.host_defined_mut()
            .get_mut::<BindingNames>()
            .unwrap()
            .0
            .insert(name);
    }
    Ok(())
}

fn target_node(
    params: &Value,
    ctx: &mut Context,
    rd: &Rc<RefCell<Option<RenderDocument>>>,
) -> Result<usize, String> {
    if let Some(id) = params.get("objectId").and_then(Value::as_str) {
        let (value, _) = resolve(id, ctx)?;
        let object = value.as_object().ok_or("Remote value is not a DOM node")?;
        return super::dom_realm::node_id_in_document(ctx, &object, rd)
            .ok_or_else(|| "Remote object is not a DOM node in this document".into());
    }
    let id = params
        .get("nodeId")
        .or_else(|| params.get("backendNodeId"))
        .and_then(Value::as_u64)
        .and_then(|id| usize::try_from(id).ok())
        .ok_or("Missing node id")?;
    if !rd
        .borrow()
        .as_ref()
        .is_some_and(|doc| doc.document().get_node(id).is_some())
    {
        return Err("Node does not exist in this document".into());
    }
    Ok(id)
}

/// Run one CDP remote-object command in its owning Boa context.
pub(super) fn handle(
    request: Value,
    ctx: &mut Context,
    queue: &Rc<TokioJobQueue>,
    rd: &Rc<RefCell<Option<RenderDocument>>>,
    dirty: &Arc<AtomicBool>,
) -> Result<Value, String> {
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .ok_or("Missing method")?;
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    match method {
        "historySnapshot" => return Ok(super::navigation::snapshot(ctx)),
        "navigateHistory" => {
            let index = if let Some(id) = params.get("entryId").and_then(Value::as_u64) {
                super::navigation::snapshot(ctx)["entries"]
                    .as_array()
                    .and_then(|entries| {
                        entries
                            .iter()
                            .position(|entry| entry["id"].as_u64() == Some(id))
                    })
                    .ok_or("Unknown history entryId")?
            } else {
                params
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|index| usize::try_from(index).ok())
                    .ok_or("Missing history index")?
            };
            super::navigation::go_to(ctx, index).map_err(|error| error.to_string())?;
            let _ = queue.run_background_jobs(ctx, 256);
            return Ok(super::navigation::snapshot(ctx));
        }
        "dispatchKeyEvent" | "insertText" | "dispatchMouseEvent" => {
            return input::handle(method, &params, ctx, rd, dirty);
        }
        "getNodeForLocation" => {
            let x = params.get("x").and_then(Value::as_f64).ok_or("Missing x")?;
            let y = params.get("y").and_then(Value::as_f64).ok_or("Missing y")?;
            if !x.is_finite() || !y.is_finite() {
                return Err("Point must be finite".into());
            }
            if params
                .get("ignorePointerEventsNone")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                return Err("Ignoring pointer-events for hit testing is unsupported".into());
            }
            let mut document = rd.borrow_mut();
            let document = document.as_mut().ok_or("No rendered document")?;
            document.flush_layout();
            let scroll = document.document().viewport_scroll();
            let mut node = document
                .document()
                .hit((x + scroll.x) as f32, (y + scroll.y) as f32)
                .ok_or("No node found at given location")?
                .node_id;
            while document.node_kind(node) != Some("element") {
                node = document
                    .parent_id(node)
                    .ok_or("No element found at given location")?;
            }
            return Ok(
                json!({"nodeId":node,"backendNodeId":node,"nodeName":document.tag_name(node).unwrap_or_default().to_uppercase()}),
            );
        }
        "getContentQuads" | "getBoxModel" => {
            let node = target_node(&params, ctx, rd)?;
            let mut document = rd.borrow_mut();
            let document = document.as_mut().ok_or("No rendered document")?;
            document.flush_layout();
            let Some((content, padding, border, margin, width, height)) =
                document.node_box_quads(node)
            else {
                return if method == "getContentQuads" {
                    Ok(json!({"quads":[]}))
                } else {
                    Err("Node has no supported layout box".into())
                };
            };
            return if method == "getContentQuads" {
                Ok(json!({"quads":[content]}))
            } else {
                Ok(
                    json!({"model":{"content":content,"padding":padding,"border":border,"margin":margin,"width":width,"height":height}}),
                )
            };
        }
        "scrollIntoViewIfNeeded" => {
            let node = target_node(&params, ctx, rd)?;
            let mut document = rd.borrow_mut();
            let document = document.as_mut().ok_or("No rendered document")?;
            document.flush_layout();
            let (x, y, w, h) = document.node_layout_rect(node);
            if w <= 0.0 || h <= 0.0 {
                return Err("Node does not have a layout box".into());
            }
            let viewport = document.viewport();
            if x < 0.0
                || y < 0.0
                || x + w > f64::from(viewport.width)
                || y + h > f64::from(viewport.height)
            {
                document.scroll_to_node(node);
                dirty.store(true, Ordering::Relaxed);
            }
            return Ok(json!({}));
        }
        "describeNode" => {
            let node = target_node(&params, ctx, rd)?;
            let document = rd.borrow();
            let document = document.as_ref().ok_or("No rendered document")?;
            let kind = document.node_kind(node).unwrap_or("element");
            let tag = document.tag_name(node).unwrap_or_default();
            let kind = if document.document().root_node().id == node {
                "document"
            } else {
                kind
            };
            let (node_type, node_name) = match kind {
                "document" => (9, "#document".to_owned()),
                "text" => (3, "#text".to_owned()),
                "comment" => (8, "#comment".to_owned()),
                _ => (1, tag.to_uppercase()),
            };
            let attributes: Vec<String> = document
                .node_attributes(node)
                .into_iter()
                .flat_map(|(key, value)| [key, value])
                .collect();
            return Ok(
                json!({"node":{"nodeId":node,"backendNodeId":node,"nodeType":node_type,"nodeName":node_name,"localName":tag,
                "nodeValue":if node_type==3 || node_type==8 {document.node_text(node)} else {String::new()},
                "childNodeCount":document.children_ids(node).len(),"attributes":attributes}}),
            );
        }
        "getLayoutMetrics" => {
            let mut document = rd.borrow_mut();
            let document = document.as_mut().ok_or("No rendered document")?;
            document.flush_layout();
            let viewport = document.viewport();
            let (width, height) = document.content_size();
            let scroll = document.document().viewport_scroll();
            let layout = json!({"pageX":scroll.x,"pageY":scroll.y,"clientWidth":viewport.width,"clientHeight":viewport.height});
            let visual = json!({"offsetX":0,"offsetY":0,"pageX":scroll.x,"pageY":scroll.y,"clientWidth":viewport.width,"clientHeight":viewport.height,"scale":1,"zoom":1});
            let content = json!({"x":0,"y":0,"width":width,"height":height});
            return Ok(
                json!({"layoutViewport":layout,"visualViewport":visual,"contentSize":content,
                "cssLayoutViewport":layout,"cssVisualViewport":visual,"cssContentSize":content}),
            );
        }
        "addBinding" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .ok_or("Missing binding name")?;
            if name.is_empty() {
                return Err("Binding name must not be empty".into());
            }
            install_bindings(ctx, vec![name.to_owned()])?;
            return Ok(json!({}));
        }
        "setFocusEmulationEnabled" => {
            let enabled = params
                .get("enabled")
                .and_then(Value::as_bool)
                .ok_or("Missing enabled")?;
            super::dom_realm::set_focus_emulation(ctx, enabled);
            return Ok(json!({}));
        }
        "resolveNode" => {
            let node_id = params
                .get("nodeId")
                .or_else(|| params.get("backendNodeId"))
                .and_then(Value::as_u64)
                .and_then(|id| usize::try_from(id).ok())
                .ok_or("Missing node id")?;
            if !rd
                .borrow()
                .as_ref()
                .is_some_and(|document| document.document().get_node(node_id).is_some())
            {
                return Err(format!("Could not find node with given id: {node_id}"));
            }
            let value =
                super::create_render_element_object(ctx, rd.clone(), node_id, dirty.clone());
            let group = params.get("objectGroup").and_then(Value::as_str);
            return Ok(json!({"object":describe(value, false, group, ctx)?}));
        }
        "requestNode" => {
            let id = params
                .get("objectId")
                .and_then(Value::as_str)
                .ok_or("Missing objectId")?;
            let (value, _) = resolve(id, ctx)?;
            let object = value.as_object().ok_or("Remote value is not a DOM node")?;
            let node_id = super::dom_realm::node_id_in_document(ctx, &object, rd)
                .ok_or("Remote object is not a DOM node in this document")?;
            return Ok(json!({"nodeId":node_id}));
        }
        "releaseObject" => {
            let id = params
                .get("objectId")
                .and_then(Value::as_str)
                .ok_or("Missing objectId")?;
            if registry(ctx).borrow_mut().entries.remove(id).is_none() {
                return Err(format!("Could not find object with given id: {id}"));
            }
            return Ok(json!({}));
        }
        "releaseObjectGroup" => {
            let group = params
                .get("objectGroup")
                .and_then(Value::as_str)
                .ok_or("Missing objectGroup")?;
            registry(ctx)
                .borrow_mut()
                .entries
                .retain(|_, entry| entry.group.as_deref() != Some(group));
            return Ok(json!({}));
        }
        "getProperties" => return properties(&params, ctx),
        "evaluate" | "callFunctionOn" => {}
        _ => return Err(format!("Runtime.{method} not implemented")),
    }
    let mut inherited_group = None;
    let result = if method == "evaluate" {
        let expression = params
            .get("expression")
            .and_then(Value::as_str)
            .ok_or("Missing expression")?;
        if super::source_nesting_exceeds(expression) {
            Err(JsNativeError::syntax()
                .with_message("expression nesting depth exceeds the engine limit")
                .into())
        } else {
            ctx.eval(Source::from_bytes(expression))
        }
    } else {
        let declaration = params
            .get("functionDeclaration")
            .and_then(Value::as_str)
            .ok_or("Missing functionDeclaration")?;
        let this = if let Some(id) = params.get("objectId").and_then(Value::as_str) {
            let (value, group) = resolve(id, ctx)?;
            inherited_group = group;
            value
        } else {
            ctx.global_object().into()
        };
        let args = params
            .get("arguments")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .map(|arg| argument(arg, ctx))
            .collect::<Result<Vec<_>, _>>()?;
        let source = format!("({declaration}\n)");
        if super::source_nesting_exceeds(&source) {
            Err(JsNativeError::syntax()
                .with_message("expression nesting depth exceeds the engine limit")
                .into())
        } else {
            ctx.eval(Source::from_bytes(&source)).and_then(|function| {
                function
                    .as_callable()
                    .ok_or_else(|| {
                        JsError::from_native(
                            JsNativeError::typ()
                                .with_message("functionDeclaration is not a function"),
                        )
                    })?
                    .call(&this, &args, ctx)
            })
        }
    };
    let group = params
        .get("objectGroup")
        .and_then(Value::as_str)
        .or(inherited_group.as_deref());
    let result = result.and_then(|value| await_value(value, &params, ctx, queue));
    // Run ordinary microtasks even when awaitPromise was not requested.
    let _ = queue.run_background_jobs(ctx, 256);
    match result {
        Ok(value) => {
            let by_value = params
                .get("returnByValue")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Ok(json!({"result":describe(value, by_value, group, ctx)?}))
        }
        Err(error) => Ok(exception(error, group, ctx)),
    }
}
