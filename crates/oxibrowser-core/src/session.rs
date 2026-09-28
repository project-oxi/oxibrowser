//! Session — browsing context group with cookie jar, storage, and history.
//!

//! Session — browsing context group with cookie jar, storage, and history.

use crate::browser::BrowserId;
use crate::config::BrowserConfig;
use crate::context::{BrowserContext, storage_origin_of};
use crate::error::{CoreError, Result};
use crate::frame::Frame;
use crate::frame::FrameId;
use crate::js::JsRuntime;
use crate::js::dom_snapshot::{DomMutation, ExecuteTiming, ScriptKind, ScriptSource};
use crate::js::runtime::JsRuntimeConfig;
use crate::js::runtime::{FetchRequestMsg, FetchResponseMsg, LocalStorageMsg, WsReqMsg};
use crate::network::HttpClient;
use crate::network::cookie::CookieJar;
use crate::network::har;
use crate::network::ws::{WsCmd, WsEvent, run_ws_connection};
use crate::page::Page;
use parking_lot::RwLock;
use percent_encoding::percent_decode_str;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use tracing::info;
use url::Url;

/// Maximum entries retained in the per-session request log (oldest evicted).
const NETWORK_LOG_CAP: usize = 500;
/// POST bodies larger than this are truncated in the request log; the
/// `post_body_truncated` flag marks the cut.
const POST_BODY_CAP: usize = 64 * 1024;

/// Error message used when a capture is refused by the password-focus guard
/// ([`Session::capture_screenshot_png`]). CDP callers match on this to
/// propagate the refusal instead of their blank-PNG fallback.
pub const CAPTURE_BLOCKED_MSG: &str = "screenshot blocked: a password input has focus";

/// One recorded HTTP request/response pair, kept in the per-session rolling
/// request log ([`Session::network_log_snapshot`]) and serializable to HAR
/// ([`crate::network::har::to_har_json`]).
#[derive(Serialize, Clone, Debug)]
pub struct RequestRecord {
    /// Session-namespace request id: `req-{n}` (document fetches),
    /// `res-{n}` (sub-resources), or `oxi-{n}` (JS fetch bridge).
    pub request_id: String,
    /// Request URL.
    pub url: String,
    /// HTTP method.
    pub method: String,
    /// CDP-style resource type (`Document`, `Script`, `Stylesheet`, `Image`,
    /// `Fetch`).
    pub resource_type: String,
    /// Request headers as sent/forwarded (best effort — client defaults are
    /// applied inside the transport and not all are observable here).
    pub request_headers: Vec<(String, String)>,
    /// Request body (POST), capped at [`POST_BODY_CAP`] bytes.
    pub post_body: Option<Vec<u8>>,
    /// Whether [`RequestRecord::post_body`] was cut at the cap.
    pub post_body_truncated: bool,
    /// Response status; `None` while in flight or on transport failure.
    pub status: Option<u16>,
    /// Response headers.
    pub response_headers: Vec<(String, String)>,
    /// Response MIME type with parameters stripped (empty when absent).
    pub mime_type: String,
    /// Request dispatch time (ms since the Unix epoch).
    pub started_at_ms: f64,
    /// Response completion time; `None` while in flight.
    pub finished_at_ms: Option<f64>,
    /// Always `false` — the session performs no response caching, so nothing
    /// can be served from cache.
    pub from_cache: bool,
    /// Response body length in bytes when the body was read; `None` when
    /// unknown (transport failure, body not read).
    pub response_body_length: Option<u64>,
}

/// Wall-clock timestamp in milliseconds since the Unix epoch.
fn unix_ms() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

/// Append a record to the shared log, evicting the oldest entry past the cap.
/// Free-standing so the background fetch bridge thread can share it.
fn record_request_start(log: &parking_lot::Mutex<VecDeque<RequestRecord>>, record: RequestRecord) {
    let mut log = log.lock();
    if log.len() >= NETWORK_LOG_CAP {
        log.pop_front();
    }
    log.push_back(record);
}

/// Complete the newest record with the given id (status/headers/length).
/// Free-standing so the background fetch bridge thread can share it.
#[allow(clippy::too_many_arguments)]
fn record_request_finish(
    log: &parking_lot::Mutex<VecDeque<RequestRecord>>,
    request_id: &str,
    status: Option<u16>,
    response_headers: Vec<(String, String)>,
    mime_type: String,
    response_body_length: Option<u64>,
) {
    let mut log = log.lock();
    if let Some(record) = log.iter_mut().rev().find(|r| r.request_id == request_id) {
        record.status = status;
        record.response_headers = response_headers;
        record.mime_type = mime_type;
        record.response_body_length = response_body_length;
        record.finished_at_ms = Some(unix_ms());
    }
}

/// Cap a POST body for logging, returning the (possibly cut) bytes and the
/// truncation flag.
fn cap_post_body(body: Option<Vec<u8>>) -> (Option<Vec<u8>>, bool) {
    match body {
        Some(bytes) if bytes.len() > POST_BODY_CAP => (Some(bytes[..POST_BODY_CAP].to_vec()), true),
        other => (other, false),
    }
}

/// Unique session ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(u32);

impl SessionId {
    fn next() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(1);
        Self(COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "session-{}", self.0)
    }
}

/// Stored HTTP response body for Network.getResponseBody.
#[derive(Debug, Clone)]
pub struct CapturedResponse {
    pub body: String,
    pub base64: bool,
    pub content_type: String,
}

/// Per-session request overrides applied to every document/sub-resource/POST
/// fetch the Session performs (`RequestOverrides.user_agent` replaces the
/// wire UA; `extra_headers` are appended after the default headers, except
/// transport-managed names — see
/// [`crate::network::client::is_transport_managed_header`]).
///
/// Populated from the CDP layer via `Emulation.setUserAgentOverride` and
/// `Network.setExtraHTTPHeaders`; read back through [`Session::overrides`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RequestOverrides {
    /// Replaces the configured UA on outgoing requests (and, when set, on the
    /// JS surface via the runtime's UA override). `None` = use config UA.
    pub user_agent: Option<String>,
    /// Extra headers appended to every Session-issued request.
    pub extra_headers: Vec<(String, String)>,
}

/// A browsing session with its own history and pages, bound to a
/// [`BrowserContext`] that provides the cookie jar, HTTP client, and
/// origin-keyed localStorage (shared with sibling sessions of the same
/// context — M-A).
pub struct Session {
    /// Unique ID.
    id: SessionId,
    /// Parent browser ID.
    #[allow(dead_code)]
    browser_id: BrowserId,
    /// Configuration.
    config: BrowserConfig,
    /// HTTP client (derived from the session's [`BrowserContext`]).
    http_client: Arc<HttpClient>,
    /// Cookie jar (derived from the session's [`BrowserContext`] —
    /// per-context isolation, M-A).
    #[allow(dead_code)]
    cookie_jar: Arc<RwLock<CookieJar>>,
    /// Active page (current document).
    active_page: Option<Page>,
    /// Navigation history (URLs visited).
    history: Vec<Url>,
    /// Current position in history.
    history_index: usize,
    /// Origin-keyed localStorage, **shared with the parent
    /// [`BrowserContext`]** (and thus with sibling sessions of the same
    /// context): `origin → {key → value}`. Writes from the JS thread land in
    /// the current page's origin bucket; closing this session leaves the
    /// context map untouched.
    local_storage: Arc<parking_lot::RwLock<HashMap<String, HashMap<String, String>>>>,
    /// Origin key of the current page (`"null"` for opaque origins), shared
    /// with the localStorage sync handler thread so JS writes land in the
    /// right bucket. `None` until the first document injection.
    current_origin: Arc<parking_lot::RwLock<Option<String>>>,
    /// Handle to the JS→sync-thread localStorage channel, used to drain
    /// pending writes (barrier) before snapshotting a bucket for navigation.
    ls_tx: std::sync::mpsc::Sender<LocalStorageMsg>,
    /// Origin of the most recently imported storage state; used to route
    /// direct `get_local_storage`/`set_local_storage` calls when no page is
    /// open (single-origin compat, design M6′).
    last_import_origin: Option<String>,
    /// Stored response bodies (requestId -> body) for getResponseBody.
    response_bodies: Arc<parking_lot::RwLock<HashMap<String, CapturedResponse>>>,
    /// JS runtime (per-session).
    js_runtime: JsRuntime,
    /// Fetch handler task handle (for cleanup).
    #[allow(dead_code)]
    fetch_task: Option<std::thread::JoinHandle<()>>,
    /// LocalStorage sync handler task handle (for cleanup).
    #[allow(dead_code)]
    local_storage_task: Option<std::thread::JoinHandle<()>>,
    /// WebSocket bridge task handle (for cleanup).
    #[allow(dead_code)]
    ws_task: Option<std::thread::JoinHandle<()>>,
    /// Whether the session has been closed.
    closed: AtomicBool,
    /// In-flight HTTP request counter shared with the fetch handler thread.
    ///
    /// Incremented when a request is dispatched (navigate / go_back /
    /// go_forward / reload / post / load_sub_resources / JS-issued fetch)
    /// and decremented when its response (or terminal error) is observed.
    /// `wait_for_condition(NetworkIdle)` polls this counter on the Tab side.
    /// Stored as `Arc<AtomicU64>` so the background `handle_fetch_requests`
    /// thread can share the same counter without holding `&Session` — matches
    /// the existing pattern for `local_storage` and `response_bodies`.
    in_flight: Arc<AtomicU64>,
    /// Shared dialog-resolution gate for blocking `alert`/`confirm`/`prompt`.
    /// Written by the CDP layer (`Page.handleJavaScriptDialog`), polled by the
    /// JS thread's dialog closures.
    dialog_gate: crate::js::DialogGate,
    /// Optional CoreEvent sender (clone of the one given to the JS runtime)
    /// so the navigate path + the fetch bridge can emit download / interception
    /// events from their async threads. Shared (Arc) so the background fetch
    /// bridge thread can read it once `set_event_sink` populates it.
    event_tx:
        std::sync::Arc<parking_lot::RwLock<Option<std::sync::mpsc::Sender<crate::js::CoreEvent>>>>,
    /// Frame-id → execution-context-id mapping for per-frame JS evaluation
    /// (Phase 8). The main frame is always context_id=1 and is NOT stored
    /// here; only child iframe contexts (≥ 2) appear.
    frame_contexts: parking_lot::RwLock<HashMap<String /* "frame-N" */, u32 /* context_id */>>,
    /// Next child execution-context id to assign (starts at 2; main=1).
    next_context_id: std::sync::atomic::AtomicU32,
    /// Per-session request overrides (UA + extra headers), threaded into every
    /// Session-issued fetch. Shared as `Arc` so the background fetch bridge
    /// thread observes mutations made via `set_overrides` (CDP
    /// `Emulation.setUserAgentOverride` / `Network.setExtraHTTPHeaders`).
    /// Session code snapshots (clones) the value before each request rather
    /// than holding the guard across `await`.
    overrides: Arc<parking_lot::RwLock<RequestOverrides>>,
    /// Monotonic counter backing document/page request ids (`req-{n}`) —
    /// response-body keys and request-log ids for navigations.
    next_request_id: AtomicU64,
    /// Monotonic counter backing sub-resource request ids (`res-{n}`) used by
    /// the sub-resource request log + `SubresourceFetch*` events.
    next_resource_id: AtomicU64,
    /// Rolling request log (capped at [`NETWORK_LOG_CAP`] entries) covering
    /// document navigations, sub-resource fetches, and JS-issued fetches.
    /// Shared as `Arc` so the background fetch bridge thread records into the
    /// same log; exported via [`Session::network_log_snapshot`] and
    /// [`crate::network::har::to_har_json`].
    network_log: Arc<parking_lot::Mutex<VecDeque<RequestRecord>>>,
    /// Offline emulation flag (CDP `Network.emulateNetworkConditions`). Shared
    /// as `Arc` so the background fetch bridge rejects JS-issued requests
    /// while offline, without holding `&Session`.
    offline: Arc<AtomicBool>,
    /// Registered init scripts `(id, source)`, run before every page's own
    /// scripts on each document injection (Playwright `addInitScript`).
    /// Managed via [`Session::add_init_script`] / [`Session::remove_init_script`].
    init_scripts: Vec<(String, String)>,
    /// Monotonic counter backing init-script ids (`"init-N"`, per session).
    init_script_counter: u64,
}

/// Configurable download directory for `Content-Disposition: attachment`
/// responses. Set via [`set_download_behavior`] (CDP `Page.setDownloadBehavior`).
static DOWNLOAD_DIR: std::sync::LazyLock<parking_lot::RwLock<Option<std::path::PathBuf>>> =
    std::sync::LazyLock::new(|| parking_lot::RwLock::new(None));

/// Set the download directory (`None` = downloads disabled / discarded).
pub fn set_download_behavior(path: Option<std::path::PathBuf>) {
    *DOWNLOAD_DIR.write() = path;
}

/// Emulated viewport override `(width, height)` set via
/// `Emulation.setDeviceMetricsOverride`. When set, navigations lay out at this
/// size instead of `BrowserConfig`'s viewport.
static VIEWPORT_OVERRIDE: std::sync::LazyLock<parking_lot::RwLock<Option<(u32, u32)>>> =
    std::sync::LazyLock::new(|| parking_lot::RwLock::new(None));

/// Install a viewport override consumed by navigation layout.
pub fn set_viewport_override(width: u32, height: u32) {
    *VIEWPORT_OVERRIDE.write() = Some((width.max(1), height.max(1)));
}

/// Clear the viewport override.
pub fn clear_viewport_override() {
    *VIEWPORT_OVERRIDE.write() = None;
}

/// Read the viewport override, if any.
pub(crate) fn current_viewport_override() -> Option<(u32, u32)> {
    *VIEWPORT_OVERRIDE.read()
}

// ---------------------------------------------------------------------------
// Fetch interception (JS fetch/XHR path)
// ---------------------------------------------------------------------------

/// Active Fetch-domain interception patterns (raw `urlPattern` strings), set by
/// CDP `Fetch.enable`. Read by the fetch bridge to decide whether a JS-originated
/// request should be paused.
static FETCH_PATTERNS: std::sync::LazyLock<parking_lot::RwLock<Vec<String>>> =
    std::sync::LazyLock::new(|| parking_lot::RwLock::new(Vec::new()));

/// Set the active interception patterns (CDP `Fetch.enable`).
pub fn set_fetch_patterns(patterns: Vec<String>) {
    *FETCH_PATTERNS.write() = patterns;
}

/// Read the active interception patterns.
pub(crate) fn fetch_patterns() -> Vec<String> {
    FETCH_PATTERNS.read().clone()
}

/// Whether a URL matches any interception pattern. A pattern is a simple glob:
/// `*` matches any substring; otherwise the URL must contain the pattern as a
/// substring (covers `http://example.com/*` and bare domain fragments).
pub(crate) fn url_matches_patterns(url: &str, patterns: &[String]) -> bool {
    if patterns.is_empty() {
        return false;
    }
    patterns.iter().any(|p| {
        if p.is_empty() {
            return false;
        }
        if p == "*" {
            return true;
        }
        // Glob: split on '*'; every non-empty segment must appear in order.
        if p.contains('*') {
            let segments: Vec<&str> = p.split('*').filter(|s| !s.is_empty()).collect();
            if segments.is_empty() {
                return true;
            }
            let mut pos = 0;
            for seg in segments {
                let Some(found) = url[pos..].find(seg) else {
                    return false;
                };
                pos += found + seg.len();
            }
            return true;
        }
        url.contains(p)
    })
}

/// Outcome of the Fetch-domain interception check for a JS-originated request.
#[derive(Debug)]
enum InterceptDecision {
    /// Proceed with the (possibly modified) request.
    Proceed {
        url: Url,
        method: String,
        headers: Vec<(String, String)>,
    },
    /// Respond directly (fail / fulfill) without a network request.
    Respond(FetchResponseMsg),
}

