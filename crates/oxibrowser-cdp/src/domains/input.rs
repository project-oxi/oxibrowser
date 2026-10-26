//! CDP Input domain handler.
//!
//! Handles keyboard and mouse input simulation for testing and automation.
//! - Input.dispatchKeyEvent — keyboard events (keydown, keyup, rawKeyDown, char)
//! - Input.dispatchMouseEvent — mouse events (mousePressed, mouseReleased, mouseMoved)
//!
//! Implementation: dispatches real DOM events via JS evaluation.
//! - Keyboard events → `document.activeElement.dispatchEvent(new KeyboardEvent(...))`
//! - Mouse events → `document.elementFromPoint(x, y).dispatchEvent(new MouseEvent(...))`
//! - insertText → updates input/textarea values via JS

use crate::domains::{DispatchContext, DomainResult};
use crate::protocol::CdpError;
use oxibrowser_core::js::{
    js_dispatch_drag_event, js_dispatch_key_event, js_dispatch_mouse_event, js_insert_text,
    js_scroll,
};
use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};

/// Dispatch Input domain methods.
pub async fn handle(method: &str, params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    match method {
        "dispatchKeyEvent" => dispatch_key_event(params, ctx).await,
        "dispatchMouseEvent" => dispatch_mouse_event(params, ctx).await,
        "dispatchDragEvent" => dispatch_drag_event(params, ctx).await,
        "insertText" => insert_text(params, ctx).await,
        "imeSetComposition" => ime_set_composition(params, ctx).await,
        "synthesizeScrollGesture" => synthesize_scroll_gesture(params, ctx).await,
        _ => Err(CdpError {
            code: -32601,
            message: format!("Input.{} not implemented", method),
        }),
    }
}

// ---------------------------------------------------------------------------
// Keyboard events
// ---------------------------------------------------------------------------

/// Input.dispatchKeyEvent — simulate keyboard events.
///
/// Dispatches a real `KeyboardEvent` on `document.activeElement` via JS.
async fn dispatch_key_event(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let p = params.ok_or_else(|| CdpError {
        code: -32602,
        message: "dispatchKeyEvent requires parameters".to_string(),
    })?;

    let event_type = p.get("type").and_then(|v| v.as_str()).unwrap_or("keyDown");
    let key = p.get("key").and_then(|v| v.as_str()).unwrap_or("");
    let code = p.get("code").and_then(|v| v.as_str()).unwrap_or("");
    let modifiers = calculate_modifiers(&p);

    // Timestamp from params (or current time)
    let timestamp = p
        .get("timestamp")
        .and_then(|v| v.as_f64())
        .unwrap_or_else(current_timestamp_ms);

    tracing::debug!(event_type, key, code, modifiers, "Input.dispatchKeyEvent");

    // Generate JS to dispatch the keyboard event
    let js = js_dispatch_key_event(key, code, event_type, modifiers, timestamp);
    let mut session_guard = ctx.session.write().await;
    let result = session_guard.evaluate_js(&js).await;

    match result {
        Ok(val) => {
            tracing::trace!(?val, "dispatchKeyEvent result");
            ctx.events.send_page_event(
                "Input.dispatchKeyEvent",
                json!({
                    "type": event_type,
                    "key": key,
                    "code": code,
                    "modifiers": modifiers,
                }),
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, "dispatchKeyEvent JS eval failed");
        }
    }

    Ok(Some(json!({
        "timestamp": timestamp,
    })))
}

/// Input.insertText — insert text as if typed (IME composition).
async fn insert_text(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let p = params.ok_or_else(|| CdpError {
        code: -32602,
        message: "insertText requires parameters".to_string(),
    })?;

    let text = p.get("text").and_then(|v| v.as_str()).unwrap_or("");
    tracing::debug!(text, "Input.insertText");

    let js = js_insert_text(text);
    let mut session_guard = ctx.session.write().await;
    let _ = session_guard.evaluate_js(&js).await;

    Ok(Some(json!({})))
}

/// Input.imeSetComposition — set IME composition.
async fn ime_set_composition(params: Option<Value>, _ctx: &DispatchContext) -> DomainResult {
    let p = params.ok_or_else(|| CdpError {
        code: -32602,
        message: "imeSetComposition requires parameters".to_string(),
    })?;

    let selections = p
        .get("segments")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    tracing::debug!(selections, "Input.imeSetComposition");

    Ok(Some(json!({})))
}

// ---------------------------------------------------------------------------
// Mouse events
// ---------------------------------------------------------------------------

