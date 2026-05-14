//! CDP Fetch domain handler.
//!
//! Handles network interception via Fetch.enable, Fetch.disable,
//! Fetch.continueRequest, Fetch.failRequest, Fetch.fulfillRequest, Fetch.continueResponse.
//!
//! When enabled, outgoing HTTP requests are matched against patterns
//! and a `Fetch.requestPaused` event is emitted for each matching request.
//! The client responds with continue/fail/fulfill.
//!
//! Architecture:
//! - Patterns stored in EventSender (globally, for all CDP sessions)
//! - emit_request_paused() called from network layer
//! - Paused requests tracked via paused_requests in DispatchContext
//! - Mock responses stored via mock_responses in DispatchContext

use crate::domains::{DispatchContext, DomainResult};
use crate::event::EventSender;
use crate::protocol::CdpError;
use serde_json::{json, Value};

/// Dispatch Fetch domain methods.
pub async fn handle(method: &str, params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    match method {
        // --- Enable/disable ---
        "enable" => enable(params, ctx),
        "disable" => disable(ctx),

        // --- Request interception actions ---
        "continueRequest" => continue_request(params, ctx).await,
        "failRequest" => fail_request(params, ctx).await,
        "fulfillRequest" => fulfill_request(params, ctx).await,
        "continueResponse" => continue_response(params, ctx).await,
        "getResponseBody" => get_response_body(params).await,
        "takeResponseBodyAsStream" => Ok(Some(json!({"streamId": 0}))),
        "restoreResponseBodyAsStream" => Ok(Some(json!({}))),

        _ => Err(CdpError {
            code: -32601,
            message: format!("Fetch.{} not implemented", method),
        }),
    }
}

// ---------------------------------------------------------------------------
// Enable / Disable
// ---------------------------------------------------------------------------

/// Fetch.enable — enables request interception with optional patterns.
fn enable(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let mut patterns = vec![FetchPattern::default()]; // Match all by default

    if let Some(p) = params {
        if let Some(arr) = p.get("patterns").and_then(|v| v.as_array()) {
            patterns.clear();
            for item in arr {
                if let Some(p) = parse_fetch_pattern(item) {
                    patterns.push(p);
                }
            }
        }
    }

    ctx.events.set_fetch_enabled(true);
    ctx.events.set_fetch_patterns(patterns.clone());
    tracing::info!("Fetch domain enabled with {} pattern(s)", patterns.len());
    Ok(Some(json!({})))
}

/// Fetch.disable — disables request interception.
fn disable(ctx: &DispatchContext) -> DomainResult {
    ctx.events.set_fetch_enabled(false);
    ctx.events.set_fetch_patterns(vec![]);
    tracing::info!("Fetch domain disabled");
    Ok(Some(json!({})))
}

// ---------------------------------------------------------------------------
// Request interception actions
// ---------------------------------------------------------------------------

/// Fetch.continueRequest — resume a paused request with modifications.
async fn continue_request(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let p = params.ok_or_else(|| CdpError {
        code: -32602,
        message: "continueRequest requires parameters".to_string(),
    })?;

    let request_id = p.get("requestId").and_then(|v| v.as_str()).unwrap_or("");
    tracing::debug!("Fetch.continueRequest for requestId={}", request_id);

    // Remove from paused requests (request will proceed normally)
    ctx.paused_requests.write().remove(request_id);
    ctx.mock_responses.write().remove(request_id);

    Ok(Some(json!({})))
}

/// Fetch.failRequest — fail a paused request with an error.
async fn fail_request(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let p = params.ok_or_else(|| CdpError {
        code: -32602,
        message: "failRequest requires parameters".to_string(),
    })?;

    let request_id = p.get("requestId").and_then(|v| v.as_str()).unwrap_or("");
    let error_reason = p.get("errorReason").and_then(|v| v.as_str()).unwrap_or("Failed");
    tracing::debug!("Fetch.failRequest for requestId={}, reason={}", request_id, error_reason);

    // Remove from paused requests (request is aborted)
    ctx.paused_requests.write().remove(request_id);
    ctx.mock_responses.write().remove(request_id);

    Ok(Some(json!({})))
}

