//! CDP Page domain handler.
//!
//! Handles Page.enable, Page.disable, Page.navigate, Page.reload,
//! Page.getFrameTree, Page.getFrameMetrics, Page.captureScreenshot,
//! Page.printToPDF.
//!
//! After Page.enable, navigation events are emitted:
//! - Page.frameNavigated
//! - Page.domContentLoadedEventFired
//! - Page.loadEventFired

use crate::domains::fetch;
use crate::domains::network;
use crate::domains::{DispatchContext, DomainResult, PausedRequest};
use crate::event::EventSender;
use crate::protocol::CdpError;
use serde_json::{json, Value};

/// Dispatch Page domain methods.
pub async fn handle(method: &str, params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    match method {
        "enable" => enable(ctx),
        "disable" => disable(ctx),
        "navigate" => navigate(params, ctx).await,
        "reload" => reload(params, ctx).await,
        "getFrameTree" => get_frame_tree(ctx).await,
        "getFrameMetrics" => get_frame_metrics(),
        "captureScreenshot" => capture_screenshot(params, ctx).await,
        "printToPDF" => print_to_pdf(params),
        "getLifecycleEvents" => Ok(Some(json!({ "events": [] }))),
        "setLifecycleEventsEnabled" => set_lifecycle_events_enabled(params, ctx),
        _ => Err(CdpError {
            code: -32601,
            message: format!("Page.{} not implemented", method),
        }),
    }
}

/// Page.enable — enables page domain events.
fn enable(ctx: &DispatchContext) -> DomainResult {
    ctx.events.set_page_enabled(true);
    Ok(Some(json!({})))
}

/// Page.disable — disables page domain events.
fn disable(ctx: &DispatchContext) -> DomainResult {
    ctx.events.set_page_enabled(false);
    Ok(Some(json!({})))
}

/// Page.setLifecycleEventsEnabled — controls lifecycle event emission.
fn set_lifecycle_events_enabled(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_default();
    let enabled = params
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    ctx.events.set_page_enabled(enabled);
    Ok(Some(json!({})))
}

/// Page.navigate — navigates to a URL using the real browser session.
///
/// After navigation completes, emits CDP events:
/// - Page.frameNavigated
/// - Page.domContentLoadedEventFired
/// - Page.loadEventFired
///
/// If Fetch domain is enabled and URL matches a pattern, emits
/// Fetch.requestPaused BEFORE the navigation and stores the request.
async fn navigate(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_default();
    let url = params
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("about:blank");

    let loader_id = format!("LID-{}", uuid::Uuid::new_v4().as_simple());

    // Pre-fetch: check fetch patterns before acquiring session lock
    let (request_id, should_pause) = {
        if ctx.events.is_fetch_enabled() {
            let patterns = ctx.events.get_fetch_patterns();
            if fetch::matches_patterns(url, &patterns) {
                let req_id = format!("REQ-{}", uuid::Uuid::new_v4().as_simple());
                (req_id, true)
            } else {
                (String::new(), false)
            }
        } else {
            (String::new(), false)
        }
    };

    // If matched, emit requestPaused BEFORE navigation (with session lock held)
    if should_pause {
        let paused = PausedRequest {
            request_id: request_id.clone(),
            url: url.to_string(),
            method: "GET".to_string(),
            resource_type: "Document".to_string(),
            frame_id: String::new(),
            headers: serde_json::Map::new(),
        };
        ctx.paused_requests.write().insert(request_id.clone(), paused);

        // Emit event before navigation
        fetch::emit_request_paused(&ctx.events, &request_id, url, "GET", &[], "Document");
    }

    let mut guard = ctx.session.write().await;
    match guard.navigate(url).await {
        Ok(()) => {
            // Clean up: remove from paused_requests after navigation
            if should_pause {
                ctx.paused_requests.write().remove(&request_id);
            }

            // Capture timestamp after navigation completes
            let timestamp = EventSender::timestamp_ms();
            let frame_id = guard
                .page()
                .map(|p| p.root_frame().id().to_string())
                .unwrap_or_else(|| "main".to_string());

            let final_url = guard
                .current_url()
                .map(|u| u.to_string())
                .unwrap_or_else(|| url.to_string());

            // Emit CDP events
            ctx.events.send_page_event(
                "Page.frameNavigated",
                json!({
                    "frame": {
                        "id": frame_id,
                        "loaderId": loader_id,
                        "url": final_url,
                        "domainAndRegistry": "",
                        "securityOrigin": final_url,
                        "mimeType": "text/html",
                        "adFrameStatus": { "adFrameType": "none" },
                        "secureContextType": "Secure",
                        "crossOriginIsolatedContextType": "NotIsolated",
                    },
                    "type": "Navigation"
                }),
            );

            ctx.events.send_page_event(
                "Page.domContentLoadedEventFired",
                json!({ "timestamp": timestamp }),
            );

            ctx.events
                .send_page_event("Page.loadEventFired", json!({ "timestamp": timestamp }));

            // Emit network lifecycle events if Network domain is enabled
            network::emit_navigation_events(
                &ctx.events,
                &request_id,
                &final_url,
                &loader_id,
                200,
                "text/html",
            );

            Ok(Some(json!({
                "frameId": frame_id,
                "loaderId": loader_id,
                "errorText": Value::Null
            })))
        }
        Err(e) => {
            // Clean up on error
            if should_pause {
                ctx.paused_requests.write().remove(&request_id);
            }
            Err(CdpError {
                code: -32000,
                message: format!("Navigation failed: {e}"),
            })
        }
    }
}

