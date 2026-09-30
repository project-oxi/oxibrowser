//! CLI output utilities — standard JSON response wrapper, truncation, field filtering.

use serde_json::Value;

/// Exit codes for agent-consumable error classification.
pub mod exit_code {
    /// Success (including truncated output).
    pub const OK: i32 = 0;
    /// Runtime error: DOM not found, JS error, element not found.
    pub const RUNTIME: i32 = 1;
    /// Input validation failure: bad URL, control chars, path traversal.
    pub const INPUT: i32 = 2;
    /// Timeout.
    pub const TIMEOUT: i32 = 3;
    /// Network error: DNS, connection refused, HTTP 4xx/5xx.
    pub const NETWORK: i32 = 4;
    /// Consent required: a grant/confirmation is missing or pending. The
    /// JSON error envelope carries structured `details`
    /// (`request_id`, `ttl`, `account`, `action`) so callers can resolve
    /// without parsing the message.
    pub const CONSENT_REQUIRED: i32 = 5;
}

/// Standard CLI JSON response wrapper.
///
/// Every JSON output from oxibrowser uses this structure so agents can
/// parse uniformly: check `ok`, then read `data` or `error`.
#[derive(Debug, serde::Serialize)]
pub struct CliResponse {
    /// Success flag. Always present.
    pub ok: bool,
    /// Result data on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    /// Human-readable error message on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Machine-readable error code on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Structured error payload on failure. Populated for
    /// `CONSENT_REQUIRED` (`{request_id, ttl, account, action}`); other
    /// codes may adopt it later. Absent on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// Metadata (timing, tab info).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

/// Response metadata.
#[derive(Debug, serde::Serialize)]
pub struct Meta {
    /// Tab ID if the command operated on a tab.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
    /// Wall-clock time in milliseconds.
    pub elapsed_ms: u64,
    /// Search source (web, github, etc.) — only for search command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Search engine (ddg, wiki, bing, etc.) — only for search command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
}

impl CliResponse {
    /// Create a success response with data.
    pub fn success(data: Value) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
            error_code: None,
            details: None,
            meta: None,
        }
    }

    /// Create a success response with data and metadata.
    pub fn success_with_meta(data: Value, tab_id: Option<String>, elapsed_ms: u64) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
            error_code: None,
            details: None,
            meta: Some(Meta {
                tab_id,
                elapsed_ms,
                source: None,
                engine: None,
            }),
        }
    }

    /// Create a success response for search commands (includes source/engine in meta).
    pub fn success_with_search_meta(
        data: Value,
        elapsed_ms: u64,
        source: &str,
        engine: &str,
    ) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
            error_code: None,
            details: None,
            meta: Some(Meta {
                tab_id: None,
                elapsed_ms,
                source: Some(source.to_string()),
                engine: Some(engine.to_string()),
            }),
        }
    }

    /// Create an error response.
    pub fn error(error: impl Into<String>, error_code: impl AsRef<str>) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(error.into()),
            error_code: Some(error_code.as_ref().to_string()),
            details: None,
            meta: None,
        }
    }

    /// Create an error response carrying a structured payload — the
    /// `CONSENT_REQUIRED` contract (`details: {request_id, ttl, account,
    /// action}`; `request_id`/`ttl` are null unless a confirmation is
    /// actually pending).
    pub fn error_with_details(
        error: impl Into<String>,
        error_code: impl AsRef<str>,
        details: Value,
    ) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(error.into()),
            error_code: Some(error_code.as_ref().to_string()),
            details: Some(details),
            meta: None,
        }
    }

    /// Create an error response with metadata.
    #[allow(dead_code)]
    pub fn error_with_meta(
        error: impl Into<String>,
        error_code: impl AsRef<str>,
        tab_id: Option<String>,
        elapsed_ms: u64,
    ) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(error.into()),
            error_code: Some(error_code.as_ref().to_string()),
            details: None,
            meta: Some(Meta {
                tab_id,
                elapsed_ms,
                source: None,
                engine: None,
            }),
        }
    }

    /// Create CliResponse from an InputError.
    pub fn from_validation(e: crate::validate::InputError) -> Self {
        CliResponse::error(e.to_string(), e.error_code())
    }

    /// Determine the exit code from the error_code field.
    pub fn exit_code(&self) -> i32 {
        if self.ok {
            return exit_code::OK;
        }
        match self.error_code.as_deref() {
            Some("INVALID_URL")
            | Some("INVALID_SELECTOR")
            | Some("INPUT_VALIDATION")
            | Some("PATH_TRAVERSAL")
            | Some("SSRF_BLOCKED") => exit_code::INPUT,
            Some("TIMEOUT") => exit_code::TIMEOUT,
            Some("NETWORK_ERROR") | Some("HTTP_ERROR") => exit_code::NETWORK,
            Some("CONSENT_REQUIRED") => exit_code::CONSENT_REQUIRED,
            _ => exit_code::RUNTIME,
        }
    }

    /// Serialize and print as JSON to stdout. Always includes meta.
    pub fn print_json(&self) -> i32 {
        let code = self.exit_code();
        let mut resp = serde_json::to_value(self).unwrap_or_else(|e| {
            serde_json::json!({"ok":false,"error":format!("serialization: {e}"),"error_code":"INTERNAL"})
        });
        // Ensure meta is always present
        if let Some(obj) = resp.as_object_mut() {
            match obj.get_mut("meta") {
                Some(meta) if !meta.is_null() => {}
                _ => {
                    obj.insert("meta".into(), serde_json::json!({"elapsed_ms": 0}));
                }
            }
        }
        println!("{}", serde_json::to_string(&resp).unwrap_or_default());
        code
    }

    /// Alias for print_json (backwards compat).
    #[allow(dead_code)]
    pub fn print(&self) -> i32 {
        self.print_json()
    }

    /// Print and exit the process.
    #[allow(dead_code)]
    pub fn print_and_exit(&self) -> ! {
        std::process::exit(self.print_json());
    }
}

