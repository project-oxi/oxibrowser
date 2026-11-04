//! CDP Input domain handler.
//!
//! Handles keyboard and mouse input simulation for testing and automation.
//! - Input.dispatchKeyEvent — keyboard events (keydown, keyup, rawKeyDown, char)
//! - Input.dispatchMouseEvent — mouse events (mousePressed, mouseReleased, mouseMoved)
//!
//! Keyboard and text input update the native renderer control value and
//! dispatch cancellable DOM events. Mouse input dispatches the browser event
//! sequence and applies the primary mousedown focus default action.

use crate::domains::{DispatchContext, DomainResult};
use crate::protocol::CdpError;
use oxibrowser_core::js::{js_dispatch_drag_event, js_scroll};
use serde_json::{Value, json};

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
    super::runtime::remote("dispatchKeyEvent", params, ctx).await
}

/// Input.insertText — insert text as if typed (IME composition).
async fn insert_text(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    super::runtime::remote("insertText", params, ctx).await
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
    super::runtime::remote("dispatchMouseEvent", params, ctx).await
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
