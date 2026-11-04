//! JS code generators for Input domain dispatch.
//!
//! These functions generate JavaScript code strings that are evaluated
//! via `JsRuntime.evaluate()` to dispatch real DOM events on elements.
//!
//! This approach is used instead of native DOM mutation because we don't
//! have a pixel layout engine. Instead, we dispatch standard Web Events
//! (MouseEvent, KeyboardEvent) that JavaScript code can already handle.

/// Generate JS to dispatch a mouse event at viewport coordinates.
///
/// Uses `document.elementFromPoint(x, y)` to find the element under the cursor,
/// then dispatches a MouseEvent on it. This fires JS event listeners registered
/// via `addEventListener` just like a real browser would.
pub fn js_dispatch_mouse_event(
    x: f64,
    y: f64,
    event_type: &str,
    button: &str,
    click_count: u32,
) -> String {
    let button_code = match button {
        "left" => 0,
        "middle" => 1,
        "right" => 2,
        _ => 0,
    };
    // event_type comes from CDP which defines fixed enum values, but escape anyway
    let event_type_json = serde_json::to_string(event_type).unwrap_or_default();
    format!(
        r#"(function() {{
            var el = document.elementFromPoint({x}, {y});
            if (el) {{
                var allowed = el.dispatchEvent(new MouseEvent({event_type_json}, {{
                    bubbles: true,
                    cancelable: true,
                    clientX: {x},
                    clientY: {y},
                    button: {button_code},
                    buttons: {button_code},
                    detail: {click_count}
                }}));
                if (allowed && {event_type_json} === 'mousedown' && {button_code} === 0) {{
                    var focusable = el;
                    while (focusable && !/^(INPUT|TEXTAREA|SELECT|BUTTON|A)$/.test(focusable.tagName || '') && !focusable.hasAttribute('tabindex')) focusable = focusable.parentElement;
                    if (focusable && !focusable.disabled && typeof focusable.focus === 'function') focusable.focus();
                }}
            }}
            return el ? el.tagName : null;
        }})()"#
    )
}

/// Generate JS to dispatch a keyboard event on the active element.
///
/// Dispatches a KeyboardEvent on `document.activeElement`. For printable
/// key events (type="char"), also updates the value of input/textarea elements.
pub fn js_dispatch_key_event(
    key: &str,
    code: &str,
    event_type: &str,
    modifiers: u32,
    timestamp: f64,
) -> String {
    // Build modifiers object string
    let shift = (modifiers & 8) != 0;
    let ctrl = (modifiers & 4) != 0;
    let alt = (modifiers & 2) != 0;
    let meta = (modifiers & 16) != 0;

    let dom_event_type = match event_type {
        "keyDown" | "rawKeyDown" => "keydown",
        "keyUp" => "keyup",
        "char" => "keypress",
        other => other,
    };
    let insert_character = event_type == "char";
    // Safely escape key/code/event_type using serde_json
    let key_json = serde_json::to_string(key).unwrap_or_default();
    let code_json = serde_json::to_string(code).unwrap_or_default();
    let event_type_json = serde_json::to_string(dom_event_type).unwrap_or_default();

    format!(
        r#"(function() {{
            var el = document.activeElement || document.body || document.documentElement || document;
            var dispatched = false;
            if (el) {{
                dispatched = el.dispatchEvent(new KeyboardEvent({event_type_json}, {{
                    bubbles: true,
                    cancelable: true,
                    key: {key_json},
                    code: {code_json},
                    shiftKey: {shift},
                    ctrlKey: {ctrl},
                    altKey: {alt},
                    metaKey: {meta},
                    timestamp: {timestamp}
                }}));

                // For printable char events, also update input values
                if ({insert_character} && dispatched) {{
                    if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA') {{
                        var start = typeof el.selectionStart === 'number' ? el.selectionStart : el.value.length;
                        var end = typeof el.selectionEnd === 'number' ? el.selectionEnd : el.value.length;
                        el.value = el.value.substring(0, start) + {key_json} + el.value.substring(end);
                        el.selectionStart = el.selectionEnd = start + {key_json}.length;
                        el.dispatchEvent(new Event('input', {{ bubbles: true }}));
                        el.dispatchEvent(new Event('change', {{ bubbles: true }}));
                    }}
                }}
            }}
            return el ? el.tagName : null;
        }})()"#,
        shift = shift,
        ctrl = ctrl,
        alt = alt,
        meta = meta,
        timestamp = timestamp
    )
}