// ---------------------------------------------------------------------------
// Output format detection
// ---------------------------------------------------------------------------

/// Whether stdout is a TTY (terminal).
pub fn is_stdout_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal()
}

/// Whether JSON output should be used.
///
/// JSON is used when:
/// 1. `--json` flag is explicitly set, OR
/// 2. stdout is not a TTY (piped/redirected)
pub fn should_output_json(explicit_json: bool) -> bool {
    explicit_json || !is_stdout_tty()
}

/// Whether JSON output should be used (alias).
#[allow(dead_code)]
pub fn json_mode(explicit_json: bool) -> bool {
    should_output_json(explicit_json)
}

// ---------------------------------------------------------------------------
// Truncation
// ---------------------------------------------------------------------------

/// Truncate string fields within a JSON object to fit within `max_bytes`.
///
/// Adds `truncated`, `total_bytes`, `returned_bytes` to the object.
/// Only truncates string-valued fields. Non-string fields are left intact
/// but counted toward the budget.
pub fn truncate_fields(data: &mut Value, max_bytes: u64) {
    if max_bytes == 0 {
        return;
    }

    let obj = match data.as_object_mut() {
        Some(o) => o,
        None => return,
    };

    // Measure total string content
    let mut total: usize = 0;
    let mut string_keys: Vec<String> = Vec::new();
    for (key, val) in obj.iter() {
        if let Some(s) = val.as_str() {
            total += s.len();
            string_keys.push(key.clone());
        }
    }

    if total <= max_bytes as usize {
        return;
    }

    // Truncate: allocate budget proportionally, prioritize earlier fields
    let non_string_size: usize = obj
        .iter()
        .filter(|(_, v)| !v.is_string())
        .map(|(_, v)| v.to_string().len())
        .sum();

    let string_budget = max_bytes as usize - non_string_size.min(max_bytes as usize);
    let mut used = 0usize;

    for key in &string_keys {
        let val = obj.get_mut(key.as_str()).unwrap();
        if let Some(s) = val.as_str() {
            let remaining = string_budget.saturating_sub(used);
            if s.len() <= remaining {
                used += s.len();
            } else {
                // Truncate at char boundary
                let mut cut = remaining;
                while !s.is_char_boundary(cut) && cut > 0 {
                    cut -= 1;
                }
                *val = Value::String(s[..cut].to_string());
                used += cut;
            }
        }
    }

    obj.insert("truncated".into(), Value::Bool(true));
    obj.insert("total_bytes".into(), serde_json::json!(total));
    obj.insert("returned_bytes".into(), serde_json::json!(max_bytes));
}

