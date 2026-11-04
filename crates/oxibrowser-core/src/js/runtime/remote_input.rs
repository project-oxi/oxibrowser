//! Host editing for CDP. Values bypass page-defined property setters; events
//! still pass through the owning DOM event targets and cancellation rules.
use super::super::*;
use serde_json::json;

fn js<T>(result: JsResult<T>) -> std::result::Result<T, String> {
    result.map_err(|error| error.to_string())
}

fn active(
    ctx: &mut Context,
    rd: &Rc<RefCell<Option<RenderDocument>>>,
    dirty: &Arc<AtomicBool>,
) -> std::result::Result<JsObject, String> {
    dom_realm::active_element(ctx, rd, dirty)
        .as_object()
        .ok_or_else(|| "No active document element".into())
}

fn focus_native(
    ctx: &mut Context,
    rd: &Rc<RefCell<Option<RenderDocument>>>,
    dirty: &Arc<AtomicBool>,
    node: usize,
) -> std::result::Result<(), String> {
    let previous = dom_realm::focused_node(rd);
    if previous == Some(node) {
        return Ok(());
    }
    dom_realm::set_focused(ctx, rd, Some(node));
    if let Some(old) = previous {
        js(dom_realm::render_event(
            ctx, rd, dirty, old, "blur", false, false,
        ))?;
        js(dom_realm::render_event(
            ctx, rd, dirty, old, "focusout", true, false,
        ))?;
    }
    js(dom_realm::render_event(
        ctx, rd, dirty, node, "focus", false, false,
    ))?;
    js(dom_realm::render_event(
        ctx, rd, dirty, node, "focusin", true, false,
    ))?;
    Ok(())
}

