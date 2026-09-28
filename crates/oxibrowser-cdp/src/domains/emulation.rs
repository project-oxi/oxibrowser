//! CDP Emulation domain handler.
//!
//! Handles `Emulation.setDeviceMetricsOverride`, `Emulation.clearDeviceMetricsOverride`,
//! `Emulation.setVisibleSize`, and `Emulation.setUserAgentOverride`.
//!
//! Stored metrics are kept in a module-level static so render code can read them
//! later via [`current_device_metrics`] without round-tripping through the protocol.

use crate::domains::{DispatchContext, DomainResult};
use crate::protocol::CdpError;
use serde_json::{Value, json};
use std::sync::LazyLock;

/// Stored device metrics override (set via `Emulation.setDeviceMetricsOverride`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DeviceMetrics {
    /// Override width in CSS pixels.
    pub width: u32,
    /// Override height in CSS pixels.
    pub height: u32,
    /// Override device scale factor.
    pub device_scale_factor: f64,
    /// Whether to emulate a mobile device.
    pub mobile: bool,
}

static DEVICE_METRICS: LazyLock<parking_lot::RwLock<Option<DeviceMetrics>>> =
    LazyLock::new(|| parking_lot::RwLock::new(None));

/// Read the currently stored device metrics override, if any.
///
/// Returns `None` after `Emulation.clearDeviceMetricsOverride` (or before any
/// `Emulation.setDeviceMetricsOverride` call).
pub fn current_device_metrics() -> Option<DeviceMetrics> {
    *DEVICE_METRICS.read()
}

/// Dispatch Emulation domain methods.
pub async fn handle(method: &str, params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    match method {
        "setDeviceMetricsOverride" => set_device_metrics_override(params),
        "clearDeviceMetricsOverride" => clear_device_metrics_override(),
        // Deprecated/non-standard CDP (`Emulation.setVisibleSize` was never in
        // the spec and modern Chrome rejects it) — fall through to -32601 like
        // any other unknown method instead of silently acknowledging.
        "setUserAgentOverride" => set_user_agent_override(params, ctx).await,
        "setEmulatedMedia" => set_emulated_media(params, ctx).await,
        "setGeolocationOverride" => set_geolocation_override(params),
        "clearGeolocationOverride" => clear_geolocation_override(),
        "setTimezoneOverride" => set_timezone_override(params),
        "clearTimezoneOverride" => {
            oxibrowser_core::js::clear_timezone_override();
            Ok(Some(json!({})))
        }
        _ => Err(CdpError {
            code: -32601,
            message: format!("Emulation.{method} not implemented"),
        }),
    }
}

/// `Emulation.setDeviceMetricsOverride` — store viewport + scale + mobile flag.
///
/// Missing fields default to `width=1280`, `height=800`, `deviceScaleFactor=1.0`,
/// `mobile=false`. `width`/`height` are clamped to `>= 1`.
fn set_device_metrics_override(params: Option<Value>) -> DomainResult {
    let params = params.unwrap_or_default();
    let width = params
        .get("width")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .unwrap_or(1280)
        .max(1);
    let height = params
        .get("height")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .unwrap_or(800)
        .max(1);
    let device_scale_factor = params
        .get("deviceScaleFactor")
        .and_then(|v| v.as_f64())
        .unwrap_or(1.0);
    let mobile = params
        .get("mobile")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    tracing::debug!(
        width,
        height,
        device_scale_factor,
        mobile,
        "Emulation.setDeviceMetricsOverride"
    );

    *DEVICE_METRICS.write() = Some(DeviceMetrics {
        width,
        height,
        device_scale_factor,
        mobile,
    });
    // Apply to layout: subsequent navigations lay out at this viewport.
    oxibrowser_core::session::set_viewport_override(width, height);

    Ok(Some(json!({})))
}

/// `Emulation.clearDeviceMetricsOverride` — drop any stored override.
fn clear_device_metrics_override() -> DomainResult {
    *DEVICE_METRICS.write() = None;
    oxibrowser_core::session::clear_viewport_override();
    tracing::debug!("Emulation.clearDeviceMetricsOverride");
    Ok(Some(json!({})))
}