// ---------------------------------------------------------------------------
// Field filtering
// ---------------------------------------------------------------------------

/// Filter a JSON object to only include specified field names.
pub fn filter_fields(data: &mut Value, fields: &[&str]) {
    if fields.is_empty() {
        return;
    }

    let obj = match data.as_object_mut() {
        Some(o) => o,
        None => return,
    };

    let allowed: std::collections::HashSet<&str> = fields.iter().copied().collect();
    let keys_to_remove: Vec<String> = obj
        .keys()
        .filter(|k| !allowed.contains(k.as_str()))
        .cloned()
        .collect();

    for key in keys_to_remove {
        obj.remove(&key);
    }
}

/// Parse a comma-separated fields string into a vec of field names.
pub fn parse_fields(fields: &str) -> Vec<&str> {
    fields
        .split(',')
        .map(|f| f.trim())
        .filter(|f| !f.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------
// Page summary
// ---------------------------------------------------------------------------

/// Build a page summary from a loaded page.
pub fn build_summary(page: &oxibrowser_core::page::Page) -> Value {
    let doc = page.root_frame().document();
    let headings: Vec<String> = {
        let h_tags = ["h1", "h2", "h3", "h4", "h5", "h6"];
        let mut result = Vec::new();
        for tag in h_tags {
            let ids = doc.query_selector_all(tag);
            for id in ids {
                if let Some(text) = doc.text_content(id) {
                    let trimmed = text.trim().to_string();
                    if !trimmed.is_empty() {
                        result.push(trimmed);
                    }
                }
            }
        }
        result
    };

    let links_count = doc.query_selector_all("a[href]").len();
    let forms_count = doc.query_selector_all("form").len();
    let images_count = doc.query_selector_all("img").len();
    let text_length = doc
        .body_id
        .and_then(|bid| doc.text_content(bid))
        .map(|t| t.len())
        .unwrap_or(0);

    serde_json::json!({
        "url": page.url().to_string(),
        "title": page.title().unwrap_or(""),
        "status": page.status(),
        "content_type": page.content_type(),
        "headings": headings,
        "links_count": links_count,
        "forms_count": forms_count,
        "images_count": images_count,
        "text_length": text_length,
    })
}

// ---------------------------------------------------------------------------
// Core error → error_code mapping
// ---------------------------------------------------------------------------

/// Map a core error to a machine-readable error code string.
pub fn core_error_code(error: &oxibrowser_core::error::CoreError) -> &'static str {
    if matches!(
        error,
        oxibrowser_core::error::CoreError::AccountLocked { .. }
    ) {
        return "ACCOUNT_LOCKED";
    }
    let msg = format!("{error}");
    if msg.contains("timeout") || msg.contains("timed out") {
        "TIMEOUT"
    } else if msg.contains("no element") || msg.contains("not found") {
        "DOM_NOT_FOUND"
    } else if msg.contains("JsError") || msg.contains("JS") {
        "JS_ERROR"
    } else if msg.contains("network") || msg.contains("dns") || msg.contains("connect") {
        "NETWORK_ERROR"
    } else if msg.contains("HTTP") || msg.contains("status") {
        "HTTP_ERROR"
    } else if msg.contains("page not loaded") {
        "PAGE_NOT_LOADED"
    } else if msg.contains("closed") {
        "TAB_CLOSED"
    } else {
        "RUNTIME_ERROR"
    }
}

/// Map a core error to an exit code.
#[allow(dead_code)]
pub fn core_exit_code(error: &oxibrowser_core::error::CoreError) -> i32 {
    match core_error_code(error) {
        "TIMEOUT" => exit_code::TIMEOUT,
        "NETWORK_ERROR" | "HTTP_ERROR" => exit_code::NETWORK,
        _ => exit_code::RUNTIME,
    }
}

// ---------------------------------------------------------------------------
// Unified CliError enum
// ---------------------------------------------------------------------------
//
// Additive: pre-existing error handling in main.rs / executor.rs is left
// untouched. New code can construct or convert into CliError to get a
// uniform {exit_code, code_str, message} surface. The `From<CoreError>`
// impl is heuristic — it inspects Display output for known markers. When
// callers have richer context (e.g. an HTTP status), they should build
// the matching variant directly instead of going through this From.

/// Unified classification of CLI errors, mapping directly to exit codes
/// and machine-readable error codes.
///
/// This enum is additive: existing error-handling call sites in
/// `main.rs` / `executor.rs` continue to format their own messages and
/// exit codes. New code that wants a uniform surface can construct a
/// [`CliError`] (or convert from a [`oxibrowser_core::error::CoreError`])
/// and use [`CliError::exit_code`] / [`CliError::code_str`].
// Additive public API — the surface mirrors `CoreError` for callers that
// want a single error type. Not all variants are constructed internally yet,
// hence the allow until downstream code starts using them.
#[allow(dead_code)]
#[derive(Debug)]
pub enum CliError {
    /// URL failed parsing or scheme/host validation (e.g. unsupported
    /// scheme, control characters in URL).
    InvalidUrl(String),
    /// CSS selector failed to parse or matched no elements in strict mode.
    InvalidSelector(String),
    /// Generic user-input validation failure (length, charset, etc.).
    InputValidation(String),
    /// Resolved output path would escape its expected root.
    PathTraversal(String),
    /// Request was blocked by the SSRF filter (private/loopback IP, etc.).
    SsrfBlocked(String),
    /// Operation exceeded its timeout. `timeout_ms` is `0` when the
    /// caller didn't track the configured budget.
    Timeout { timeout_ms: u64 },
    /// Network-level failure (DNS, connect, TLS handshake).
    Network(String),
    /// HTTP non-success status. `url` is the URL that produced it.
    Http { status: u16, url: String },
    /// JavaScript evaluation failed (syntax error, thrown exception,
    /// recursion / loop / stack budget exceeded).
    JsError(String),
    /// Catch-all for browser runtime errors not covered by a more
    /// specific variant.
    Runtime(String),
    /// Search-engine / search-result error (engine unreachable, empty
    /// result set, etc.).
    Search(String),
}

#[allow(dead_code)]
impl CliError {
    /// Exit code for this error class. Mirrors the [`exit_code`] module's
    /// constants but adds INPUT (`2`) for the four validation variants.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::InvalidUrl(_)
            | Self::InvalidSelector(_)
            | Self::InputValidation(_)
            | Self::PathTraversal(_)
            | Self::SsrfBlocked(_) => 2,
            Self::Timeout { .. } => 3,
            Self::Network(_) | Self::Http { .. } => 4,
            Self::JsError(_) | Self::Runtime(_) | Self::Search(_) => 1,
        }
    }

    /// Stable, machine-readable code for this error class.
    pub fn code_str(&self) -> &str {
        match self {
            Self::InvalidUrl(_) => "INVALID_URL",
            Self::InvalidSelector(_) => "INVALID_SELECTOR",
            Self::InputValidation(_) => "INPUT_VALIDATION",
            Self::PathTraversal(_) => "PATH_TRAVERSAL",
            Self::SsrfBlocked(_) => "SSRF_BLOCKED",
            Self::Timeout { .. } => "TIMEOUT",
            Self::Network(_) => "NETWORK_ERROR",
            Self::Http { .. } => "HTTP_ERROR",
            Self::JsError(_) => "JS_ERROR",
            Self::Runtime(_) => "RUNTIME_ERROR",
            Self::Search(_) => "SEARCH_ERROR",
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUrl(m)
            | Self::InvalidSelector(m)
            | Self::InputValidation(m)
            | Self::PathTraversal(m)
            | Self::SsrfBlocked(m)
            | Self::Network(m)
            | Self::JsError(m)
            | Self::Runtime(m)
            | Self::Search(m) => f.write_str(m),
            Self::Timeout { timeout_ms } => {
                write!(f, "operation timed out after {timeout_ms} ms")
            }
            Self::Http { status, url } => {
                write!(f, "HTTP {status} for {url}")
            }
        }
    }
}

