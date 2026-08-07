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
//!
//! Network events are emitted in the correct order:
//! 1. Network.requestWillBeSent (before navigation)
//! 2. Navigation executes
//! 3. Page.frameNavigated
//! 4. Network.responseReceived
//! 5. Network.loadingFinished
//! 6. Page.domContentLoadedEventFired
//! 7. Page.loadEventFired

use crate::domains::network;
use crate::domains::{DispatchContext, DomainResult};
use crate::event::EventSender;
use crate::protocol::CdpError;
use serde_json::{Value, json};

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
        "printToPDF" => print_to_pdf(params, ctx).await,
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
/// Emits events in correct CDP order:
/// 1. Network.requestWillBeSent
/// 2. Navigation executes
/// 3. Page.frameNavigated
/// 4. Network.responseReceived
/// 5. Network.loadingFinished
/// 6. Page.domContentLoadedEventFired
/// 7. Page.loadEventFired
async fn navigate(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_default();
    let url = params
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("about:blank");

    let loader_id = format!("LID-{}", uuid::Uuid::new_v4().as_simple());
    let request_id = format!("REQ-{}", uuid::Uuid::new_v4().as_simple());

    // 1. Emit Network.requestWillBeSent FIRST (before navigation)
    let pre_timestamp = EventSender::timestamp_ms();
    ctx.events.send_network_event(
        "Network.requestWillBeSent",
        json!({
            "requestId": request_id,
            "loaderId": loader_id,
            "documentURL": url,
            "request": {
                "url": url,
                "method": "GET",
                "headers": {},
                "initialPriority": "VeryHigh",
                "urlFragment": "",
            },
            "timestamp": pre_timestamp,
            "wallTime": pre_timestamp / 1000.0,
            "initiator": { "type": "other" },
            "type": "Document",
            "frameId": "main",
            "hasUserGesture": false,
        }),
    );

    // 2. Execute navigation
    let mut guard = ctx.session.write().await;
    match guard.navigate(url).await {
        Ok(()) => {
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

            // 3. Emit Page.frameNavigated
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

            // 4-5. Emit Network.responseReceived and Network.loadingFinished
            network::emit_response_events(
                &ctx.events,
                &request_id,
                &final_url,
                &loader_id,
                200,
                "text/html",
            );

            // 6. Emit Page.domContentLoadedEventFired
            ctx.events.send_page_event(
                "Page.domContentLoadedEventFired",
                json!({ "timestamp": timestamp }),
            );

            // 7. Emit Page.loadEventFired
            ctx.events
                .send_page_event("Page.loadEventFired", json!({ "timestamp": timestamp }));

            // Fetch.requestPaused will be emitted from Session::navigate
            // once Fetch interception is fully integrated with the HTTP client

            Ok(Some(json!({
                "frameId": frame_id,
                "loaderId": loader_id,
                "errorText": Value::Null
            })))
        }
        Err(e) => Err(CdpError {
            code: -32000,
            message: format!("Navigation failed: {e}"),
        }),
    }
}

/// Page.reload — reloads the current page and emits lifecycle events.
///
/// Emits events in the same order as navigate:
/// 1. Network.requestWillBeSent
/// 2. Reload executes
/// 3. Page.frameNavigated
/// 4. Network.responseReceived
/// 5. Network.loadingFinished
/// 6. Page.domContentLoadedEventFired
/// 7. Page.loadEventFired
async fn reload(_params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let loader_id = format!("LID-{}", uuid::Uuid::new_v4().as_simple());
    let request_id = format!("REQ-{}", uuid::Uuid::new_v4().as_simple());

    // 1. Capture current URL before emitting events (read lock)
    let current_url = {
        let guard = ctx.session.read().await;
        guard
            .current_url()
            .map(|u| u.to_string())
            .unwrap_or_else(|| "about:blank".to_string())
    };

    // 2. Emit Network.requestWillBeSent FIRST with the current URL
    let pre_timestamp = EventSender::timestamp_ms();
    ctx.events.send_network_event(
        "Network.requestWillBeSent",
        json!({
            "requestId": request_id,
            "loaderId": loader_id,
            "documentURL": current_url,
            "request": {
                "url": current_url,
                "method": "GET",
                "headers": {},
                "initialPriority": "VeryHigh",
                "urlFragment": "",
            },
            "timestamp": pre_timestamp,
            "wallTime": pre_timestamp / 1000.0,
            "initiator": { "type": "other" },
            "type": "Document",
            "frameId": "main",
            "hasUserGesture": false,
        }),
    );

    // 3. Execute reload
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

            // 3. Emit Page.frameNavigated
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

            // 4-5. Emit Network.responseReceived and Network.loadingFinished
            network::emit_response_events(
                &ctx.events,
                &request_id,
                &final_url,
                &loader_id,
                200,
                "text/html",
            );

            // 6. Emit Page.domContentLoadedEventFired
            ctx.events.send_page_event(
                "Page.domContentLoadedEventFired",
                json!({ "timestamp": timestamp }),
            );

            // 7. Emit Page.loadEventFired
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
/// Renders the DOM as a PNG image using text-based rendering with bitmap font.
async fn capture_screenshot(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_default();
    let _format = params
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("png");
    let viewport_width = params
        .get("clip")
        .and_then(|v| v.get("width"))
        .and_then(|v| v.as_f64())
        .unwrap_or(1280.0) as u32;

    let guard = ctx.session.read().await;
    let png_bytes: Vec<u8> = match guard.page() {
        Some(page) => page
            .to_screenshot_png_live(viewport_width.max(64))
            .unwrap_or_else(|_| {
                oxibrowser_core::css::text_to_png("", viewport_width.max(64)).unwrap_or_default()
            }),
        None => oxibrowser_core::css::text_to_png("", viewport_width.max(64)).unwrap_or_default(),
    };

    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD.encode(&png_bytes);

    Ok(Some(json!({
        "data": data,
        "metadata": {
            "pageScaleFactor": 1,
            "deviceWidth": viewport_width,
            "deviceHeight": 720
        }
    })))
}

/// Page.printToPDF — prints the page to PDF.
///
/// Currently returns an empty PDF. Full PDF generation requires the `printpdf` dependency.
async fn print_to_pdf(_params: Option<Value>, _ctx: &DispatchContext) -> DomainResult {
    // TODO: Add printpdf dependency for real PDF generation
    // Tracking: https://github.com/oxibrowser/oxibrowser/issues/TODO
    Ok(Some(json!({
        "data": "",
        "stream": ""
    })))
}