/// `Emulation.setGeolocationOverride` — install coordinates consumed by
/// `navigator.geolocation.getCurrentPosition`.
fn set_geolocation_override(params: Option<Value>) -> DomainResult {
    let params = params.unwrap_or_default();
    // Playwright sends {latitude, longitude, accuracy}.
    let latitude = params
        .get("latitude")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let longitude = params
        .get("longitude")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let accuracy = params
        .get("accuracy")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    oxibrowser_core::js::set_geolocation_override(latitude, longitude, accuracy);
    Ok(Some(json!({})))
}

/// `Emulation.clearGeolocationOverride` — drop the override; getCurrentPosition
/// then reports POSITION_UNAVAILABLE.
fn clear_geolocation_override() -> DomainResult {
    oxibrowser_core::js::clear_geolocation_override();
    Ok(Some(json!({})))
}

/// `Emulation.setTimezoneOverride` — set the IANA timezone for Intl/Date.
fn set_timezone_override(params: Option<Value>) -> DomainResult {
    let params = params.unwrap_or_default();
    let tz = params
        .get("timezoneId")
        .and_then(|v| v.as_str())
        .unwrap_or("UTC");
    oxibrowser_core::js::set_timezone_override(tz);
    Ok(Some(json!({})))
}

/// `Emulation.setUserAgentOverride` — install a per-session UA override.
///
/// `params.userAgent` becomes the UA on outgoing requests
/// (`Session::RequestOverrides.user_agent`) and on the JS surface
/// (`navigator.userAgent` + stealth profile), applied immediately via
/// `JsRuntime::set_user_agent`. An absent/empty/null `userAgent` clears the
/// override. `Network.setExtraHTTPHeaders` state (`extra_headers`) is
/// preserved.
///
/// `platform` / `userAgentMetadata.platform` are accepted but deliberately
/// not stored: the stealth layer derives the platform from the UA string
/// itself (`js::stealth::ChromeProfile::from_ua`), so `navigator.platform`,
/// `userAgentData.platform`, and the WebGL renderer stay consistent with the
/// overridden UA without extra state.
async fn set_user_agent_override(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_default();
    let ua = params
        .get("userAgent")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let clear = ua.is_empty();
    let ua_opt = (!clear).then(|| ua.to_string());

    let mut session = ctx.session.write().await;
    let mut overrides = session.overrides().clone();
    overrides.user_agent = ua_opt.clone();
    session.set_overrides(overrides);
    session.set_js_user_agent(ua_opt);
    drop(session);

    tracing::debug!(ua = %ua, "Emulation.setUserAgentOverride");
    Ok(Some(json!({})))
}

