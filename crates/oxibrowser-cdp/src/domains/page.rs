//! CDP Page domain handler.
//!
//! Handles Page.enable, Page.disable, Page.navigate, Page.reload,
//! Page.getFrameTree, Page.getFrameMetrics, Page.captureScreenshot,
//! Page.printToPDF, Page.getNavigationHistory, Page.getLifecycleEvents,
//! and the screencast subset (Page.startScreencast,
//! Page.stopScreencast, Page.screencastFrameAck).
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
use image::codecs::jpeg::JpegEncoder;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use tokio::sync::RwLock;

/// Dispatch Page domain methods.
pub async fn handle(method: &str, params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    match method {
        "enable" => enable(ctx),
        "disable" => disable(ctx),
        "navigate" => navigate(params, ctx).await,
        "reload" => reload(params, ctx).await,
        "getFrameTree" => get_frame_tree(ctx).await,
        "createIsolatedWorld" => create_isolated_world(params, ctx).await,
        "getFrameMetrics" | "getLayoutMetrics" => {
            super::runtime::remote("getLayoutMetrics", params, ctx).await
        }
        "captureScreenshot" => capture_screenshot(params, ctx).await,
        "printToPDF" => print_to_pdf(params, ctx).await,
        "startScreencast" => start_screencast(params, ctx).await,
        "stopScreencast" => stop_screencast(ctx),
        "screencastFrameAck" => screencast_frame_ack(params),
        "setDownloadBehavior" => set_download_behavior(params, ctx),
        "getLifecycleEvents" => get_lifecycle_events(ctx),
        "setLifecycleEventsEnabled" => set_lifecycle_events_enabled(params, ctx),
        // Dialog handling: alert/confirm/prompt default to non-blocking
        // (no-throw), so acknowledging the dialog is a no-op ack.
        // Resolve a pending alert/confirm/prompt. The page's JS thread blocks
        // polling the dialog gate until this writes the resolution.
        "handleJavaScriptDialog" => handle_javascript_dialog(params, ctx).await,
        "addScriptToEvaluateOnNewDocument" => {
            add_script_to_evaluate_on_new_document(params, ctx).await
        }
        "removeScriptToEvaluateOnNewDocument" => {
            remove_script_to_evaluate_on_new_document(params, ctx).await
        }
        // Common Playwright/Puppeteer Page methods — acknowledged as no-ops so
        // they don't 404 the client. Real implementations land per phase.
        "bringToFront" => bring_to_front(),
        "getNavigationHistory" => get_navigation_history(ctx).await,
        "navigateToHistoryEntry" => navigate_to_history_entry(params, ctx).await,
        "setBypassCSP" => set_bypass_csp(),
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

/// Page.getNavigationHistory — returns the session's navigation history.
///
/// Entry ids are 1-based positions. Only the current entry carries the page
/// title (the session tracks one loaded document); other entries report an
/// empty title. Every entry uses the `link` transition type since redirect
/// chains are not recorded.
async fn get_navigation_history(ctx: &DispatchContext) -> DomainResult {
    let mut session = ctx.session.write().await;
    let snapshot = session.navigation_history_snapshot().await?;
    let index = snapshot["index"].as_u64().unwrap_or(0) as usize;
    let title = session.page().and_then(|page| page.title()).unwrap_or("");
    let entries=snapshot["entries"].as_array().map(|entries|entries.iter().enumerate().map(|(position,entry)|json!({
        "id":entry["id"],"url":entry["url"],"userTypedURL":entry["url"],"title":if position==index {title} else {""},"transitionType":"other"
    })).collect::<Vec<_>>()).unwrap_or_default();
    Ok(Some(json!({"currentIndex":index,"entries":entries})))
}

async fn navigate_to_history_entry(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_else(|| json!({}));
    let id = params
        .get("entryId")
        .and_then(Value::as_u64)
        .ok_or_else(|| CdpError {
            code: -32602,
            message: "Missing history entryId".into(),
        })?;
    let mut session = ctx.session.write().await;
    let same_document = session.navigate_history_entry(id).await?;
    if !same_document {
        let url = session
            .current_url()
            .map(ToString::to_string)
            .unwrap_or_default();
        let loader = format!("LID-{}", uuid::Uuid::new_v4());
        let frame = ctx.events.main_frame_id();
        ctx.events.clear_reported_worlds(&session.id().to_string());
        ctx.events
            .send_runtime_event("Runtime.executionContextsCleared", json!({}));
        ctx.events.send_page_event("Page.frameNavigated",json!({"frame":{"id":frame,"url":url,"loaderId":loader,"mimeType":"text/html"},"type":"Navigation"}));
        announce_child_frames(ctx, &session, frame, &loader);
        announce_document_contexts(ctx, &mut session, &url).await?;
        for name in ["DOMContentLoaded", "load"] {
            ctx.events.send_page_event("Page.lifecycleEvent",json!({"frameId":frame,"loaderId":loader,"name":name,"timestamp":EventSender::timestamp_ms()/1000.0}));
        }
    }
    Ok(Some(json!({})))
}

/// Return the lifecycle events buffered for this target.
fn get_lifecycle_events(ctx: &DispatchContext) -> DomainResult {
    let events=ctx.events.lifecycle_events().into_iter().map(|(method,_,timestamp)|json!({
        "frameId":ctx.events.main_frame_id(),"name":method.strip_prefix("Page.").unwrap_or(&method),"timestamp":timestamp
    })).collect::<Vec<_>>();
    Ok(Some(json!({"events":events})))
}

/// Page.setBypassCSP — accepted as a no-op.
///
/// The engine never enforces CSP in the first place, so there is no bypass
/// state to toggle; the call always succeeds.
fn set_bypass_csp() -> DomainResult {
    Ok(Some(json!({})))
}

/// Page.bringToFront — accepted as a no-op.
///
/// There is no OS window or tab-visibility concept, so every target is
/// always effectively frontmost; nothing to activate.
fn bring_to_front() -> DomainResult {
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

    // 1. Emit Network.requestWillBeSent FIRST (before navigation).
    // Only the event payload is redacted — `url` itself drives the real
    // navigation below.
    let event_url = oxibrowser_core::security::redact::redact_url_query(
        url,
        &oxibrowser_core::security::redact::active_profile(),
    );
    let pre_timestamp = EventSender::timestamp_ms();
    ctx.events.send_network_event(
        "Network.requestWillBeSent",
        json!({
            "requestId": request_id,
            "loaderId": loader_id,
            "documentURL": event_url,
            "request": {
                "url": event_url,
                "method": "GET",
                "headers": {},
                "initialPriority": "VeryHigh",
                "urlFragment": "",
            },
            "timestamp": pre_timestamp,
            "wallTime": pre_timestamp / 1000.0,
            "initiator": { "type": "other" },
            "type": "Document",
            "frameId": ctx.events.main_frame_id(),
            "hasUserGesture": false,
        }),
    );

    // 1b. Fetch interception: if enabled and the URL matches a pattern, emit
    // `Fetch.requestPaused` and await the client's decision (continue/fail/fulfill).
    let mut effective_url = url.to_string();
    let patterns = ctx.events.get_fetch_patterns();
    if !patterns.is_empty() && crate::domains::fetch::matches_patterns(url, &patterns) {
        use base64::Engine;
        use oxibrowser_core::network::InterceptAction;
        let intercept_id = format!("INT-{}", uuid::Uuid::new_v4().as_simple());
        let decision = crate::domains::fetch::emit_request_paused(
            &intercept_id,
            url,
            "GET",
            &[],
            "Document",
            &ctx.fetch_registry,
            &ctx.events,
        )
        .await;
        match decision {
            Ok(InterceptAction::Fail { error_reason }) => {
                return Ok(Some(json!({
                    "frameId": ctx.events.main_frame_id(),
                    "loaderId": loader_id,
                    "errorText": error_reason
                })));
            }
            Ok(InterceptAction::Continue { url: Some(u), .. }) => effective_url = u,
            Ok(InterceptAction::Fulfill { body, .. }) => {
                // Mock the response: navigate to a data: URL carrying the body.
                let b64 = base64::engine::general_purpose::STANDARD.encode(&body);
                effective_url = format!("data:text/html;charset=utf-8;base64,{b64}");
            }
            _ => {} // Continue unmodified, or no decision — proceed normally.
        }
    }

    // 2. Execute navigation
    let mut guard = ctx.session.write().await;
    match guard.navigate(&effective_url).await {
        Ok(()) => {
            ctx.events.clear_reported_worlds(&guard.id().to_string());
            ctx.events
                .send_runtime_event("Runtime.executionContextsCleared", json!({}));
            // Capture timestamp after navigation completes
            let timestamp = EventSender::timestamp_ms();
            let frame_id = ctx.events.main_frame_id().to_owned();

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

            announce_child_frames(ctx, &guard, &frame_id, &loader_id);

            announce_document_contexts(ctx, &mut guard, &final_url).await?;

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
            for name in ["DOMContentLoaded", "load"] {
                ctx.events.send_page_event("Page.lifecycleEvent", json!({
                    "frameId":frame_id, "loaderId":loader_id, "name":name, "timestamp":timestamp / 1000.0
                }));
            }

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
    // (event payload only — the reload itself uses the session's real URL).
    let event_url = oxibrowser_core::security::redact::redact_url_query(
        &current_url,
        &oxibrowser_core::security::redact::active_profile(),
    );
    let pre_timestamp = EventSender::timestamp_ms();
    ctx.events.send_network_event(
        "Network.requestWillBeSent",
        json!({
            "requestId": request_id,
            "loaderId": loader_id,
            "documentURL": event_url,
            "request": {
                "url": event_url,
                "method": "GET",
                "headers": {},
                "initialPriority": "VeryHigh",
                "urlFragment": "",
            },
            "timestamp": pre_timestamp,
            "wallTime": pre_timestamp / 1000.0,
            "initiator": { "type": "other" },
            "type": "Document",
            "frameId": ctx.events.main_frame_id(),
            "hasUserGesture": false,
        }),
    );

    // 3. Execute reload
    let mut guard = ctx.session.write().await;
    match guard.reload().await {
        Ok(()) => {
            ctx.events.clear_reported_worlds(&guard.id().to_string());
            ctx.events
                .send_runtime_event("Runtime.executionContextsCleared", json!({}));
            // Capture timestamp after reload completes
            let timestamp = EventSender::timestamp_ms();
            let frame_id = ctx.events.main_frame_id().to_owned();

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

            announce_child_frames(ctx, &guard, &frame_id, &loader_id);

            announce_document_contexts(ctx, &mut guard, &final_url).await?;

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
            for name in ["DOMContentLoaded", "load"] {
                ctx.events.send_page_event("Page.lifecycleEvent", json!({
                    "frameId":frame_id, "loaderId":loader_id, "name":name, "timestamp":timestamp / 1000.0
                }));
            }

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

fn announce_child_frames(
    ctx: &DispatchContext,
    session: &oxibrowser_core::session::Session,
    root_id: &str,
    loader: &str,
) {
    fn walk(
        ctx: &DispatchContext,
        frame: &oxibrowser_core::frame::Frame,
        parent: &str,
        loader: &str,
        contexts: &HashMap<String, u32>,
    ) {
        for child in frame.children() {
            let id = child.id().to_string();
            let url = child.url();
            ctx.events.send_page_event(
                "Page.frameAttached",
                json!({"frameId":id,"parentFrameId":parent}),
            );
            ctx.events.send_page_event("Page.frameNavigated", json!({"frame":{
                "id":id,"parentId":parent,"loaderId":loader,"url":url.to_string(),"securityOrigin":url.origin().unicode_serialization(),"mimeType":"text/html"
            },"type":"Navigation"}));
            if let Some(context) = contexts.get(&id) {
                ctx.events.send_runtime_event("Runtime.executionContextCreated", json!({"context":{
                    "id":context,"origin":url.origin().unicode_serialization(),"name":format!("iframe:{id}"),"uniqueId":format!("context-{}",uuid::Uuid::new_v4()),
                    "auxData":{"isDefault":true,"type":"default","frameId":id}
                }}));
            }
            for name in ["DOMContentLoaded", "load"] {
                ctx.events.send_page_event("Page.lifecycleEvent",json!({"frameId":id,"loaderId":loader,"name":name,"timestamp":EventSender::timestamp_ms()/1000.0}));
            }
            walk(ctx, child, &id, loader, contexts);
        }
    }
    if let Some(page) = session.page() {
        walk(
            ctx,
            page.root_frame(),
            root_id,
            loader,
            &session.frame_context_map().read(),
        );
    }
}

fn announce_isolated_context(
    ctx: &DispatchContext,
    owner: &str,
    frame: &str,
    world: &str,
    id: u32,
    origin: &str,
) {
    if !ctx.events.report_world_once(owner, id) {
        return;
    }
    ctx.events.send_runtime_event("Runtime.executionContextCreated", json!({"context":{
        "id":id, "origin":origin, "name":world, "uniqueId":format!("context-{}",uuid::Uuid::new_v4()),
        "auxData":{"isDefault":false,"type":"isolated","frameId":frame}
    }}));
}

async fn create_isolated_world(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_else(|| json!({}));
    let frame = params
        .get("frameId")
        .and_then(Value::as_str)
        .ok_or_else(|| CdpError {
            code: -32602,
            message: "Missing frameId".into(),
        })?;
    let world = params
        .get("worldName")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut session = ctx.session.write().await;
    let owner = session.id().to_string();
    let internal_frame = if frame == ctx.events.main_frame_id() {
        "main"
    } else {
        frame
    };
    let id = session.create_isolated_world(internal_frame, world).await?;
    let origin = session
        .current_url()
        .map(|url| url.origin().unicode_serialization())
        .unwrap_or_default();
    announce_isolated_context(ctx, &owner, frame, world, id, &origin);
    Ok(Some(json!({"executionContextId":id})))
}

async fn announce_document_contexts(
    ctx: &DispatchContext,
    session: &mut oxibrowser_core::session::Session,
    url: &str,
) -> Result<(), CdpError> {
    let owner = session.id().to_string();
    let origin = url::Url::parse(url)
        .ok()
        .map(|url| url.origin().unicode_serialization())
        .unwrap_or_default();
    ctx.events.send_runtime_event("Runtime.executionContextCreated", json!({"context":{
        "id":1, "origin":origin, "name":"main", "uniqueId":format!("context-{}",uuid::Uuid::new_v4()),
        "auxData":{"isDefault":true,"type":"default","frameId":ctx.events.main_frame_id()}
    }}));
    let frames: Vec<String> = std::iter::once("main".to_owned())
        .chain(session.frame_context_map().read().keys().cloned())
        .collect();
    for (_, world) in ctx.events.world_scripts(&owner) {
        for frame in &frames {
            let id = session.create_isolated_world(frame, &world).await?;
            announce_isolated_context(
                ctx,
                &owner,
                if frame == "main" {
                    ctx.events.main_frame_id()
                } else {
                    frame.as_str()
                },
                &world,
                id,
                &origin,
            );
        }
    }
    Ok(())
}

/// Page.getFrameTree — returns the actual frame tree from the session,
/// including child iframe frames (Phase 8).
async fn get_frame_tree(ctx: &DispatchContext) -> DomainResult {
    let guard = ctx.session.read().await;
    match guard.page() {
        Some(page) => {
            let frame = page.root_frame();
            let mut node = frame_tree_node(frame, None, ctx.events.main_frame_id());
            node["frame"]["id"] = json!(ctx.events.main_frame_id());
            Ok(Some(json!({ "frameTree": node })))
        }
        None => Ok(Some(json!({
            "frameTree": {
                "frame": {
                    "id": ctx.events.main_frame_id(),
                    "url": "about:blank",
                    "securityOrigin": "",
                    "mimeType": "text/html"
                },
                "childFrames": []
            }
        }))),
    }
}

/// Build a recursive frame-tree JSON node for `Page.getFrameTree`.
fn frame_tree_node(
    frame: &oxibrowser_core::frame::Frame,
    parent: Option<&str>,
    root: &str,
) -> Value {
    let url = frame.url();
    let id = if parent.is_none() {
        root.to_owned()
    } else {
        frame.id().to_string()
    };
    let child_frames: Vec<Value> = frame
        .children()
        .iter()
        .map(|child| frame_tree_node(child, Some(&id), root))
        .collect();
    let mut node = json!({"frame":{"id":id,"url":url.to_string(),"securityOrigin":url.origin().unicode_serialization(),"mimeType":"text/html"},"childFrames":child_frames});
    if let Some(parent) = parent {
        node["frame"]["parentId"] = json!(parent);
    }
    node
}

/// Page.captureScreenshot supports viewport PNGs and integral clips within it.
async fn capture_screenshot(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_default();
    let invalid = |message: &str| CdpError {
        code: -32602,
        message: message.into(),
    };
    if params
        .get("format")
        .is_some_and(|format| format.as_str() != Some("png"))
    {
        return Err(invalid("Only PNG viewport screenshots are supported"));
    }
    for (name, supported) in [
        ("captureBeyondViewport", false),
        ("fromSurface", true),
        ("optimizeForSpeed", false),
    ] {
        if params
            .get(name)
            .is_some_and(|value| value.as_bool() != Some(supported))
        {
            return Err(invalid(&format!("Unsupported screenshot parameter {name}")));
        }
    }
    if params.get("quality").is_some() {
        return Err(invalid("Screenshot quality is unsupported for PNG"));
    }
    let mut guard = ctx.session.write().await;
    let clip = if let Some(clip) = params.get("clip") {
        let metrics = guard
            .remote_command(1, json!({"method":"getLayoutMetrics","params":{}}))
            .await?;
        let viewport = &metrics["cssLayoutViewport"];
        let number = |name: &str| {
            clip.get(name)
                .and_then(Value::as_f64)
                .filter(|n| n.is_finite())
                .ok_or_else(|| invalid("Screenshot clip requires finite x, y, width and height"))
        };
        if clip
            .get("scale")
            .is_some_and(|scale| scale.as_f64() != Some(1.0))
        {
            return Err(invalid("Screenshot clip scale must be 1"));
        }
        let x = number("x")? - viewport["pageX"].as_f64().unwrap_or(0.0);
        let y = number("y")? - viewport["pageY"].as_f64().unwrap_or(0.0);
        let width = number("width")?;
        let height = number("height")?;
        if [x, y, width, height]
            .iter()
            .any(|v| v.fract().abs() > 1e-7 || *v < 0.0 || *v > u32::MAX as f64)
            || width == 0.0
            || height == 0.0
            || x + width > viewport["clientWidth"].as_f64().unwrap_or(0.0)
            || y + height > viewport["clientHeight"].as_f64().unwrap_or(0.0)
        {
            return Err(invalid(
                "Screenshot clip must be an integral rectangle inside the current viewport",
            ));
        }
        Some((
            x.round() as u32,
            y.round() as u32,
            width as u32,
            height as u32,
        ))
    } else {
        None
    };
    let mut png = guard.capture_viewport_screenshot_png().await?;
    drop(guard);
    if let Some((x, y, width, height)) = clip {
        let dimensions =
            image::ImageReader::with_format(Cursor::new(&png), image::ImageFormat::Png)
                .into_dimensions()
                .map_err(|error| invalid(&format!("Invalid rendered PNG: {error}")))?;
        if x + width > dimensions.0 || y + height > dimensions.1 {
            return Err(invalid("Screenshot clip exceeds the rendered viewport"));
        }
        if (x, y, width, height) != (0, 0, dimensions.0, dimensions.1) {
            let image = image::load_from_memory(&png)
                .map_err(|error| invalid(&format!("PNG decoding failed: {error}")))?;
            let mut output = Cursor::new(Vec::new());
            image
                .crop_imm(x, y, width, height)
                .write_to(&mut output, image::ImageFormat::Png)
                .map_err(|error| invalid(&format!("PNG encoding failed: {error}")))?;
            png = output.into_inner();
        }
    }
    use base64::Engine;
    Ok(Some(
        json!({"data":base64::engine::general_purpose::STANDARD.encode(png)}),
    ))
}

/// Resolve `Page.printToPDF` margins (inches) into one millimetre margin.
///
/// Only *provided* margins are averaged (inches → mm). When none is provided
/// the 10 mm default applies; explicit zeros are honored as borderless rather
/// than falling back to the default.
fn margin_mm_from_params(params: &Value) -> f64 {
    let provided: Vec<f64> = ["marginTop", "marginBottom", "marginLeft", "marginRight"]
        .iter()
        .filter_map(|k| params.get(*k).and_then(Value::as_f64))
        .collect();
    if provided.is_empty() {
        10.0
    } else {
        provided.iter().sum::<f64>() / provided.len() as f64 * 25.4
    }
}

/// Page.printToPDF — prints the page to PDF.
///
/// Captures the full rendered page (same path as `captureScreenshot`) and
/// slices it into pages via `png_to_pdf_paged`. Supported params: `landscape`
/// (bool), `marginTop/-Bottom/-Left/-Right` (inches; the provided ones are
/// averaged, falling back to the 10 mm default only when none is given), and
/// `paperWidth` (mapped to Letter when within 0.25in of 8.5in, else A4).
/// Header/footer, page ranges, and scale are accepted but ignored.
async fn print_to_pdf(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_default();
    let viewport_width = params
        .get("clip")
        .and_then(|v| v.get("width"))
        .and_then(|v| v.as_f64())
        .unwrap_or(1280.0) as u32;

    let mut guard = ctx.session.write().await;
    let png_bytes: Vec<u8> = match guard.capture_screenshot_png(viewport_width.max(64)).await {
        Ok(png) => png,
        // Same guard as captureScreenshot: a password-focus refusal must not
        // silently become a blank-PDF page.
        Err(e)
            if e.to_string()
                .contains(oxibrowser_core::session::CAPTURE_BLOCKED_MSG) =>
        {
            return Err(e.into());
        }
        Err(_) => oxibrowser_core::blank_png(viewport_width.max(64), 800),
    };
    drop(guard);

    let landscape = params
        .get("landscape")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let margin_mm = margin_mm_from_params(&params);
    let page_size = match params.get("paperWidth").and_then(|v| v.as_f64()) {
        Some(w) if (w - 8.5).abs() < 0.25 => oxibrowser_core::PdfPageSize::Letter,
        _ => oxibrowser_core::PdfPageSize::A4,
    };
    let opts = oxibrowser_core::PdfPageOptions {
        page_size,
        orientation: if landscape {
            oxibrowser_core::PdfOrientation::Landscape
        } else {
            oxibrowser_core::PdfOrientation::Portrait
        },
        margin_mm,
    };
    let pdf_bytes = oxibrowser_core::png_to_pdf_paged(&png_bytes, &opts).map_err(|e| CdpError {
        code: -32000,
        message: format!("PDF render failed: {e}"),
    })?;
    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD.encode(&pdf_bytes);
    Ok(Some(json!({ "data": data, "stream": "" })))
}

/// `Page.setDownloadBehavior` — configure the directory downloads are saved to.
fn set_download_behavior(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_else(|| json!({}));
    let behavior = params
        .get("behavior")
        .and_then(Value::as_str)
        .unwrap_or("allow");
    let path = params
        .get("downloadPath")
        .and_then(Value::as_str)
        .map(std::path::PathBuf::from);
    ctx.browser_context
        .set_cdp_download_behavior(behavior, path, true)
        .map_err(|error| CdpError {
            code: -32602,
            message: error.to_string(),
        })?;
    Ok(Some(json!({})))
}

/// Page.addScriptToEvaluateOnNewDocument — register a script that runs before
/// any page script on every subsequent document (navigations and reloads).
///
/// Returns `{"identifier": id}`; pass that id to
/// `Page.removeScriptToEvaluateOnNewDocument`. Only `source` is honored —
/// `worldName`/`runImmediately` are not (scripts always run in the main
/// world at document start).
async fn add_script_to_evaluate_on_new_document(
    params: Option<Value>,
    ctx: &DispatchContext,
) -> DomainResult {
    let params = params.ok_or_else(|| CdpError {
        code: -32602,
        message: "addScriptToEvaluateOnNewDocument requires parameters".to_string(),
    })?;
    let source = params
        .get("source")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CdpError {
            code: -32602,
            message: "source required".to_string(),
        })?;
    if let Some(world) = params
        .get("worldName")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
    {
        if !source.is_empty() {
            return Err(CdpError {
                code: -32000,
                message: "Nonempty isolated-world document-start scripts are not supported".into(),
            });
        }
        let mut session = ctx.session.write().await;
        let owner = session.id().to_string();
        let identifier = ctx.events.add_world_script(owner.clone(), world.to_owned());
        let id = session.create_isolated_world("main", world).await?;
        announce_isolated_context(ctx, &owner, ctx.events.main_frame_id(), world, id, "");
        return Ok(Some(json!({"identifier":identifier})));
    }
    let identifier = ctx
        .session
        .write()
        .await
        .add_init_script(source.to_string());
    tracing::debug!(identifier = %identifier, "Page.addScriptToEvaluateOnNewDocument");
    Ok(Some(json!({ "identifier": identifier })))
}

/// Page.removeScriptToEvaluateOnNewDocument — drop a previously registered
/// init script.
///
/// Like Chrome, removing an unknown identifier still succeeds: the reply is
/// always `{}` whether or not the script existed.
async fn remove_script_to_evaluate_on_new_document(
    params: Option<Value>,
    ctx: &DispatchContext,
) -> DomainResult {
    let params = params.ok_or_else(|| CdpError {
        code: -32602,
        message: "removeScriptToEvaluateOnNewDocument requires parameters".to_string(),
    })?;
    let identifier = params
        .get("identifier")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CdpError {
            code: -32602,
            message: "identifier required".to_string(),
        })?;
    let mut session = ctx.session.write().await;
    let removed = ctx
        .events
        .remove_world_script(&session.id().to_string(), identifier)
        || session.remove_init_script(identifier);
    tracing::debug!(identifier = %identifier, removed, "Page.removeScriptToEvaluateOnNewDocument");
    Ok(Some(json!({})))
}

/// Page.handleJavaScriptDialog — accept or dismiss a pending
/// `alert`/`confirm`/`prompt` dialog. Writes the resolution into the session's
/// shared dialog gate, waking the blocked JS thread.
async fn handle_javascript_dialog(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_default();
    let accept = params
        .get("accept")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let prompt_text = params
        .get("promptText")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    // Write directly to the shared gate — no session lock, so this resolves a
    // dialog even while a blocking evaluate holds the session write lock.
    *ctx.dialog_gate.lock() = Some(oxibrowser_core::js::DialogResult {
        accept,
        prompt_text,
    });
    Ok(Some(json!({})))
}

// ---------------------------------------------------------------------------
// Screencast: Page.startScreencast / Page.screencastFrame / Page.stopScreencast
// / Page.screencastFrameAck
//
// Frames are produced through the same capture path as `Page.captureScreenshot`
// (`Session::capture_screenshot_png`, the live post-JS Blitz render) by a
// per-stream background pump. Emission is flow-controlled like Chrome's:
// a frame goes out only when the document generation changed AND the previous
// frame was acked; `Page.screencastFrameAck` releases the next changed frame.
// ---------------------------------------------------------------------------

/// Base screencast tick period. `everyNthFrame` samples every Nth tick, so the
/// effective frame period is `SCREENCAST_TICK_MS * everyNthFrame`. Deliberately
/// modest: each frame is a full Blitz page render on the CPU.
const SCREENCAST_TICK_MS: u64 = 100;

/// Capture width passed to `Session::capture_screenshot_png` (layout uses the
/// session's configured viewport; this mirrors `captureScreenshot`'s default).
const SCREENCAST_CAPTURE_WIDTH: u32 = 1280;

/// Parsed `Page.startScreencast` parameters (fixed for the stream's lifetime).
#[derive(Clone, Copy, Debug)]
struct ScreencastParams {
    format: ScreencastFormat,
    quality: u32,
    max_width: Option<u32>,
    max_height: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScreencastFormat {
    Jpeg,
    Png,
}

impl ScreencastParams {
    /// Parse with CDP defaults: jpeg format, jpeg quality 80, no resize.
    fn from_params(params: &Value) -> Self {
        let format = match params.get("format").and_then(Value::as_str) {
            Some("png") => ScreencastFormat::Png,
            _ => ScreencastFormat::Jpeg,
        };
        let quality = params
            .get("quality")
            .and_then(Value::as_u64)
            .unwrap_or(80)
            .clamp(0, 100) as u32;
        let max_width = params
            .get("maxWidth")
            .and_then(Value::as_u64)
            .map(|v| v.max(1) as u32);
        let max_height = params
            .get("maxHeight")
            .and_then(Value::as_u64)
            .map(|v| v.max(1) as u32);
        Self {
            format,
            quality,
            max_width,
            max_height,
        }
    }
}

/// Emission decision for one screencast tick.
#[derive(Debug, PartialEq, Eq)]
enum ScreencastDecision {
    Send,
    Skip,
}

/// Pure generation/ack suppression state machine (screencast flow control).
///
/// - the first tick always samples, so the initial frame goes out promptly;
/// - a frame is emitted only when the document generation differs from the
///   last emitted one AND no previously emitted frame is still unacked;
/// - ack releases the *next changed* frame — it never resends an unchanged one;
/// - with `every_nth_frame > 1`, changes are only sampled every Nth tick
///   (the first tick excepted); ticks blocked on an ack still count, so the
///   change observed just before the ack is released immediately.
struct ScreencastGate {
    every_nth_frame: u32,
    ticks_since_frame: u32,
    last_emitted_generation: Option<u64>,
    pending_ack: bool,
}

impl ScreencastGate {
    fn new(every_nth_frame: u32) -> Self {
        Self {
            every_nth_frame: every_nth_frame.max(1),
            ticks_since_frame: 0,
            last_emitted_generation: None,
            pending_ack: false,
        }
    }

    /// Evaluate one pump tick against the current document generation.
    fn on_tick(&mut self, generation: u64) -> ScreencastDecision {
        self.ticks_since_frame = self.ticks_since_frame.saturating_add(1);
        let sample = self.last_emitted_generation.is_none()
            || self.ticks_since_frame >= self.every_nth_frame;
        if !sample {
            return ScreencastDecision::Skip;
        }
        let changed = self.last_emitted_generation != Some(generation);
        if changed && !self.pending_ack {
            ScreencastDecision::Send
        } else {
            ScreencastDecision::Skip
        }
    }

    /// Record an emitted frame: store its generation, restart the sampling
    /// cadence, and gate further frames until the client acks.
    fn on_frame_sent(&mut self, generation: u64) {
        self.last_emitted_generation = Some(generation);
        self.ticks_since_frame = 0;
        self.pending_ack = true;
    }

    /// Apply `Page.screencastFrameAck`. Returns false when no frame was
    /// pending (duplicate ack); the registry-level unknown-sessionId case is
    /// handled by the caller.
    fn on_ack(&mut self) -> bool {
        if self.pending_ack {
            self.pending_ack = false;
            true
        } else {
            false
        }
    }
}

/// An active screencast stream.
struct ScreencastStream {
    /// Owning target session, identified by the `Arc` pointer of its core
    /// `Session` — stable for the session's lifetime, scopes `stopScreencast`.
    owner: usize,
    gate: Arc<Mutex<ScreencastGate>>,
    /// Cleared by `stopScreencast` (or session close) to end the pump.
    active: Arc<AtomicBool>,
}

/// Registry of active screencasts, keyed by screencast sessionId. Keys are
/// process-unique UUIDs, so concurrent connections never collide; lookups by
/// sessionId (`screencastFrameAck`) are therefore unambiguous.
static SCREENCASTS: LazyLock<Mutex<HashMap<String, ScreencastStream>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Lock the screencast registry, recovering from poisoning (a panic in one
/// handler must not permanently break screencast for the process).
fn screencasts() -> MutexGuard<'static, HashMap<String, ScreencastStream>> {
    SCREENCASTS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `Page.startScreencast` — begin streaming frames of the live document.
///
/// Registers a stream and spawns its pump; frames arrive as
/// `Page.screencastFrame` events carrying this stream's `sessionId`, which the
/// client must echo back in `Page.screencastFrameAck`.
async fn start_screencast(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_default();
    let stream_params = ScreencastParams::from_params(&params);
    let every_nth_frame = params
        .get("everyNthFrame")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1) as u32;

    let stream_id = format!("screencast-{}", uuid::Uuid::new_v4().as_simple());
    let gate = Arc::new(Mutex::new(ScreencastGate::new(every_nth_frame)));
    let active = Arc::new(AtomicBool::new(true));

    screencasts().insert(
        stream_id.clone(),
        ScreencastStream {
            owner: Arc::as_ptr(&ctx.session) as usize,
            gate: gate.clone(),
            active: active.clone(),
        },
    );

    let session = ctx.session.clone();
    let events = ctx.events.clone();
    let sid = stream_id;
    tokio::spawn(screencast_pump(
        session,
        events,
        sid,
        gate,
        active,
        stream_params,
    ));

    Ok(Some(json!({})))
}

/// Registry-entry lifetime guard for one screencast pump. Removing the entry
/// on drop means the registry can never outlive its pump: the pump removes
/// its own entry on every exit path — `stopScreencast`, session close, client
/// disconnect, or a panic unwinding out of the task.
struct ScreencastPumpGuard {
    stream_id: String,
}

impl Drop for ScreencastPumpGuard {
    fn drop(&mut self) {
        screencasts().remove(&self.stream_id);
    }
}

/// Background pump for one screencast stream: ticks every
/// `SCREENCAST_TICK_MS`, and when the gate releases a frame, captures the live
/// document (same path as `Page.captureScreenshot`), resizes/encodes it, and
/// emits `Page.screencastFrame`. Ends — removing the registry entry via
/// `ScreencastPumpGuard` — when `stopScreencast` clears `active`, the owning
/// session closes, or the event channel closes (client gone).
async fn screencast_pump(
    session: Arc<RwLock<oxibrowser_core::session::Session>>,
    events: EventSender,
    stream_id: String,
    gate: Arc<Mutex<ScreencastGate>>,
    active: Arc<AtomicBool>,
    params: ScreencastParams,
) {
    let _registry = ScreencastPumpGuard {
        stream_id: stream_id.clone(),
    };
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(SCREENCAST_TICK_MS));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        if !active.load(Ordering::Relaxed) {
            break;
        }

        // The client vanished (WebSocket dispatch loop dropped the event
        // receiver): stop rendering and release the registry entry instead of
        // pumping full-page renders at 10 Hz into a closed channel.
        if events.is_closed() {
            break;
        }

        // The document generation drives emission; a closed session ends the
        // pump (clients that stop screencasting go through stopScreencast).
        let generation = {
            let guard = session.read().await;
            if guard.is_closed() {
                break;
            }
            guard.page().map(|p| p.generation())
        };
        let Some(generation) = generation else {
            continue;
        };

        let send = gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .on_tick(generation);
        if send != ScreencastDecision::Send {
            continue;
        }

        let png = {
            let mut guard = session.write().await;
            guard.capture_screenshot_png(SCREENCAST_CAPTURE_WIDTH).await
        };
        if !active.load(Ordering::Relaxed) {
            break;
        }
        // A failed capture is logged and skipped: nothing is emitted and the
        // generation stays unsent, so the next tick retries the render.
        let Some(png) = screencast_capture_png(&stream_id, png) else {
            continue;
        };
        // Resize + JPEG/PNG encode + base64 are pure CPU work on an owned
        // buffer: run them off the async worker.
        let frame = tokio::task::spawn_blocking(move || encode_screencast_frame(&png, params))
            .await
            .ok()
            .flatten();
        let Some((data, width, height)) = frame else {
            tracing::warn!(session_id = %stream_id, "screencast frame encode failed");
            continue;
        };

        gate.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .on_frame_sent(generation);
        events.send_event(
            "Page.screencastFrame",
            json!({
                "data": data,
                "metadata": {
                    "offsetTop": 0,
                    "pageScaleFactor": 1,
                    "viewportWidth": width,
                    "viewportHeight": height,
                    "timestamp": EventSender::timestamp_ms(),
                },
                "sessionId": stream_id,
            }),
        );
    }
}

/// Resolve one tick's screencast capture into a PNG ready for encoding.
///
/// `None` (capture failed) skips the tick: the pump never substitutes a
/// fabricated blank frame for a real render, and since the tick's generation
/// is only recorded on successful emission, the next tick retries.
fn screencast_capture_png(
    stream_id: &str,
    png: Result<Vec<u8>, oxibrowser_core::error::CoreError>,
) -> Option<Vec<u8>> {
    match png {
        Ok(png) => Some(png),
        Err(e) => {
            tracing::warn!(
                session_id = %stream_id,
                error = %e,
                "screencast capture failed; skipping tick"
            );
            None
        }
    }
}

/// `Page.stopScreencast` — stop the screencast(s) started on this session.
/// Silent no-op when none is active (matches CDP semantics; takes no params).
fn stop_screencast(ctx: &DispatchContext) -> DomainResult {
    let owner = Arc::as_ptr(&ctx.session) as usize;
    let mut streams = screencasts();
    let stopped: Vec<String> = streams
        .iter()
        .filter(|(_, s)| s.owner == owner)
        .map(|(id, _)| id.clone())
        .collect();
    for id in stopped {
        if let Some(stream) = streams.remove(&id) {
            stream.active.store(false, Ordering::Relaxed);
        }
    }
    Ok(Some(json!({})))
}

/// `Page.screencastFrameAck` — release the next changed frame for a stream.
fn screencast_frame_ack(params: Option<Value>) -> DomainResult {
    let params = params.unwrap_or_default();
    let stream_id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| CdpError {
            code: -32602,
            message: "screencastFrameAck requires parameters: sessionId".to_string(),
        })?;
    let streams = screencasts();
    match streams.get(stream_id) {
        Some(stream) => {
            stream
                .gate
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .on_ack();
            Ok(Some(json!({})))
        }
        None => Err(CdpError {
            code: -32601,
            message: format!("sessionId not found: {stream_id}"),
        }),
    }
}

/// Resize (never upscale), encode, and base64 one captured PNG frame.
/// Returns `(data, width, height)` of the encoded frame.
fn encode_screencast_frame(png: &[u8], params: ScreencastParams) -> Option<(String, u32, u32)> {
    use base64::Engine;
    let mut img = image::load_from_memory(png).ok()?;
    if params.max_width.is_some() || params.max_height.is_some() {
        // Fit within the bounds preserving aspect ratio; Chrome only
        // downscales screencast frames, so never enlarge.
        let max_w = params.max_width.unwrap_or(u32::MAX).max(1) as f64;
        let max_h = params.max_height.unwrap_or(u32::MAX).max(1) as f64;
        let scale = (max_w / img.width().max(1) as f64)
            .min(max_h / img.height().max(1) as f64)
            .min(1.0);
        if scale < 1.0 {
            let new_w = ((img.width() as f64 * scale).round() as u32).max(1);
            let new_h = ((img.height() as f64 * scale).round() as u32).max(1);
            img = img.resize_exact(new_w, new_h, image::imageops::FilterType::Triangle);
        }
    }
    let (width, height) = (img.width(), img.height());
    let mut encoded = Vec::new();
    match params.format {
        ScreencastFormat::Jpeg => {
            let mut encoder =
                JpegEncoder::new_with_quality(&mut encoded, params.quality.clamp(0, 100) as u8);
            encoder
                .encode(
                    img.to_rgb8().as_raw(),
                    width,
                    height,
                    image::ExtendedColorType::Rgb8,
                )
                .ok()?;
        }
        ScreencastFormat::Png => {
            img.write_to(&mut Cursor::new(&mut encoded), image::ImageFormat::Png)
                .ok()?;
        }
    }
    Some((
        base64::engine::general_purpose::STANDARD.encode(&encoded),
        width,
        height,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn test_screencast_gate_sends_first_frame() {
        let mut gate = ScreencastGate::new(1);
        assert_eq!(gate.on_tick(10), ScreencastDecision::Send);
        gate.on_frame_sent(10);
    }

    #[test]
    fn test_screencast_gate_suppresses_unchanged_generation() {
        let mut gate = ScreencastGate::new(1);
        gate.on_tick(10);
        gate.on_frame_sent(10);
        assert_eq!(gate.on_tick(10), ScreencastDecision::Skip);
    }

    #[test]
    fn test_screencast_gate_sends_on_changed_generation() {
        let mut gate = ScreencastGate::new(1);
        gate.on_tick(10);
        gate.on_frame_sent(10);
        gate.on_ack(); // client released the first frame
        assert_eq!(gate.on_tick(11), ScreencastDecision::Send);
    }

    #[test]
    fn test_screencast_gate_holds_until_ack_then_releases_changed_frame() {
        let mut gate = ScreencastGate::new(1);
        gate.on_tick(10);
        gate.on_frame_sent(10);
        // Document changed, but the emitted frame is unacked: nothing further.
        assert_eq!(gate.on_tick(11), ScreencastDecision::Skip);
        // Ack releases the next CHANGED frame.
        assert!(gate.on_ack());
        assert_eq!(gate.on_tick(11), ScreencastDecision::Send);
    }

    #[test]
    fn test_screencast_gate_ack_does_not_resend_unchanged_frame() {
        let mut gate = ScreencastGate::new(1);
        gate.on_tick(10);
        gate.on_frame_sent(10);
        assert!(gate.on_ack());
        assert_eq!(gate.on_tick(10), ScreencastDecision::Skip);
        assert_eq!(gate.on_tick(12), ScreencastDecision::Send);
    }

    #[test]
    fn test_screencast_gate_duplicate_ack_reports_not_pending() {
        let mut gate = ScreencastGate::new(1);
        gate.on_tick(1);
        gate.on_frame_sent(1);
        assert!(gate.on_ack());
        assert!(!gate.on_ack());
    }

    #[test]
    fn test_screencast_gate_every_nth_frame_samples_changes() {
        let mut gate = ScreencastGate::new(3);
        gate.on_tick(1);
        gate.on_frame_sent(1);
        gate.on_ack(); // first frame released
        // Changes on ticks 1-2 fall between samples; tick 3 releases the latest.
        assert_eq!(gate.on_tick(2), ScreencastDecision::Skip);
        assert_eq!(gate.on_tick(3), ScreencastDecision::Skip);
        assert_eq!(gate.on_tick(4), ScreencastDecision::Send);
    }

    #[test]
    fn test_screencast_gate_first_tick_bypasses_every_nth_sampling() {
        let mut gate = ScreencastGate::new(5);
        assert_eq!(gate.on_tick(7), ScreencastDecision::Send);
    }

    #[test]
    fn test_screencast_gate_pending_ack_does_not_consume_the_sample_slot() {
        let mut gate = ScreencastGate::new(1);
        gate.on_tick(1);
        gate.on_frame_sent(1);
        // Changed while unacked, then acked: the same change is released on
        // the very next tick (blocked ticks keep the gate ready to sample).
        assert_eq!(gate.on_tick(2), ScreencastDecision::Skip);
        gate.on_ack();
        assert_eq!(gate.on_tick(2), ScreencastDecision::Send);
    }

    #[test]
    fn test_screencast_params_defaults_to_jpeg_quality_80() {
        let params = ScreencastParams::from_params(&json!({}));
        assert_eq!(params.format, ScreencastFormat::Jpeg);
        assert_eq!(params.quality, 80);
        assert_eq!(params.max_width, None);
        assert_eq!(params.max_height, None);
    }

    #[test]
    fn test_screencast_params_png_and_quality_clamped() {
        let params = ScreencastParams::from_params(&json!({ "format": "png", "quality": 250 }));
        assert_eq!(params.format, ScreencastFormat::Png);
        assert_eq!(params.quality, 100);
        let params = ScreencastParams::from_params(&json!({ "quality": 0 }));
        assert_eq!(params.format, ScreencastFormat::Jpeg);
        assert_eq!(params.quality, 0);
    }

    #[test]
    fn test_screencast_params_max_dimensions() {
        let params = ScreencastParams::from_params(&json!({ "maxWidth": 640, "maxHeight": 480 }));
        assert_eq!(params.max_width, Some(640));
        assert_eq!(params.max_height, Some(480));
    }

    #[test]
    fn test_encode_screencast_frame_png_respects_max_dimensions() {
        let png = oxibrowser_core::blank_png(800, 600);
        let params = ScreencastParams::from_params(
            &json!({ "format": "png", "maxWidth": 400, "maxHeight": 400 }),
        );
        let (data, width, height) = encode_screencast_frame(&png, params).expect("encode");
        assert_eq!((width, height), (400, 300), "aspect preserved, downscaled");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&data)
            .expect("valid base64");
        assert_eq!(&decoded[..4], &[0x89, b'P', b'N', b'G'], "png format");
    }

    #[test]
    fn test_encode_screencast_frame_jpeg_never_upscales() {
        let png = oxibrowser_core::blank_png(320, 200);
        let params = ScreencastParams::from_params(
            &json!({ "format": "jpeg", "quality": 40, "maxWidth": 800, "maxHeight": 800 }),
        );
        let (data, width, height) = encode_screencast_frame(&png, params).expect("encode");
        assert_eq!((width, height), (320, 200), "smaller frame stays put");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&data)
            .expect("valid base64");
        assert_eq!(
            &decoded[..3],
            &[0xFF, 0xD8, 0xFF],
            "jpeg SOI marker with quality path"
        );
    }

    // -- Page.printToPDF margins (provided-only averaging) --

    #[test]
    fn test_print_to_pdf_margin_defaults_when_none_provided() {
        assert_eq!(margin_mm_from_params(&json!({})), 10.0);
        // Non-margin params do not count as provided.
        assert_eq!(margin_mm_from_params(&json!({ "landscape": true })), 10.0);
    }

    #[test]
    fn test_print_to_pdf_margin_averages_only_provided_values() {
        // A single provided margin is not quartered.
        let one = margin_mm_from_params(&json!({ "marginTop": 1.0 }));
        assert!(
            (one - 25.4).abs() < 1e-9,
            "1in alone must stay 25.4mm, got {one}"
        );
        let two = margin_mm_from_params(&json!({ "marginTop": 1.0, "marginBottom": 2.0 }));
        assert!(
            (two - 38.1).abs() < 1e-9,
            "mean of 1in+2in must be 38.1mm, got {two}"
        );
    }

    #[test]
    fn test_print_to_pdf_margin_explicit_zeros_mean_borderless() {
        let m = margin_mm_from_params(&json!({
            "marginTop": 0.0,
            "marginBottom": 0.0,
            "marginLeft": 0.0,
            "marginRight": 0.0,
        }));
        assert_eq!(
            m, 0.0,
            "explicit zeros must be honored, not fall back to the 10mm default"
        );
    }

    // -- Screencast capture failure (no blank-frame substitution) --

    #[test]
    fn test_screencast_capture_failure_yields_no_frame() {
        let failed = Err(oxibrowser_core::error::CoreError::ScreenshotError(
            "render failed".to_string(),
        ));
        assert!(
            screencast_capture_png("sc-1", failed).is_none(),
            "a failed capture must never fabricate a blank frame"
        );
        let ok = Ok(oxibrowser_core::blank_png(640, 480));
        assert!(screencast_capture_png("sc-1", ok).is_some());
    }

    #[test]
    fn test_screencast_gate_retries_generation_after_skipped_capture() {
        // Tick 1 releases the frame but the capture fails: no emission, no
        // on_frame_sent — the same generation is released again next tick.
        let mut gate = ScreencastGate::new(1);
        assert_eq!(gate.on_tick(10), ScreencastDecision::Send);
        // (capture fails here — nothing emitted, generation not recorded)
        assert_eq!(
            gate.on_tick(10),
            ScreencastDecision::Send,
            "an unsent generation must be retried, not suppressed"
        );
        gate.on_frame_sent(10);
        assert_eq!(gate.on_tick(10), ScreencastDecision::Skip);
    }
}