/// Input.dispatchMouseEvent — simulate mouse events with browser-like sequences.
///
/// Fires realistic event sequences:
/// - mousePressed → mousedown
/// - mouseReleased → mouseup + click (left button only)
/// - mouseMoved → mousemove
async fn dispatch_mouse_event(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let p = params.ok_or_else(|| CdpError {
        code: -32602,
        message: "dispatchMouseEvent requires parameters".to_string(),
    })?;

    let event_type = p
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("mouseMoved");
    let x = p.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let y = p.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let button = p.get("button").and_then(|v| v.as_str()).unwrap_or("none");
    let click_count = p.get("clickCount").and_then(|v| v.as_i64()).unwrap_or(0) as u32;

    tracing::debug!(
        event_type,
        x,
        y,
        button,
        click_count,
        "Input.dispatchMouseEvent"
    );

    // Determine event sequence based on CDP event type
    let mut events: Vec<&str> = Vec::new();
    match event_type {
        "mousePressed" => {
            events.push("mousedown");
        }
        "mouseReleased" => {
            events.push("mouseup");
            // Click only fires for left-button primary clicks
            if button == "none" || button == "left" {
                events.push("click");
            }
        }
        "mouseMoved" => {
            events.push("mousemove");
        }
        _ => {
            events.push(event_type);
        }
    }

    let mut session_guard = ctx.session.write().await;
    for &evt in &events {
        let js = js_dispatch_mouse_event(x, y, evt, button, click_count);
        match session_guard.evaluate_js(&js).await {
            Ok(val) => {
                tracing::trace!(event = evt, ?val, "dispatchMouseEvent event");
                ctx.events.send_page_event(
                    "Input.dispatchMouseEvent",
                    json!({
                        "type": evt,
                        "x": x,
                        "y": y,
                        "button": button,
                        "clickCount": click_count,
                    }),
                );
            }
            Err(e) => {
                tracing::warn!(event = evt, error = %e, "dispatchMouseEvent JS eval failed");
            }
        }
    }

    Ok(Some(json!({})))
}

/// Input.dispatchDragEvent — simulate drag events.
async fn dispatch_drag_event(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let p = params.ok_or_else(|| CdpError {
        code: -32602,
        message: "dispatchDragEvent requires parameters".to_string(),
    })?;

    let event_type = p.get("type").and_then(|v| v.as_str()).unwrap_or("dragOver");
    let x = p.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let y = p.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);

    tracing::debug!(event_type, x, y, "Input.dispatchDragEvent");

    let js = js_dispatch_drag_event(x, y, event_type);
    let mut session_guard = ctx.session.write().await;
    let _result = session_guard.evaluate_js(&js).await;

    Ok(Some(json!({})))
}

/// Input.synthesizeScrollGesture — scroll the page by the gesture delta.
///
/// `xDelta`/`yDelta` (falling back to the spec's `xDistance`/`yDistance`)
/// are applied as an instant scroll-by via `js::mouse::js_scroll`. Positive
/// deltas scroll right/down, matching `scrollLeft`/`scrollTop` semantics.
/// `gestureType`, `x`/`y` and the speed/repeat parameters are accepted but
/// ignored — no timed pointer sequence is synthesized.
async fn synthesize_scroll_gesture(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let p = params.unwrap_or_default();
    let dx = p
        .get("xDelta")
        .or_else(|| p.get("xDistance"))
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let dy = p
        .get("yDelta")
        .or_else(|| p.get("yDistance"))
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);

    tracing::debug!(dx, dy, "Input.synthesizeScrollGesture");

    let js = js_scroll(dx, dy);
    let mut session_guard = ctx.session.write().await;
    let _result = session_guard.evaluate_js(&js).await;

    Ok(Some(json!({})))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Translate CDP modifiers into the shared input generator's flags.
fn calculate_modifiers(params: &serde_json::Value) -> u32 {
    let mut m = 0u32;
    let cdp = params
        .get("modifiers")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    // CDP uses Alt=1, Control=2, Meta=4, Shift=8. The shared input
    // generator uses Alt=2, Control=4, Meta=16, Shift=8 instead.
    for (wire, internal) in [(1, 2), (2, 4), (4, 16), (8, 8)] {
        if cdp & wire != 0 {
            m |= internal;
        }
    }
    if params
        .get("shiftKey")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        m |= 8;
    }
    if params
        .get("ctrlKey")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        m |= 4;
    }
    if params
        .get("altKey")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        m |= 2;
    }
    if params
        .get("metaKey")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        m |= 16;
    }
    m
}

/// Get current timestamp in milliseconds since epoch.
fn current_timestamp_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1000.0
}

#[cfg(test)]
mod tests {
    use super::calculate_modifiers;
    use serde_json::json;

    #[test]
    fn cdp_keyboard_modifier_bits_keep_their_meaning() {
        for (wire, internal) in [(0, 0), (1, 2), (2, 4), (4, 16), (8, 8), (15, 30)] {
            assert_eq!(calculate_modifiers(&json!({"modifiers": wire})), internal);
        }
        assert_eq!(calculate_modifiers(&json!({"modifiers": -1})), 0);
        assert_eq!(
            calculate_modifiers(&json!({"modifiers": 1, "ctrlKey": true, "shiftKey": true})),
            14,
        );
        assert_eq!(
            calculate_modifiers(&json!({"altKey": true, "metaKey": true})),
            18,
        );
    }
}