/// `Emulation.setEmulatedMedia` — emulate media features.
///
/// Only the `prefers-color-scheme` feature is honored: value `dark` maps to
/// `Session::set_media_color_scheme(Some(true))`, `light` to `Some(false)`.
/// The call has replace semantics — an absent/empty `features` list (or one
/// without a usable `prefers-color-scheme` entry) clears the override.
///
/// Every other feature name (and unsupported `prefers-color-scheme` values)
/// is logged with `tracing::warn!` and ignored; the response reports what
/// was skipped as `{"ignored": [...]}`.
///
/// Timing: layout reflects the new scheme the next time a document is
/// generated; `matchMedia` probes see it immediately.
async fn set_emulated_media(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_default();
    let mut scheme: Option<bool> = None;
    let mut ignored: Vec<String> = Vec::new();
    if let Some(features) = params.get("features").and_then(|v| v.as_array()) {
        for feature in features {
            let name = feature
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let value = feature
                .get("value")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if name == "prefers-color-scheme" {
                match value {
                    "dark" => scheme = Some(true),
                    "light" => scheme = Some(false),
                    other => {
                        tracing::warn!(
                            value = %other,
                            "Emulation.setEmulatedMedia: unsupported prefers-color-scheme value ignored"
                        );
                        ignored.push(format!("prefers-color-scheme={other}"));
                    }
                }
            } else {
                tracing::warn!(
                    feature = %name,
                    "Emulation.setEmulatedMedia: unsupported feature ignored"
                );
                ignored.push(name.to_string());
            }
        }
    }
    ctx.session.write().await.set_media_color_scheme(scheme);
    Ok(Some(json!({ "ignored": ignored })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::event_channel;
    use oxibrowser_core::network::intercept::shared_registry;
    use oxibrowser_core::session::RequestOverrides;
    use oxibrowser_core::{Browser, BrowserConfig};
    use serde_json::json;
    use std::sync::Arc;

    // The device metrics override lives in a process-global static, so tests
    // that touch it must run serially to avoid cross-contamination when cargo
    // runs them on multiple threads.
    static TEST_LOCK: LazyLock<tokio::sync::Mutex<()>> =
        LazyLock::new(|| tokio::sync::Mutex::new(()));

    /// Acquire the serial-test guard. RAII — released on drop.
    async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().await
    }

    /// Build a DispatchContext backed by a real Browser session.
    async fn make_ctx() -> DispatchContext {
        let mut config = BrowserConfig::headless();
        config.enable_ssrf_filter = false;
        let browser = Arc::new(Browser::new(config).await.unwrap());
        let session = browser.new_session().await.unwrap();
        let (events, _rx) = event_channel();
        DispatchContext {
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
        }
    }

    #[tokio::test]
    async fn set_device_metrics_override_stores_values() {
        let _g = serial().await;
        let ctx = make_ctx().await;
        *DEVICE_METRICS.write() = None;

        let params = json!({
            "width": 375u32,
            "height": 812u32,
            "deviceScaleFactor": 3.0,
            "mobile": true,
        });
        let result = handle("setDeviceMetricsOverride", Some(params), &ctx)
            .await
            .unwrap();
        assert_eq!(result, Some(json!({})));

        let stored = current_device_metrics().expect("metrics should be set");
        assert_eq!(stored.width, 375);
        assert_eq!(stored.height, 812);
        assert_eq!(stored.device_scale_factor, 3.0);
        assert!(stored.mobile);
    }

    #[tokio::test]
    async fn clear_device_metrics_override_returns_empty_and_clears_state() {
        let _g = serial().await;
        let ctx = make_ctx().await;
        *DEVICE_METRICS.write() = Some(DeviceMetrics {
            width: 100,
            height: 200,
            device_scale_factor: 1.0,
            mobile: false,
        });

        let result = handle("clearDeviceMetricsOverride", None, &ctx)
            .await
            .unwrap();
        assert_eq!(result, Some(json!({})));

        assert!(current_device_metrics().is_none());
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_implemented() {
        let ctx = make_ctx().await;
        let result = handle("setCPUThrottlingRate", None, &ctx).await;
        let err = result.expect_err("expected error");
        assert_eq!(err.code, -32601);
        assert!(
            err.message.contains("Emulation.setCPUThrottlingRate"),
            "message should name the unknown method: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn set_visible_size_is_not_implemented() {
        // `Emulation.setVisibleSize` is non-standard and was removed from the
        // ack list — it must fall through to the -32601 fallback.
        let ctx = make_ctx().await;
        let err = handle("setVisibleSize", None, &ctx)
            .await
            .expect_err("expected method-not-implemented");
        assert_eq!(err.code, -32601);
    }

    #[tokio::test]
    async fn set_user_agent_override_updates_session_and_clears() {
        let ctx = make_ctx().await;

        // Set an override; extra headers (set out-of-band) must be preserved.
        ctx.session.write().await.set_overrides(RequestOverrides {
            user_agent: None,
            extra_headers: vec![("X-Keep".to_string(), "1".to_string())],
        });
        let params = json!({ "userAgent": "Mozilla/5.0 (TestUA/1.0)" });
        let result = handle("setUserAgentOverride", Some(params), &ctx)
            .await
            .unwrap();
        assert_eq!(result, Some(json!({})));

        {
            let session = ctx.session.read().await;
            assert_eq!(
                session.overrides().user_agent.as_deref(),
                Some("Mozilla/5.0 (TestUA/1.0)")
            );
            assert_eq!(session.effective_ua(), "Mozilla/5.0 (TestUA/1.0)");
            assert_eq!(
                session.overrides().extra_headers,
                vec![("X-Keep".to_string(), "1".to_string())]
            );
        }

        // Empty UA clears the override but keeps extra headers; a null
        // userAgent does the same.
        for params in [json!({ "userAgent": "" }), json!({ "userAgent": null })] {
            handle("setUserAgentOverride", Some(params), &ctx)
                .await
                .unwrap();
            let session = ctx.session.read().await;
            assert!(session.overrides().user_agent.is_none());
            assert!(!session.effective_ua().is_empty());
            assert_eq!(
                session.overrides().extra_headers,
                vec![("X-Keep".to_string(), "1".to_string())]
            );
        }
    }

    #[tokio::test]
    async fn set_user_agent_override_missing_params_clears() {
        let ctx = make_ctx().await;
        handle(
            "setUserAgentOverride",
            Some(json!({ "userAgent": "UA/1" })),
            &ctx,
        )
        .await
        .unwrap();
        assert!(ctx.session.read().await.overrides().user_agent.is_some());

        handle("setUserAgentOverride", None, &ctx).await.unwrap();
        assert!(ctx.session.read().await.overrides().user_agent.is_none());
    }

    #[tokio::test]
    async fn set_device_metrics_override_clamps_zero_dimensions() {
        let _g = serial().await;
        let ctx = make_ctx().await;
        *DEVICE_METRICS.write() = None;

        let params = json!({
            "width": 0u32,
            "height": 0u32,
        });
        handle("setDeviceMetricsOverride", Some(params), &ctx)
            .await
            .unwrap();

        let stored = current_device_metrics().expect("metrics should be set");
        assert!(stored.width >= 1);
        assert!(stored.height >= 1);
    }

    #[tokio::test]
    async fn set_device_metrics_override_applies_defaults_when_params_missing() {
        let _g = serial().await;
        let ctx = make_ctx().await;
        *DEVICE_METRICS.write() = None;

        handle("setDeviceMetricsOverride", None, &ctx)
            .await
            .unwrap();
        let stored = current_device_metrics().expect("metrics should be set");
        assert_eq!(stored.width, 1280);
        assert_eq!(stored.height, 800);
        assert_eq!(stored.device_scale_factor, 1.0);
        assert!(!stored.mobile);
    }

    // -- Emulation.setEmulatedMedia -------------------------------------
    //
    // The color-scheme override is a process-global (render static + JS
    // runtime state), so these tests run under the same serial lock as the
    // device-metrics tests.

    /// Live `prefers-color-scheme: dark` state via the session's JS runtime.
    async fn probe_dark(ctx: &DispatchContext) -> bool {
        let result = ctx
            .session
            .write()
            .await
            .evaluate_js("matchMedia('(prefers-color-scheme: dark)').matches")
            .await
            .expect("probe evaluate");
        result.value.and_then(|v| v.as_bool()).expect("bool result")
    }

    #[tokio::test]
    async fn set_emulated_media_applies_dark_and_light() {
        let _g = serial().await;
        let ctx = make_ctx().await;

        let result = handle(
            "setEmulatedMedia",
            Some(json!({ "features": [
                { "name": "prefers-color-scheme", "value": "dark" },
            ] })),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(result, Some(json!({ "ignored": [] })));
        assert!(probe_dark(&ctx).await, "dark override flips matchMedia");

        let result = handle(
            "setEmulatedMedia",
            Some(json!({ "features": [
                { "name": "prefers-color-scheme", "value": "light" },
            ] })),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(result, Some(json!({ "ignored": [] })));
        assert!(!probe_dark(&ctx).await, "light override restores light");
    }

    #[tokio::test]
    async fn set_emulated_media_ignores_unsupported_features() {
        let _g = serial().await;
        let ctx = make_ctx().await;

        let result = handle(
            "setEmulatedMedia",
            Some(json!({ "features": [
                { "name": "prefers-contrast", "value": "more" },
                { "name": "prefers-color-scheme", "value": "no-preference" },
            ] })),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(
            result,
            Some(json!({
                "ignored": [
                    "prefers-contrast",
                    "prefers-color-scheme=no-preference",
                ],
            }))
        );
        // Replace semantics: no usable entry → override cleared → light.
        assert!(!probe_dark(&ctx).await);
    }

    #[tokio::test]
    async fn set_emulated_media_empty_or_missing_features_clear() {
        let _g = serial().await;
        let ctx = make_ctx().await;

        handle(
            "setEmulatedMedia",
            Some(json!({ "features": [
                { "name": "prefers-color-scheme", "value": "dark" },
            ] })),
            &ctx,
        )
        .await
        .unwrap();
        assert!(probe_dark(&ctx).await);

        // Empty list clears.
        let result = handle("setEmulatedMedia", Some(json!({ "features": [] })), &ctx)
            .await
            .unwrap();
        assert_eq!(result, Some(json!({ "ignored": [] })));
        assert!(
            !probe_dark(&ctx).await,
            "empty features clears the override"
        );

        // Missing params entirely also clears (idempotent no-op).
        handle("setEmulatedMedia", None, &ctx).await.unwrap();
        assert!(!probe_dark(&ctx).await);
    }
}