/// If Fetch interception is enabled and `url` matches a pattern, pause the
/// request (insert a `PausedRequest` + emit `CoreEvent::RequestPaused`), await
/// the client's decision, and return it. Otherwise (or on no decision) proceed
/// unchanged. The empty-pattern fast path returns immediately — no behavior
/// change when interception is not enabled.
async fn maybe_intercept(
    event_tx: &std::sync::Arc<
        parking_lot::RwLock<Option<std::sync::mpsc::Sender<crate::js::CoreEvent>>>,
    >,
    request_id: u64,
    url: &str,
    method: &str,
    headers: &[(String, String)],
) -> InterceptDecision {
    use crate::js::CoreEvent;
    use crate::network::intercept::{InterceptAction, PausedRequest, shared_registry};
    use tokio::sync::oneshot;

    let patterns = fetch_patterns();
    if !url_matches_patterns(url, &patterns) {
        return InterceptDecision::Proceed {
            url: Url::parse(url).unwrap_or_else(|_| Url::parse("about:blank").unwrap()),
            method: method.to_string(),
            headers: headers.to_vec(),
        };
    }

    let pause_id = format!("oxi-int-{}", uuid::Uuid::new_v4().as_simple());
    let (tx, rx) = oneshot::channel::<InterceptAction>();
    shared_registry().insert(
        pause_id.clone(),
        PausedRequest {
            url: url.to_string(),
            method: method.to_string(),
            headers: headers.to_vec(),
            resource_type: "XHR".to_string(),
            tx,
        },
    );

    if let Some(sender) = event_tx.read().as_ref() {
        let _ = sender.send(CoreEvent::RequestPaused {
            request_id: pause_id.clone(),
            url: url.to_string(),
            method: method.to_string(),
            headers: headers.to_vec(),
            resource_type: "XHR".to_string(),
            timestamp: current_time_ms(),
        });
    }

    match rx.await {
        Ok(InterceptAction::Continue {
            url: Some(u),
            method: Some(m),
            headers: h,
            ..
        }) => InterceptDecision::Proceed {
            url: Url::parse(&u).unwrap_or_else(|_| Url::parse(url).unwrap()),
            method: m,
            headers: h,
        },
        Ok(InterceptAction::Continue { .. }) => InterceptDecision::Proceed {
            url: Url::parse(url).unwrap(),
            method: method.to_string(),
            headers: headers.to_vec(),
        },
        Ok(InterceptAction::Fail { error_reason }) => {
            InterceptDecision::Respond(FetchResponseMsg {
                id: request_id,
                status: 0,
                status_text: "Network Error".to_string(),
                url: url.to_string(),
                headers: vec![],
                body: String::new(),
                error: Some(error_reason),
            })
        }
        Ok(InterceptAction::Fulfill {
            status_code,
            status_text,
            headers,
            body,
        }) => InterceptDecision::Respond(FetchResponseMsg {
            id: request_id,
            status: status_code,
            status_text,
            url: url.to_string(),
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
            error: None,
        }),
        Err(_) => {
            // No decision (client never responded) — proceed with the request.
            InterceptDecision::Proceed {
                url: Url::parse(url).unwrap(),
                method: method.to_string(),
                headers: headers.to_vec(),
            }
        }
    }
}

fn current_time_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// Fetch handler
// ---------------------------------------------------------------------------