impl std::error::Error for CliError {}

impl From<oxibrowser_core::error::CoreError> for CliError {
    fn from(e: oxibrowser_core::error::CoreError) -> Self {
        let msg = e.to_string();
        if msg.contains("SSRF") || msg.contains("ssrf") {
            Self::SsrfBlocked(msg)
        } else if msg.contains("timeout") || msg.contains("Timeout") || msg.contains("timed out") {
            Self::Timeout { timeout_ms: 0 }
        } else if msg.contains("network")
            || msg.contains("Network")
            || msg.contains("dns")
            || msg.contains("connect")
        {
            Self::Network(msg)
        } else if msg.contains("HTTP") || msg.contains("status") {
            // Heuristic; callers with a real status should build Http { .. }.
            Self::Runtime(msg)
        } else if msg.contains("JsError")
            || msg.contains("JS")
            || msg.contains("javascript")
            || msg.contains("JavaScript")
        {
            Self::JsError(msg)
        } else {
            Self::Runtime(msg)
        }
    }
}

// ---------------------------------------------------------------------------
// Print helpers for non-JSON mode
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_response_success_serializes() {
        let r = CliResponse::success(serde_json::json!({"url": "https://example.com"}));
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains(r#""ok":true"#));
        assert!(json.contains(r#""url":"https://example.com""#));
        assert!(!json.contains("error"));
    }

    #[test]
    fn test_response_error_serializes() {
        let r = CliResponse::error("not found", "DOM_NOT_FOUND");
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains(r#""ok":false"#));
        assert!(json.contains("not found"));
        assert!(json.contains("DOM_NOT_FOUND"));
        assert!(!json.contains("data"));
    }

    #[test]
    fn test_exit_code_mapping() {
        assert_eq!(
            CliResponse::error("x", "DOM_NOT_FOUND").exit_code(),
            exit_code::RUNTIME
        );
        assert_eq!(
            CliResponse::error("x", "INVALID_URL").exit_code(),
            exit_code::INPUT
        );
        assert_eq!(
            CliResponse::error("x", "TIMEOUT").exit_code(),
            exit_code::TIMEOUT
        );
        assert_eq!(
            CliResponse::error("x", "NETWORK_ERROR").exit_code(),
            exit_code::NETWORK
        );
        assert_eq!(
            CliResponse::success(serde_json::json!({})).exit_code(),
            exit_code::OK
        );
    }

    #[test]
    fn test_truncate_fields_no_truncation_needed() {
        let mut data = serde_json::json!({"text": "hello"});
        truncate_fields(&mut data, 100);
        // No truncation needed → no `truncated` key added
        assert!(data.get("truncated").is_none());
    }

    #[test]
    fn test_truncate_fields_truncates() {
        let mut data =
            serde_json::json!({"a": "short", "b": "a very long string that exceeds the limit"});
        truncate_fields(&mut data, 10);
        assert!(data.get("truncated").unwrap().as_bool().unwrap());
        assert!(data.get("total_bytes").unwrap().as_u64().unwrap() > 10);
        assert_eq!(data.get("returned_bytes").unwrap().as_u64().unwrap(), 10);
    }

    #[test]
    fn test_filter_fields() {
        let mut data =
            serde_json::json!({"url": "https://example.com", "title": "Example", "html": "<html>"});
        filter_fields(&mut data, &["url", "title"]);
        assert!(data.get("url").is_some());
        assert!(data.get("title").is_some());
        assert!(data.get("html").is_none());
    }

    #[test]
    fn test_parse_fields() {
        let fields = parse_fields("url, title, status");
        assert_eq!(fields, vec!["url", "title", "status"]);
    }

    // -- CliError -----------------------------------------------------------

    #[test]
    fn cli_error_exit_codes_match_existing_module() {
        // Validation class (2) must match exit_code::INPUT.
        assert_eq!(
            CliError::InvalidUrl("x".into()).exit_code(),
            exit_code::INPUT
        );
        assert_eq!(
            CliError::InvalidSelector("x".into()).exit_code(),
            exit_code::INPUT
        );
        assert_eq!(
            CliError::InputValidation("x".into()).exit_code(),
            exit_code::INPUT
        );
        assert_eq!(
            CliError::PathTraversal("x".into()).exit_code(),
            exit_code::INPUT
        );
        assert_eq!(
            CliError::SsrfBlocked("x".into()).exit_code(),
            exit_code::INPUT
        );
        // Timeout class.
        assert_eq!(
            CliError::Timeout { timeout_ms: 5_000 }.exit_code(),
            exit_code::TIMEOUT
        );
        // Network class.
        assert_eq!(
            CliError::Network("x".into()).exit_code(),
            exit_code::NETWORK
        );
        assert_eq!(
            CliError::Http {
                status: 503,
                url: "https://x".into()
            }
            .exit_code(),
            exit_code::NETWORK
        );
        // Runtime class (1).
        assert_eq!(
            CliError::JsError("x".into()).exit_code(),
            exit_code::RUNTIME
        );
        assert_eq!(
            CliError::Runtime("x".into()).exit_code(),
            exit_code::RUNTIME
        );
        assert_eq!(CliError::Search("x".into()).exit_code(), exit_code::RUNTIME);
    }

    #[test]
    fn cli_error_code_strs_are_stable() {
        assert_eq!(CliError::InvalidUrl("x".into()).code_str(), "INVALID_URL");
        assert_eq!(
            CliError::InvalidSelector("x".into()).code_str(),
            "INVALID_SELECTOR"
        );
        assert_eq!(
            CliError::InputValidation("x".into()).code_str(),
            "INPUT_VALIDATION"
        );
        assert_eq!(
            CliError::PathTraversal("x".into()).code_str(),
            "PATH_TRAVERSAL"
        );
        assert_eq!(CliError::SsrfBlocked("x".into()).code_str(), "SSRF_BLOCKED");
        assert_eq!(CliError::Timeout { timeout_ms: 0 }.code_str(), "TIMEOUT");
        assert_eq!(CliError::Network("x".into()).code_str(), "NETWORK_ERROR");
        assert_eq!(
            CliError::Http {
                status: 404,
                url: "u".into()
            }
            .code_str(),
            "HTTP_ERROR"
        );
        assert_eq!(CliError::JsError("x".into()).code_str(), "JS_ERROR");
        assert_eq!(CliError::Runtime("x".into()).code_str(), "RUNTIME_ERROR");
        assert_eq!(CliError::Search("x".into()).code_str(), "SEARCH_ERROR");
    }

    #[test]
    fn cli_error_from_core_error_classifies_ssrf() {
        // A network error mentioning a private/loopback address still has
        // no dedicated SSRF marker in the Display form of CoreError; the
        // builder is meant for direct construction in the SSRF path.
        // This test ensures the From impl does NOT silently misclassify
        // an arbitrary error as SSRF — only SSRF substring-bearing
        // messages should qualify.
        let e = oxibrowser_core::error::CoreError::NetworkError("127.0.0.1".into());
        let cli: CliError = e.into();
        // Falls through to Network (substring SSRF not present).
        assert_eq!(cli.code_str(), "NETWORK_ERROR");
    }

    #[test]
    fn cli_error_from_core_error_classifies_timeout() {
        // CoreError::Timeout displays as "timeout: ..." → Timeout.
        let e = oxibrowser_core::error::CoreError::Timeout("page load".into());
        let cli: CliError = e.into();
        assert_eq!(cli.code_str(), "TIMEOUT");
        assert_eq!(cli.exit_code(), exit_code::TIMEOUT);

        // ConnectionTimeout also contains "timeout".
        let e2 = oxibrowser_core::error::CoreError::ConnectionTimeout("connect".into());
        let cli2: CliError = e2.into();
        assert_eq!(cli2.code_str(), "TIMEOUT");
    }

    #[test]
    fn cli_error_from_core_error_classifies_network() {
        let e = oxibrowser_core::error::CoreError::NetworkError("connection refused".into());
        let cli: CliError = e.into();
        assert_eq!(cli.code_str(), "NETWORK_ERROR");
        assert_eq!(cli.exit_code(), exit_code::NETWORK);
    }

    #[test]
    fn cli_error_from_core_error_classifies_js() {
        let e = oxibrowser_core::error::CoreError::JsError("ReferenceError: x".into());
        let cli: CliError = e.into();
        assert_eq!(cli.code_str(), "JS_ERROR");
        assert_eq!(cli.exit_code(), exit_code::RUNTIME);
    }

    #[test]
    fn cli_error_from_core_error_classifies_invalid_url() {
        let e = oxibrowser_core::error::CoreError::InvalidUrl("parse fail".into());
        let cli: CliError = e.into();
        // Display message contains "URL" → falls into Runtime per the
        // current heuristic. The intent is for callers to construct
        // InvalidUrl directly when they know the cause, while keeping
        // the From impl conservative.
        assert_eq!(cli.code_str(), "RUNTIME_ERROR");
    }

    #[test]
    fn consent_required_maps_to_exit_5_with_details() {
        let resp = CliResponse::error_with_details(
            "consent required: gh-work may not be used for `login`",
            "CONSENT_REQUIRED",
            serde_json::json!({
                "error_code": "CONSENT_REQUIRED",
                "request_id": null,
                "ttl": null,
                "account": "gh-work",
                "action": "login"
            }),
        );
        assert_eq!(resp.exit_code(), 5);
        assert_eq!(resp.exit_code(), exit_code::CONSENT_REQUIRED);
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["details"]["account"], "gh-work");
        assert_eq!(v["error_code"], "CONSENT_REQUIRED");

        // Plain errors carry no `details` key — the envelope stays minimal.
        let plain = CliResponse::error("boom", "RUNTIME_ERROR");
        assert_eq!(plain.exit_code(), 1);
        let pv = serde_json::to_value(&plain).unwrap();
        assert!(pv.get("details").is_none());
    }

    #[test]
    fn cli_error_from_core_error_falls_back_to_runtime() {
        // CoreError with no known marker maps to Runtime.
        let e = oxibrowser_core::error::CoreError::SessionError("closed".into());
        let cli: CliError = e.into();
        assert_eq!(cli.code_str(), "RUNTIME_ERROR");
        assert_eq!(cli.exit_code(), exit_code::RUNTIME);
    }

    #[test]
    fn cli_error_display_messages_remain_accessible() {
        // Quick sanity that Display round-trips a message and Debug is
        // available for log output without panicking.
        let err = CliError::InvalidSelector("bad selector".into());
        assert_eq!(format!("{err}"), "bad selector");
        let _ = format!("{err:?}");
    }
}