/// Fetch.fulfillRequest — return a fake response for a paused request.
async fn fulfill_request(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let p = params.ok_or_else(|| CdpError {
        code: -32602,
        message: "fulfillRequest requires parameters".to_string(),
    })?;

    let request_id = p.get("requestId").and_then(|v| v.as_str()).unwrap_or("");
    let status_code = p.get("statusCode").and_then(|v| v.as_i64()).unwrap_or(200) as u16;
    let status_text = p.get("statusText").and_then(|v| v.as_str()).unwrap_or("OK");
    let body = p.get("body").and_then(|v| v.as_str()).unwrap_or("");
    let base64_encoded = p.get("base64Encoded").and_then(|v| v.as_bool()).unwrap_or(false);

    // Extract response headers
    let mut headers = serde_json::Map::new();
    if let Some(h) = p.get("responseHeaders").and_then(|v| v.as_array()) {
        for item in h {
            if let (Some(k), Some(v)) = (item.get("name").and_then(|x| x.as_str()),
                                          item.get("value").and_then(|x| x.as_str())) {
                headers.insert(k.to_string(), json!(v));
            }
        }
    }

    // Store mock response
    let mock = crate::domains::MockResponse {
        body: body.to_string(),
        status: status_code,
        headers: headers.clone(),
        base64_encoded,
    };
    ctx.mock_responses.write().insert(request_id.to_string(), mock);
    // Remove from paused requests
    ctx.paused_requests.write().remove(request_id);

    tracing::debug!(
        "Fetch.fulfillRequest for requestId={}, status={}",
        request_id, status_code
    );

    Ok(Some(json!({
        "responseCode": status_code,
        "responsePhrase": status_text,
        "responseHeaders": headers,
        "binary": base64_encoded,
    })))
}

/// Fetch.continueResponse — continue a paused request with a modified response.
async fn continue_response(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let p = params.ok_or_else(|| CdpError {
        code: -32602,
        message: "continueResponse requires parameters".to_string(),
    })?;

    // Remove from paused requests (request will proceed normally)
    if let Some(request_id) = p.get("requestId").and_then(|v| v.as_str()) {
        ctx.paused_requests.write().remove(request_id);
        ctx.mock_responses.write().remove(request_id);
    }

    Ok(Some(json!({})))
}

/// Fetch.getResponseBody — returns body for an intercepted request.
async fn get_response_body(params: Option<Value>) -> DomainResult {
    let _request_id = params
        .as_ref()
        .and_then(|p| p.get("requestId").and_then(|v| v.as_str()))
        .unwrap_or("");

    // TODO: Look up actual body from paused request
    Ok(Some(json!({
        "body": "",
        "base64Encoded": false,
    })))
}

// ---------------------------------------------------------------------------
// Event emission (called from network layer during requests)
// ---------------------------------------------------------------------------

/// Emit a `Fetch.requestPaused` event for an intercepted request.
pub fn emit_request_paused(
    events: &EventSender,
    request_id: &str,
    url: &str,
    method: &str,
    headers: &[(String, String)],
    resource_type: &str,
) {
    let headers_json: serde_json::Map<String, serde_json::Value> = headers
        .iter()
        .map(|(k, v)| (k.clone(), json!(v)))
        .collect();

    events.send_fetch_event(
        "Fetch.requestPaused",
        json!({
            "requestId": request_id,
            "request": {
                "url": url,
                "method": method,
                "headers": headers_json,
                "initialPriority": "VeryHigh",
                "urlFragment": "",
                "postData": serde_json::Value::Null,
            },
            "resourceType": resource_type,
            "frameId": "main",
            "networkIntercepted": true,
        }),
    );
}

// ---------------------------------------------------------------------------
// Pattern matching
// ---------------------------------------------------------------------------

/// A request interception pattern.
#[derive(Debug, Clone, Default)]
pub struct FetchPattern {
    /// URL pattern (glob or regex pattern).
    pub url_pattern: String,
    /// Resource type filter (Document, Script, Image, XHR, etc.).
    pub resource_type: Option<String>,
    /// Request stage filter (Request, Response).
    pub request_stage: Option<String>,
}