fn dispatch(
    ctx: &mut Context,
    target: &JsObject,
    rd: &Rc<RefCell<Option<RenderDocument>>>,
    dirty: &Arc<AtomicBool>,
    _constructor: &str,
    kind: &str,
    init: Value,
) -> std::result::Result<bool, String> {
    let event = dom_realm::synthetic_event(
        ctx,
        kind,
        init.get("bubbles")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        init.get("cancelable")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    )
    .as_object()
    .ok_or("Native event construction failed")?;
    if let Some(fields) = init.as_object() {
        for (name, value) in fields {
            let value = js(JsValue::from_json(value, ctx))?;
            js(event.create_data_property_or_throw(JsString::from(name.as_str()), value, ctx))?;
        }
    }
    js(event.define_property_or_throw(
        js_string!("isTrusted"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(true)
            .writable(false)
            .configurable(false),
        ctx,
    ))?;
    let node = dom_realm::node_id_in_document(ctx, target, rd)
        .ok_or("Input target is not a live native node")?;
    js(dispatch_render_event(
        ctx,
        rd.clone(),
        dirty.clone(),
        node,
        event.into(),
    ))
}

fn focus_next(
    ctx: &mut Context,
    rd: &Rc<RefCell<Option<RenderDocument>>>,
    dirty: &Arc<AtomicBool>,
    current: &JsObject,
    reverse: bool,
) -> std::result::Result<(), String> {
    let current = dom_realm::node_id_in_document(ctx, current, rd);
    let mut nodes = Vec::new();
    {
        let cell = rd.borrow();
        let document = cell.as_ref().ok_or("No rendered document")?;
        let mut stack = vec![document.document().root_node().id];
        while let Some(node) = stack.pop() {
            let children = document.children_ids(node);
            stack.extend(children.into_iter().rev());
            if document.node_attr(node, "disabled").is_some()
                || document.node_attr(node, "hidden").is_some()
                || document.node_attr(node, "tabindex").as_deref() == Some("-1")
            {
                continue;
            }
            let tag = document.tag_name(node).unwrap_or_default();
            if (matches!(tag.as_str(), "input" | "textarea" | "select" | "button")
                || (tag == "a" && document.node_attr(node, "href").is_some())
                || document.node_attr(node, "tabindex").is_some())
                && document.node_attr(node, "type").as_deref() != Some("hidden")
            {
                nodes.push(node);
            }
        }
    }
    if nodes.is_empty() {
        return Ok(());
    }
    let next = match nodes.iter().position(|node| Some(*node) == current) {
        Some(index) if reverse => (index + nodes.len() - 1) % nodes.len(),
        Some(index) => (index + 1) % nodes.len(),
        None if reverse => nodes.len() - 1,
        None => 0,
    };
    focus_native(ctx, rd, dirty, nodes[next])?;
    Ok(())
}

fn edit(
    ctx: &mut Context,
    target: &JsObject,
    rd: &Rc<RefCell<Option<RenderDocument>>>,
    dirty: &Arc<AtomicBool>,
    text: &str,
    key: &str,
) -> std::result::Result<(), String> {
    let Some(node) = dom_realm::node_id_in_document(ctx, target, rd) else {
        return Ok(());
    };
    let value = {
        let cell = rd.borrow();
        let document = cell.as_ref().ok_or("No rendered document")?;
        if !matches!(
            document.tag_name(node).as_deref(),
            Some("input" | "textarea")
        ) || document.node_attr(node, "disabled").is_some()
            || document.node_attr(node, "readonly").is_some()
            || matches!(
                document.node_attr(node, "type").as_deref(),
                Some("checkbox" | "radio" | "file" | "submit" | "button" | "reset" | "hidden")
            )
        {
            return Ok(());
        }
        document.node_attr(node, "value").unwrap_or_else(|| {
            if document.tag_name(node).as_deref() == Some("textarea") {
                document.node_text(node)
            } else {
                String::new()
            }
        })
    };
    let mut units: Vec<u16> = value.encode_utf16().collect();
    let (mut start, mut end) =
        dom_realm::control_selection(rd, node).unwrap_or((units.len(), units.len()));
    start = start.min(units.len());
    end = end.max(start).min(units.len());
    let input_type = match key {
        "Backspace" => "deleteContentBackward",
        "Delete" => "deleteContentForward",
        _ => "insertText",
    };
    match key {
        "ArrowLeft" => {
            start = start.saturating_sub(1);
            end = start;
        }
        "ArrowRight" => {
            start = (end + 1).min(units.len());
            end = start;
        }
        _ => {
            if key == "Backspace" && start == end && start > 0 {
                start -= 1;
                if (0xDC00..=0xDFFF).contains(&units[start]) && start > 0 {
                    start -= 1;
                }
            }
            if key == "Delete" && start == end && end < units.len() {
                end += 1;
                if (0xD800..=0xDBFF).contains(&units[end - 1]) && end < units.len() {
                    end += 1;
                }
            }
            if text.is_empty() && start == end {
                return Ok(());
            }
            if !dispatch(
                ctx,
                target,
                rd,
                dirty,
                "InputEvent",
                "beforeinput",
                json!({"bubbles":true,"cancelable":true,"inputType":input_type,"data":text}),
            )? {
                return Ok(());
            }
            let insert: Vec<u16> = text.encode_utf16().collect();
            units.splice(start..end, insert.iter().copied());
            start += insert.len();
            end = start;
            let value = String::from_utf16_lossy(&units);
            if let Some(document) = rd.borrow_mut().as_mut() {
                document.set_attribute(node, "value", &value);
            }
            dirty.store(true, Ordering::Relaxed);
            // Update the native backing store before dispatch: a React value
            // tracker on the page's setter must observe this as user input.
            dom_realm::set_control_selection(rd, node, start, end);
            dispatch(
                ctx,
                target,
                rd,
                dirty,
                "InputEvent",
                "input",
                json!({"bubbles":true,"cancelable":false,"inputType":input_type,"data":text}),
            )?;
            return Ok(());
        }
    }
    dom_realm::set_control_selection(rd, node, start, end);
    Ok(())
}

pub(super) fn handle(
    method: &str,
    params: &Value,
    ctx: &mut Context,
    rd: &Rc<RefCell<Option<RenderDocument>>>,
    dirty: &Arc<AtomicBool>,
) -> std::result::Result<Value, String> {
    if method == "dispatchMouseEvent" {
        let kind = params
            .get("type")
            .and_then(Value::as_str)
            .ok_or("Missing mouse event type")?;
        let x = params.get("x").and_then(Value::as_f64).ok_or("Missing x")?;
        let y = params.get("y").and_then(Value::as_f64).ok_or("Missing y")?;
        if !x.is_finite() || !y.is_finite() {
            return Err("Mouse point must be finite".into());
        }
        let button = params
            .get("button")
            .and_then(Value::as_str)
            .unwrap_or("none");
        if !matches!(button, "left" | "middle" | "right" | "none") {
            return Err("Unsupported mouse button".into());
        }
        let clicks = params
            .get("clickCount")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(u64::from(u32::MAX)) as u32;
        let events: &[&str] = match kind {
            "mouseMoved" => &["mousemove"],
            "mousePressed" => &["mousedown"],
            "mouseReleased" if matches!(button, "left" | "none") => &["mouseup", "click"],
            "mouseReleased" => &["mouseup"],
            _ => return Err("Unsupported mouse event type".into()),
        };
        for event in events {
            let node = {
                let mut cell = rd.borrow_mut();
                let document = cell.as_mut().ok_or("No rendered document")?;
                document.flush_layout();
                let scroll = document.document().viewport_scroll();
                let Some(hit) = document
                    .document()
                    .hit((x + scroll.x) as f32, (y + scroll.y) as f32)
                else {
                    continue;
                };
                let mut node = hit.node_id;
                while document.node_kind(node) != Some("element") {
                    let Some(parent) = document.parent_id(node) else {
                        break;
                    };
                    node = parent;
                }
                node
            };
            let target = create_render_element_object(ctx, rd.clone(), node, dirty.clone())
                .as_object()
                .ok_or("Mouse target has no native wrapper")?;
            let button_code = match button {
                "left" => 0,
                "middle" => 1,
                "right" => 2,
                _ => -1,
            };
            let buttons = if *event == "mousedown" {
                match button {
                    "left" => 1,
                    "right" => 2,
                    "middle" => 4,
                    _ => 0,
                }
            } else {
                0
            };
            let allowed = dispatch(
                ctx,
                &target,
                rd,
                dirty,
                "MouseEvent",
                event,
                json!({"bubbles":true,"cancelable":true,"clientX":x,"clientY":y,"button":button_code,"buttons":buttons,"detail":clicks}),
            )?;
            if allowed && *event == "mousedown" && button == "left" {
                let focusable = {
                    let cell = rd.borrow();
                    let document = cell.as_ref().ok_or("No rendered document")?;
                    let mut current = Some(node);
                    let mut found = None;
                    while let Some(node) = current {
                        let tag = document.tag_name(node).unwrap_or_default();
                        if document.node_attr(node, "disabled").is_none()
                            && (matches!(tag.as_str(), "input" | "textarea" | "select" | "button")
                                || (tag == "a" && document.node_attr(node, "href").is_some())
                                || document.node_attr(node, "tabindex").is_some())
                        {
                            found = Some(node);
                            break;
                        }
                        current = document.parent_id(node);
                    }
                    found
                };
                if let Some(node) = focusable {
                    focus_native(ctx, rd, dirty, node)?;
                }
            }
        }
        return Ok(json!({}));
    }
    let target = active(ctx, rd, dirty)?;
    if method == "insertText" {
        let text = params
            .get("text")
            .and_then(Value::as_str)
            .ok_or("Missing text")?;
        edit(ctx, &target, rd, dirty, text, "")?;
        return Ok(json!({}));
    }
    let kind = params
        .get("type")
        .and_then(Value::as_str)
        .ok_or("Missing key event type")?;
    let event = match kind {
        "keyDown" | "rawKeyDown" => "keydown",
        "keyUp" => "keyup",
        "char" => "keypress",
        _ => return Err("Unsupported key event type".into()),
    };
    let key = params.get("key").and_then(Value::as_str).unwrap_or("");
    let code = params.get("code").and_then(Value::as_str).unwrap_or("");
    let modifiers = params.get("modifiers").and_then(Value::as_u64).unwrap_or(0);
    let allowed = dispatch(
        ctx,
        &target,
        rd,
        dirty,
        "KeyboardEvent",
        event,
        json!({"key":key,"code":code,"bubbles":true,"cancelable":true,
        "shiftKey":modifiers&8!=0,"ctrlKey":modifiers&2!=0,"altKey":modifiers&1!=0,"metaKey":modifiers&4!=0,"repeat":params.get("autoRepeat").and_then(Value::as_bool).unwrap_or(false)}),
    )?;
    if allowed && matches!(kind, "keyDown" | "rawKeyDown" | "char") {
        if key == "Tab" {
            focus_next(ctx, rd, dirty, &target, modifiers & 8 != 0)?;
        } else if matches!(key, "Backspace" | "Delete" | "ArrowLeft" | "ArrowRight") {
            edit(ctx, &target, rd, dirty, "", key)?;
        } else if kind != "rawKeyDown" && modifiers & (1 | 2 | 4) == 0 {
            let text = params
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or(if kind == "char" { key } else { "" });
            if !text.is_empty() && key != "Enter" {
                edit(ctx, &target, rd, dirty, text, "")?;
            }
        }
    }
    Ok(json!({}))
}