/// Dispatch fetch requests from the JS thread to real HTTP I/O.
///
/// Runs a minimal tokio runtime and **spawns an independent task per request**
/// (Phase 3), so concurrent in-flight fetches run in parallel rather than
/// serially. Each task awaits its own `http_client.fetch`, then pushes a single
/// `FetchResponseMsg { id, .. }` onto the shared `response_tx` (routed back to
/// the JS thread's `PENDING_FETCH` registry by `id`) and decrements `in_flight`.
///
/// `in_flight` is incremented before spawn and decremented exactly once per
/// task on every terminal branch — including the error/early-return paths — so
/// the counter never leaks and `wait_for_condition(NetworkIdle)` observes real
/// parallelism.
#[allow(clippy::too_many_arguments)] // channel/protocol boundary: one arg per field
fn handle_fetch_requests(
    fetch_rx: std::sync::mpsc::Receiver<FetchRequestMsg>,
    response_tx: std::sync::mpsc::Sender<FetchResponseMsg>,
    http_client: Arc<HttpClient>,
    _cookie_jar: Arc<RwLock<CookieJar>>,
    max_body_bytes: usize,
    in_flight: Arc<AtomicU64>,
    event_tx: std::sync::Arc<
        parking_lot::RwLock<Option<std::sync::mpsc::Sender<crate::js::CoreEvent>>>,
    >,
    offline: Arc<AtomicBool>,
    network_log: Arc<parking_lot::Mutex<VecDeque<RequestRecord>>>,
    overrides: Arc<parking_lot::RwLock<RequestOverrides>>,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("failed to create tokio runtime for fetch: {}", e);
            return;
        }
    };

    rt.block_on(async {
        loop {
            // try_recv + sleep().await: must yield to the current-thread
            // runtime so spawned tasks get polled. A blocking recv() would
            // park the OS thread and starve the runtime (deadlock).
            match fetch_rx.try_recv() {
                Ok(request) => {
                    // Mark in-flight before spawning — a NetworkIdle observer
                    // may read the counter at any moment.
                    in_flight.fetch_add(1, Ordering::Relaxed);

                    let http_client = http_client.clone();
                    let response_tx = response_tx.clone();
                    let in_flight = in_flight.clone();
                    let event_tx = event_tx.clone();
                    let offline = offline.clone();
                    let network_log = network_log.clone();
                    let overrides = overrides.clone();

                    // Spawn an independent task per request so concurrent
                    // fetches run in parallel (Phase 3), not one-at-a-time.
                    tokio::spawn(async move {
                        let id = request.id;
                        let method = request.method;
                        let headers = request.headers;
                        let body = request.body;
                        let origin = request.origin;
                        let url_str = request.url;

                        // Request-log entry for this JS-issued fetch. The id
                        // namespace matches the CDP `Network.requestWillBeSent`
                        // events minted on the JS thread (`oxi-{n}`).
                        let record_id = crate::js::runtime::cdp_request_id(id);
                        let (post_body, post_body_truncated) = cap_post_body(body.clone());
                        record_request_start(
                            &network_log,
                            RequestRecord {
                                request_id: record_id.clone(),
                                url: url_str.clone(),
                                method: method.clone(),
                                resource_type: "Fetch".to_string(),
                                request_headers: headers.clone(),
                                post_body,
                                post_body_truncated,
                                status: None,
                                response_headers: Vec::new(),
                                mime_type: String::new(),
                                started_at_ms: unix_ms(),
                                finished_at_ms: None,
                                // No response cache exists — nothing can come
                                // from cache.
                                from_cache: false,
                                response_body_length: None,
                            },
                        );
                        // Snapshot the overrides (UA + extra headers) so this
                        // request carries exactly what was set at dispatch
                        // time, without holding the lock across awaits.
                        let ov = overrides.read().clone();

                        let fail_record =
                            |network_log: &Arc<parking_lot::Mutex<VecDeque<RequestRecord>>>,
                             status: Option<u16>| {
                                record_request_finish(
                                    network_log,
                                    &record_id,
                                    status,
                                    Vec::new(),
                                    String::new(),
                                    None,
                                );
                            };

                        // Offline emulation: reject JS-issued requests before
                        // any network I/O (mirrors the document path check).
                        if offline.load(Ordering::Relaxed) {
                            fail_record(&network_log, None);
                            let _ = response_tx.send(FetchResponseMsg {
                                id,
                                status: 0,
                                status_text: "Network Error".to_string(),
                                url: url_str,
                                headers: vec![],
                                body: String::new(),
                                error: Some("offline".to_string()),
                            });
                            in_flight.fetch_sub(1, Ordering::Relaxed);
                            return;
                        }

                        if Url::parse(&url_str).is_err() {
                            fail_record(&network_log, Some(400));
                            let _ = response_tx.send(FetchResponseMsg {
                                id,
                                status: 400,
                                status_text: "Invalid URL".to_string(),
                                url: url_str,
                                headers: vec![],
                                body: String::new(),
                                error: Some("invalid URL".to_string()),
                            });
                            in_flight.fetch_sub(1, Ordering::Relaxed);
                            return;
                        }

                        // Fetch-domain interception (JS fetch/XHR path).
                        let decision =
                            maybe_intercept(&event_tx, id, &url_str, &method, &headers).await;
                        let resp = match decision {
                            InterceptDecision::Respond(msg) => {
                                fail_record(&network_log, Some(msg.status));
                                let _ = response_tx.send(msg);
                                in_flight.fetch_sub(1, Ordering::Relaxed);
                                return;
                            }
                            InterceptDecision::Proceed {
                                url,
                                method,
                                headers,
                            } => {
                                http_client
                                    .request_with_context_with_overrides(
                                        &url,
                                        &method,
                                        &headers,
                                        body,
                                        origin.as_deref(),
                                        Some(&ov),
                                    )
                                    .await
                            }
                        };
                        match resp {
                            Ok(response) => {
                                let status = response.status().as_u16();
                                let status_text = response
                                    .status()
                                    .canonical_reason()
                                    .unwrap_or("")
                                    .to_string();
                                let resp_url = response.uri().to_string();
                                let headers: Vec<(String, String)> = response
                                    .headers()
                                    .iter()
                                    .map(|(k, v)| {
                                        (k.to_string(), v.to_str().unwrap_or("").to_string())
                                    })
                                    .collect();
                                let mime_type = headers
                                    .iter()
                                    .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                                    .map(|(_, v)| HttpClient::mime_without_params(v))
                                    .unwrap_or_default();
                                let (resp_body, truncated) =
                                    match HttpClient::read_body_limited(response, max_body_bytes)
                                        .await
                                    {
                                        Ok((buf, truncated)) => (buf, truncated),
                                        Err(e) => {
                                            fail_record(&network_log, Some(status));
                                            let _ = response_tx.send(FetchResponseMsg {
                                                id,
                                                status,
                                                status_text,
                                                url: resp_url,
                                                headers,
                                                body: String::new(),
                                                error: Some(format!("failed to read body: {}", e)),
                                            });
                                            in_flight.fetch_sub(1, Ordering::Relaxed);
                                            return;
                                        }
                                    };
                                if truncated {
                                    tracing::warn!(
                                        url = %resp_url,
                                        max_bytes = max_body_bytes,
                                        "fetch body truncated"
                                    );
                                }
                                let body_len = resp_body.len() as u64;
                                let body = String::from_utf8_lossy(&resp_body).into_owned();

                                record_request_finish(
                                    &network_log,
                                    &record_id,
                                    Some(status),
                                    headers.clone(),
                                    mime_type,
                                    Some(body_len),
                                );

                                let _ = response_tx.send(FetchResponseMsg {
                                    id,
                                    status,
                                    status_text,
                                    url: resp_url,
                                    headers,
                                    body,
                                    error: None,
                                });
                                in_flight.fetch_sub(1, Ordering::Relaxed);
                            }
                            Err(e) => {
                                fail_record(&network_log, None);
                                let _ = response_tx.send(FetchResponseMsg {
                                    id,
                                    status: 0,
                                    status_text: "Network Error".to_string(),
                                    url: url_str,
                                    headers: vec![],
                                    body: String::new(),
                                    error: Some(e.to_string()),
                                });
                                in_flight.fetch_sub(1, Ordering::Relaxed);
                            }
                        }
                    });
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            }
        }
    });
}
/// Background WebSocket bridge: routes Connect/Send/Close from the JS thread
/// to per-socket tokio tasks. Events flow straight to the JS-thread
/// `WS_EVENT_RX` via the shared event channel (id-routed). Mirrors
/// `handle_fetch_requests` (try_recv + sleep polling — never a blocking recv
/// inside the current-thread runtime, or spawned socket tasks stall).
pub(crate) fn handle_ws_requests(
    ws_req_rx: std::sync::mpsc::Receiver<WsReqMsg>,
    ws_event_tx: std::sync::mpsc::Sender<WsEvent>,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("failed to create tokio runtime for ws: {}", e);
            return;
        }
    };
    let mut sockets: std::collections::HashMap<u64, tokio::sync::mpsc::Sender<WsCmd>> =
        std::collections::HashMap::new();
    rt.block_on(async move {
        loop {
            while let Ok(req) = ws_req_rx.try_recv() {
                match req {
                    WsReqMsg::Connect { id, url, protocols } => {
                        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<WsCmd>(16);
                        let event_tx = ws_event_tx.clone();
                        sockets.insert(id, cmd_tx);
                        tokio::spawn(async move {
                            run_ws_connection(id, url, protocols, cmd_rx, event_tx).await;
                        });
                    }
                    WsReqMsg::Send { id, data } => {
                        if let Some(tx) = sockets.get(&id) {
                            let _ = tx.try_send(WsCmd::Send(data));
                        }
                    }
                    WsReqMsg::Close { id, code, reason } => {
                        if let Some(tx) = sockets.get(&id) {
                            let _ = tx.try_send(WsCmd::Close { code, reason });
                        }
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    });
}

// ---------------------------------------------------------------------------
// LocalStorage sync handler
// ---------------------------------------------------------------------------
/// Applies JS `localStorage.setItem/removeItem/clear` calls to the context's
/// **origin-keyed** storage map: every message carries the origin captured at
/// `register_local_storage` time, so a navigation that swaps the page before
/// the message is processed cannot misroute (cross-origin bleed) or drop the
/// write. A `Drain` barrier acknowledges once all previously queued messages
/// have been applied.
fn handle_local_storage_sync(
    ls_rx: std::sync::mpsc::Receiver<LocalStorageMsg>,
    local_storage: Arc<parking_lot::RwLock<HashMap<String, HashMap<String, String>>>>,
) {
    while let Ok(msg) = ls_rx.recv() {
        match msg {
            LocalStorageMsg::Drain(ack) => {
                let _ = ack.send(());
            }
            LocalStorageMsg::SetItem { origin, key, value } => {
                local_storage
                    .write()
                    .entry(origin)
                    .or_default()
                    .insert(key, value);
            }
            LocalStorageMsg::RemoveItem { origin, key } => {
                local_storage.write().entry(origin).or_default().remove(&key);
            }
            LocalStorageMsg::Clear { origin } => {
                local_storage.write().entry(origin).or_default().clear();
            }
        }
    }
}

/// RAII guard for the Session in-flight request counter.
///
/// Increments on construction; decrements on drop. Using a guard instead of
/// manual `fetch_add` / `fetch_sub` pairs ensures the counter always returns
/// to its correct value even when an awaited HTTP call returns `Err` and
/// the caller early-returns via `?` — the guard's `Drop` runs regardless.
struct InFlightGuard {
    counter: Arc<AtomicU64>,
}

impl InFlightGuard {
    fn new(counter: Arc<AtomicU64>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self { counter }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Session {
    /// Create a new session bound to `context`.
    ///
    /// The session derives its HTTP client, cookie jar, and origin-keyed
    /// localStorage map from the context — sessions sharing a context share
    /// cookies and web storage; sessions in different contexts are fully
    /// isolated (M-A).
    #[tracing::instrument(skip(config, context), err)]
    pub async fn new(
        browser_id: BrowserId,
        config: BrowserConfig,
        context: Arc<BrowserContext>,
    ) -> Result<Self> {
        let http_client = context.http_client();
        let cookie_jar = context.cookie_jar().clone();
        let js_config = JsRuntimeConfig::from(&config);

        // Fetch channels: request sender (JS→background) + shared response
        // receiver (background→JS, id-routed). Phase 3 async fetch.
        let (fetch_tx, fetch_rx) = std::sync::mpsc::channel();
        let (fetch_resp_tx, fetch_resp_rx) = std::sync::mpsc::channel::<FetchResponseMsg>();

        // Create localStorage sync channel
        let (ls_tx, ls_rx) = std::sync::mpsc::channel::<LocalStorageMsg>();

        // Create JS runtime and wire up fetch channels
        let mut js_runtime = JsRuntime::with_config(js_config);
        js_runtime.set_fetch_channel(fetch_tx, fetch_resp_rx);
        js_runtime.set_local_storage_channel(ls_tx.clone());
        // Dialog gate: shared cell for blocking alert/confirm/prompt, resolved
        // by the CDP layer via Page.handleJavaScriptDialog.
        let dialog_gate: crate::js::DialogGate = Arc::new(parking_lot::Mutex::new(None));
        js_runtime.set_dialog_gate(dialog_gate.clone());
        // WebSocket channels: request sender (JS→bridge) + shared event
        // receiver (bridge→JS, id-routed). Phase 4 WebSocket.
        let (ws_req_tx, ws_req_rx) = std::sync::mpsc::channel::<WsReqMsg>();
        let (ws_event_tx, ws_event_rx) = std::sync::mpsc::channel::<WsEvent>();
        js_runtime.set_ws_channel(ws_req_tx, ws_event_rx);

        // Spawn fetch handler on a blocking thread
        let http_client_clone = http_client.clone();
        let cookie_jar_clone = cookie_jar.clone();
        let in_flight = Arc::new(AtomicU64::new(0));
        let in_flight_clone = in_flight.clone();
        let offline = Arc::new(AtomicBool::new(false));
        let offline_clone = offline.clone();
        let max_body_bytes = config.max_response_body_bytes;
        let event_tx = std::sync::Arc::new(parking_lot::RwLock::new(None));
        let event_tx_clone = event_tx.clone();
        let network_log = Arc::new(parking_lot::Mutex::new(VecDeque::new()));
        let network_log_clone = network_log.clone();
        let overrides = Arc::new(parking_lot::RwLock::new(RequestOverrides::default()));
        let overrides_clone = overrides.clone();
        let fetch_task = Some(std::thread::spawn(move || {
            handle_fetch_requests(
                fetch_rx,
                fetch_resp_tx,
                http_client_clone,
                cookie_jar_clone,
                max_body_bytes,
                in_flight_clone,
                event_tx_clone,
                offline_clone,
                network_log_clone,
                overrides_clone,
            );
        }));
        // Spawn WebSocket bridge handler thread (Phase 4)
        let ws_task = Some(std::thread::spawn(move || {
            handle_ws_requests(ws_req_rx, ws_event_tx);
        }));

        // Session-side origin tracking: used to route direct storage access
        // (storage_origin) and to stamp each navigation's bucket seed. The
        // localStorage sync thread no longer reads it — messages carry their
        // own origin.
        let current_origin: Arc<parking_lot::RwLock<Option<String>>> =
            Arc::new(parking_lot::RwLock::new(None));

        // Spawn localStorage sync handler thread. The map is the context's
        // origin-keyed storage map (shared with sibling sessions); each
        // message carries its own origin so no shared origin cell is needed
        // for routing.
        let local_storage_arc = context.storage_map();
        let ls_arc_clone = local_storage_arc.clone();
        let local_storage_task = Some(std::thread::spawn(move || {
            handle_local_storage_sync(ls_rx, ls_arc_clone);
        }));

        if let Err(e) = js_runtime.set_cookie_jar(cookie_jar.clone()) {
            tracing::warn!("failed to set cookie jar: {}", e);
        }

        Ok(Self {
            id: SessionId::next(),
            browser_id,
            config,
            http_client,
            cookie_jar,
            active_page: None,
            history: Vec::new(),
            history_index: 0,
            local_storage: local_storage_arc,
            current_origin,
            ls_tx,
            last_import_origin: None,
            response_bodies: Arc::new(parking_lot::RwLock::new(HashMap::new())),
            js_runtime,
            fetch_task,
            local_storage_task,
            ws_task,
            closed: AtomicBool::new(false),
            dialog_gate,
            event_tx,
            frame_contexts: parking_lot::RwLock::new(HashMap::new()),
            next_context_id: std::sync::atomic::AtomicU32::new(2),
            in_flight,
            overrides,
            next_request_id: AtomicU64::new(0),
            next_resource_id: AtomicU64::new(0),
            network_log,
            offline,
            init_scripts: Vec::new(),
            init_script_counter: 0,
        })
    }

    /// Navigate to a URL.
    #[tracing::instrument(skip(self), fields(session = %self.id), err)]
    pub async fn navigate(&mut self, url: &str) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(CoreError::SessionClosed);
        }

        let parsed = Url::parse(url)?;

        // `data:` URLs are resolved locally (no HTTP fetch) so the stealth
        // surface can be exercised fully offline.
        // `about:` URLs create an empty local page (no HTTP fetch).
        // `about:blank` is the canonical case, but we accept any about:<path>
        // and render it identically to about:blank for now.
        if parsed.scheme() == "about" {
            return self.navigate_about().await;
        }

        if parsed.scheme() == "data" {
            return self.navigate_data_url(&parsed).await;
        }

        info!(url = %parsed, "navigating");

        // Offline emulation blocks only real network fetches — `about:` and
        // `data:` pages are local and stay reachable (matches Chrome DevTools
        // offline emulation).
        self.ensure_online()?;

        // Snapshot overrides for this navigation (headers + request log).
        let ov = self.overrides.read().clone();

        // Fetch the document
        let start = std::time::Instant::now();
        let started_at_ms = unix_ms();
        let _in_flight = InFlightGuard::new(self.in_flight.clone());
        let response = self
            .http_client
            .fetch_with_overrides(&parsed, Some(&ov))
            .await?;
        let status = response.status().as_u16();
        let final_url = Url::parse(&response.uri().to_string()).unwrap_or_else(|_| parsed.clone());

        // Check for HTTP errors
        if status >= 400 {
            return Err(CoreError::HttpError {
                status,
                message: format!("HTTP {} for {}", status, parsed),
            });
        }
        let ct_header = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("text/html")
            .to_string();
        let content_disposition = response
            .headers()
            .get("content-disposition")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let response_headers = HttpClient::response_headers(&response);
        let max = self.config.max_response_body_bytes;
        let (bytes, truncated) = HttpClient::read_body_limited(response, max).await?;
        if truncated {
            tracing::warn!(final_url = %final_url, max_bytes = max, "navigate body truncated");
        }

        // Download handling: a `Content-Disposition: attachment` response (or
        // a non-HTML content type with a download dir configured) is saved to
        // the download directory instead of being rendered.
        if content_disposition
            .to_ascii_lowercase()
            .contains("attachment")
        {
            if let Err(e) = self.handle_download(&final_url, &content_disposition, &bytes) {
                tracing::warn!(error = %e, "download handling failed; falling back to render");
            } else {
                return Ok(());
            }
        }

        let html = crate::encoding::decode_html(&bytes, Some(&ct_header));

        tracing::debug!(status, final_url = %final_url, elapsed_ms = start.elapsed().as_millis() as u64, "page fetched");

        // Record the document fetch + store the body for
        // Network.getResponseBody under the same session-monotonic
        // `req-{n}` id (single requestId namespace for document fetches).
        let request_id = self.next_doc_request_id();
        {
            let mut record = self.begin_record(
                &request_id,
                final_url.as_str(),
                "GET",
                "Document",
                &ov,
                None,
            );
            record.started_at_ms = started_at_ms;
            record.status = Some(status);
            record.response_headers = response_headers;
            record.mime_type = HttpClient::mime_without_params(&ct_header);
            record.response_body_length = Some(bytes.len() as u64);
            record.finished_at_ms = Some(unix_ms());
            self.push_record(record);
        }
        if !html.is_empty() {
            self.store_response_body(&request_id, html.clone(), &ct_header);
            tracing::trace!(request_id, body_len = html.len(), "response body stored");
        }

        tracing::debug!(html_bytes = html.len(), "response decoded");

        // Resolve external <link rel=stylesheet> into a single inline <style>
        // block so Blitz parses the document once with the rules in place
        // (and never tries to join hrefs against a `data:`-scheme base URL,
        // which would panic). The fetch step happens here, before
        // `Page::from_html` is called, so the post-injection html is what
        // `page.content()` returns — important because `inject_dom_snapshot`
        // re-pushes that same html into the JS thread's RenderDocument.
        let html = self
            .inline_external_stylesheets(&html, final_url.as_str())
            .await;

        // Create a new page for this navigation (use final URL after redirects)
        let mut page = Page::from_html(final_url.clone(), &html, status, ct_header).await?;

        // Phase 8: populate child <iframe> frames by fetching each
        // src (resolved against the page URL) and parsing it into a child Frame.
        self.populate_iframes(&mut page, &final_url).await;

        // Update history
        if self.history.is_empty() {
            // First navigation — just push
        } else if self.history_index < self.history.len() - 1 {
            self.history.truncate(self.history_index + 1);
        }
        self.history.push(final_url);
        self.history_index = self.history.len() - 1;

        self.active_page = Some(page);
        self.js_runtime.clear_child_contexts();
        self.frame_contexts.write().clear();
        self.next_context_id
            .store(2, std::sync::atomic::Ordering::Relaxed);

        // Inject DOM snapshot into JS runtime
        self.inject_dom_snapshot().await;

        // Phase 8: build per-frame execution contexts for child iframes.
        self.inject_child_frames().await;

        Ok(())
    }

    /// Save a downloaded attachment to the configured download directory and
    /// emit a [`CoreEvent::Download`].
    ///
    /// Directory resolution: the CDP `Page.setDownloadBehavior` override
    /// ([`set_download_behavior`]) wins; otherwise the session config's
    /// [`BrowserConfig::download_dir`] (or its platform-temp default via
    /// [`BrowserConfig::download_dir_or_default`]). The directory is created
    /// at save time. On save failure a [`CoreEvent::DownloadFailed`] is
    /// emitted and `Err` is returned so the caller falls back to rendering
    /// the body.
    fn handle_download(&self, url: &Url, disposition: &str, bytes: &[u8]) -> Result<()> {
        let dir = DOWNLOAD_DIR
            .read()
            .clone()
            .unwrap_or_else(|| self.config.download_dir_or_default());
        // The GUID is allocated up front so the failure event can reference
        // the same download id the CDP layer would have seen on success.
        let guid = format!("dl-{}", uuid::Uuid::new_v4().as_simple());
        if let Err(e) = std::fs::create_dir_all(&dir) {
            let error = e.to_string();
            tracing::warn!(dir = %dir.display(), error = %error, "download directory creation failed");
            if let Some(tx) = self.event_tx.read().as_ref() {
                let _ = tx.send(crate::js::CoreEvent::DownloadFailed {
                    guid,
                    url: url.to_string(),
                    error,
                });
            }
            return Err(CoreError::NetworkError(format!(
                "failed to create download directory: {e}"
            )));
        }

        let filename = filename_from_disposition(disposition)
            .or_else(|| {
                url.path_segments()
                    .and_then(|mut s| s.next_back())
                    .filter(|n| !n.is_empty())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "download.bin".to_string());
        // Sanitize: keep only the basename (no path traversal).
        let safe = std::path::Path::new(&filename)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| "download.bin".to_string());
        let save_path = dir.join(&safe);

        if let Err(e) = std::fs::write(&save_path, bytes) {
            let error = e.to_string();
            tracing::warn!(url = %url, save_path = %save_path.display(), error = %error, "download save failed");
            if let Some(tx) = self.event_tx.read().as_ref() {
                let _ = tx.send(crate::js::CoreEvent::DownloadFailed {
                    guid,
                    url: url.to_string(),
                    error,
                });
            }
            return Err(CoreError::NetworkError(format!(
                "failed to write download: {e}"
            )));
        }

        if let Some(tx) = self.event_tx.read().as_ref() {
            let _ = tx.send(crate::js::CoreEvent::Download {
                guid: guid.clone(),
                url: url.to_string(),
                filename: safe.clone(),
                save_path: save_path.to_string_lossy().into_owned(),
                total_bytes: bytes.len(),
            });
        }
        tracing::info!(url = %url, filename = %safe, bytes = bytes.len(), "download saved");
        Ok(())
    }

    /// Fetch each `<iframe>` in **every** frame (root and descendants) and
    /// attach the fetched document as a child [`Frame`] (Phase 8 population
    /// step, extended in W3b for nested iframes).
    ///
    /// Operates level-by-level: at each level every frame's `<iframe>` elements
    /// are collected, fetched asynchronously, and the produced [`Frame`]s are
    /// then attached to the correct parent via `Frame::find_mut_by_id`. Mixing
    /// the fetch await with a `&mut Frame` borrow would collide, so the
    /// framework here is
    ///   1. collect (immutable references),
    ///   2. fetch + parse (produces new `Frame` values, no page borrows),
    ///   3. attach (mutates `page`'s root frame tree).
    ///
    /// Failures (bad URL, network error) are logged and skipped so a single
    /// broken iframe can't abort navigation.
    async fn populate_iframes(&self, page: &mut Page, base_url: &Url) {
        let mut worklist: Vec<FrameId> = vec![page.root_frame().id()];
        // `pages` mutated while we hold root_frame_mut; we never hold it across
        // an await — the fetch phase runs without a page borrow.
        while let Some(parent_id) = worklist.pop() {
            // Phase 1: collect this parent's iframe list without holding the
            // mutable borrow past a single synchronous walk.
            let iframes: Vec<crate::js::dom_snapshot::IframeElement> = {
                let Some(parent) = page.root_frame_mut().find_mut_by_id(parent_id) else {
                    continue;
                };
                parent.document().extract_iframes()
                // `parent` borrow ends here.
            };
            if iframes.is_empty() {
                continue;
            }
            // Phase 2: fetch + parse (no page borrow).
            let parent_url_for_join: Option<Url> = {
                let Some(parent_ref) = page.root_frame().find_by_id(parent_id) else {
                    continue;
                };
                Some(parent_ref.url().clone())
            };
            let mut new_frames: Vec<Frame> = Vec::with_capacity(iframes.len());
            for iframe in iframes {
                // 1. srcdoc → inline content, no fetch (W3a).
                if let Some(srcdoc) = iframe.srcdoc {
                    let child_url = Url::parse("about:srcdoc").unwrap_or_else(|_| base_url.clone());
                    match Frame::from_html(child_url, &srcdoc).await {
                        Ok(child) => new_frames.push(child),
                        Err(e) => tracing::warn!(error = %e, "failed to parse srcdoc iframe"),
                    }
                    continue;
                }
                let Some(src) = iframe.src else { continue };
                let join_base = parent_url_for_join.as_ref().unwrap_or(base_url);
                let Ok(full) = join_base.join(&src) else {
                    continue;
                };
                // 2a. non-http(s) (about:blank, javascript:, etc.) → empty
                // child (W3a).
                if full.scheme() != "http" && full.scheme() != "https" {
                    let child_url = Url::parse("about:blank").unwrap_or_else(|_| base_url.clone());
                    let empty = "<!DOCTYPE html><html><head></head><body></body></html>";
                    match Frame::from_html(child_url, empty).await {
                        Ok(child) => new_frames.push(child),
                        Err(e) => tracing::warn!(
                            src = %src,
                            error = %e,
                            "failed to parse about:blank iframe"
                        ),
                    }
                    continue;
                }
                // 2b. http(s) → fetch + parse (original behavior).
                match self
                    .http_client
                    .fetch_text_with_overrides(&full, Some(&self.snapshot_overrides()))
                    .await
                {
                    Ok(child_html) => match Frame::from_html(full.clone(), &child_html).await {
                        Ok(child) => new_frames.push(child),
                        Err(e) => tracing::warn!(
                            src = %src,
                            error = %e,
                            "failed to parse iframe document"
                        ),
                    },
                    Err(e) => tracing::warn!(src = %src, error = %e, "failed to fetch iframe"),
                }
            }
            // Phase 3: attach each freshly-built frame to its parent, then
            // schedule the new frame for population on the next iteration.
            for frame in new_frames {
                let new_id = frame.id();
                let parent = page.root_frame_mut().find_mut_by_id(parent_id);
                if let Some(parent) = parent {
                    parent.add_child(frame);
                    worklist.push(new_id);
                }
            }
        }
    }

    /// Navigate to a URL with automatic retries on transient failures.
    ///
    /// Retries DNS errors, connection timeouts, and 5xx errors with
    /// exponential backoff (500ms, 1000ms, 1500ms, ...).
    async fn navigate_data_url(&mut self, url: &Url) -> Result<()> {
        let data_str = url.as_str();
        let data_part = data_str.strip_prefix("data:").unwrap_or("");
        let (mime, encoded_body) = if let Some(comma_idx) = data_part.find(',') {
            let mime = data_part[..comma_idx].trim().to_string();
            let body = &data_part[comma_idx + 1..];
            (mime, body)
        } else {
            ("text/plain".to_string(), data_part)
        };

        // Percent-decode the body (the url crate encodes special chars)
        let body = percent_decode_str(encoded_body)
            .decode_utf8()
            .unwrap_or_else(|_| encoded_body.into());

        let page = Page::from_html(url.clone(), &body, 200, mime.clone()).await?;
        if self.history.is_empty() {
        } else if self.history_index < self.history.len() - 1 {
            self.history.truncate(self.history_index + 1);
        }
        self.history.push(url.clone());
        self.history_index = self.history.len() - 1;
        self.active_page = Some(page);
        self.inject_dom_snapshot().await;
        Ok(())
    }

    /// Navigate to an `about:` URL — creates an empty page without network fetch.
    /// `about:blank` is the canonical case; `about:srcdoc`, `about:config`, etc.
    /// all render as a blank HTML5 document for simplicity.
    async fn navigate_about(&mut self) -> Result<()> {
        const ABOUT_HTML: &str = r#"<!DOCTYPE html><html><head><meta charset="utf-8"><title>about:blank</title></head><body></body></html>"#;
        let about_url = Url::parse("about:blank").unwrap();
        let page = Page::from_html(about_url.clone(), ABOUT_HTML, 200, "text/html".into()).await?;
        if self.history.is_empty() {
        } else if self.history_index < self.history.len() - 1 {
            self.history.truncate(self.history_index + 1);
        }
        self.history.push(about_url.clone());
        self.history_index = self.history.len() - 1;
        self.active_page = Some(page);
        self.inject_dom_snapshot().await;
        Ok(())
    }
    #[tracing::instrument(skip(self), fields(session = %self.id), err)]
    pub async fn navigate_with_retry(&mut self, url: &str, max_retries: u32) -> Result<()> {
        let mut last_error: Option<CoreError> = None;

        for attempt in 0..=max_retries {
            match self.navigate(url).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    let is_retryable = match &e {
                        CoreError::DnsError(_)
                        | CoreError::ConnectionTimeout(_)
                        | CoreError::NetworkError(_) => true,
                        CoreError::HttpError { status, .. } => *status >= 500,
                        _ => false,
                    };

                    if !is_retryable || attempt >= max_retries {
                        return Err(e);
                    }

                    last_error = Some(e);
                    let delay = std::time::Duration::from_millis(500 * (attempt + 1) as u64);
                    info!(
                        attempt = attempt + 1,
                        max_retries,
                        delay_ms = delay.as_millis(),
                        "retrying navigation"
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }

        Err(last_error
            .unwrap_or_else(|| CoreError::NavigationFailed("no retry attempts were made".into())))
    }
    pub async fn go_back(&mut self) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(CoreError::SessionClosed);
        }
        if self.history_index > 0 {
            self.ensure_online()?;
            self.history_index -= 1;
            let url = self.history[self.history_index].clone();

            let ov = self.overrides.read().clone();
            let started_at_ms = unix_ms();

            // Re-fetch without adding to history
            let _in_flight = InFlightGuard::new(self.in_flight.clone());
            let response = self
                .http_client
                .fetch_with_overrides(&url, Some(&ov))
                .await?;
            let status = response.status().as_u16();
            let response_headers = HttpClient::response_headers(&response);
            let ct_header = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("text/html")
                .to_string();
            let max = self.config.max_response_body_bytes;
            let (bytes, truncated) = HttpClient::read_body_limited(response, max).await?;
            if truncated {
                tracing::warn!(url = %url, max_bytes = max, "history body truncated");
            }
            let html = crate::encoding::decode_html(&bytes, Some(&ct_header));
            self.record_document_fetch(
                &url,
                status,
                &ct_header,
                response_headers,
                bytes.len(),
                &ov,
                started_at_ms,
            );
            self.active_page = Some(Page::from_html(url, &html, 200, ct_header).await?);
            self.inject_dom_snapshot().await;
            Ok(())
        } else {
            Err(CoreError::NavigationFailed("no previous page".into()))
        }
    }

    /// Navigate forward in history.
    pub async fn go_forward(&mut self) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(CoreError::SessionClosed);
        }
        if self.history_index < self.history.len() - 1 {
            self.ensure_online()?;
            self.history_index += 1;
            let url = self.history[self.history_index].clone();

            let ov = self.overrides.read().clone();
            let started_at_ms = unix_ms();

            let _in_flight = InFlightGuard::new(self.in_flight.clone());
            let response = self
                .http_client
                .fetch_with_overrides(&url, Some(&ov))
                .await?;
            let status = response.status().as_u16();
            let response_headers = HttpClient::response_headers(&response);
            let ct_header = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("text/html")
                .to_string();
            let max = self.config.max_response_body_bytes;
            let (bytes, truncated) = HttpClient::read_body_limited(response, max).await?;
            if truncated {
                tracing::warn!(url = %url, max_bytes = max, "history body truncated");
            }
            let html = crate::encoding::decode_html(&bytes, Some(&ct_header));
            self.record_document_fetch(
                &url,
                status,
                &ct_header,
                response_headers,
                bytes.len(),
                &ov,
                started_at_ms,
            );
            self.active_page = Some(Page::from_html(url, &html, 200, ct_header).await?);
            self.inject_dom_snapshot().await;
            Ok(())
        } else {
            Err(CoreError::NavigationFailed("no next page".into()))
        }
    }

    /// Reload the current page.
    pub async fn reload(&mut self) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(CoreError::SessionClosed);
        }
        if let Some(url) = self.current_url() {
            self.ensure_online()?;

            let ov = self.overrides.read().clone();
            let started_at_ms = unix_ms();

            let _in_flight = InFlightGuard::new(self.in_flight.clone());
            let response = self
                .http_client
                .fetch_with_overrides(url, Some(&ov))
                .await?;
            let status = response.status().as_u16();
            let response_headers = HttpClient::response_headers(&response);
            let ct_header = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("text/html")
                .to_string();
            let max = self.config.max_response_body_bytes;
            let (bytes, truncated) = HttpClient::read_body_limited(response, max).await?;
            if truncated {
                tracing::warn!(url = %url, max_bytes = max, "reload body truncated");
            }
            let html = crate::encoding::decode_html(&bytes, Some(&ct_header));
            self.record_document_fetch(
                url,
                status,
                &ct_header,
                response_headers,
                bytes.len(),
                &ov,
                started_at_ms,
            );
            self.active_page = Some(Page::from_html(url.clone(), &html, 200, ct_header).await?);
            self.inject_dom_snapshot().await;
            Ok(())
        } else {
            Err(CoreError::NavigationFailed("no current page".into()))
        }
    }

    /// Send a POST request and load the response as a page.
    ///
    /// The `content_type` determines how the body is encoded:
    /// - `"application/json"` — body is parsed as JSON and sent as JSON
    /// - `"application/x-www-form-urlencoded"` — body is parsed as `key=value&key2=value2` form data
    /// - Any other value — body is sent as raw bytes
    #[tracing::instrument(skip(self, body), fields(session = %self.id), err)]
    pub async fn post(&mut self, url: &str, body: &str, content_type: &str) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(CoreError::SessionClosed);
        }
        let parsed = Url::parse(url)?;

        info!(url = %parsed, content_type, "POST request");

        self.ensure_online()?;

        let _in_flight = InFlightGuard::new(self.in_flight.clone());
        let ov = self.overrides.read().clone();
        let started_at_ms = unix_ms();
        let response = match content_type {
            "application/json" => {
                let json_value = serde_json::from_str::<serde_json::Value>(body)
                    .unwrap_or(serde_json::Value::Null);
                self.http_client
                    .post_json_with_overrides(&parsed, &json_value, Some(&ov))
                    .await?
            }
            "application/x-www-form-urlencoded" => {
                let form: Vec<(&str, &str)> = body
                    .split('&')
                    .filter_map(|pair| {
                        let mut parts = pair.splitn(2, '=');
                        Some((parts.next()?, parts.next().unwrap_or("")))
                    })
                    .collect();
                self.http_client
                    .post_form_with_overrides(&parsed, &form, Some(&ov))
                    .await?
            }
            _ => {
                self.http_client
                    .post_with_overrides(&parsed, body.to_string(), Some(&ov))
                    .await?
            }
        };

        let status = response.status().as_u16();
        let response_headers = HttpClient::response_headers(&response);
        let ct = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("text/html")
            .to_string();

        let final_url = Url::parse(&response.uri().to_string()).unwrap_or_else(|_| parsed.clone());

        let bytes = response
            .bytes()
            .await
            .map_err(|e| CoreError::NetworkError(e.to_string()))?;

        let html = crate::encoding::decode_html(&bytes, Some(&ct));

        // Record the POST navigation in the request log (`req-{n}`).
        {
            let request_id = self.next_doc_request_id();
            let (post_body, post_body_truncated) = cap_post_body(Some(body.as_bytes().to_vec()));
            let mut record = self.begin_record(
                &request_id,
                final_url.as_str(),
                "POST",
                "Document",
                &ov,
                None,
            );
            record
                .request_headers
                .push(("Content-Type".to_string(), content_type.to_string()));
            record.started_at_ms = started_at_ms;
            record.post_body = post_body;
            record.post_body_truncated = post_body_truncated;
            record.status = Some(status);
            record.response_headers = response_headers;
            record.mime_type = HttpClient::mime_without_params(&ct);
            record.response_body_length = Some(bytes.len() as u64);
            record.finished_at_ms = Some(unix_ms());
            self.push_record(record);
        }

        // Create a new page for this navigation (use final URL after redirects)
        let page = Page::from_html(final_url.clone(), &html, status, ct).await?;

        // Update history
        if self.history.is_empty() {
            // First navigation
        } else if self.history_index < self.history.len() - 1 {
            self.history.truncate(self.history_index + 1);
        }
        self.history.push(final_url);
        self.history_index = self.history.len() - 1;

        self.active_page = Some(page);

        // Inject DOM snapshot into JS runtime
        self.inject_dom_snapshot().await;

        Ok(())
    }

    /// Evaluate JavaScript.
    ///
    /// Works with or without an active page. Without a page, the DOM bridge
    /// (document.querySelector etc.) will return empty/null results, but
    /// pure JS expressions (arithmetic, JSON, etc.) work fine.
    ///
    /// After evaluation, any DOM mutations recorded by JS (setAttribute,
    /// click, value setter) are applied to the actual DOM and the snapshot
    /// is re-injected into the JS runtime.
    pub async fn evaluate_js(
        &mut self,
        expression: &str,
    ) -> Result<crate::js::runtime::JsEvalResult> {
        self.evaluate_js_with_await(expression, false).await
    }

    /// Evaluate a JS expression, optionally awaiting Promise resolution.
    #[tracing::instrument(skip(self), fields(session = %self.id), err)]
    pub async fn evaluate_js_with_await(
        &mut self,
        expression: &str,
        await_promise: bool,
    ) -> Result<crate::js::runtime::JsEvalResult> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(CoreError::SessionClosed);
        }
        tracing::debug!(expr_len = expression.len(), await = await_promise, "evaluating JS");
        let result = self
            .js_runtime
            .evaluate_with_await(expression, await_promise)
            .await?;

        // DOM edits are now applied live to the RenderDocument by the JS
        // bindings themselves — no mutation log to drain/apply. Only
        // JS-triggered navigation (location.href / assign / reload) is still
        // signalled via the mutation channel, because it needs async network I/O.
        // Take the dirty flag BEFORE the drain: RenderDocument-direct
        // bindings set it instead of journaling, so ordering doesn't lose
        // either signal for this eval.
        let dom_dirty = self.js_runtime.take_dom_dirty();
        let drained = self.js_runtime.drain_mutations();
        let had_mutations = !drained.is_empty();
        for m in drained {
            match m {
                DomMutation::Navigate { url } => {
                    tracing::debug!(url = %url, "JS-triggered navigation");
                    self.navigate(&url).await?;
                }
                DomMutation::Reload => {
                    tracing::debug!("JS-triggered reload");
                    self.reload().await?;
                }
                _ => {} // DOM edits handled directly on the RenderDocument.
            }
        }

        // Screencast generation: journaling bindings leave a non-empty drain,
        // but bindings that mutate the RenderDocument directly (render-element
        // textContent/setAttribute/appendChild/style/click, etc.) only set the
        // shared dom_dirty flag — either signal means the document changed.
        // Read-only evals cost no extra frame.
        if (had_mutations || dom_dirty)
            && let Some(page) = self.page_mut()
        {
            page.bump_generation();
        }

        Ok(result)
    }

    /// Install the CoreEvent sink so console / exception / fetch / WebSocket /
    /// dialog events flow to an observer (typically the CDP layer). Called by
    /// the CDP session once it has created its event drainer.
    pub fn set_event_sink(&mut self, tx: std::sync::mpsc::Sender<crate::js::CoreEvent>) {
        *self.event_tx.write() = Some(tx.clone());
        self.js_runtime.set_event_sink(tx);
    }

    /// Resolve a pending `alert`/`confirm`/`prompt` dialog. Called by the CDP
    /// `Page.handleJavaScriptDialog` handler; wakes the blocked JS thread.
    pub fn resolve_dialog(&self, accept: bool, prompt_text: Option<String>) {
        *self.dialog_gate.lock() = Some(crate::js::DialogResult {
            accept,
            prompt_text,
        });
    }

    /// Clone of the shared dialog-resolution gate. Lets the CDP layer resolve a
    /// pending dialog WITHOUT acquiring the session lock (which a blocking
    /// `alert()` holds via `evaluate_js`).
    pub fn dialog_gate(&self) -> crate::js::DialogGate {
        self.dialog_gate.clone()
    }

    /// Capture a full-page PNG screenshot of the live (post-JS) document.
    ///
    /// Renders the current `RenderDocument` — which JS mutates directly — via
    /// the JS thread. This is a consistent snapshot between JS ticks, with no
    /// serialize/reparse round-trip (the legacy `DomSnapshot` bridge is gone).
    /// The document is laid out at the session's configured viewport.
    ///
    /// Guarded: refuses to capture while a password input has focus, so a
    /// plaintext password can never land in a PNG. The guard error must
    /// propagate — CDP callers must not fall back to a blank PNG.
    pub async fn capture_screenshot_png(&mut self, _viewport_width: u32) -> Result<Vec<u8>> {
        if self.password_field_focused().await? {
            crate::security::audit::record(crate::security::audit::event(
                crate::security::audit::AuditEventKind::PolicyViolation,
                crate::security::audit::AuditDecision::Deny,
                "capture_blocked_password_focus",
            ));
            return Err(crate::error::CoreError::JsError(
                CAPTURE_BLOCKED_MSG.to_string(),
            ));
        }
        let opts = oxibrowser_render::CaptureOpts {
            viewport: None,
            full_page: true,
        };
        self.js_runtime.capture_png(opts).await
    }

    /// Evaluate the active-element probe. Returns `false` (capture allowed)
    /// if the probe itself fails — the probe is trivial JS and a page-level
    /// failure must not brick screenshots.
    async fn password_field_focused(&mut self) -> Result<bool> {
        let result = self
            .evaluate_js(&crate::js::form::js_active_password_probe())
            .await?;
        match result.value {
            Some(serde_json::Value::Bool(b)) => Ok(b),
            _ => Ok(false),
        }
    }

    /// Inject the current page into the JS runtime.
    ///
    /// Builds the `RenderDocument` (the single DOM source of truth that JS
    /// mutates directly) from the page HTML, then also seeds the legacy
    /// `DomSnapshot` (still used by `document.title`/`document.cookie`/window
    /// globals until the webapi DOM is retired) and the page URL.
    async fn inject_dom_snapshot(&mut self) {
        let (html, url, mut scripts) = match &self.active_page {
            Some(page) => {
                let html = page.content().to_string();
                let url = self
                    .current_url()
                    .map(|u| u.as_str().to_string())
                    .unwrap_or_default();
                let scripts = page.root_frame().extract_scripts();
                (html, url, scripts)
            }
            None => return,
        };

        // External <link rel=stylesheet> resolution (W2-pre / §5.2):
        // Blitz's parser panics when a `<link>` href is joined against a
        // `data:` base URL (cannot_be_a_base = true → unwrap in blitz-dom).
        // To avoid that path entirely, we fetch each stylesheet, fold its
        // rules into an inline `<style>` block, and strip the `<link>` from
        // the HTML before handing it to `set_document_with_scripts`. The
        // inline path is already exercised by @font-face rules.
        let html = self.inline_external_stylesheets(&html, &url).await;

        // Fetch external (<script src>) bodies in document order, filling each
        // sequential + in-order for Phase 1 (parallel fetch is Phase 3).
        if !scripts.is_empty() {
            let base = Url::parse(&url).ok();
            for s in scripts.iter_mut() {
                let Some(src) = s.src_url.clone() else {
                    continue;
                };
                let Some(full_url) = base.as_ref().and_then(|b| b.join(&src).ok()) else {
                    continue;
                };
                let _in_flight = InFlightGuard::new(self.in_flight.clone());
                match self
                    .http_client
                    .fetch_text_with_overrides(&full_url, Some(&self.snapshot_overrides()))
                    .await
                {
                    Ok(body) => s.source = body,
                    Err(e) => {
                        tracing::warn!(src = %src, error = %e, "failed to fetch external script")
                    }
                }
            }
        }

        // Init scripts (Playwright `addInitScript`): classic inline sources
        // (`ScriptKind` has no dedicated inline variant — `Classic` is the
        // inline form) prepended so they execute before every page script in
        // the shared `run_navigation_scripts` loop. A failing init script is
        // recorded through the loop's existing error sink
        // (`CoreEvent::Exception` + warn log) and execution continues; a
        // runaway script is bounded by the nav-script limits like any page
        // script.
        if !self.init_scripts.is_empty() {
            let init: Vec<ScriptSource> = self
                .init_scripts
                .iter()
                .map(|(id, source)| {
                    tracing::trace!(script = %id, bytes = source.len(), "queueing init script");
                    ScriptSource {
                        source: source.clone(),
                        src_url: None,
                        kind: ScriptKind::Classic,
                        execute: ExecuteTiming::Defer,
                    }
                })
                .collect();
            let page_scripts = std::mem::replace(&mut scripts, init);
            scripts.extend(page_scripts);
        }

        // Set window.location BEFORE running page scripts: `set_page_url`
        // re-registers the whole `window` global, so it must precede script
        // execution — otherwise any `window.*` properties a script sets
        // (window.onload handlers, framework globals, etc.) would be wiped.
        // M-A: every document injection also re-points the storage origin at
        // THIS page's origin and swaps the JS-side localStorage to that
        // origin's context bucket — so values never bleed across origins
        // (design §3.2). Opaque origins (`about:`, `data:`) converge on the
        // literal `"null"` bucket (documented M-A limitation).
        let page_origin = match Url::parse(&url) {
            Ok(u) => storage_origin_of(&u),
            Err(_) => "null".to_string(),
        };
        *self.current_origin.write() = Some(page_origin.clone());
        // Synchronize JS-thread localStorage writes BEFORE reading this
        // page's bucket: a Drain barrier acks only after the sync thread has
        // applied every message queued before it, so the seed below can't
        // miss a just-executed `localStorage.setItem(...)` (writes are also
        // origin-stamped, so none can bleed into the new page's bucket).
        // Timeout is a deadlock guard — the sync thread may be gone; warn
        // and proceed.
        let (ack_tx, ack_rx) = std::sync::mpsc::channel();
        if self.ls_tx.send(LocalStorageMsg::Drain(ack_tx)).is_ok()
            && ack_rx.recv_timeout(std::time::Duration::from_millis(250)).is_err()
        {
            tracing::warn!(
                "localStorage drain barrier timed out; navigation seed may miss pending JS writes"
            );
        }
        let seed = Some(
            self.local_storage
                .read()
                .get(&page_origin)
                .cloned()
                .unwrap_or_default(),
        );
        self.js_runtime.set_page_url_with_storage_seed(&url, seed);
        // Build/replace the render document AND execute the page's `<script>`
        // tags (Phase 1 keystone).
        let viewport = current_viewport_override()
            .unwrap_or((self.config.viewport_width, self.config.viewport_height));
        // Load @font-face webfonts declared in inline <style> and stage them for
        // the document build (public-API path via DocumentConfig.font_ctx; no fork).
        let font_urls = crate::fonts::extract_font_face_urls(&html);
        if !font_urls.is_empty() {
            let base = Url::parse(&url).ok();
            let mut fonts = Vec::new();
            for furl in font_urls {
                let Some(full) = base.as_ref().and_then(|b| b.join(&furl).ok()) else {
                    continue;
                };
                if full.scheme() != "http" && full.scheme() != "https" {
                    continue;
                }
                match self
                    .http_client
                    .fetch_bytes_with_overrides(&full, Some(&self.snapshot_overrides()))
                    .await
                {
                    Ok(bytes) => fonts.push(bytes),
                    Err(e) => tracing::warn!(url = %full, error = %e, "@font-face fetch failed"),
                }
            }
            if !fonts.is_empty() {
                self.js_runtime.set_pending_fonts(fonts);
            }
        }
        if let Err(e) = self
            .js_runtime
            .set_document_with_scripts(&html, Some(&url), viewport, scripts)
            .await
        {
            tracing::warn!(error = %e, "failed to build render document; falling back");
        }
        // Derive the DomSnapshot from the (now-current) RenderDocument so every
        // reader — JS metadata bindings, CDP DOM/OXI, extract — reflects JS
        // mutations, not a stale navigate-time copy.
        let snapshot = match self.js_runtime.dom_snapshot(&url).await {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(error = %e, "failed to derive DOM snapshot");
                None
            }
        };
        tracing::debug!(
            node_count = snapshot.as_ref().map(|s| s.nodes.len()).unwrap_or(0),
            "DOM snapshot injected"
        );
        self.js_runtime.set_dom_snapshot(snapshot);
    }

    /// Fetch each `<link rel=stylesheet href=…>`, fold the rules into a single
    /// inline `<style>` block, and strip the `<link>` tags from `html`. Used by
    /// [`Self::inject_dom_snapshot`] before handing HTML to Blitz, since Blitz
    /// cannot resolve `<link>` hrefs against `data:` URLs (panics) and does
    /// not otherwise fetch external stylesheets on its own.
    ///
    /// Failures (bad URL, network error) are logged and skipped — a single
    async fn inline_external_stylesheets(&self, html: &str, base_url: &str) -> String {
        use std::sync::atomic::Ordering;
        let links = external_stylesheet_links(html);
        tracing::debug!(
            count = links.len(),
            base_url,
            "inline_external_stylesheets: scanning"
        );
        let base = match Url::parse(base_url) {
            Ok(u) => u,
            Err(_) => return html.to_string(),
        };
        let mut combined_css = String::new();
        for href in links {
            let Ok(full) = base.join(&href) else { continue };
            if full.scheme() != "http" && full.scheme() != "https" {
                continue;
            }
            self.in_flight.fetch_add(1, Ordering::Relaxed);
            let _g = InFlightGuard::new(self.in_flight.clone());
            tracing::debug!(%full, "fetching external stylesheet");
            match self
                .http_client
                .fetch_text_with_overrides(&full, Some(&self.snapshot_overrides()))
                .await
            {
                Ok(css) => {
                    tracing::debug!(bytes = css.len(), %full, "fetched external stylesheet");
                    if !combined_css.is_empty() {
                        combined_css.push('\n');
                    }
                    combined_css.push_str(&css);
                }
                Err(e) => {
                    tracing::warn!(url = %full, error = %e, "failed to fetch external stylesheet")
                }
            }
        }
        let stripped = strip_stylesheet_links(html);
        if combined_css.is_empty() {
            return stripped;
        }
        tracing::debug!(bytes = combined_css.len(), "injecting inline <style> block");
        inject_inline_style(&stripped, &combined_css)
    }

    /// Build per-frame execution contexts for each **descendant** iframe
    /// (Phase 8, extended in W3b to multi-level frame trees).
    ///
    /// Walks the page's root frame tree and for every frame (root excluded —
    /// the root is built by `SetDocument`, not here) assigns a unique
    /// `context_id` (≥ 2), fetches external `<script src>` bodies, and sends
    /// a `SetFrameDocument` command to the JS thread which creates a dedicated
    /// `Context` + `RenderDocument` and runs the frame's scripts. The
    /// frame-id → context-id mapping is stored in `frame_contexts` for CDP
    /// routing (`Runtime.evaluate` with `contextId`).
    async fn inject_child_frames(&mut self) {
        let Some(page) = &self.active_page else {
            return;
        };
        let base_url = match self.current_url() {
            Some(u) => u.clone(),
            None => return,
        };
        let viewport = current_viewport_override()
            .unwrap_or((self.config.viewport_width, self.config.viewport_height));

        // Phase 1: collect every (frame_id_str, url, scripts) tuple without
        // holding a page borrow. `page.root_frame()` is immutable so this is
        // a single walk over the full tree.
        let mut stack: Vec<&Frame> = vec![page.root_frame()];
        let mut children: Vec<(String, String, Vec<crate::js::dom_snapshot::ScriptSource>)> =
            Vec::new();
        while let Some(frame) = stack.pop() {
            for child in frame.children().iter() {
                children.push((
                    child.id().to_string(),
                    child.url().to_string(),
                    child.extract_scripts(),
                ));
                stack.push(child);
            }
        }

        for (frame_id_str, url_str, mut scripts) in children {
            // Fetch external <script src> bodies for this child frame.
            if !scripts.is_empty() {
                let base = Url::parse(&url_str).ok().or_else(|| base_url.join("").ok());
                for s in scripts.iter_mut() {
                    let Some(src) = s.src_url.clone() else {
                        continue;
                    };
                    let Some(full_url) = base.as_ref().and_then(|b| b.join(&src).ok()) else {
                        continue;
                    };
                    let _in_flight = InFlightGuard::new(self.in_flight.clone());
                    match self
                        .http_client
                        .fetch_text_with_overrides(&full_url, Some(&self.snapshot_overrides()))
                        .await
                    {
                        Ok(body) => s.source = body,
                        Err(e) => {
                            tracing::warn!(src = %src, error = %e, "failed to fetch child frame script")
                        }
                    }
                }
            }

            // Locate the child frame by id so the right frame is built —
            // includes deeply-nested frames added by `populate_iframes`.
            let html = self
                .active_page
                .as_ref()
                .and_then(|p| p.root_frame().find_by_frame_id_str(&frame_id_str))
                .map(|f| f.html().to_string())
                .unwrap_or_default();
            if html.is_empty() {
                tracing::warn!(
                    frame_id = %frame_id_str,
                    url = %url_str,
                    "child frame html missing — skipping context build"
                );
                continue;
            }

            let context_id = self
                .next_context_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            if let Err(e) = self
                .js_runtime
                .set_frame_document(context_id, &html, &url_str, viewport, scripts)
                .await
            {
                tracing::warn!(frame_id = %frame_id_str, url = %url_str, error = %e, "failed to build child frame context");
                continue;
            }
            tracing::debug!(frame_id = %frame_id_str, context_id, url = %url_str, "child frame context built");
            self.frame_contexts.write().insert(frame_id_str, context_id);
        }
    }

    /// Evaluate a JS expression in a specific frame's execution context
    /// (Phase 8). `context_id` must correspond to a known frame context.
    pub async fn evaluate_js_in_context(
        &mut self,
        expression: &str,
        context_id: u32,
        await_promise: bool,
    ) -> Result<crate::js::runtime::JsEvalResult> {
        self.js_runtime
            .evaluate_in_context(expression, context_id, await_promise)
            .await
    }

    /// Return the frame-id → context-id map for CDP execution-context routing
    /// (Phase 8). Includes only child frames; the main frame is always
    /// context_id=1.
    pub fn frame_context_map(&self) -> &parking_lot::RwLock<HashMap<String, u32>> {
        &self.frame_contexts
    }

    /// Serialize the live (post-JS) document to a [`DomSnapshot`].
    ///
    /// For CDP DOM/OXI and `extract` readers — reflects JS mutations because it
    /// is derived from the `RenderDocument` on the JS thread.
    pub async fn dom_snapshot(&mut self) -> Result<Option<crate::js::dom_snapshot::DomSnapshot>> {
        let url = self
            .current_url()
            .map(|u| u.as_str().to_string())
            .unwrap_or_default();
        match self.js_runtime.dom_snapshot(&url).await {
            Ok(s) => Ok(Some(s)),
            Err(CoreError::ScreenshotError(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Wait for a CSS selector to match an element in the current page.
    ///
    /// Polls the active page's DOM every 50ms until the selector matches
    /// or the timeout is exceeded.
    pub async fn wait_for(&mut self, selector: &str, timeout_ms: u64) -> Result<()> {
        let start = std::time::Instant::now();
        let duration = std::time::Duration::from_millis(timeout_ms);

        let expr = format!(
            "document.querySelector({}) !== null",
            serde_json::to_string(selector).unwrap_or_else(|_| "null".into())
        );
        loop {
            // Check the LIVE (post-JS) DOM. Each evaluate drains microtasks +
            // due timers, advancing the event loop so delayed renders surface.
            if let Ok(r) = self.evaluate_js(&expr).await
                && r.value == Some(serde_json::Value::Bool(true))
            {
                return Ok(());
            }

            if start.elapsed() >= duration {
                return Err(CoreError::NavigationFailed(format!(
                    "wait_for('{}') timed out after {}ms",
                    selector, timeout_ms
                )));
            }

            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// Get the current page (if any).
    pub fn page(&self) -> Option<&Page> {
        self.active_page.as_ref()
    }

    /// Get the current page mutably.
    pub fn page_mut(&mut self) -> Option<&mut Page> {
        self.active_page.as_mut()
    }

    /// Get the current URL.
    pub fn current_url(&self) -> Option<&Url> {
        self.active_page.as_ref().map(|p| p.url())
    }

    /// Get the session ID.
    pub fn id(&self) -> SessionId {
        self.id
    }

    /// Get the parent browser ID.
    pub fn browser_id(&self) -> BrowserId {
        self.browser_id
    }

    /// Get the HTTP client.
    pub fn http_client(&self) -> Arc<HttpClient> {
        self.http_client.clone()
    }

    /// Replace the per-session request overrides (CDP
    /// `Emulation.setUserAgentOverride` / `Network.setExtraHTTPHeaders`).
    pub fn set_overrides(&mut self, ov: RequestOverrides) {
        *self.overrides.write() = ov;
    }

    /// Read the current request overrides (a snapshot — the stored value is
    /// shared with the background fetch bridge thread).
    pub fn overrides(&self) -> RequestOverrides {
        self.snapshot_overrides()
    }

    /// Snapshot (clone) the current request overrides. Session code clones
    /// instead of holding the lock across `await`.
    fn snapshot_overrides(&self) -> RequestOverrides {
        self.overrides.read().clone()
    }

    /// UA actually in effect: the override if set, else the configured UA.
    /// This is what document/sub-resource requests carry on the wire.
    pub fn effective_ua(&self) -> String {
        self.overrides
            .read()
            .user_agent
            .clone()
            .unwrap_or_else(|| self.config.user_agent.clone())
    }

    /// Toggle offline emulation (CDP `Network.emulateNetworkConditions`).
    ///
    /// While offline, every network fetch the Session would issue — document
    /// navigations, history traversals, sub-resources, POSTs, and JS-issued
    /// fetches — fails immediately with an "offline" error instead of
    /// performing I/O.
    pub fn set_offline(&self, on: bool) {
        self.offline.store(on, Ordering::SeqCst);
    }

    /// Whether offline emulation is active.
    pub fn is_offline(&self) -> bool {
        self.offline.load(Ordering::SeqCst)
    }

    /// Return an "offline" error when offline emulation is active.
    fn ensure_online(&self) -> Result<()> {
        if self.is_offline() {
            return Err(CoreError::NetworkError("offline".into()));
        }
        Ok(())
    }

    /// Push a JS-surface user-agent override (`navigator.userAgent` + the
    /// stealth fingerprint profile). `None` clears it. Used by the CDP
    /// `Emulation.setUserAgentOverride` handler so the JS surface and the
    /// wire UA agree.
    pub fn set_js_user_agent(&self, ua: Option<String>) {
        self.js_runtime.set_user_agent(ua);
    }

    /// Override the CSS `prefers-color-scheme` media feature: `Some(true)`
    /// = dark, `Some(false)` = light, `None` = clear (default light).
    /// `matchMedia` probes see the change immediately; layout picks it up at
    /// the next document generation. Used by the CDP
    /// `Emulation.setEmulatedMedia` handler.
    pub fn set_media_color_scheme(&self, dark: Option<bool>) {
        self.js_runtime.set_media_color_scheme(dark);
    }

    /// Snapshot of currently in-flight HTTP requests (navigates + JS fetches).
    ///
    /// Returns the count of dispatched requests whose response (or terminal
    /// error) has not yet been observed. `wait_for_condition(NetworkIdle)`
    /// polls this value via the Tab layer; it is also useful for tests and
    /// for surfacing load progress in higher layers. The counter is shared
    /// with the background fetch handler thread via `Arc<AtomicU64>` and
    /// updated under `Relaxed` ordering — fast to read, may briefly
    /// straddle a request start/complete.
    pub fn in_flight_requests(&self) -> u64 {
        self.in_flight.load(Ordering::Relaxed)
    }

    /// Get navigation history.
    pub fn history(&self) -> &[Url] {
        &self.history
    }

    /// Get history position.
    pub fn history_index(&self) -> usize {
        self.history_index
    }

    /// Set a local storage value.
    ///
    /// Lands in the origin bucket selected by [`Session::storage_origin`] —
    /// the current page's origin when a page is open, else the last imported
    /// storage state's origin, else the opaque `"null"` bucket.
    pub fn set_local_storage(&self, key: impl Into<String>, value: impl Into<String>) {
        let origin = self.storage_origin();
        self.local_storage
            .write()
            .entry(origin)
            .or_default()
            .insert(key.into(), value.into());
    }

    /// Origin bucket key for direct localStorage access: the current page's
    /// origin, else the origin of the last imported storage state, else the
    /// opaque-origin bucket (`"null"`).
    fn storage_origin(&self) -> String {
        if let Some(origin) = self.current_origin.read().clone() {
            return origin;
        }
        if let Some(origin) = &self.last_import_origin {
            return origin.clone();
        }
        "null".to_string()
    }

    /// Register an init script (Playwright `Page.addInitScript`).
    ///
    /// The `source` is evaluated in the page context before the page's own
    /// `<script>` tags on **every** subsequent document injection
    /// (navigate / go_back / go_forward / reload / about:/data: pages), after
    /// window globals are (re-)registered. Returns a stable script id
    /// (`"init-N"`, monotonic per session) usable with
    /// [`Session::remove_init_script`].
    pub fn add_init_script(&mut self, source: String) -> String {
        self.init_script_counter += 1;
        let id = format!("init-{}", self.init_script_counter);
        tracing::debug!(script = %id, bytes = source.len(), "init script registered");
        self.init_scripts.push((id.clone(), source));
        id
    }

    /// Remove a previously registered init script by id. Returns `true` when
    /// a script with that id existed and was removed.
    pub fn remove_init_script(&mut self, id: &str) -> bool {
        let before = self.init_scripts.len();
        self.init_scripts.retain(|(script_id, _)| script_id != id);
        let removed = self.init_scripts.len() < before;
        if removed {
            tracing::debug!(script = %id, "init script removed");
        }
        removed
    }

    /// Export the session's storage state (Playwright `storageState`).
    ///
    /// Cookies come from the session's [`CookieJar`]; localStorage carries the
    /// **current page's origin bucket only** from the context's origin-keyed
    /// map (single-origin compat, design M6′: multi-origin export is future
    /// work). Entries stored while visiting other origins stay in their own
    /// buckets and are not folded in.
    /// With no active page (or an opaque origin, e.g. `about:blank` / `data:`
    /// URLs), `origins` is empty.
    pub fn export_state(&self) -> crate::storage_state::StorageState {
        let cookies = self.cookie_jar.read().get_all();
        let origins = match self.current_url() {
            Some(url) if matches!(url.origin(), url::Origin::Tuple(..)) => {
                let origin_key = storage_origin_of(url);
                let map = self.local_storage.read();
                match map.get(&origin_key) {
                    Some(bucket) if !bucket.is_empty() => {
                        vec![crate::storage_state::OriginState {
                            origin: origin_key,
                            local_storage: bucket
                                .iter()
                                .map(|(k, v)| crate::storage_state::LocalStorageEntry {
                                    name: k.clone(),
                                    value: v.clone(),
                                })
                                .collect(),
                        }]
                    }
                    _ => Vec::new(),
                }
            }
            _ => Vec::new(),
        };
        crate::storage_state::StorageState { cookies, origins }
    }

    /// Import a storage state (Playwright `storageState` merge): cookies are
    /// merged into the session's [`CookieJar`] (same-name/path cookies per
    /// domain are replaced), and each `OriginState`'s localStorage entries
    /// merge into **their own origin's bucket** of the context's origin-keyed
    /// map — origin identity is preserved (fixes the former flat merge that
    /// bled entries across origins). A subsequent navigation to a matching
    /// origin hands that bucket to the JS thread via `SetPageUrl`; with no
    /// page open, direct `get_local_storage`/`set_local_storage` calls route
    /// to the last imported origin.
    pub fn import_state(&mut self, st: &crate::storage_state::StorageState) -> Result<()> {
        {
            let mut jar = self.cookie_jar.write();
            for cookie in &st.cookies {
                jar.insert_entry(cookie.clone());
            }
        }
        tracing::debug!(cookies = st.cookies.len(), "storageState cookies merged");
        if st.origins.is_empty() {
            return Ok(());
        }
        {
            let mut ls = self.local_storage.write();
            for origin in &st.origins {
                let bucket = ls.entry(origin.origin.clone()).or_default();
                for kv in &origin.local_storage {
                    bucket.insert(kv.name.clone(), kv.value.clone());
                }
            }
        }
        self.last_import_origin = st.origins.last().map(|o| o.origin.clone());
        tracing::debug!(
            origins = st.origins.len(),
            "storageState localStorage merged into per-origin buckets"
        );
        Ok(())
    }

    /// Get a local storage value.
    ///
    /// Reads the origin bucket selected by [`Session::storage_origin`] (see
    /// [`Session::set_local_storage`]).
    pub fn get_local_storage(&self, key: &str) -> Option<String> {
        let origin = self.storage_origin();
        self.local_storage.read().get(&origin)?.get(key).cloned()
    }

    /// Store a response body for later retrieval (Network.getResponseBody).
    pub fn store_response_body(&self, request_id: &str, body: String, content_type: &str) {
        let mut guard = self.response_bodies.write();
        guard.insert(
            request_id.to_string(),
            CapturedResponse {
                body,
                base64: false,
                content_type: content_type.to_string(),
            },
        );
    }

    /// Get a stored response body by request ID.
    pub fn get_response_body(&self, request_id: &str) -> Option<CapturedResponse> {
        self.response_bodies.read().get(request_id).cloned()
    }

    /// Mint the next session-monotonic document/page request id (`req-{n}`).
    /// Shared by the response-body store, the request log, and (via the CDP
    /// layer) `Network.requestWillBeSent` correlation.
    fn next_doc_request_id(&self) -> String {
        format!(
            "req-{}",
            self.next_request_id.fetch_add(1, Ordering::Relaxed) + 1
        )
    }

    /// Mint the next session-monotonic sub-resource request id (`res-{n}`).
    fn next_sub_request_id(&self) -> String {
        format!(
            "res-{}",
            self.next_resource_id.fetch_add(1, Ordering::Relaxed) + 1
        )
    }

    /// Start a [`RequestRecord`] for a Session-issued request. The returned
    /// record is incomplete (`status: None`) until the caller fills the
    /// response fields and passes it to [`Session::push_record`].
    fn begin_record(
        &self,
        request_id: &str,
        url: &str,
        method: &str,
        resource_type: &str,
        ov: &RequestOverrides,
        post_body: Option<Vec<u8>>,
    ) -> RequestRecord {
        let (post_body, post_body_truncated) = cap_post_body(post_body);
        let mut request_headers = Vec::with_capacity(ov.extra_headers.len() + 1);
        request_headers.push(("User-Agent".to_string(), self.effective_ua()));
        for (name, value) in &ov.extra_headers {
            if name.eq_ignore_ascii_case("user-agent") {
                continue; // already represented by the effective UA
            }
            request_headers.push((name.clone(), value.clone()));
        }
        RequestRecord {
            request_id: request_id.to_string(),
            url: url.to_string(),
            method: method.to_string(),
            resource_type: resource_type.to_string(),
            request_headers,
            post_body,
            post_body_truncated,
            status: None,
            response_headers: Vec::new(),
            mime_type: String::new(),
            started_at_ms: unix_ms(),
            finished_at_ms: None,
            // No response cache exists — nothing can come from cache.
            from_cache: false,
            response_body_length: None,
        }
    }

    /// Append a completed/in-progress record to the rolling request log,
    /// evicting the oldest entry past [`NETWORK_LOG_CAP`].
    fn push_record(&self, record: RequestRecord) {
        record_request_start(&self.network_log, record);
    }

    /// Complete the newest record with the given id: response status,
    /// headers, MIME type and body length.
    fn record_request_finish_by_id(
        &self,
        request_id: &str,
        status: Option<u16>,
        response_headers: Vec<(String, String)>,
        mime_type: String,
        response_body_length: Option<u64>,
    ) {
        record_request_finish(
            &self.network_log,
            request_id,
            status,
            response_headers,
            mime_type,
            response_body_length,
        );
    }

    /// Record a history-navigation / reload document fetch (fresh `req-{n}`
    /// id; these paths don't store response bodies, so the id only feeds the
    /// request log).
    #[allow(clippy::too_many_arguments)] // channel/protocol boundary: one arg per field
    fn record_document_fetch(
        &self,
        url: &Url,
        status: u16,
        ct_header: &str,
        response_headers: Vec<(String, String)>,
        body_len: usize,
        ov: &RequestOverrides,
        started_at_ms: f64,
    ) {
        let request_id = self.next_doc_request_id();
        let mut record = self.begin_record(&request_id, url.as_str(), "GET", "Document", ov, None);
        record.started_at_ms = started_at_ms;
        record.status = Some(status);
        record.response_headers = response_headers;
        record.mime_type = HttpClient::mime_without_params(ct_header);
        record.response_body_length = Some(body_len as u64);
        record.finished_at_ms = Some(unix_ms());
        self.push_record(record);
    }

    /// Emit a [`crate::js::CoreEvent`] to the attached sink, if any. No-op
    /// when no observer is installed (e.g. the CLI path).
    fn emit_core_event(&self, event: crate::js::CoreEvent) {
        if let Some(sender) = self.event_tx.read().as_ref() {
            let _ = sender.send(event);
        }
    }

    /// Snapshot of the rolling request log (oldest first). Covers document
    /// navigations (`navigate`/`reload`/`go_back`/`go_forward`/`post`),
    /// sub-resource fetches, and JS-issued `fetch`/XHR.
    pub fn network_log_snapshot(&self) -> Vec<RequestRecord> {
        self.network_log.lock().iter().cloned().collect()
    }

    /// Clear the rolling request log.
    pub fn clear_network_log(&self) {
        self.network_log.lock().clear();
    }

    /// HAR 1.2 JSON export of the current request log
    /// ([`crate::network::har::to_har_json`]).
    pub fn network_log_har(&self) -> serde_json::Value {
        har::to_har_json(&self.network_log_snapshot())
    }

    /// Raw (unredacted) HAR view — `--har-raw` only. Carries cookies, bearer
    /// tokens, and POST bodies verbatim; the CLI audit-logs this call.
    pub fn network_log_har_raw(&self) -> serde_json::Value {
        har::to_har_json_raw(&self.network_log_snapshot())
    }

    /// Get the cookie jar for this session.
    pub fn cookie_jar(&self) -> &Arc<RwLock<crate::network::CookieJar>> {
        &self.cookie_jar
    }

    /// Close the session.
    #[tracing::instrument(skip(self), fields(session = %self.id), err)]
    pub async fn close(&mut self) -> Result<()> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        info!(id = %self.id, "session closed");
        self.active_page = None;
        self.history.clear();
        // Deliberately NOT clearing `local_storage`: the map is shared with
        // the parent BrowserContext (M-A) — clearing here would wipe sibling
        // sessions' storage and defeat per-context persistence. Context
        // storage is disposed via `BrowserContext::clear_storage` /
        // `Browser::dispose_context` (M-B+).
        Ok(())
    }

    /// Whether the session has been closed.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Replace the active page and inject DOM snapshot (for testing).
    #[cfg(test)]
    pub async fn inject_dom_snapshot_for_test(&mut self, page: Page) {
        self.active_page = Some(page);
        self.inject_dom_snapshot().await;
    }

    /// Test-only: clone the in-flight counter's `Arc` so tests can
    /// simulate request starts/completions without driving a real
    /// `Session::navigate` / `handle_fetch_requests` round-trip.
    #[cfg(test)]
    pub fn in_flight_counter_handle_for_test(&self) -> Arc<AtomicU64> {
        self.in_flight.clone()
    }

    /// Fetch sub-resources (JS, CSS, images) referenced by the current page.
    ///
    /// Extracts resource URLs from the DOM, fetches them over HTTP,
    /// and attaches them as `Resource` objects to the page (with the real
    /// HTTP status / MIME type from the response). Each fetch is recorded in
    /// the session request log under a session-monotonic `res-{n}` id and,
    /// when an event sink is attached, emits
    /// [`crate::js::CoreEvent::SubresourceFetchRequest`] /
    /// [`SubresourceFetchResponse`](crate::js::CoreEvent::SubresourceFetchResponse)
    /// / [`SubresourceFetchFailed`](crate::js::CoreEvent::SubresourceFetchFailed).
    ///
    /// Returns the number of resources successfully loaded.
    pub async fn load_sub_resources(&mut self) -> usize {
        use crate::js::dom_snapshot::ResourceKind;

        let resource_urls = match self.active_page.as_ref() {
            Some(page) => page.root_frame().extract_resource_urls(),
            None => return 0,
        };

        if resource_urls.is_empty() {
            return 0;
        }

        // Offline emulation: skip all sub-resource I/O. The document path
        // surfaces the same condition as an "offline" error; this path has no
        // error channel, so it reports zero loaded resources instead.
        if self.is_offline() {
            tracing::warn!(
                count = resource_urls.len(),
                "offline: skipping sub-resource fetches"
            );
            return 0;
        }

        let base_url = match self.current_url() {
            Some(u) => u.clone(),
            None => return 0,
        };

        let ov = self.snapshot_overrides();

        let mut loaded = 0;
        for res in &resource_urls {
            // Resolve relative URLs against the page URL
            let full_url = match base_url.join(&res.url) {
                Ok(u) => u,
                Err(_) => continue,
            };

            let (resource_type, cdp_type) = match res.kind {
                ResourceKind::Script => (crate::network::resource::ResourceType::Script, "Script"),
                ResourceKind::Stylesheet => (
                    crate::network::resource::ResourceType::Stylesheet,
                    "Stylesheet",
                ),
                ResourceKind::Image => (crate::network::resource::ResourceType::Image, "Image"),
                ResourceKind::Iframe => {
                    (crate::network::resource::ResourceType::Document, "Document")
                }
            };

            let request_id = self.next_sub_request_id();
            let started_at_ms = unix_ms();

            self.emit_core_event(crate::js::CoreEvent::SubresourceFetchRequest {
                request_id: request_id.clone(),
                url: full_url.to_string(),
                method: "GET".to_string(),
                resource_type: cdp_type.to_string(),
                timestamp: started_at_ms,
            });

            let mut record =
                self.begin_record(&request_id, full_url.as_str(), "GET", cdp_type, &ov, None);
            record.started_at_ms = started_at_ms;
            self.push_record(record);

            let _in_flight = InFlightGuard::new(self.in_flight.clone());
            match self
                .http_client
                .fetch_response_with_overrides(&full_url, Some(&ov))
                .await
            {
                Ok(fetched) => {
                    // Text resources decode as UTF-8 (lossy) so downstream
                    // consumers keep seeing `String` bodies; images/iframes
                    // keep raw bytes.
                    let body = match res.kind {
                        ResourceKind::Script | ResourceKind::Stylesheet => {
                            bytes::Bytes::from(String::from_utf8_lossy(&fetched.body).into_owned())
                        }
                        ResourceKind::Image | ResourceKind::Iframe => {
                            bytes::Bytes::from(fetched.body)
                        }
                    };
                    let length = body.len() as u64;

                    let resource = crate::network::resource::Resource {
                        url: full_url.to_string(),
                        resource_type,
                        status: fetched.status,
                        mime_type: fetched.mime_type.clone(),
                        body,
                        loaded_at: std::time::Instant::now(),
                    };
                    if let Some(page) = self.active_page.as_mut() {
                        page.add_resource(resource);
                    }
                    loaded += 1;

                    self.record_request_finish_by_id(
                        &request_id,
                        Some(fetched.status),
                        fetched.headers.clone(),
                        fetched.mime_type.clone(),
                        Some(length),
                    );
                    self.emit_core_event(crate::js::CoreEvent::SubresourceFetchResponse {
                        request_id: request_id.clone(),
                        url: full_url.to_string(),
                        status: fetched.status,
                        mime_type: fetched.mime_type,
                        length,
                        timestamp: unix_ms(),
                    });
                }
                Err(e) => {
                    self.record_request_finish_by_id(
                        &request_id,
                        None,
                        Vec::new(),
                        String::new(),
                        None,
                    );
                    self.emit_core_event(crate::js::CoreEvent::SubresourceFetchFailed {
                        request_id: request_id.clone(),
                        url: full_url.to_string(),
                        error_text: e.to_string(),
                        timestamp: unix_ms(),
                    });
                    tracing::warn!(
                        url = %full_url,
                        error = %e,
                        "failed to load sub-resource"
                    );
                }
            }
        }

        tracing::info!(
            loaded = loaded,
            total = resource_urls.len(),
            "sub-resources loaded"
        );
        loaded
    }
}

/// Extract the `filename` from a `Content-Disposition` header value.
/// Handles both `filename="name"` and `filename=name` (case-insensitive).
fn filename_from_disposition(disposition: &str) -> Option<String> {
    let lower = disposition.to_ascii_lowercase();
    let idx = lower.find("filename=")?;
    let rest = &disposition[idx + "filename=".len()..];
    let rest = rest.trim_start();
    if let Some(stripped) = rest.strip_prefix('"') {
        // Quoted: take until closing quote.
        stripped.split('"').next().map(|s| s.to_string())
    } else {
        // Unquoted: take until ';' or end.
        rest.split(';').next().map(|s| s.trim().to_string())
    }
    .filter(|s| !s.is_empty())
}

// External stylesheet plumbing lives in [`crate::dom_link`].
use crate::dom_link::{external_stylesheet_links, inject_inline_style, strip_stylesheet_links};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::BrowserId;
    use crate::config::BrowserConfig;
    use crate::context::ContextId;
    use crate::network::HttpClient;
    use crate::network::cookie::CookieJar;
    use crate::page::Page;

    /// Session-bound [`BrowserContext`] with a fresh jar/client and the
    /// given config.
    fn make_context(config: &BrowserConfig) -> Arc<BrowserContext> {
        let http_client = Arc::new(HttpClient::new(
            config,
            Arc::new(RwLock::new(CookieJar::new())),
        )
        .unwrap());
        Arc::new(BrowserContext::with_cookie_jar(
            ContextId::test_next(),
            None,
            http_client,
            Arc::new(RwLock::new(CookieJar::new())),
        ))
    }

    /// Build a Session with SSRF disabled (so tests can reach loopback mocks)
    /// and the default (high) nav-script limits.
    async fn make_session() -> Session {
        let mut config = BrowserConfig::headless();
        config.enable_ssrf_filter = false;
        let context = make_context(&config);
        Session::new(BrowserId::next(), config, context)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn effective_ua_prefers_override_then_config() {
        let mut config = BrowserConfig::headless();
        config.enable_ssrf_filter = false;
        let configured = config.user_agent.clone();
        let context = make_context(&config);
        let mut session = Session::new(BrowserId::next(), config, context)
            .await
            .unwrap();

        // No override → configured UA.
        session.set_overrides(RequestOverrides::default());
        assert_eq!(session.effective_ua(), configured);

        // Override set → override wins; extra headers round-trip.
        session.set_overrides(RequestOverrides {
            user_agent: Some("OverrideUA/9.9".into()),
            extra_headers: vec![("X-Test".to_string(), "1".to_string())],
        });
        assert_eq!(session.effective_ua(), "OverrideUA/9.9");
        assert_eq!(
            session.overrides().extra_headers,
            vec![("X-Test".to_string(), "1".to_string())]
        );

        // Clearing the override restores the configured UA.
        session.set_overrides(RequestOverrides::default());
        assert_eq!(session.effective_ua(), configured);
    }

    #[tokio::test]
    async fn offline_mode_rejects_navigate_and_allows_local_pages() {
        let mut session = make_session().await;
        assert!(!session.is_offline());

        session.set_offline(true);
        assert!(session.is_offline());

        // Network navigation fails fast with the offline error — before any
        // DNS/connection attempt (the URL is not even resolved).
        let err = session
            .navigate("https://offline.invalid/example")
            .await
            .unwrap_err();
        match err {
            CoreError::NetworkError(msg) => assert_eq!(msg, "offline"),
            other => panic!("expected NetworkError(\"offline\"), got {other:?}"),
        }

        // Local pages stay reachable while offline (no network fetch).
        assert!(session.navigate("about:blank").await.is_ok());
        assert!(session.navigate("data:text/html,<b>hi</b>").await.is_ok());

        session.set_offline(false);
        assert!(!session.is_offline());
    }

    #[tokio::test]
    async fn test_inject_dom_snapshot_runs_inline_scripts() {
        // Phase 1 keystone end-to-end: a page's inline <script> must execute
        // during document injection, mutating the live DOM that a later
        // evaluate observes — exactly like headless Chrome.
        let mut session = make_session().await;
        let html = r#"<html><head></head><body>
            <div id="app">placeholder</div>
            <script>document.getElementById('app').textContent = 'rendered';</script>
            </body></html>"#;
        let url = Url::parse("https://test.local/").unwrap();
        let page = Page::from_html(url, html, 200, "text/html".into())
            .await
            .unwrap();
        session.inject_dom_snapshot_for_test(page).await;

        let r = session
            .evaluate_js("document.getElementById('app').textContent")
            .await
            .expect("evaluate");
        assert_eq!(
            r.value,
            Some(serde_json::json!("rendered")),
            "inline script ran during injection"
        );
    }

    #[tokio::test]
    async fn test_ready_state_is_complete_after_inject() {
        let mut session = make_session().await;
        let html = r#"<html><body><script>window.__any = 1;</script></body></html>"#;
        let url = Url::parse("https://test.local/").unwrap();
        let page = Page::from_html(url, html, 200, "text/html".into())
            .await
            .unwrap();
        session.inject_dom_snapshot_for_test(page).await;

        let r = session
            .evaluate_js("document.readyState")
            .await
            .expect("evaluate");
        assert_eq!(r.value, Some(serde_json::json!("complete")));
    }

    #[test]
    fn test_filename_from_disposition() {
        assert_eq!(
            filename_from_disposition("attachment; filename=\"report.pdf\""),
            Some("report.pdf".into())
        );
        assert_eq!(
            filename_from_disposition("attachment; filename=data.csv"),
            Some("data.csv".into())
        );
        assert_eq!(filename_from_disposition("inline"), None);
    }

    #[tokio::test]
    async fn test_init_script_runs_before_page_script_on_data_url() {
        // Acceptance (#1): a registered init script must execute before the
        // page's own <script> — proven on a data: URL page whose inline
        // script appends 'P' after the init script's 'I'.
        let mut session = make_session().await;
        let id = session
            .add_init_script("globalThis.__order = (globalThis.__order || '') + 'I';".to_string());
        assert_eq!(id, "init-1", "init script ids are init-N, monotonic");
        session
            .navigate(
                "data:text/html,<html><body><script>globalThis.__order = \
                 (globalThis.__order || '') + 'P';</script></body></html>",
            )
            .await
            .expect("navigate data: URL");

        let r = session
            .evaluate_js("globalThis.__order")
            .await
            .expect("evaluate");
        assert_eq!(
            r.value,
            Some(serde_json::json!("IP")),
            "init script must run before the page script"
        );

        // Init scripts persist across navigations and re-run every time. The
        // JS context itself persists, so the accumulated string grows.
        session
            .navigate("about:blank")
            .await
            .expect("navigate about");
        let r = session
            .evaluate_js("globalThis.__order")
            .await
            .expect("evaluate");
        assert_eq!(
            r.value,
            Some(serde_json::json!("IPI")),
            "init script re-ran on the next page (before its scripts — about:blank has none)"
        );
    }

    #[tokio::test]
    async fn test_init_script_add_remove_monotonic_ids() {
        let mut session = make_session().await;
        assert_eq!(session.add_init_script("// a".into()), "init-1");
        let second = session.add_init_script("// b".into());
        assert_eq!(second, "init-2", "counter is monotonic per session");
        assert!(
            session.remove_init_script("init-1"),
            "removing an existing id returns true"
        );
        assert!(
            !session.remove_init_script("init-1"),
            "removing a missing id returns false"
        );
        assert!(session.remove_init_script(&second));
        assert!(!session.remove_init_script(&second));
    }

    #[tokio::test]
    async fn test_storage_state_export_import_roundtrip() {
        use crate::network::cookie::{CookieEntry, SameSite};
        use crate::storage_state::{LocalStorageEntry, OriginState, StorageState};
        let mut session = make_session().await;

        let st = StorageState {
            cookies: vec![CookieEntry {
                name: "sid".into(),
                value: "abc".into(),
                path: Some("/".into()),
                domain: Some("example.com".into()),
                secure: true,
                http_only: true,
                same_site: Some(SameSite::Lax),
                ..Default::default()
            }],
            origins: vec![OriginState {
                origin: "https://example.com".into(),
                local_storage: vec![LocalStorageEntry {
                    name: "k".into(),
                    value: "v".into(),
                }],
            }],
        };
        session.import_state(&st).expect("import");

        // Cookies merged into the jar (insert_entry semantics).
        let all = session.cookie_jar().read().get_all();
        assert_eq!(all.len(), 1, "cookie merged");
        assert_eq!(all[0].name, "sid");
        assert_eq!(all[0].value, "abc");
        assert_eq!(all[0].domain.as_deref(), Some("example.com"));
        assert_eq!(all[0].same_site, Some(SameSite::Lax));

        // No active page yet → export has cookies but no origins.
        let exported = session.export_state();
        assert_eq!(exported.cookies.len(), 1);
        assert!(exported.origins.is_empty(), "no active page → no origins");

        // Attach a page on the matching origin: the single storage map is
        // exported under that one origin.
        let page = Page::from_html(
            Url::parse("https://example.com/x").unwrap(),
            "<html><body></body></html>",
            200,
            "text/html".into(),
        )
        .await
        .unwrap();
        session.inject_dom_snapshot_for_test(page).await;

        let exported = session.export_state();
        assert_eq!(exported.origins.len(), 1, "single-origin limitation");
        assert_eq!(exported.origins[0].origin, "https://example.com");
        assert_eq!(
            exported.origins[0].local_storage,
            vec![LocalStorageEntry {
                name: "k".into(),
                value: "v".into()
            }]
        );

        // Playwright-compatible JSON shape (sameSite casing + localStorage key).
        let json = serde_json::to_string(&exported).unwrap();
        assert!(
            json.contains(r#""sameSite":"Lax""#),
            "sameSite casing: {json}"
        );
        assert!(
            json.contains(r#""localStorage""#),
            "localStorage key: {json}"
        );

        // Full round-trip into a fresh session.
        let back: StorageState = serde_json::from_str(&json).unwrap();
        let mut s2 = make_session().await;
        s2.import_state(&back).expect("re-import");
        assert_eq!(s2.cookie_jar().read().get_all().len(), 1);
        assert_eq!(s2.get_local_storage("k").as_deref(), Some("v"));
    }

    #[tokio::test]
    async fn test_import_state_restores_local_storage_on_same_origin() {
        use crate::storage_state::{LocalStorageEntry, OriginState, StorageState};
        let mut session = make_session().await;
        session
            .import_state(&StorageState {
                cookies: vec![],
                origins: vec![OriginState {
                    origin: "https://example.com".into(),
                    local_storage: vec![LocalStorageEntry {
                        name: "token".into(),
                        value: "t0k3n".into(),
                    }],
                }],
            })
            .expect("import");

        // M-A: the imported bucket is handed to the JS thread when a page on
        // the SAME origin is injected — not to whatever origin loads next.
        let page = Page::from_html(
            Url::parse("https://example.com/x").unwrap(),
            "<html><body></body></html>",
            200,
            "text/html".into(),
        )
        .await
        .unwrap();
        session.inject_dom_snapshot_for_test(page).await;
        let r = session
            .evaluate_js("localStorage.getItem('token')")
            .await
            .expect("evaluate");
        assert_eq!(
            r.value,
            Some(serde_json::json!("t0k3n")),
            "imported localStorage must be visible on its own origin"
        );

        // And must NOT bleed onto a different (opaque) origin.
        session.navigate("about:blank").await.expect("navigate");
        let r = session
            .evaluate_js("localStorage.getItem('token')")
            .await
            .expect("evaluate");
        assert_eq!(
            r.value,
            Some(serde_json::Value::Null),
            "imported origin's localStorage must not bleed across origins"
        );
    }

    #[tokio::test]
    async fn test_import_state_merges_into_per_origin_buckets() {
        use crate::storage_state::{LocalStorageEntry, OriginState, StorageState};
        let mut session = make_session().await;
        session
            .navigate("about:blank")
            .await
            .expect("first navigate");
        session
            .evaluate_js("localStorage.setItem('a', '1'); localStorage.setItem('c', '1');")
            .await
            .expect("seed existing storage");

        // Import lands in the https://example.com bucket — it must not touch
        // the opaque ("null") bucket the about: page writes into.
        session
            .import_state(&StorageState {
                cookies: vec![],
                origins: vec![OriginState {
                    origin: "https://example.com".into(),
                    local_storage: vec![
                        LocalStorageEntry {
                            name: "a".into(),
                            value: "2".into(),
                        },
                        LocalStorageEntry {
                            name: "b".into(),
                            value: "3".into(),
                        },
                    ],
                }],
            })
            .expect("import");

        // Revisiting about: swaps back to the "null" bucket: existing values
        // persist, imported keys are absent (no cross-origin bleeding).
        session
            .navigate("about:blank")
            .await
            .expect("second navigate");
        let r = session
            .evaluate_js(
                "(function(){ return { a: localStorage.getItem('a'), \
                 b: localStorage.getItem('b'), c: localStorage.getItem('c') }; })()",
            )
            .await
            .expect("evaluate");
        let o = r.value.expect("object result");
        assert_eq!(
            o["a"],
            serde_json::json!("1"),
            "opaque-origin bucket is untouched by the import"
        );
        assert_eq!(
            o["b"],
            serde_json::Value::Null,
            "imported keys must not bleed across origins"
        );
        assert_eq!(o["c"], serde_json::json!("1"));

        // Loading the imported origin serves that origin's own bucket.
        let page = Page::from_html(
            Url::parse("https://example.com/x").unwrap(),
            "<html><body></body></html>",
            200,
            "text/html".into(),
        )
        .await
        .unwrap();
        session.inject_dom_snapshot_for_test(page).await;
        let r = session
            .evaluate_js(
                "(function(){ return { a: localStorage.getItem('a'), \
                 b: localStorage.getItem('b'), c: localStorage.getItem('c') }; })()",
            )
            .await
            .expect("evaluate");
        let o = r.value.expect("object result");
        assert_eq!(o["a"], serde_json::json!("2"), "import overwrote the key");
        assert_eq!(o["b"], serde_json::json!("3"), "import added the key");
        assert_eq!(
            o["c"],
            serde_json::Value::Null,
            "other origin's keys stay partitioned"
        );
    }


    #[tokio::test]
    async fn test_import_state_two_origins_route_to_own_buckets() {
        use crate::storage_state::{LocalStorageEntry, OriginState, StorageState};
        let mut session = make_session().await;
        session
            .import_state(&StorageState {
                cookies: vec![],
                origins: vec![
                    OriginState {
                        origin: "https://a.test".into(),
                        local_storage: vec![LocalStorageEntry {
                            name: "ka".into(),
                            value: "va".into(),
                        }],
                    },
                    OriginState {
                        origin: "https://b.test".into(),
                        local_storage: vec![LocalStorageEntry {
                            name: "kb".into(),
                            value: "vb".into(),
                        }],
                    },
                ],
            })
            .expect("import");

        // No page open: direct access routes to the LAST imported origin.
        assert_eq!(
            session.get_local_storage("kb").as_deref(),
            Some("vb"),
            "last imported origin is the fallback route"
        );
        assert_eq!(
            session.get_local_storage("ka"),
            None,
            "the other origin's keys must not be visible through b.test"
        );

        // Each page origin serves exactly its own bucket via JS.
        for (page_url, own_key, own_val, other_key) in [
            ("https://a.test/", "ka", "va", "kb"),
            ("https://b.test/", "kb", "vb", "ka"),
        ] {
            let page = Page::from_html(
                Url::parse(page_url).unwrap(),
                "<html><body></body></html>",
                200,
                "text/html".into(),
            )
            .await
            .unwrap();
            session.inject_dom_snapshot_for_test(page).await;
            let r = session
                .evaluate_js(&format!(
                    "(function(){{ return {{ own: localStorage.getItem('{own_key}'), \
                     other: localStorage.getItem('{other_key}') }}; }})()"
                ))
                .await
                .expect("evaluate");
            let o = r.value.expect("object result");
            assert_eq!(o["own"], serde_json::json!(own_val));
            assert_eq!(
                o["other"],
                serde_json::Value::Null,
                "buckets stay partitioned per origin"
            );
        }
    }

    #[tokio::test]
    async fn test_local_storage_persists_across_same_origin_revisit() {
        let mut session = make_session().await;
        session
            .navigate("data:text/html,<h1>one</h1>")
            .await
            .expect("data nav");
        session
            .evaluate_js("localStorage.setItem('k', 'v')")
            .await
            .expect("setItem");

        // Re-navigate (same opaque origin → "null" bucket): the JS-side map
        // is re-seeded from the bucket, so the value survives — with NO wait
        // between the write and the navigation (the drain barrier guarantees
        // the sync thread applied the write before the seed is read).
        session
            .navigate("data:text/html,<h1>two</h1>")
            .await
            .expect("second data nav");
        let r = session
            .evaluate_js("localStorage.getItem('k')")
            .await
            .expect("evaluate");
        assert_eq!(
            r.value,
            Some(serde_json::json!("v")),
            "same-origin revisit must keep localStorage"
        );
    }

    /// F2 (a): a `setItem` immediately followed by a cross-origin navigation
    /// must land in the WRITING page's origin bucket (messages carry their
    /// origin), never in the new page's bucket. No polling/wait: the write is
    /// still in flight when the navigation starts.
    #[tokio::test]
    async fn test_local_storage_write_immediately_before_nav_lands_in_own_bucket() {
        let mut session = make_session().await;
        let page = Page::from_html(
            Url::parse("https://write.test/").unwrap(),
            "<html><body></body></html>",
            200,
            "text/html".into(),
        )
        .await
        .unwrap();
        session.inject_dom_snapshot_for_test(page).await;
        session
            .evaluate_js("localStorage.setItem('secret', 's1')")
            .await
            .expect("setItem");

        // Immediate cross-origin navigation (opaque origin, no network wait).
        session.navigate("about:blank").await.expect("nav");

        let map = session.local_storage.read().clone();
        assert_eq!(
            map.get("https://write.test").and_then(|b| b.get("secret")).map(String::as_str),
            Some("s1"),
            "write must land in the writing origin's bucket"
        );
        assert!(
            !map.get("null").is_some_and(|b| b.contains_key("secret")),
            "write must not bleed into the new page's opaque bucket"
        );
    }

    /// F2 (b): a `setItem` immediately before navigation must survive a
    /// same-origin revisit — the drain barrier ensures the pending write is
    /// applied before the navigation seeds the JS-side map from the bucket.
    #[tokio::test]
    async fn test_local_storage_write_immediately_before_nav_survives_revisit() {
        let mut session = make_session().await;
        let mk_page = || async {
            Page::from_html(
                Url::parse("https://write.test/").unwrap(),
                "<html><body></body></html>",
                200,
                "text/html".into(),
            )
            .await
            .unwrap()
        };
        session.inject_dom_snapshot_for_test(mk_page().await).await;
        session
            .evaluate_js("localStorage.setItem('k', 'v2')")
            .await
            .expect("setItem");

        // Immediate away + back, no waits anywhere.
        session.navigate("about:blank").await.expect("away nav");
        session
            .inject_dom_snapshot_for_test(mk_page().await)
            .await;
        let r = session
            .evaluate_js("localStorage.getItem('k')")
            .await
            .expect("evaluate");
        assert_eq!(
            r.value,
            Some(serde_json::json!("v2")),
            "pending write must be applied before the revisit seeds storage"
        );
    }

    /// `DOWNLOAD_DIR` is a process-wide static — tests that touch it
    /// (override set/clear or the config-fallback path) hold this guard.
    static DOWNLOAD_DIR_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn test_download_save_failure_emits_download_failed() {
        let mut session = make_session().await;
        let _guard = DOWNLOAD_DIR_GUARD.lock().await;
        let (tx, rx) = std::sync::mpsc::channel();
        session.set_event_sink(tx);

        // A path under /dev/null can never hold a directory → save must fail.
        set_download_behavior(Some(std::path::PathBuf::from("/dev/null/oxi-dl-fail")));
        let url = Url::parse("https://example.com/file.bin").unwrap();
        let result = session.handle_download(&url, "attachment; filename=file.bin", b"data");
        set_download_behavior(None);
        drop(_guard);

        let err = result.expect_err("unwritable target must fail the save");
        assert!(
            matches!(err, CoreError::NetworkError(_)),
            "expected NetworkError, got {err:?}"
        );
        match rx.try_recv().expect("DownloadFailed event must be emitted") {
            crate::js::CoreEvent::DownloadFailed { url, error, .. } => {
                assert!(
                    url.contains("file.bin"),
                    "event carries the source URL: {url}"
                );
                assert!(!error.is_empty(), "event carries the I/O error");
            }
            other => panic!("expected DownloadFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_download_uses_config_download_dir_when_override_unset() {
        let mut session = make_session().await;
        let _guard = DOWNLOAD_DIR_GUARD.lock().await;
        // No CDP override: the config field resolves the target directory.
        let dir = std::env::temp_dir().join(format!("oxi-dl-cfg-{}", uuid::Uuid::new_v4()));
        session.config.download_dir = Some(dir.clone());
        set_download_behavior(None);
        let (tx, rx) = std::sync::mpsc::channel();
        session.set_event_sink(tx);

        let url = Url::parse("https://example.com/report.pdf").unwrap();
        session
            .handle_download(&url, "attachment; filename=report.pdf", b"pdf-bytes")
            .expect("save with config download_dir");

        let saved = dir.join("report.pdf");
        assert_eq!(
            std::fs::read(&saved).expect("file saved to config dir"),
            b"pdf-bytes",
            "saved content matches"
        );
        match rx.try_recv().expect("Download event must be emitted") {
            crate::js::CoreEvent::Download {
                filename,
                save_path,
                total_bytes,
                ..
            } => {
                assert_eq!(filename, "report.pdf");
                assert_eq!(save_path, saved.to_string_lossy());
                assert_eq!(total_bytes, 9);
            }
            other => panic!("expected Download, got {other:?}"),
        }
        let _ = std::fs::remove_file(&saved);
        let _ = std::fs::remove_dir(&dir);
    }

    #[tokio::test]
    async fn test_navigate_to_attachment_downloads_file() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/file"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-disposition", "attachment; filename=\"hello.txt\"")
                    .set_body_string("downloaded-body"),
            )
            .mount(&server)
            .await;

        let dir = std::env::temp_dir().join(format!("oxi-dl-{}", uuid::Uuid::new_v4()));
        let _guard = DOWNLOAD_DIR_GUARD.lock().await;
        set_download_behavior(Some(dir.clone()));

        let mut session = make_session().await;
        let url = format!("{}/file", server.uri());
        session.navigate(&url).await.expect("navigate");

        let saved = dir.join("hello.txt");
        assert!(saved.exists(), "download file should exist at {saved:?}");
        assert_eq!(
            std::fs::read_to_string(&saved).unwrap(),
            "downloaded-body",
            "saved content should match the response body"
        );
        set_download_behavior(None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_iframe_population() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    "<html><body><iframe src=\"/child.html\"></iframe></body></html>",
                ),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/child.html"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/html")
                    .set_body_string("<html><body><p>inside-iframe</p></body></html>"),
            )
            .mount(&server)
            .await;

        let mut session = make_session().await;
        session
            .navigate(&format!("{}/", server.uri()))
            .await
            .expect("navigate");

        let children = session.page().expect("page").root_frame().children();
        assert_eq!(children.len(), 1, "iframe should populate one child frame");
        let has_text = children[0]
            .document()
            .nodes
            .values()
            .any(|n| n.text_content.contains("inside-iframe"));
        assert!(
            has_text,
            "child frame should contain the fetched iframe content"
        );
    }

    #[tokio::test]
    async fn test_viewport_override_applies_to_layout() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "<html><body><div style=\"width:100%;height:10px\"></div></body></html>",
            ))
            .mount(&server)
            .await;

        set_viewport_override(500, 400);
        let mut session = make_session().await;
        session
            .navigate(&format!("{}/", server.uri()))
            .await
            .expect("navigate");
        let png = session.capture_screenshot_png(500).await.expect("capture");
        clear_viewport_override();

        let img = image::load_from_memory(&png).expect("decode");
        // Full-page capture reflects the laid-out content width (≥ override width).
        assert!(
            img.width() >= 500,
            "viewport override should drive layout width, got {}",
            img.width()
        );
    }

    // multi_thread: the test blocks on a std mpsc recv_timeout while a
    // spawned task must run — a current-thread runtime would starve it.
    // Single test fn: the two scenarios share the process-wide FETCH_PATTERNS
    // static, so they MUST run serially (parallel tests would race on it).
    #[tokio::test(flavor = "multi_thread")]
    async fn test_maybe_intercept_js_fetch_interception() {
        use crate::js::CoreEvent;
        use crate::network::intercept::{InterceptAction, shared_registry};

        // --- Fulfill scenario: matching pattern pauses, decision resolves. ---
        set_fetch_patterns(vec!["http://example.com".to_string()]);
        let (tx, rx) = std::sync::mpsc::channel::<CoreEvent>();
        let event_tx = std::sync::Arc::new(parking_lot::RwLock::new(Some(tx)));

        let task_tx = event_tx.clone();
        let task = tokio::spawn(async move {
            maybe_intercept(&task_tx, 7, "http://example.com/api", "GET", &[]).await
        });

        // The bridge must emit a RequestPaused event carrying the pause id.
        let ev = rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("RequestPaused event");
        let pause_id = match ev {
            CoreEvent::RequestPaused { request_id, .. } => request_id,
            other => panic!("expected RequestPaused, got {other:?}"),
        };

        // Resolve the paused request with a Fulfill.
        let paused = shared_registry()
            .take(&pause_id)
            .expect("paused request in registry");
        paused
            .tx
            .send(InterceptAction::Fulfill {
                status_code: 200,
                status_text: "OK".to_string(),
                headers: vec![],
                body: b"mock-body".to_vec(),
            })
            .unwrap();

        let decision = task.await.expect("task");
        match decision {
            InterceptDecision::Respond(msg) => {
                assert_eq!(msg.id, 7);
                assert_eq!(msg.status, 200);
                assert_eq!(msg.body, "mock-body");
            }
            other => panic!("expected Respond, got {other:?}"),
        }

        // --- Empty-pattern fast path: no pause, proceeds unchanged. ---
        set_fetch_patterns(vec![]);
        let decision = maybe_intercept(&event_tx, 3, "http://example.com/api", "GET", &[]).await;
        match decision {
            InterceptDecision::Proceed { url, .. } => {
                assert_eq!(url.as_str(), "http://example.com/api");
            }
            other => panic!("expected Proceed with empty patterns, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_iframe_creates_child_execution_context() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<html><body>
                        <h1 id="main-title">main-page</h1>
                        <iframe src="/child.html"></iframe>
                       </body></html>"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/child.html"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/html")
                    .set_body_string(
                        r#"<html><body>
                            <p id="iframe-text">inside-iframe-content</p>
                            <script>window.__iframeVar = 42;</script>
                           </body></html>"#,
                    ),
            )
            .mount(&server)
            .await;

        let mut session = make_session().await;
        session
            .navigate(&format!("{}/", server.uri()))
            .await
            .expect("navigate");

        // The frame_contexts map should contain one child entry.
        let frame_map = session.frame_context_map().read().clone();
        assert_eq!(
            frame_map.len(),
            1,
            "one child iframe should have a context: {frame_map:?}"
        );
        let (_child_frame_id, &child_context_id) = frame_map.iter().next().unwrap();
        assert!(
            child_context_id >= 2,
            "child context_id should be ≥ 2, got {child_context_id}"
        );

        // Evaluate in the child frame context: query the iframe's DOM.
        let r = session
            .evaluate_js_in_context(
                "document.getElementById('iframe-text').textContent",
                child_context_id,
                false,
            )
            .await
            .expect("evaluate in child context");
        assert_eq!(
            r.value,
            Some(serde_json::json!("inside-iframe-content")),
            "child context eval should return the iframe's DOM text"
        );

        // The iframe's script should have executed (window.__iframeVar = 42).
        let r2 = session
            .evaluate_js_in_context("window.__iframeVar", child_context_id, false)
            .await
            .expect("evaluate iframe var");
        assert_eq!(
            r2.value,
            Some(serde_json::json!(42)),
            "child frame scripts should execute in their own context"
        );

        // Main frame should NOT see the iframe's global (isolation).
        let r3 = session
            .evaluate_js("typeof window.__iframeVar")
            .await
            .expect("evaluate in main context");
        assert_eq!(
            r3.value,
            Some(serde_json::json!("undefined")),
            "main frame should be isolated from child frame globals"
        );
    }

    /// W4a: document + sub-resource fetches land in the session request log
    /// with real status/MIME, emit `SubresourceFetch*` core events, and
    /// serialize to a HAR 1.2 document with one entry per request.
    #[tokio::test]
    async fn test_network_log_records_documents_and_subresources_with_events() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // NOTE: the <img> uses an ABSOLUTE url on purpose. blitz-html eagerly
        // resolves relative sub-resource URLs during parsing against its
        // placeholder `data:` base URL (RenderDocument::from_html sets the
        // real base URL only after parsing), so a relative <img src> panics
        // inside Page::from_html — a pre-existing render-layer bug, unrelated
        // to the network log. Absolute URLs resolve against any base.
        let img_url = format!("{}/img.png", server.uri());
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(
                    format!(
                        r#"<html><head></head><body><img src="{img_url}"><p>hi</p></body></html>"#,
                    )
                    .into_bytes(),
                    "text/html; charset=utf-8",
                ),
            )
            .mount(&server)
            .await;
        let png_magic: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        Mock::given(method("GET"))
            .and(path("/img.png"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(png_magic.clone(), "image/png"))
            .mount(&server)
            .await;

        let mut session = make_session().await;
        let (tx, rx) = std::sync::mpsc::channel::<crate::js::CoreEvent>();
        session.set_event_sink(tx);

        session
            .navigate(&format!("{}/", server.uri()))
            .await
            .expect("navigate");
        let loaded = session.load_sub_resources().await;
        assert_eq!(loaded, 1, "one image sub-resource should load");

        // Request log: document + image records with real status/MIME.
        let log = session.network_log_snapshot();
        assert_eq!(log.len(), 2, "document + image records, got: {log:?}");
        let doc = &log[0];
        assert_eq!(
            doc.request_id, "req-1",
            "document ids use the req- namespace"
        );
        assert_eq!(doc.method, "GET");
        assert_eq!(doc.resource_type, "Document");
        assert_eq!(doc.status, Some(200));
        assert_eq!(doc.mime_type, "text/html");
        assert!(doc.finished_at_ms.is_some());
        assert!(
            doc.response_body_length.unwrap_or(0) > 0,
            "document body length should be observed"
        );
        assert!(!doc.from_cache, "no cache exists");
        let img = &log[1];
        assert_eq!(
            img.request_id, "res-1",
            "sub-resource ids use the res- namespace"
        );
        assert_eq!(img.resource_type, "Image");
        assert_eq!(img.status, Some(200));
        assert_eq!(img.mime_type, "image/png");
        assert_eq!(img.response_body_length, Some(png_magic.len() as u64));

        // Subresource events reached the event sink.
        let mut saw_request = false;
        let mut saw_response = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                crate::js::CoreEvent::SubresourceFetchRequest {
                    request_id,
                    url,
                    method,
                    resource_type,
                    ..
                } => {
                    saw_request = true;
                    assert_eq!(request_id, "res-1");
                    assert_eq!(method, "GET");
                    assert_eq!(resource_type, "Image");
                    assert_eq!(url, img_url);
                }
                crate::js::CoreEvent::SubresourceFetchResponse {
                    request_id,
                    status,
                    mime_type,
                    length,
                    ..
                } => {
                    saw_response = true;
                    assert_eq!(request_id, "res-1");
                    assert_eq!(status, 200);
                    assert_eq!(mime_type, "image/png");
                    assert_eq!(length, png_magic.len() as u64);
                }
                _ => {}
            }
        }
        assert!(saw_request, "SubresourceFetchRequest event missing");
        assert!(saw_response, "SubresourceFetchResponse event missing");

        // HAR 1.2 export: one entry per request, real response metadata.
        let har = session.network_log_har();
        assert_eq!(har["log"]["version"], "1.2");
        assert_eq!(har["log"]["creator"]["name"], "oxibrowser");
        let entries = har["log"]["entries"].as_array().expect("entries array");
        assert_eq!(entries.len(), 2, "one HAR entry per request");
        assert_eq!(entries[0]["request"]["url"], doc.url);
        assert_eq!(entries[0]["response"]["status"], 200);
        assert_eq!(entries[0]["response"]["content"]["mimeType"], "text/html");
        assert_eq!(entries[1]["request"]["url"], img.url);
        assert_eq!(entries[1]["response"]["content"]["mimeType"], "image/png");
        assert_eq!(
            entries[1]["response"]["content"]["size"],
            png_magic.len() as i64
        );

        // clear_network_log empties the log (and the HAR export).
        session.clear_network_log();
        assert!(session.network_log_snapshot().is_empty());
    }

    /// W4a: JS-issued `fetch` requests flow through the bridge with the
    /// session's request overrides applied on the wire, and land in the
    /// request log under the `oxi-{n}` id namespace (matching the
    /// `Network.requestWillBeSent` events minted on the JS thread).
    #[tokio::test]
    async fn test_js_fetch_bridge_applies_overrides_and_logs() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // Only reachable with the override UA — a 200 proves the override
        // reached the wire through the fetch bridge.
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(b"<html><body></body></html>".to_vec(), "text/html"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(header("user-agent", "BridgeUA/1.0"))
            .and(path("/api"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(b"{\"ok\":true}".to_vec(), "application/json"),
            )
            .mount(&server)
            .await;

        let mut session = make_session().await;
        session.set_overrides(RequestOverrides {
            user_agent: Some("BridgeUA/1.0".into()),
            extra_headers: vec![("X-Test".to_string(), "1".to_string())],
        });
        session
            .navigate(&format!("{}/", server.uri()))
            .await
            .expect("navigate");

        let body = session
            .evaluate_js_with_await(
                // NOTE: the runtime's fetch() only forwards an allowlist of
                // header names (content-type/accept/authorization/user-agent/
                // cookie); use one of those so the header reaches the bridge.
                "fetch('/api', {headers: {'accept': 'application/json'}}).then(r => r.text())",
                true,
            )
            .await
            .expect("js fetch through bridge");
        assert_eq!(body.value, Some(serde_json::json!("{\"ok\":true}")));

        // The bridge record: oxi-1 (first JS fetch id), finished with the
        // real response metadata.
        let log = session.network_log_snapshot();
        let fetch_record = log
            .iter()
            .find(|r| r.request_id == "oxi-1")
            .expect("js fetch record in network log");
        assert_eq!(fetch_record.method, "GET");
        assert_eq!(fetch_record.resource_type, "Fetch");
        assert!(
            fetch_record.url.ends_with("/api"),
            "url: {}",
            fetch_record.url
        );
        assert_eq!(fetch_record.status, Some(200));
        assert_eq!(fetch_record.mime_type, "application/json");
        assert_eq!(
            fetch_record.response_body_length,
            Some(b"{\"ok\":true}".len() as u64)
        );
        assert!(fetch_record.finished_at_ms.is_some());
        assert!(
            fetch_record
                .request_headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("accept") && v == "application/json"),
            "JS request headers forwarded to the log, got: {:?}",
            fetch_record.request_headers
        );
    }
}