impl FetchPattern {
    /// Check if a URL matches this pattern.
    pub fn matches_url(&self, url: &str) -> bool {
        if self.url_pattern.is_empty() || self.url_pattern == "*" {
            return true;
        }
        let pattern = &self.url_pattern;

        // Both starts and ends with * → contains (substring)
        if pattern.starts_with('*') && pattern.ends_with('*') {
            let inner = &pattern[1..pattern.len() - 1];
            return url.contains(inner);
        }

        // Only ends with * → prefix match (starts with)
        if pattern.ends_with('*') {
            let prefix = &pattern[..pattern.len() - 1];
            return url.starts_with(prefix);
        }

        // Only starts with * → suffix match (ends with)
        if let Some(suffix) = pattern.strip_prefix('*') {
            return url.ends_with(suffix) || url.contains(suffix);
        }

        // No wildcards → exact match
        url == pattern
    }
}

/// Parse a CDP FetchPattern JSON object.
fn parse_fetch_pattern(value: &serde_json::Value) -> Option<FetchPattern> {
    let obj = value.as_object()?;
    Some(FetchPattern {
        url_pattern: obj.get("urlPattern")
            .and_then(|v| v.as_str())
            .unwrap_or("*")
            .to_string(),
        resource_type: obj.get("resourceType")
            .and_then(|v| v.as_str())
            .map(String::from),
        request_stage: obj.get("requestStage")
            .and_then(|v| v.as_str())
            .map(String::from),
    })
}

/// Check if a request URL matches any enabled pattern.
pub fn matches_patterns(url: &str, patterns: &[FetchPattern]) -> bool {
    patterns.iter().any(|p| p.matches_url(url))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fetch_pattern_matches_all() {
        let pattern = FetchPattern::default();
        assert!(pattern.matches_url("http://example.com"));
        assert!(pattern.matches_url("https://anything.example.org/path"));
        assert!(pattern.matches_url("data:text/html,<h1>Hi</h1>"));
    }

    #[test]
    fn test_fetch_pattern_matches_prefix() {
        let pattern = FetchPattern {
            url_pattern: "http://example.com/*".to_string(),
            ..Default::default()
        };
        assert!(pattern.matches_url("http://example.com/"));
        assert!(pattern.matches_url("http://example.com/path"));
        assert!(!pattern.matches_url("http://other.com/"));
    }

    #[test]
    fn test_fetch_pattern_matches_suffix() {
        let pattern = FetchPattern {
            url_pattern: "*.example.com".to_string(),
            ..Default::default()
        };
        assert!(pattern.matches_url("http://foo.example.com"));
        assert!(pattern.matches_url("http://bar.example.com/path"));
        assert!(!pattern.matches_url("http://example.com"));
    }

    #[test]
    fn test_fetch_pattern_matches_substring() {
        let pattern = FetchPattern {
            url_pattern: "*api*".to_string(),
            ..Default::default()
        };
        assert!(pattern.matches_url("http://example.com/api/v1"));
        assert!(pattern.matches_url("https://my-api.example.com"));
        assert!(!pattern.matches_url("http://example.com/rest"));
    }

    #[test]
    fn test_fetch_pattern_exact_match() {
        let pattern = FetchPattern {
            url_pattern: "http://example.com/path".to_string(),
            ..Default::default()
        };
        assert!(pattern.matches_url("http://example.com/path"));
        assert!(!pattern.matches_url("http://example.com/other"));
    }

    #[test]
    fn test_matches_patterns() {
        let patterns = vec![
            FetchPattern {
                url_pattern: "*.example.com".to_string(),
                ..Default::default()
            },
            FetchPattern {
                url_pattern: "http://api.site.com/*".to_string(),
                ..Default::default()
            },
        ];
        assert!(matches_patterns("http://foo.example.com", &patterns));
        assert!(matches_patterns("http://api.site.com/data", &patterns));
        assert!(!matches_patterns("http://other.com/", &patterns));
    }
}