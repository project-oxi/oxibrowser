//! CDP Tracing domain handler.
//!
//! Handles `Tracing.start`, `Tracing.end`, and `Tracing.getCategories`.
//!
//! Minimal-but-functional for automation clients (Playwright
//! `page.tracing.start()`/`stop()`): `start` records the active flag +
//! categories; `end` emits a `Tracing.dataCollected` event with a minimal
//! Chromium-format trace (a `TracingStartedInBrowser` metadata event) followed
//! by `Tracing.tracingComplete`. A full timeline/network tracer is out of
//! scope; the CDP surface accepts and completes the start/stop contract.

use crate::domains::{DispatchContext, DomainResult};
use crate::event::EventSender;
use crate::protocol::CdpError;
use serde_json::{Value, json};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// Whether a trace is currently being recorded.
static TRACING_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Trace categories requested by `Tracing.start` (mirrored into the trace).
static TRACE_CATEGORIES: LazyLock<parking_lot::RwLock<Vec<String>>> =
    LazyLock::new(|| parking_lot::RwLock::new(Vec::new()));

/// Dispatch Tracing domain methods.
pub fn handle(method: &str, params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    match method {
        "start" => start(params),
        "end" => {
            end(&ctx.events);
            Ok(Some(json!({})))
        }
        "getCategories" => Ok(Some(json!({ "categories": common_categories() }))),
        _ => Err(CdpError {
            code: -32601,
            message: format!("Tracing.{method} not implemented"),
        }),
    }
}

/// `Tracing.start` — begin a trace with the requested categories.
fn start(params: Option<Value>) -> DomainResult {
    let params = params.unwrap_or_default();
    let categories: Vec<String> = params
        .get("categories")
        .and_then(|v| v.as_str())
        .map(|s| s.split(',').map(|c| c.trim().to_string()).collect())
        .unwrap_or_else(common_categories);
    *TRACE_CATEGORIES.write() = categories;
    TRACING_ACTIVE.store(true, Ordering::Relaxed);
    tracing::debug!("Tracing.start");
    Ok(Some(json!({})))
}

/// `Tracing.end` — stop tracing and emit `dataCollected` + `tracingComplete`.
fn end(events: &EventSender) {
    if !TRACING_ACTIVE.swap(false, Ordering::Relaxed) {
        return;
    }
    let ts = current_time_micros();
    // Minimal Chromium-format trace: one metadata event. Real traces carry
    // timeline/network events; automation clients need the container shape.
    let metadata = json!({
        "args": { "data": { "startTime": ts } },
        "cat": "__metadata",
        "name": "TracingStartedInBrowser",
        "ph": "M",
        "pid": 1,
        "tid": 1,
        "ts": ts
    });
    let trace_events = vec![metadata];
    events.send_event("Tracing.dataCollected", json!({ "value": trace_events }));
    events.send_event(
        "Tracing.tracingComplete",
        json!({
            "dataLossOccurred": false,
            "traceFormat": "json"
        }),
    );
    tracing::debug!("Tracing.end");
}

/// Common Chromium trace categories.
fn common_categories() -> Vec<String> {
    [
        "devtools.timeline",
        "v8.execute",
        "blink.user_timing",
        "blink.console",
        "disabled-by-default-devtools.timeline",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn current_time_micros() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as f64)
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domains::DispatchContext;
    use crate::event::event_channel;
    use oxibrowser_core::network::intercept::shared_registry;
    use oxibrowser_core::{Browser, BrowserConfig};
    use serde_json::json;

    use std::sync::Arc;

    async fn make_ctx() -> (DispatchContext, crate::event::EventReceiver) {
        let mut config = BrowserConfig::headless();
        config.enable_ssrf_filter = false;
        let browser = Arc::new(Browser::new(config).await.unwrap());
        let session = browser.new_session().await.unwrap();
        let (events, rx) = event_channel();
        let ctx = DispatchContext {
            session,
            events,
            fetch_registry: shared_registry(),
            dialog_gate: Arc::new(parking_lot::Mutex::new(None)),
            browser: browser.clone(),
            child_targets: Arc::new(crate::domains::TargetRegistry::new()),
            browser_context: browser.default_context(),
            credentials: None,
            role: crate::session::RoleKind::Agent,
            logins: None,
        };
        (ctx, rx)
    }

    #[tokio::test]
    async fn get_categories_returns_common_categories() {
        let (ctx, _rx) = make_ctx().await;
        let r = handle("getCategories", None, &ctx).unwrap().unwrap();
        assert!(r["categories"].is_array());
        assert!(!r["categories"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn start_and_end_flow() {
        let (ctx, mut rx) = make_ctx().await;
        assert!(handle("start", Some(json!({ "categories": "v8.execute" })), &ctx).is_ok());
        assert!(TRACING_ACTIVE.load(Ordering::Relaxed));

        assert!(handle("end", None, &ctx).is_ok());
        assert!(!TRACING_ACTIVE.load(Ordering::Relaxed));

        // Expect dataCollected + tracingComplete events on the receiver.
        let mut saw_collected = false;
        let mut saw_complete = false;
        for _ in 0..8 {
            if let Ok(Some(msg)) =
                tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await
            {
                if msg.method == "Tracing.dataCollected" {
                    saw_collected = true;
                }
                if msg.method == "Tracing.tracingComplete" {
                    saw_complete = true;
                }
            } else {
                break;
            }
        }
        assert!(saw_collected, "Tracing.dataCollected should be emitted");
        assert!(saw_complete, "Tracing.tracingComplete should be emitted");
    }

    #[tokio::test]
    async fn unknown_method_returns_error() {
        let (ctx, _rx) = make_ctx().await;
        assert!(handle("bogus", None, &ctx).is_err());
    }
}
