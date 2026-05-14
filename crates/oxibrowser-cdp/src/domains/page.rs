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
/// - `format: "png"` (default): pixel-perfect PNG screenshot
/// - `format: "jpeg"`: JPEG screenshot with `quality` param (1-100)
/// - `format: "text"`: CSS text screenshot (ASCII art rendering)
///
/// Clip options:
/// - `clip.x`, `clip.y`, `clip.width`, `clip.height` — capture a region
/// - `captureBeyondViewport: true` — auto-height to full content
#[allow(unused_variables, unused_assignments)]
async fn capture_screenshot(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params_val = params.unwrap_or_default();
    let format = params_val
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
            let _quality = params_val
                .get("quality")
                .and_then(|v| v.as_u64())
                .unwrap_or(80) as u8;

            // Viewport dimensions from params or defaults
            let mut width = params_val
                .get("width")
                .and_then(|v| v.as_u64())
                .unwrap_or(1280) as u32;
            let mut height = params_val
                .get("height")
                .and_then(|v| v.as_u64())
                .unwrap_or(720) as u32;
            let _scale = params_val
                .get("deviceScaleFactor")
                .and_then(|v: &Value| v.as_f64())
                .unwrap_or(1.0) as f32;

            // clip region: override width/height and offset rendering
            let clip = params_val.get("clip");
            let clip_x;
            let clip_y;
            if let Some(clip) = clip {
                clip_x = clip.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                clip_y = clip.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                width = clip.get("width").and_then(|v| v.as_u64()).unwrap_or(width as u64) as u32;
                height = clip.get("height").and_then(|v| v.as_u64()).unwrap_or(height as u64) as u32;
            } else {
                clip_x = 0.0;
                clip_y = 0.0;
            }

            // captureBeyondViewport: auto-expand height to content
            let _beyond_viewport = params_val
                .get("captureBeyondViewport")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            // Try real rendering if the render feature is enabled
            #[cfg(feature = "render")]
            {
                let guard = ctx.session.read().await;
                let html = guard
                    .page()
                    .map(|p| p.content())
                    .unwrap_or("<html><body></body></html>");

                let mut config = oxibrowser_render::RenderConfig::new()
                    .size(width.max(16), height.max(16))
                    .scale(scale)
                    .auto_height(beyond_viewport);

                // For clip: render full viewport then crop
                // (Blitz doesn't natively support offset rendering)
                if clip.is_some() {
                    // Render at clip dimensions
                    config = config.size(width.max(16), height.max(16));
                }

                match oxibrowser_render::render_to_png(html, config) {
                    Ok(png_bytes) => {
                        // If clip specified with offset, crop the PNG
                        let final_bytes: Vec<u8> = if clip.is_some() && (clip_x > 0.0 || clip_y > 0.0) {
                            crop_png(&png_bytes, clip_x, clip_y, width, height)
                                .unwrap_or(png_bytes)
                        } else {
                            png_bytes
                        };

                        // Encode JPEG if requested
                        let encoded = if format == "jpeg" {
                            png_to_jpeg(&final_bytes, quality).unwrap_or_else(|_| {
                                base64::Engine::encode(
                                    &base64::engine::general_purpose::STANDARD,
                                    &final_bytes,
                                )
                            })
                        } else {
                            base64::Engine::encode(
                                &base64::engine::general_purpose::STANDARD,
                                &final_bytes,
                            )
                        };

                        return Ok(Some(json!({
                            "data": encoded,
                            "metadata": {
                                "pageScaleFactor": scale,
                                "deviceWidth": width,
                                "deviceHeight": height
                            }
                        })));
                    }
                    Err(e) => {
                        tracing::warn!("Screenshot rendering failed: {}", e);
                        // Fall through to placeholder
                    }
                }
            }

            // Placeholder — used when render feature is disabled
            let placeholder = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPj/HwADBwIAMCbHYQAAAABJRU5ErkJggg==";

            Ok(Some(json!({
                "data": placeholder,
                "metadata": {
                    "pageScaleFactor": 1,
                    "deviceWidth": width,
                    "deviceHeight": height
                }
            })))
        }
    }
}

/// Crop a PNG buffer to the specified region.
#[cfg(feature = "render")]
fn crop_png(
    png_bytes: &[u8],
    x: f32,
    y: f32,
    crop_w: u32,
    crop_h: u32,
) -> Result<Vec<u8>, String> {
    let img = image::load_from_memory(png_bytes)
        .map_err(|e: image::ImageError| e.to_string())?;
    let cropped = img.crop_imm(
        x as u32,
        y as u32,
        crop_w.min(img.width()),
        crop_h.min(img.height()),
    );
    let mut buf = Vec::new();
    cropped
        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .map_err(|e: image::ImageError| e.to_string())?;
    Ok(buf)
}

/// Convert PNG bytes to base64-encoded JPEG with quality control.
#[cfg(feature = "render")]
fn png_to_jpeg(png_bytes: &[u8], quality: u8) -> Result<String, String> {
    use base64::Engine;

    let img = image::load_from_memory(png_bytes)
        .map_err(|e: image::ImageError| e.to_string())?;
    let mut buf = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
    img.write_with_encoder(encoder)
        .map_err(|e: image::ImageError| e.to_string())?;
    Ok(Engine::encode(&base64::engine::general_purpose::STANDARD, &buf))
}

/// Page.printToPDF — prints the page to PDF.
///
/// Returns actual PDF when render feature is enabled,
/// otherwise returns a placeholder.
async fn print_to_pdf(params: Option<Value>, _ctx: &DispatchContext) -> DomainResult {
    let params_val = params.unwrap_or_default();

    // Try real PDF rendering if the render feature is enabled
    #[cfg(feature = "render")]
    {
        let guard = ctx.session.read().await;
        let html = guard
            .page()
            .map(|p| p.content())
            .unwrap_or("<html><body></body></html>");

        let width = params_val
            .get("paperWidth")
            .and_then(|v| v.as_f64())
            .unwrap_or(8.5); // inches
        let height = params_val
            .get("paperHeight")
            .and_then(|v| v.as_f64())
            .unwrap_or(11.0); // inches
        let scale = params_val
            .get("scale")
            .and_then(|v| v.as_f64())
            .unwrap_or(1.0) as f32;
        let landscape = params_val
            .get("landscape")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let print_background = params_val
            .get("printBackground")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        // Convert inches to pixels at 96 DPI
        let px_w = (width * 96.0) as u32;
        let px_h = (height * 96.0) as u32;
        let (render_w, render_h) = if landscape {
            (px_h.max(16), px_w.max(16))
        } else {
            (px_w.max(16), px_h.max(16))
        };

        let background = if print_background {
            [255, 255, 255, 255]
        } else {
            [0, 0, 0, 0] // transparent
        };

        let config = oxibrowser_render::RenderConfig::new()
            .size(render_w, render_h)
            .scale(scale)
            .auto_height(true)
            .background(background)
            .format(oxibrowser_render::OutputFormat::Pdf);

        match oxibrowser_render::render(html, config) {
            Ok(pdf_bytes) => {
                let encoded = base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    &pdf_bytes,
                );
                return Ok(Some(json!({
                    "data": encoded,
                    "stream": ""
                })));
            }
            Err(e) => {
                tracing::warn!("PDF rendering failed: {}", e);
                // Fall through to placeholder
            }
        }
    }

    let _ = params_val;
    let _ = params_val;
    Ok(Some(json!({
        "data": "",
        "stream": ""
    })))
}