/// Page.reload — reloads the current page and emits lifecycle events.
async fn reload(_params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let loader_id = format!("LID-{}", uuid::Uuid::new_v4().as_simple());

    let mut guard = ctx.session.write().await;
    match guard.reload().await {
        Ok(()) => {
            // Capture timestamp after reload completes
            let timestamp = EventSender::timestamp_ms();
            let frame_id = guard
                .page()
                .map(|p| p.root_frame().id().to_string())
                .unwrap_or_else(|| "main".to_string());

            let final_url = guard
                .current_url()
                .map(|u| u.to_string())
                .unwrap_or_else(|| "about:blank".to_string());

            ctx.events.send_page_event(
                "Page.frameNavigated",
                json!({
                    "frame": {
                        "id": frame_id,
                        "loaderId": loader_id,
                        "url": final_url,
                        "mimeType": "text/html",
                    },
                    "type": "Navigation"
                }),
            );

            ctx.events.send_page_event(
                "Page.domContentLoadedEventFired",
                json!({ "timestamp": timestamp }),
            );

            ctx.events
                .send_page_event("Page.loadEventFired", json!({ "timestamp": timestamp }));

            Ok(Some(json!({
                "frameId": frame_id,
                "loaderId": loader_id
            })))
        }
        Err(e) => Err(CdpError {
            code: -32000,
            message: format!("Reload failed: {e}"),
        }),
    }
}

/// Page.getFrameTree — returns the actual frame tree from the session.
async fn get_frame_tree(ctx: &DispatchContext) -> DomainResult {
    let guard = ctx.session.read().await;
    match guard.page() {
        Some(page) => {
            let frame = page.root_frame();
            let url = frame.url();
            Ok(Some(json!({
                "frameTree": {
                    "frame": {
                        "id": frame.id().to_string(),
                        "url": url.to_string(),
                        "securityOrigin": url.origin().unicode_serialization(),
                        "mimeType": "text/html"
                    },
                    "childFrames": []
                }
            })))
        }
        None => Ok(Some(json!({
            "frameTree": {
                "frame": {
                    "id": "main",
                    "url": "about:blank",
                    "securityOrigin": "",
                    "mimeType": "text/html"
                },
                "childFrames": []
            }
        }))),
    }
}

/// Page.getFrameMetrics — returns frame layout metrics.
fn get_frame_metrics() -> DomainResult {
    Ok(Some(json!({
        "layoutViewport": {
            "pageX": 0,
            "pageY": 0,
            "clientWidth": 1280,
            "clientHeight": 720
        },
        "visualViewport": {
            "offsetX": 0,
            "offsetY": 0,
            "pageX": 0,
            "pageY": 0,
            "clientWidth": 1280,
            "clientHeight": 720,
            "scale": 1,
            "zoom": 1
        },
        "contentSize": {
            "width": 1280,
            "height": 720
        }
    })))
}

/// Page.captureScreenshot — captures a screenshot of the page.
///
/// Supports:
/// - `format: "png"` (default): returns a 1x1 transparent PNG placeholder
/// - `format: "text"`: returns CSS text screenshot (ASCII art rendering)
async fn capture_screenshot(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_default();
    let format = params
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("png");

    match format {
        "text" => {
            // CSS text screenshot
            let guard = ctx.session.read().await;
            let text = guard
                .page()
                .map(|p| p.to_text_screenshot())
                .unwrap_or_default();

            let encoded = base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                text.as_bytes(),
            );

            Ok(Some(json!({
                "data": encoded,
                "format": "text"
            })))
        }
        _ => {
            // PNG placeholder (1x1 transparent)
            let placeholder = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPj/HwADBwIAMCbHYQAAAABJRU5ErkJggg==";

            Ok(Some(json!({
                "data": placeholder,
                "metadata": {
                    "pageScaleFactor": 1,
                    "deviceWidth": 1280,
                    "deviceHeight": 720
                }
            })))
        }
    }
}

/// Page.printToPDF — prints the page to PDF.
///
/// Placeholder until rendering is available.
fn print_to_pdf(_params: Option<Value>) -> DomainResult {
    Ok(Some(json!({
        "data": "",
        "stream": ""
    })))
}