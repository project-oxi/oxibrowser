//! CDP Network domain handler.
//!
//! Handles Network domain methods and emits network lifecycle events when enabled:
//! - Network.enable / disable
//! - Network.requestWillBeSent, responseReceived, loadingFinished (events)
//! - Cookie CRUD: getAllCookies, setCookie, deleteCookies
//! - Response body: getResponseBody (stub)

use crate::domains::{DispatchContext, DomainResult};
use crate::event::EventSender;
use crate::protocol::CdpError;
use oxibrowser_core::security::redact::{RedactionProfile, redact_url_query};
use serde_json::{Value, json};

/// Redaction profile for CDP network event URLs. Event headers are currently
/// emitted as empty objects — if request headers are ever added to these
/// events they MUST pass through `redact::redact_headers` (design §7 P0-1).
fn event_url_redaction() -> RedactionProfile {
    oxibrowser_core::security::redact::active_profile()
}

/// Dispatch Network domain methods.
pub async fn handle(method: &str, params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    match method {
        // --- State ---
        "enable" => enable(ctx),
        "disable" => disable(ctx),
        // No HTTP cache layer exists (responses are always fetched fresh), so
        // enabling cache-disabling is a no-op over an empty set.
        "setCacheDisabled" => Ok(Some(json!({}))),
        "setExtraHTTPHeaders" => set_extra_http_headers(params, ctx).await,
        "emulateNetworkConditions" => emulate_network_conditions(params, ctx).await,

        // --- Cookies ---
        "getAllCookies" => get_all_cookies(ctx).await,
        "getCookies" => get_cookies(params, ctx).await,
        "setCookie" => set_cookie(params, ctx).await,
        "deleteCookies" => delete_cookies(params, ctx).await,

        // --- Response body ---
        "getResponseBody" => get_response_body(params, ctx).await,
        "getRequestPostData" => get_request_post_data(params, ctx).await,

        // --- Extra ---
        "setRequestInterception" => Ok(Some(json!({}))),
        "authRequired" => Ok(Some(json!({}))),

        _ => Err(CdpError {
            code: -32601,
            message: format!("Network.{} not implemented", method),
        }),
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Network.enable — enables network tracking.
fn enable(ctx: &DispatchContext) -> DomainResult {
    ctx.events.set_network_enabled(true);
    Ok(Some(json!({})))
}

/// Network.disable — disables network tracking.
fn disable(ctx: &DispatchContext) -> DomainResult {
    ctx.events.set_network_enabled(false);
    Ok(Some(json!({})))
}

/// `Network.setExtraHTTPHeaders` — replace the session's extra header set.
///
/// `params.headers` is a header object (`name → string value`); `null` or a
/// missing param clears the extra headers. The `user_agent` override set via
/// `Emulation.setUserAgentOverride` is preserved (Chrome keeps the two
/// independent). Transport-managed header names (`Cookie`, `Host`,
/// `Content-Length` — case-insensitive) are accepted here but skipped by the
/// HTTP client at request time, since the cookie jar / connection own them.
async fn set_extra_http_headers(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let headers_val = params
        .and_then(|p| p.get("headers").cloned())
        .unwrap_or(Value::Null);

    let mut session = ctx.session.write().await;
    let mut overrides = session.overrides().clone();
    overrides.extra_headers = match headers_val {
        Value::Object(map) => map
            .into_iter()
            .filter_map(|(name, value)| value.as_str().map(|v| (name, v.to_string())))
            .collect(),
        // null / missing / non-object → clear.
        _ => Vec::new(),
    };
    session.set_overrides(overrides);
    drop(session);

    tracing::debug!("Network.setExtraHTTPHeaders");
    Ok(Some(json!({})))
}

/// `Network.emulateNetworkConditions` — only the `offline` flag is honored;
/// it gates every Session-issued fetch (documents, sub-resources, POSTs) and
/// JS-issued fetches at the bridge. `latency` (ms) and
/// `downloadThroughput`/`uploadThroughput` (bytes/s) are accepted for
/// protocol compatibility but not applied — there is no throttling layer.
async fn emulate_network_conditions(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_default();
    let offline = params
        .get("offline")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    ctx.session.read().await.set_offline(offline);
    tracing::debug!(offline, "Network.emulateNetworkConditions");
    Ok(Some(json!({})))
}

// ---------------------------------------------------------------------------
// Cookies
// ---------------------------------------------------------------------------

/// Network.getAllCookies — returns all cookies for the session.
async fn get_all_cookies(ctx: &DispatchContext) -> DomainResult {
    super::deny_in_credential_mode(ctx)?;
    let session = ctx.session.read().await;
    let jar_guard = session.cookie_jar().read();
    let cookies = jar_guard.get_all();

    let result: Vec<serde_json::Value> = cookies
        .iter()
        .map(|c| {
            json!({
                "name": c.name,
                "value": c.value,
                "domain": c.domain.as_deref().unwrap_or(""),
                "path": c.path.as_deref().unwrap_or("/"),
                "expires": -1.0_f64,
                "size": c.name.len() + c.value.len(),
                "httpOnly": c.http_only,
                "secure": c.secure,
                "session": true,
                "sameParty": false,
                "sameSite": "None",
                "priority": "Medium",
                "partitionKey": serde_json::Value::Null,
            })
        })
        .collect();

    Ok(Some(json!({ "cookies": result })))
}

/// Network.getCookies — returns cookies for specific URLs.
async fn get_cookies(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    super::deny_in_credential_mode(ctx)?;
    let urls = params
        .as_ref()
        .and_then(|p| p.get("urls"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let session = ctx.session.read().await;
    let jar_guard = session.cookie_jar().read();

    let mut result = Vec::new();
    for url_str in &urls {
        if let Ok(url) = url::Url::parse(url_str) {
            let cookies_str = jar_guard.cookies_for_url(&url);
            for cookie_str in cookies_str.split(';') {
                let cookie_str = cookie_str.trim();
                if let Some(eq) = cookie_str.find('=') {
                    let name = cookie_str[..eq].trim().to_string();
                    let value = cookie_str[eq + 1..].trim().to_string();
                    result.push(json!({
                        "name": name,
                        "value": value,
                        "domain": url.domain().unwrap_or(""),
                        "path": "/",
                        "expires": -1.0_f64,
                        "size": name.len() + value.len(),
                        "httpOnly": false,
                        "secure": url.scheme() == "https",
                        "session": true,
                        "sameSite": "Lax",
                    }));
                }
            }
        }
    }

    Ok(Some(json!({ "cookies": result })))
}

/// Network.setCookie — creates a cookie with given properties.
async fn set_cookie(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    super::deny_in_credential_mode(ctx)?;
    let p = params.ok_or_else(|| CdpError {
        code: -32602,
        message: "setCookie requires parameters".to_string(),
    })?;

    let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let value = p.get("value").and_then(|v| v.as_str()).unwrap_or("");
    let url = p.get("url").and_then(|v| v.as_str());
    let path = p.get("path").and_then(|v| v.as_str()).unwrap_or("/");
    let secure = p.get("secure").and_then(|v| v.as_bool()).unwrap_or(false);
    let http_only = p.get("httpOnly").and_then(|v| v.as_bool()).unwrap_or(false);

    let target_url = url.ok_or_else(|| CdpError {
        code: -32602,
        message: "setCookie requires url".to_string(),
    })?;

    let parsed = url::Url::parse(target_url).map_err(|_| CdpError {
        code: -32602,
        message: format!("invalid URL: {}", target_url),
    })?;

    let session = ctx.session.read().await;
    let mut jar_guard = session.cookie_jar().write();

    let mut header = format!("{}={}", name, value);
    header.push_str(&format!("; Path={}", path));
    if secure {
        header.push_str("; Secure");
    }
    if http_only {
        header.push_str("; HttpOnly");
    }

    jar_guard.store(&parsed, &header);

    Ok(Some(json!({ "success": true })))
}

/// Network.deleteCookies — removes cookies matching the given name for a URL.
async fn delete_cookies(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    super::deny_in_credential_mode(ctx)?;
    let p = params.ok_or_else(|| CdpError {
        code: -32602,
        message: "deleteCookies requires parameters".to_string(),
    })?;

    let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let url = p.get("url").and_then(|v| v.as_str()).unwrap_or("");

    let parsed = url::Url::parse(url).map_err(|_| CdpError {
        code: -32602,
        message: format!("invalid URL: {}", url),
    })?;

    let session = ctx.session.read().await;
    let mut jar_guard = session.cookie_jar().write();
    jar_guard.remove(&parsed, name);

    Ok(Some(json!({ "success": true })))
}

// ---------------------------------------------------------------------------
// Response body
// ---------------------------------------------------------------------------

/// Network.getResponseBody — returns body of a network response.
async fn get_response_body(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.ok_or_else(|| CdpError {
        code: -32602,
        message: "getResponseBody requires parameters".to_string(),
    })?;

    let request_id = params
        .get("requestId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CdpError {
            code: -32602,
            message: "requestId required".to_string(),
        })?;

    let session = ctx.session.read().await;

    if let Some(captured) = session.get_response_body(request_id) {
        let body = if captured.base64 {
            // For binary content, we could base64-encode here
            captured.body
        } else {
            captured.body
        };
        Ok(Some(json!({
            "body": body,
            "base64Encoded": captured.base64,
        })))
    } else {
        Err(CdpError {
            code: -32602,
            message: format!("Could not find body for requestId: {}", request_id),
        })
    }
}

/// `Network.getRequestPostData` — return the recorded POST body of a request.
///
/// Looks `params.requestId` up in the session's rolling network log. When the
/// body was truncated at the log cap (`post_body_truncated`), the stored
/// prefix is decoded and returned as-is: the remainder was never captured,
/// so a truncated prefix beats failing the call (documented deviation from
/// Chrome, which returns the full body).
async fn get_request_post_data(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.ok_or_else(|| CdpError {
        code: -32602,
        message: "getRequestPostData requires parameters".to_string(),
    })?;
    let request_id = params
        .get("requestId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CdpError {
            code: -32602,
            message: "requestId required".to_string(),
        })?;

    let records = ctx.session.read().await.network_log_snapshot();
    let post_data = request_post_data(&records, request_id)?;
    Ok(Some(json!({ "postData": post_data })))
}

/// Resolve `Network.getRequestPostData` against a network-log snapshot.
///
/// Pure helper: `Ok(body)` when the request carries a POST body (decoded
/// UTF-8, lossy for invalid sequences); the `-32000`
/// "No resource with given identifier found" error when the id is unknown or
/// the request recorded no body.
fn request_post_data(
    records: &[oxibrowser_core::session::RequestRecord],
    request_id: &str,
) -> Result<String, CdpError> {
    records
        .iter()
        .find(|r| r.request_id == request_id)
        .and_then(|r| r.post_body.as_deref())
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .ok_or(CdpError {
            code: -32000,
            message: "No resource with given identifier found".to_string(),
        })
}

// ---------------------------------------------------------------------------
// Event emission (called from Page domain during navigation)
// ---------------------------------------------------------------------------

/// Emit network events for a navigation request.
///
/// Emits all three events (requestWillBeSent, responseReceived, loadingFinished).
/// Used by callers that need the full lifecycle in one call.
pub fn emit_navigation_events(
    events: &EventSender,
    request_id: &str,
    url: &str,
    loader_id: &str,
    status: u16,
    content_type: &str,
) {
    let timestamp = EventSender::timestamp_ms();
    let url = redact_url_query(url, &event_url_redaction());

    events.send_network_event(
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
            "timestamp": timestamp,
            "wallTime": timestamp / 1000.0,
            "initiator": { "type": "other" },
            "type": "Document",
            "frameId": "main",
            "hasUserGesture": false,
        }),
    );

    emit_response_events(events, request_id, &url, loader_id, status, content_type);
}