/// Generate JS for Input.insertText — type text into the focused element.
pub fn js_insert_text(text: &str) -> String {
    // Safely escape text using serde_json (handles quotes, backslashes, etc.)
    let text_json = serde_json::to_string(text).unwrap_or_default();
    // Use character count, not byte length, for cursor positioning
    let char_count = text.chars().count();
    format!(
        r#"(function() {{
            var el = document.activeElement;
            if (el) {{
                if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA') {{
                    var start = el.selectionStart || el.value.length;
                    var end = el.selectionEnd || el.value.length;
                    el.value = el.value.substring(0, start) + {text_json} + el.value.substring(end);
                    el.selectionStart = el.selectionEnd = start + {char_count};
                    el.dispatchEvent(new Event('input', {{ bubbles: true }}));
                    el.dispatchEvent(new Event('change', {{ bubbles: true }}));
                }}
            }}
            return el ? el.tagName : null;
        }})()"#,
    )
}

/// Generate JS to dispatch a drag event at viewport coordinates.
pub fn js_dispatch_drag_event(x: f64, y: f64, event_type: &str) -> String {
    let event_type_json = serde_json::to_string(event_type).unwrap_or_default();
    format!(
        r#"(function() {{
            var el = document.elementFromPoint({x}, {y});
            if (el) {{
                el.dispatchEvent(new DragEvent({event_type_json}, {{
                    bubbles: true,
                    cancelable: true,
                    clientX: {x},
                    clientY: {y}
                }}));
            }}
            return el ? el.tagName : null;
        }})()"#
    )
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mouse_click_generates_valid_js() {
        let js = js_dispatch_mouse_event(100.0, 200.0, "mousePressed", "left", 1);
        assert!(js.contains("document.elementFromPoint(100, 200)"));
        assert!(js.contains("MouseEvent"));
        assert!(js.contains("mousePressed"));
        assert!(js.contains("clientX"));
        assert!(js.contains("bubbles"));
    }

    #[test]
    fn test_mouse_button_codes() {
        assert!(js_dispatch_mouse_event(0.0, 0.0, "mousedown", "left", 1).contains("button: 0"));
        assert!(js_dispatch_mouse_event(0.0, 0.0, "mousedown", "right", 1).contains("button: 2"));
        assert!(js_dispatch_mouse_event(0.0, 0.0, "mousedown", "middle", 1).contains("button: 1"));
    }

    #[test]
    fn test_keydown_generates_valid_js() {
        let js = js_dispatch_key_event("Enter", "Enter", "keyDown", 0, 0.0);
        assert!(js.contains("KeyboardEvent"));
        assert!(js.contains("keydown"));
        assert!(js.contains("key: \"Enter\""));
        assert!(js.contains("code: \"Enter\""));
    }

    #[test]
    fn test_keydown_with_modifiers() {
        let js = js_dispatch_key_event("a", "KeyA", "keyDown", 12, 0.0); // ctrl+shift
        assert!(js.contains("ctrlKey: true"));
        assert!(js.contains("shiftKey: true"));
        assert!(js.contains("altKey: false"));
        assert!(js.contains("metaKey: false"));
    }

    #[test]
    fn test_char_event_updates_input_value() {
        let js = js_dispatch_key_event("x", "KeyX", "char", 0, 0.0);
        assert!(js.contains("selectionStart"));
        assert!(js.contains("input"));
        assert!(js.contains("change"));
    }

    #[test]
    fn test_insert_text() {
        let js = js_insert_text("hello world");
        assert!(js.contains("hello world"));
        assert!(js.contains("selectionStart"));
        assert!(js.contains("input"));
    }

    #[test]
    fn test_insert_text_escapes_quotes() {
        // Both single and double quotes must be properly escaped via serde_json
        let js = js_insert_text(r#"it's a "test""#);
        // serde_json will produce: "it's a \"test\""
        assert!(js.contains("it's a \\\"test\\\""));
    }

    #[test]
    fn test_insert_text_unicode_char_count() {
        // Multi-byte UTF-8 character: é is 2 bytes but 1 character
        let js = js_insert_text("é");
        // Should use character count (1), not byte length (2)
        assert!(
            js.contains("start + 1"),
            "Should use char count not byte length"
        );
        assert!(!js.contains("start + 2"), "Should NOT use byte length");
    }
}