/// Emit only the response lifecycle events (responseReceived + loadingFinished).
///
/// Used when requestWillBeSent was already emitted before navigation,
/// and only the response events are needed after navigation completes.
pub fn emit_response_events(
    events: &EventSender,
    request_id: &str,
    url: &str,
    loader_id: &str,
    status: u16,
    content_type: &str,
) {
    let timestamp = EventSender::timestamp_ms();
    let url = redact_url_query(url, &event_url_redaction());

    events.send_network_event(
        "Network.responseReceived",
        json!({
            "requestId": request_id,
            "loaderId": loader_id,
            "timestamp": timestamp,
            "type": "Document",
            "response": {
                "url": url,
                "status": status,
                "statusText": if status == 200 { "OK" } else { "" },
                "headers": { "Content-Type": content_type },
                "mimeType": content_type,
                "connectionReused": false,
                "connectionId": 0.0,
                "encodedDataLength": 0.0,
                "securityState": "secure",
            },
            "frameId": "main",
        }),
    );

    events.send_network_event(
        "Network.loadingFinished",
        json!({
            "requestId": request_id,
            "timestamp": timestamp,
            "encodedDataLength": 0.0,
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::event_channel;
    use oxibrowser_core::network::intercept::shared_registry;
    use oxibrowser_core::session::RequestOverrides;
    use oxibrowser_core::{Browser, BrowserConfig};
    use std::sync::Arc;

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
    async fn set_extra_http_headers_replaces_and_clears() {
        let ctx = make_ctx().await;

        // Seed a UA override that must survive header updates.
        ctx.session.write().await.set_overrides(RequestOverrides {
            user_agent: Some("OverrideUA/1.0".to_string()),
            extra_headers: Vec::new(),
        });

        let params = json!({
            "headers": { "X-Test": "1", "Accept-Language": "ko-KR" },
        });
        handle("setExtraHTTPHeaders", Some(params), &ctx)
            .await
            .unwrap();
        {
            let session = ctx.session.read().await;
            // JSON objects are unordered — compare as sets.
            let mut got = session.overrides().extra_headers.clone();
            got.sort();
            assert_eq!(
                got,
                vec![
                    ("Accept-Language".to_string(), "ko-KR".to_string()),
                    ("X-Test".to_string(), "1".to_string()),
                ]
            );
            // user_agent preserved.
            assert_eq!(
                session.overrides().user_agent.as_deref(),
                Some("OverrideUA/1.0")
            );
        }

        // Explicit null clears extra headers, still preserving the UA.
        handle(
            "setExtraHTTPHeaders",
            Some(json!({ "headers": null })),
            &ctx,
        )
        .await
        .unwrap();
        {
            let session = ctx.session.read().await;
            assert!(session.overrides().extra_headers.is_empty());
            assert_eq!(
                session.overrides().user_agent.as_deref(),
                Some("OverrideUA/1.0")
            );
        }
    }

    #[tokio::test]
    async fn emulate_network_conditions_toggles_offline() {
        let ctx = make_ctx().await;
        assert!(!ctx.session.read().await.is_offline());

        handle(
            "emulateNetworkConditions",
            Some(json!({ "offline": true, "latency": 100, "downloadThroughput": -1.0, "uploadThroughput": -1.0 })),
            &ctx,
        )
        .await
        .unwrap();
        assert!(ctx.session.read().await.is_offline());

        handle(
            "emulateNetworkConditions",
            Some(json!({ "offline": false })),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!ctx.session.read().await.is_offline());
    }

    #[tokio::test]
    async fn set_cache_disabled_acknowledges() {
        let ctx = make_ctx().await;
        let r = handle(
            "setCacheDisabled",
            Some(json!({ "cacheDisabled": true })),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(r, Some(json!({})));
    }

    // -- Network.getRequestPostData --------------------------------------

    use oxibrowser_core::session::RequestRecord;

    fn record_with_post_body(request_id: &str, post_body: Option<Vec<u8>>) -> RequestRecord {
        RequestRecord {
            request_id: request_id.to_string(),
            url: "http://example.com/submit".to_string(),
            method: "POST".to_string(),
            resource_type: "Fetch".to_string(),
            request_headers: Vec::new(),
            post_body,
            post_body_truncated: false,
            status: Some(200),
            response_headers: Vec::new(),
            mime_type: String::new(),
            started_at_ms: 1_000_000.0,
            finished_at_ms: Some(1_000_042.0),
            from_cache: false,
            response_body_length: None,
        }
    }

    #[test]
    fn request_post_data_miss_is_no_such_resource_error() {
        let records = vec![record_with_post_body("oxi-1", Some(b"payload".to_vec()))];
        let err = request_post_data(&records, "oxi-404").expect_err("unknown id must error");
        assert_eq!(err.code, -32000);
        assert_eq!(err.message, "No resource with given identifier found");
    }

    #[test]
    fn request_post_data_decodes_body_and_errors_when_body_absent() {
        // Known id with a body → decoded (lossy UTF-8) string.
        let records = vec![record_with_post_body(
            "oxi-1",
            Some("héllo=1".as_bytes().to_vec()),
        )];
        assert_eq!(request_post_data(&records, "oxi-1").unwrap(), "héllo=1");

        // Known id without a recorded body → same -32000 as a miss.
        let records = vec![record_with_post_body("oxi-2", None)];
        let err = request_post_data(&records, "oxi-2").expect_err("absent body must error");
        assert_eq!(err.code, -32000);
    }
}
