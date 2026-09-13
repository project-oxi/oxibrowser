#![allow(clippy::arc_with_non_send_sync)]
//! JavaScript runtime using boa_engine with a persistent context.
//!
//! boa_engine is a pure Rust JavaScript engine (ES2024+), no C dependencies.
//!
//! ## Architecture
//!
//! `boa_engine::Context` is `!Send` (internal GC pointers use `NonNull`).
//! To keep `JsRuntime: Send + Sync` for tokio, we run the `Context` on a
//! dedicated **std::thread** and communicate via `mpsc` channels.
//!
//! ```text
//! main thread (async)          JS thread (sync, std::thread)
//! ┌─────────────────┐          ┌──────────────────┐
//! │ JsRuntime        │──send──→│ Context (영구)    │
//! │  evaluate()     │          │  console.log 등록 │
//! │  set_global()   │          │  eval(script)     │
//! │  set_dom()      │          │  document 객체    │
//! │  console_output  │←─recv──│  json_value 반환  │
//! └─────────────────┘          └──────────────────┘
//! ```
//!
//! This means JS state (variables, functions, closures) **persists across
//! evaluate() calls** — exactly like a real browser.

use parking_lot::{Mutex, RwLock};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};

use base64::Engine;
use boa_engine::builtins::promise::ResolvingFunctions;
use boa_engine::object::builtins::{JsArray, JsPromise};
use boa_engine::object::{FunctionObjectBuilder, JsObject};
use boa_engine::property::Attribute;
use boa_engine::{Context, JsString, JsValue, NativeFunction, Source, js_string};
use serde_json::Value;

use crate::css::LayoutEngine;
use crate::error::{CoreError, Result};
use crate::js::dom_snapshot::{DomMutation, DomNode, DomSnapshot, ScriptSource};
use crate::js::job_queue::TokioJobQueue;
use crate::network::cookie::CookieJar;
use crate::network::ws::{WsData, WsEvent};
use oxibrowser_render::{CaptureOpts, RenderDocument, RenderError, Viewport};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Global counter for unique node IDs, avoids collisions in tight loops.
/// Starts at 1_000_000 to stay above any parsed DOM snapshot IDs.
static NEXT_NODE_ID: AtomicU64 = AtomicU64::new(1_000_000);

// ── Thread-local listener registry ─────────────────────────────────────────
// Event listeners keyed by node_id → event_type → callbacks.
// Thread-local because boa `Context` is !Send — every closure runs on the
// same JS thread. This lets the bubbling walk find listeners registered via
// any element object, regardless of object identity (each DOM query mints a
// fresh JS object, so `__listeners` on one instance is invisible to another).
thread_local! {
    #[allow(clippy::type_complexity)]
    static LISTENER_REGISTRY: RefCell<HashMap<(u32, u32), HashMap<String, Vec<JsObject>>>> =
        RefCell::new(HashMap::new());
}

thread_local! {
    /// `document.readyState`. Transitions "loading" → "interactive" →
    /// "complete" during navigation script execution (Phase 1). Defaults to
    /// "complete" so legacy `set_document` (no scripts) reports a ready doc.
    static DOC_READY_STATE: std::cell::Cell<&'static str> =
        const { std::cell::Cell::new("complete") };
}

// ── Async fetch / XHR pending registry (Phase 3) ───────────────────────────
// Holds everything needed to settle an in-flight fetch/XHR when its response
// arrives on the shared response channel. Thread-local because boa GC values
// (`ResolvingFunctions`, `JsValue` callbacks) are `!Send` — they live on the JS
// thread, where `drain_pending_fetch_responses` runs. Same rooting model as the
// listener registry above.
enum PendingFetch {
    Fetch {
        resolvers: ResolvingFunctions,
        /// Optional AbortSignal. Polled by the drain pass; when `.aborted`
        /// becomes true the promise is rejected with an AbortError.
        signal: Option<JsObject>,
        /// Owning execution context (frame). Responses are settled only when
        /// their owning context is the active one (Phase 8).
        context_id: u32,
    },
    Xhr {
        ready_state: Arc<RwLock<f64>>,
        status: Arc<RwLock<f64>>,
        resp_body: Arc<RwLock<String>>,
        resp_hdrs: Arc<RwLock<String>>,
        onload: Arc<RwLock<Option<JsValue>>>,
        onerror: Arc<RwLock<Option<JsValue>>>,
        onrsc: Arc<RwLock<Option<JsValue>>>,
        /// Owning execution context (frame).
        context_id: u32,
    },
}

impl PendingFetch {
    fn context_id(&self) -> u32 {
        match self {
            PendingFetch::Fetch { context_id, .. } | PendingFetch::Xhr { context_id, .. } => {
                *context_id
            }
        }
    }
}

/// JS-side per-socket WebSocket state. The live JS object owns all properties
/// (url, readyState, protocol, on*, hidden `__listeners_<type>` arrays); this
/// wrapper exists only to track liveness for the idle condition
/// (`pending_ws = any non-Closed`).
enum WsState {
    Live { obj: JsObject, context_id: u32 },
    Closed,
}

impl WsState {
    fn context_id(&self) -> Option<u32> {
        match self {
            WsState::Live { context_id, .. } => Some(*context_id),
            WsState::Closed => None,
        }
    }
}

/// JS→bridge WebSocket request, id-keyed (mirrors `FetchRequestMsg`).
pub enum WsReqMsg {
    Connect {
        id: u64,
        url: String,
        protocols: Vec<String>,
    },
    Send {
        id: u64,
        data: WsData,
    },
    Close {
        id: u64,
        code: Option<u16>,
        reason: Option<String>,
    },
}

thread_local! {
    static PENDING_FETCH: RefCell<HashMap<u64, PendingFetch>> =
        RefCell::new(HashMap::new());
}

thread_local! {
    static NEXT_FETCH_ID: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
}

thread_local! {
    /// The shared fetch response receiver, installed by `SetFetchChannel`.
    static RESPONSE_RX: RefCell<Option<Receiver<FetchResponseMsg>>> =
        const { RefCell::new(None) };
}

thread_local! {
    /// Fetch responses received during another context's pump but belonging to
    /// a different (non-active) frame. Buffered and retried when the owning
    /// context next pumps (Phase 8 per-frame isolation).
    static DEFERRED_RESPONSES: RefCell<Vec<FetchResponseMsg>> = const { RefCell::new(Vec::new()) };
}

thread_local! {
    /// The current page origin (`scheme://host[:port]`), updated on every
    /// navigation. Read by the fetch native so cross-origin requests carry an
    /// `Origin` header and a `Referer` (CORS / referrer policy).
    static CURRENT_ORIGIN: RefCell<Option<String>> = const { RefCell::new(None) };
}

// ── Per-frame execution context tracking (Phase 8) ────────────────────────
// The JS thread processes one frame's Context at a time. This cell records
// *which* context is currently active, so thread-local registries (listeners,
// pending fetch/WS) can tag/namespace their entries. Defaults to 1 (main
// frame); the loop sets it before entering any frame's eval/pump.
thread_local! {
    static ACTIVE_CONTEXT_ID: std::cell::Cell<u32> = const { std::cell::Cell::new(1) };
}

/// Read the currently-active execution context id (set by `js_thread_loop`
/// before entering a frame's `Context`). Used to namespace thread-local
/// registries so per-frame entries never collide.
fn active_context_id() -> u32 {
    ACTIVE_CONTEXT_ID.with(|c| c.get())
}

/// Set the current page origin (called from the SetDocument/SetPageUrl
/// handlers). `page_url` is the full document URL; the origin is derived.
fn set_current_origin(page_url: &str) {
    let origin = url::Url::parse(page_url)
        .ok()
        .map(|u| u.origin().ascii_serialization())
        .filter(|s| !s.is_empty() && !s.starts_with("null"));
    CURRENT_ORIGIN.with(|c| *c.borrow_mut() = origin);
}

/// Read the current page origin for request-context headers (CORS/Referer).
fn current_origin() -> Option<String> {
    CURRENT_ORIGIN.with(|c| c.borrow().clone())
}

// ---------------------------------------------------------------------------
// Emulation overrides (geolocation / timezone) — cross-thread so the CDP layer
// (Emulation domain, async main thread) can set them while the JS thread reads.
// ---------------------------------------------------------------------------

/// Geolocation override coordinates: `(latitude, longitude, accuracy_meters)`.
static GEOLOCATION_OVERRIDE: std::sync::LazyLock<parking_lot::RwLock<Option<(f64, f64, f64)>>> =
    std::sync::LazyLock::new(|| parking_lot::RwLock::new(None));

/// Install a geolocation override consumed by `navigator.geolocation.getCurrentPosition`.
pub fn set_geolocation_override(lat: f64, lon: f64, accuracy: f64) {
    *GEOLOCATION_OVERRIDE.write() = Some((lat, lon, accuracy));
}

/// Clear the geolocation override.
pub fn clear_geolocation_override() {
    *GEOLOCATION_OVERRIDE.write() = None;
}

/// Read the geolocation override (used by the JS geolocation API).
fn geolocation_override() -> Option<(f64, f64, f64)> {
    *GEOLOCATION_OVERRIDE.read()
}

/// Timezone override (IANA name, e.g. `America/New_York`).
static TIMEZONE_OVERRIDE: std::sync::LazyLock<parking_lot::RwLock<Option<String>>> =
    std::sync::LazyLock::new(|| parking_lot::RwLock::new(None));

/// Install a timezone override consumed by `Intl`/`Date`.
pub fn set_timezone_override(tz: &str) {
    *TIMEZONE_OVERRIDE.write() = Some(tz.to_string());
}

/// Clear the timezone override.
pub fn clear_timezone_override() {
    *TIMEZONE_OVERRIDE.write() = None;
}

/// Read the timezone override, falling back to the detected system timezone.
pub(crate) fn effective_timezone() -> String {
    TIMEZONE_OVERRIDE
        .read()
        .clone()
        .unwrap_or_else(detect_system_timezone)
}

/// Detect the system IANA timezone (TZ env, /etc/localtime, fallback UTC).
fn detect_system_timezone() -> String {
    if let Ok(tz) = std::env::var("TZ")
        && tz.contains('/')
    {
        return tz;
    }
    if let Ok(target) = std::fs::read_link("/etc/localtime") {
        let s = target.to_string_lossy().into_owned();
        if let Some(idx) = s.rfind("zoneinfo/") {
            let tail = &s[idx + "zoneinfo/".len()..];
            if tail.contains('/') {
                return tail.to_string();
            }
        }
    }
    "UTC".to_string()
}

/// Mint a fresh fetch request id on the JS thread (never 0).
fn next_fetch_id() -> u64 {
    NEXT_FETCH_ID.with(|c| {
        let id = c.get();
        c.set(id.wrapping_add(1));
        id
    })
}
thread_local! {
    /// Per-socket JS state + callbacks, keyed by id. Mirrors `PENDING_FETCH`.
    static PENDING_WS: RefCell<HashMap<u64, WsState>> =
        RefCell::new(HashMap::new());
    /// Shared WS event receiver (background→JS, id-routed), installed by
    /// `SetWsChannel`.
    static WS_EVENT_RX: RefCell<Option<Receiver<WsEvent>>> =
        const { RefCell::new(None) };
    /// Shared WS request sender (JS→bridge): Connect/Send/Close.
    static WS_REQ_TX: RefCell<Option<Sender<WsReqMsg>>> =
        const { RefCell::new(None) };
    static NEXT_WS_ID: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
}

thread_local! {
    /// WS events received during another context's pump but belonging to a
    /// different frame. Buffered and retried when the owning context next pumps.
    static DEFERRED_WS_EVENTS: RefCell<Vec<WsEvent>> = const { RefCell::new(Vec::new()) };
}

/// Mint a fresh WebSocket id on the JS thread (never 0).
fn next_ws_id() -> u64 {
    NEXT_WS_ID.with(|c| {
        let id = c.get();
        c.set(id.wrapping_add(1));
        id
    })
}

/// Register a listener callback for a node in the thread-local registry.
fn registry_add(node_id: u32, event_type: &str, callback: JsObject) {
    let key = (active_context_id(), node_id);
    LISTENER_REGISTRY.with(|r| {
        r.borrow_mut()
            .entry(key)
            .or_default()
            .entry(event_type.to_string())
            .or_default()
            .push(callback);
    });
}

/// Get all callbacks for a node + event type (cloned out to release the borrow
/// before calling them — callbacks may themselves call addEventListener).
fn registry_get(node_id: u32, event_type: &str) -> Vec<JsObject> {
    let key = (active_context_id(), node_id);
    LISTENER_REGISTRY.with(|r| {
        r.borrow()
            .get(&key)
            .and_then(|m| m.get(event_type))
            .cloned()
            .unwrap_or_default()
    })
}

/// Remove all callbacks for a node + event type.
fn registry_remove(node_id: u32, event_type: &str) {
    let key = (active_context_id(), node_id);
    LISTENER_REGISTRY.with(|r| {
        if let Some(m) = r.borrow_mut().get_mut(&key) {
            m.remove(event_type);
        }
    })
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Result of a JavaScript evaluation.
#[derive(Debug, Clone)]
pub struct JsEvalResult {
    /// The return value as a JSON value (if any).
    pub value: Option<Value>,
    /// Exception message (if an error occurred).
    pub exception: Option<String>,
    /// Console output captured during execution.
    pub console_output: Vec<String>,
    /// Whether the evaluation was aborted due to a timeout.
    /// When true, the JS context was reset and previous state (variables, etc.) is lost.
    pub timed_out: bool,
}

impl JsEvalResult {
    /// Create a successful result with a value.
    pub fn ok(value: Value) -> Self {
        Self {
            value: Some(value),
            exception: None,
            console_output: Vec::new(),
            timed_out: false,
        }
    }

    /// Create a result with no return value (void/undefined).
    pub fn void() -> Self {
        Self {
            value: None,
            exception: None,
            console_output: Vec::new(),
            timed_out: false,
        }
    }

    /// Create an error result.
    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            value: None,
            exception: Some(msg.into()),
            console_output: Vec::new(),
            timed_out: false,
        }
    }

    /// Create a timeout result (context was reset).
    pub fn timeout(timeout_ms: u64) -> Self {
        Self {
            value: None,
            exception: Some(format!(
                "JS execution timed out after {timeout_ms}ms — context was reset, previous state lost"
            )),
            console_output: Vec::new(),
            timed_out: true,
        }
    }

    /// Whether the evaluation succeeded (no exception).
    pub fn is_ok(&self) -> bool {
        self.exception.is_none()
    }
}

// ---------------------------------------------------------------------------
// Command / Response types (channel messages)
// ---------------------------------------------------------------------------
/// Serializable info about a node in the [`RenderDocument`], returned by the
/// async query façades. `id` is the opaque `NodeId` valid on the JS thread.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NodeInfo {
    /// Opaque node id (valid only on the JS thread's `RenderDocument`).
    pub id: usize,
    /// Lowercased tag name, or `None` for non-element nodes.
    pub tag: Option<String>,
    /// Recursive text content of the node.
    pub text: String,
    /// `(name, value)` attribute pairs (empty for non-elements / text nodes).
    pub attributes: Vec<(String, String)>,
}

/// Commands sent from the async main thread to the JS thread.
enum JsCommand {
    /// Evaluate a JS expression.
    Eval {
        /// Execution context (frame) to evaluate in. 1 = main frame (default).
        context_id: u32,
        expression: String,
        timeout_ms: Option<u64>,
        max_loop_iterations: Option<u64>,
        max_recursion: Option<usize>,
        max_stack_size: Option<usize>,
        await_promise: bool,
        response_tx: Sender<JsResponse>,
    },
    /// Set a global variable in the persistent Context.
    SetGlobal {
        name: String,
        value: Value,
        response_tx: Sender<JsResponse>,
    },
    /// Update the DOM snapshot available to `document` object.
    SetDom {
        snapshot: Box<Option<DomSnapshot>>,
        response_tx: Sender<JsResponse>,
    },
    /// Update the page URL (for window.location).
    SetPageUrl {
        url: String,
        response_tx: Sender<JsResponse>,
    },
    /// Set the fetch channel so JS can make real HTTP requests.
    SetFetchChannel {
        request_tx: std::sync::mpsc::Sender<FetchRequestMsg>,
        response_rx: std::sync::mpsc::Receiver<FetchResponseMsg>,
        response_tx: Sender<JsResponse>,
    },
    /// Install the WebSocket channels so JS can open realtime connections.
    SetWsChannel {
        request_tx: std::sync::mpsc::Sender<WsReqMsg>,
        response_rx: std::sync::mpsc::Receiver<WsEvent>,
        response_tx: Sender<JsResponse>,
    },
    /// Set the localStorage sync channel so JS operations propagate to Session.
    SetLocalStorageChannel {
        tx: std::sync::mpsc::Sender<LocalStorageMsg>,
        response_tx: Sender<JsResponse>,
    },
    /// Set the CookieJar so document.cookie can read/write real cookies.
    SetCookieJar {
        jar: Arc<RwLock<CookieJar>>,
        response_tx: Sender<JsResponse>,
    },
    /// Install the CoreEvent sink so JS-side events (console, exceptions,
    /// fetch/ws lifecycle, dialogs) flow to the CDP layer / observer.
    SetEventSink {
        tx: std::sync::mpsc::Sender<CoreEvent>,
        response_tx: Sender<JsResponse>,
    },
    /// Install the shared dialog-resolution gate (for blocking
    /// alert/confirm/prompt resolved by `Page.handleJavaScriptDialog`).
    SetDialogGate {
        gate: DialogGate,
        response_tx: Sender<JsResponse>,
    },
    /// Build (or replace) the `RenderDocument` on the JS thread from HTML.
    SetDocument {
        html: String,
        base_url: Option<String>,
        viewport: (u32, u32),
        /// @font-face webfont bytes (already fetched) to register into the
        /// RenderDocument's FontContext. Empty = no custom fonts.
        fonts: Vec<Vec<u8>>,
        /// Page `<script>` sources to execute after the document is built
        /// (inline + fetched external, in document order). Empty = legacy
        /// behavior (no script execution).
        scripts: Vec<ScriptSource>,
        /// Nav-script execution limits — much higher than the eval caps; see
        /// `JsRuntimeConfig::nav_script_*`. Carried per-command because the JS
        /// thread holds no config.
        nav_loop_limit: u64,
        nav_recursion_limit: usize,
        nav_stack_limit: usize,
        nav_timeout_ms: u64,
        response_tx: Sender<JsResponse>,
    },
    /// Capture a PNG of the current `RenderDocument`.
    Capture {
        opts: CaptureOpts,
        response_tx: Sender<JsResponse>,
    },
    /// Query all nodes matching a CSS selector against the `RenderDocument`.
    Query {
        selector: String,
        response_tx: Sender<JsResponse>,
    },
    /// Serialize the current RenderDocument to a DomSnapshot for async-side
    /// readers (CDP DOM/OXI, extract). Reflects all JS mutations.
    GetDocumentSnapshot {
        url: String,
        response_tx: Sender<JsResponse>,
    },
    /// Build a child frame's execution context: creates a new `Context` +
    /// `RenderDocument`, runs the frame's `<script>` tags, and registers it
    /// under `context_id` (Phase 8). The main frame (context_id=1) is built
    /// via `SetDocument`; this is for iframe children only.
    SetFrameDocument {
        context_id: u32,
        html: String,
        base_url: String,
        viewport: (u32, u32),
        scripts: Vec<ScriptSource>,
        nav_loop_limit: u64,
        nav_recursion_limit: usize,
        nav_stack_limit: usize,
        nav_timeout_ms: u64,
        response_tx: Sender<JsResponse>,
    },
    /// Drop all child-frame contexts (context_id > 1) on navigation.
    ClearChildContexts { response_tx: Sender<JsResponse> },
    /// Shut down the JS thread.
    Shutdown,
}

/// Responses sent from the JS thread back to the main thread.
enum JsResponse {
    /// Result of an Eval command.
    EvalResult {
        value: Option<Value>,
        exception: Option<String>,
        console_output: Vec<String>,
        timed_out: bool,
    },
    /// Ack for SetGlobal / SetDom / Shutdown.
    Done,
    /// PNG bytes returned by a `Capture` command.
    CaptureResult { png: Vec<u8> },
    /// Serialized DomSnapshot of the current RenderDocument.
    Snapshot(Box<crate::js::dom_snapshot::DomSnapshot>),
    /// Nodes returned by a `Query` command.
    QueryResult { nodes: Vec<NodeInfo> },
    /// Error from a `SetDocument` / `Capture` / `Query` command.
    Error { message: String },
}

// ---------------------------------------------------------------------------
// Fetch message types
/// A fetch request from JS.
pub struct FetchRequestMsg {
    /// Unique id minted on the JS thread; routes the response back to its
    /// pending slot in `PENDING_FETCH`. Never 0.
    pub id: u64,
    /// URL to fetch.
    pub url: String,
    /// HTTP method.
    pub method: String,
    /// Request headers (name, value pairs).
    pub headers: Vec<(String, String)>,
    /// Request body as raw bytes (UTF-8 for string bodies; arbitrary bytes
    pub body: Option<Vec<u8>>,
    /// The originating page's origin (`scheme://host[:port]`), used for CORS
    /// `Origin`/`Referer` headers. `None` when no page is loaded.
    pub origin: Option<String>,
}

/// HTTP response sent back to the JS thread via the shared response channel.
#[derive(Debug)]
pub struct FetchResponseMsg {
    /// Echoes the request `id`.
    pub id: u64,
    /// HTTP status code.
    pub status: u16,
    /// HTTP status text.
    pub status_text: String,
    /// Final URL (after redirects).
    pub url: String,
    /// Response headers.
    pub headers: Vec<(String, String)>,
    /// Response body text.
    pub body: String,
    /// Error message if request failed.
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// CoreEvent sink (core → CDP / observers)
// ---------------------------------------------------------------------------

/// A core-originated event destined for the CDP layer (or any observer).
///
/// Core cannot name CDP types, so this is a neutral enum the CDP drainer
/// translates into CDP events. Pushed from the JS thread via [`push_event`];
/// a no-op when no sink is attached (e.g. the CLI `fetch` path with no CDP).
#[derive(Debug, Clone)]
pub enum CoreEvent {
    /// `console.log/info/warn/error` call.
    Console {
        level: ConsoleLevel,
        /// Typed arguments (preserve number/boolean/object/null/undefined so
        /// the CDP layer builds proper `RemoteObject`s instead of always
        /// stringifying). See [`ConsoleArg`].
        args: Vec<ConsoleArg>,
        timestamp: f64,
    },
    /// Uncaught exception from `evaluate()` / navigation scripts.
    Exception {
        message: String,
        /// Error constructor name (e.g. `TypeError`, `RangeError`); used as the
        /// CDP `exceptionDetails.exception.className`. Defaults to `Error`.
        name: String,
        /// Best-effort `.stack` string from the thrown object, if any. boa 0.20
        /// does not surface real source locations on `JsNativeError`/`Error`,
        /// so this is typically `None` or a synthetic trace.
        stack: Option<String>,
        timestamp: f64,
    },
    /// JS-initiated `fetch()` / `XMLHttpRequest` request dispatched.
    FetchRequest {
        request_id: String,
        url: String,
        method: String,
        headers: Vec<(String, String)>,
        post_data: Option<Vec<u8>>,
        timestamp: f64,
    },
    /// `fetch` / XHR response received.
    FetchResponse {
        request_id: String,
        url: String,
        status: u16,
        mime_type: String,
        timestamp: f64,
    },
    /// `fetch` / XHR response body finished loading.
    FetchLoadingFinished { request_id: String, timestamp: f64 },
    /// WebSocket frame sent or received.
    WsFrame {
        direction: WsDirection,
        request_id: String,
        /// WebSocket opcode: `1` = text, `2` = binary.
        opcode: u8,
        /// Text payload for opcodes 1; base64 for opcode 2.
        data: String,
        timestamp: f64,
    },
    /// A JS-originated `fetch`/`XHR` request paused by Fetch-domain
    /// interception. The core fetch bridge inserts a `PausedRequest` into the
    /// shared registry under `request_id`; the CDP drainer translates this into
    /// `Fetch.requestPaused`, and the client's `continue/fail/fulfill` resolves
    /// the registry entry.
    RequestPaused {
        request_id: String,
        url: String,
        method: String,
        headers: Vec<(String, String)>,
        resource_type: String,
        timestamp: f64,
    },
    /// `alert` / `confirm` / `prompt` dialog requested by the page.
    Dialog {
        dialog_type: DialogType,
        message: String,
        default_value: Option<String>,
    },
    /// A file download was triggered (navigation to a `Content-Disposition:
    /// attachment` response). Emitted from the navigate path.
    Download {
        /// Stable download id (CDP `GUID`).
        guid: String,
        /// Source URL.
        url: String,
        /// Suggested filename (from Content-Disposition or URL basename).
        filename: String,
        /// Absolute path the file was saved to.
        save_path: String,
        /// Total bytes received.
        total_bytes: usize,
    },
}

/// Console log severity, mirrored to CDP `Runtime.consoleAPICalled.type`
/// and `Log.entryAdded.entry.level`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleLevel {
    Log,
    Info,
    Warn,
    Error,
}

impl ConsoleLevel {
    /// CDP `Runtime.consoleAPICalled` `type` value.
    pub fn api_type(&self) -> &'static str {
        match self {
            Self::Log => "log",
            Self::Info => "info",
            Self::Warn => "warning",
            Self::Error => "error",
        }
    }
}

/// A single `console.*` argument, preserving enough type info for the CDP
/// layer to build a typed `RemoteObject` (instead of always stringifying).
/// Core cannot name CDP types, so this neutral enum is translated by
/// `emit_console` in the CDP crate.
#[derive(Debug, Clone)]
pub enum ConsoleArg {
    String(String),
    Number(f64),
    Boolean(bool),
    Null,
    Undefined,
    /// A non-primitive: its constructor name (for `className`) and its
    /// stringified form (for console output / `description`).
    Object {
        class_name: String,
        description: String,
    },
}

impl ConsoleArg {
    /// The display string used for the `console` output buffer and the
    /// `Log.entryAdded` text (mirrors `JsValue::to_string`).
    pub fn display(&self) -> String {
        match self {
            Self::String(s) => s.clone(),
            Self::Number(n) => format_console_number(*n),
            Self::Boolean(b) => b.to_string(),
            Self::Null => "null".to_string(),
            Self::Undefined => "undefined".to_string(),
            Self::Object { description, .. } => description.clone(),
        }
    }
}

/// Format an f64 the way JS `console` would display it (integer-valued
/// numbers render without a trailing `.0`, matching boa's `toString`).
fn format_console_number(n: f64) -> String {
    if n.is_nan() {
        "NaN".to_string()
    } else if n.is_infinite() {
        if n > 0.0 {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        }
    } else if n.fract() == 0.0 && n.abs() < 1e21 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// Classify a JS argument into a typed [`ConsoleArg`]. `display` is the
/// precomputed `JsValue::to_string` form (used for object descriptions and
/// kept consistent for primitives).
fn classify_console_arg(arg: &JsValue, display: &str, ctx: &mut Context) -> ConsoleArg {
    match arg {
        JsValue::String(_) => ConsoleArg::String(display.to_string()),
        JsValue::Integer(n) => ConsoleArg::Number(*n as f64),
        JsValue::Rational(n) => ConsoleArg::Number(*n),
        JsValue::Boolean(b) => ConsoleArg::Boolean(*b),
        JsValue::Null => ConsoleArg::Null,
        JsValue::Undefined => ConsoleArg::Undefined,
        JsValue::Object(obj) => {
            let class_name = (|| {
                let ctor = obj.get(js_string!("constructor"), ctx).ok()?;
                let co = ctor.as_object()?;
                let name_val = co.get(js_string!("name"), ctx).ok()?;
                name_val
                    .as_string()
                    .map(|s| s.to_std_string_escaped())
                    .filter(|s| !s.is_empty())
            })()
            .unwrap_or_else(|| "Object".to_string());
            ConsoleArg::Object {
                class_name,
                description: display.to_string(),
            }
        }
        JsValue::Symbol(_) | JsValue::BigInt(_) => ConsoleArg::Object {
            class_name: "Object".to_string(),
            description: display.to_string(),
        },
    }
}

/// Direction of a WebSocket frame relative to the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsDirection {
    Sent,
    Received,
}

/// Kind of blocking dialog (`window.alert` / `confirm` / `prompt`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogType {
    Alert,
    Confirm,
    Prompt,
}

impl DialogType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Alert => "alert",
            Self::Confirm => "confirm",
            Self::Prompt => "prompt",
        }
    }
}

/// Resolution written by the CDP layer (`Page.handleJavaScriptDialog`) and
/// polled by the JS thread's `alert`/`confirm`/`prompt` closures.
#[derive(Debug, Clone)]
pub struct DialogResult {
    /// Whether the user accepted (`true`) or dismissed (`false`).
    pub accept: bool,
    /// Text for an accepted `prompt` (`None` otherwise).
    pub prompt_text: Option<String>,
}

/// Shared cell for pending dialog resolution. Cheap to clone (`Arc`); the
/// JS thread polls its thread-local clone while the CDP layer writes via the
/// [`Session`](crate::session::Session)'s clone.
pub type DialogGate = Arc<Mutex<Option<DialogResult>>>;

thread_local! {
    /// CoreEvent sink sender, installed by `SetEventSink`. `None` when no
    /// observer (e.g. CDP) is attached — pushes are then no-ops.
    static EVENT_TX: RefCell<Option<Sender<CoreEvent>>> = const { RefCell::new(None) };
}

thread_local! {
    /// Pending dialog resolution gate, installed by `SetDialogGate`.
    static DIALOG_GATE: RefCell<Option<DialogGate>> = const { RefCell::new(None) };
}

fn push_event(ev: CoreEvent) {
    EVENT_TX.with(|cell| {
        let borrowed = cell.borrow();
        if let Some(tx) = borrowed.as_ref() {
            let _ = tx.send(ev);
        }
    });
}

/// Whether a CoreEvent sink (observer) is currently attached.
fn event_sink_attached() -> bool {
    EVENT_TX.with(|cell| cell.borrow().is_some())
}

/// Current wall-clock timestamp in milliseconds since the Unix epoch.
fn now_ms() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1000.0
}

/// Capture a PNG of the live `RenderDocument`, composing shadow trees first
/// when any are present.
///
/// Blitz's `BaseDocument` is a single flat tree with no shadow/host/slot
/// concept, so shadow + slotted content is invisible to a direct
/// [`RenderDocument::capture_png`]. When shadow roots are registered, this
/// builds the flattened [`DomSnapshot`] (whose compose pass distributes light
/// children into `<slot>` positions), serializes it to HTML, reparses into a
/// throwaway `RenderDocument` at the same viewport, and rasterizes that. The
/// no-shadow fast path rasterizes the live document directly (no round-trip,
/// no lossiness).
fn capture_png_composed(
    doc: &mut RenderDocument,
    opts: &CaptureOpts,
) -> std::result::Result<Vec<u8>, RenderError> {
    if !crate::js::dom_snapshot::has_shadow_roots() {
        return doc.capture_png(opts);
    }
    let snap = crate::js::dom_snapshot::DomSnapshot::from_render_document(doc, "", "");
    let html = snap.to_html();
    let vp = opts.viewport.unwrap_or_else(|| doc.viewport());
    let mut fresh = RenderDocument::from_html(&html, None, vp)
        .map_err(|e| RenderError::Render(format!("shadow compose reparse failed: {e}")))?;
    fresh.capture_png(opts)
}

/// Parse `html` and append the resulting nodes as shadow children of `host_id`
/// in the live [`RenderDocument`]. Powers `shadowRoot.innerHTML = html`.
///
/// The fragment is parsed into a throwaway snapshot (via Blitz), then each
/// top-level node is recreated in the live render doc and registered as a
/// shadow child (detached — never parented under the host, which would make it
/// light DOM). Descendants are appended normally so the compose pass's subtree
/// walk picks them up.
fn append_html_fragment_to_shadow(
    rd: &Rc<RefCell<Option<RenderDocument>>>,
    host_id: u32,
    html: &str,
) {
    let snap = crate::js::dom_snapshot::parse_html_fragment_to_snapshot(html);
    let body_id = snap.body_id.unwrap_or(snap.root_id);
    let top: Vec<u32> = snap
        .nodes
        .get(&body_id)
        .map(|n| n.children.clone())
        .unwrap_or_default();
    let mut guard = rd.borrow_mut();
    let Some(doc) = guard.as_mut() else {
        return;
    };
    for snap_child in top {
        if let Some(live_id) = recreate_subtree_from_snapshot(doc, &snap, snap_child, None) {
            crate::js::dom_snapshot::push_shadow_child(host_id, live_id as u32);
        }
    }
}

/// Recreate a snapshot subtree in the live [`RenderDocument`].
///
/// If `live_parent` is `Some`, the recreated node is appended to it; if
/// `None`, it is left detached (a top-level shadow child). Returns the new
/// live node id, or `None` for nodes we don't model (comments, the document).
fn recreate_subtree_from_snapshot(
    doc: &mut RenderDocument,
    snap: &crate::js::dom_snapshot::DomSnapshot,
    snap_id: u32,
    live_parent: Option<usize>,
) -> Option<usize> {
    let node = snap.nodes.get(&snap_id)?;
    let live_id = match node.node_type {
        // Text node.
        3 => doc.create_text_node(&node.text_content),
        // Element.
        1 => {
            let id = doc.create_element(&node.tag);
            for (k, v) in &node.attributes {
                doc.set_attribute(id, k, v);
            }
            for &child in &node.children {
                recreate_subtree_from_snapshot(doc, snap, child, Some(id));
            }
            id
        }
        // Comments / document / unknown — skip.
        _ => return None,
    };
    if let Some(p) = live_parent {
        doc.append_child(p, live_id);
    }
    Some(live_id)
}

/// Build a throwaway composed snapshot from the live document (refreshing the
/// slot-assignment registries) and return the light children distributed into
/// the `<slot>` with id `node_id`. Used by `slot.assignedNodes()`/
/// `assignedElements()` so they reflect live state without a prior snapshot.
fn refresh_slot_assignments(rd: &Rc<RefCell<Option<RenderDocument>>>, node_id: usize) -> Vec<u32> {
    let guard = rd.borrow();
    if let Some(d) = guard.as_ref() {
        let _ = crate::js::dom_snapshot::DomSnapshot::from_render_document(d, "", "");
        crate::js::dom_snapshot::slot_assigned_nodes(node_id as u32)
    } else {
        Vec::new()
    }
}

/// Attach declarative shadow roots (`<template shadowrootmode="open|closed">`)
/// found in the parsed document, before page scripts run.
///
/// For each such template, its parent element (the host) gets a shadow root
/// whose children are the template's content; the template node is then
/// detached (Blitz's `remove_node` keeps the subtree accessible by id, so the
/// compose pass can still walk it). Mirrors the HTML spec's declarative shadow
/// DOM create-a-shadow-root step.
fn process_declarative_shadow_dom(doc: &mut RenderDocument) {
    use crate::js::dom_snapshot::ShadowMode;
    // Collect candidate templates with an immutable borrow, then mutate.
    let found: Vec<(usize, ShadowMode, usize, Vec<usize>)> = {
        let base = doc.document();
        let root = doc.root_element_id();
        let mut found = Vec::new();
        let mut stack = vec![root];
        let mut visited = std::collections::HashSet::<usize>::new();
        while let Some(id) = stack.pop() {
            if !visited.insert(id) {
                continue;
            }
            let Some(node) = base.get_node(id) else {
                continue;
            };
            let is_dsd_template = node
                .data
                .downcast_element()
                .is_some_and(|e| e.name.local.as_ref() == "template")
                && node.attrs().is_some_and(|a| {
                    a.iter()
                        .any(|x| x.name.local.as_ref() == "shadowrootmode" && !x.value.is_empty())
                });
            if is_dsd_template
                && let (Some(mode_str), Some(parent)) = (
                    node.attrs().and_then(|a| {
                        a.iter()
                            .find(|x| x.name.local.as_ref() == "shadowrootmode")
                            .map(|x| x.value.clone())
                    }),
                    node.parent,
                )
            {
                let mode = if mode_str.eq_ignore_ascii_case("closed") {
                    ShadowMode::Closed
                } else {
                    ShadowMode::Open
                };
                found.push((id, mode, parent, node.children.clone()));
                // Don't descend: the template's content is captured above.
                continue;
            }
            for &c in &node.children {
                stack.push(c);
            }
        }
        found
    };
    for (tmpl_id, mode, parent_id, child_ids) in found {
        crate::js::dom_snapshot::register_shadow_host(parent_id as u32, mode);
        for cid in &child_ids {
            crate::js::dom_snapshot::push_shadow_child(parent_id as u32, *cid as u32);
        }
        doc.remove_node(tmpl_id);
    }
}

/// Block (poll) the JS thread until the pending dialog is resolved or the
/// timeout elapses. Returns `default` on timeout. Never blocks on a channel
/// `recv()` — the CDP async layer writes the resolution via the [`DialogGate`].
fn wait_dialog_resolution(default: DialogResult, timeout: Duration) -> DialogResult {
    let start = Instant::now();
    loop {
        let resolved =
            DIALOG_GATE.with(|g| g.borrow().as_ref().and_then(|gate| gate.lock().take()));
        if let Some(r) = resolved {
            return r;
        }
        if start.elapsed() >= timeout {
            return default;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Format a fetch request id for CDP correlation (`"oxi-{id}"`). The same id
/// is reused for the matching response so clients can correlate the pair.
fn cdp_request_id(id: u64) -> String {
    format!("oxi-{id}")
}

/// Extract the mime type (content-type, sans parameters) from response headers.
fn content_type_mime(headers: &[(String, String)]) -> String {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.split(';').next().unwrap_or(v).trim().to_string())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// LocalStorage sync messages
// ---------------------------------------------------------------------------

/// Messages sent from JS localStorage operations to Session for sync.
#[derive(Debug)]
pub enum LocalStorageMsg {
    /// localStorage.setItem(key, value)
    SetItem(String, String),
    /// localStorage.removeItem(key)
    RemoveItem(String),
    /// localStorage.clear()
    Clear,
}

// ---------------------------------------------------------------------------
// JsRuntime
// ---------------------------------------------------------------------------

/// Configuration for JS runtime limits and timeouts.
#[derive(Debug, Clone)]
pub struct JsRuntimeConfig {
    /// Default timeout in ms for each evaluate() call.
    pub timeout_ms: u64,
    /// Max recursion depth.
    pub max_recursion: usize,
    /// Max loop iterations.
    pub max_loop_iterations: u64,
    /// Max operand stack size.
    pub max_stack_size: usize,
    /// Nav-script limits — separate, much higher caps for page `<script>`
    /// execution (see Phase 1 spec). The eval caps above are for agent
    /// one-shot snippets and would silently skip real SPA bundles.
    pub nav_script_max_loop_iterations: u64,
    pub nav_script_max_recursion: usize,
    pub nav_script_max_stack_size: usize,
    /// Wall-clock budget (ms) for the whole script-execution + settle phase.
    pub nav_script_timeout_ms: u64,
    /// Viewport width (pixels, 0 = headless).
    pub viewport_width: u32,
    /// Viewport height (pixels, 0 = headless).
    pub viewport_height: u32,
    /// User-Agent exposed to JS (navigator.userAgent). Drives the stealth
    /// fingerprint profile — must match the UA sent over the wire.
    pub user_agent: String,
}

impl Default for JsRuntimeConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 5000,
            max_recursion: 100,
            max_loop_iterations: 100_000,
            max_stack_size: 1024,
            nav_script_max_loop_iterations: 500_000_000,
            nav_script_max_recursion: 4_096,
            nav_script_max_stack_size: 16_384,
            nav_script_timeout_ms: 30_000,
            viewport_width: 1280,
            viewport_height: 720,
            user_agent: "Mozilla/5.0 (OxiBrowser/0.1.0; +https://github.com/oxios/oxibrowser)"
                .to_string(),
        }
    }
}

impl From<&crate::config::BrowserConfig> for JsRuntimeConfig {
    fn from(config: &crate::config::BrowserConfig) -> Self {
        Self {
            timeout_ms: config.js_timeout_ms,
            max_recursion: config.js_max_recursion,
            max_loop_iterations: config.js_max_loop_iterations,
            max_stack_size: config.js_max_stack_size,
            nav_script_max_loop_iterations: config.nav_script_max_loop_iterations,
            nav_script_max_recursion: config.nav_script_max_recursion,
            nav_script_max_stack_size: config.nav_script_max_stack_size,
            nav_script_timeout_ms: config.nav_script_timeout_ms,
            viewport_width: config.viewport_width,
            viewport_height: config.viewport_height,
            user_agent: config.user_agent.clone(),
        }
    }
}

/// A JavaScript runtime backed by boa_engine with a persistent context.
///
/// The `boa_engine::Context` lives on a dedicated OS thread and persists
/// across `evaluate()` calls, so JS variables, functions, and closures
/// survive between invocations.
///
/// Thread-safe: `Send + Sync` via channel communication.
pub struct JsRuntime {
    /// Channel to send commands to the JS thread.
    cmd_tx: Sender<JsCommand>,
    /// Shared console output buffer (also shared with JS thread closures).
    console_output: Arc<RwLock<Vec<String>>>,
    /// Shared mutation buffer — JS thread pushes, main thread drains.
    mutations: Arc<RwLock<Vec<DomMutation>>>,
    /// Global variables tracked on the Rust side.
    globals: RwLock<HashMap<String, Value>>,
    /// Runtime configuration (limits, timeouts).
    config: JsRuntimeConfig,
    /// Channel to send fetch requests (set via set_fetch_channel()).
    fetch_tx: Option<std::sync::mpsc::Sender<FetchRequestMsg>>,
    /// @font-face webfont bytes staged for the next `SetDocument` (consumed on send).
    pending_fonts: Vec<Vec<u8>>,
}

impl JsRuntime {
    /// Create a new JS runtime with default configuration.
    pub fn new() -> Self {
        Self::with_config(JsRuntimeConfig::default())
    }

    /// Create a new JS runtime with the given configuration.
    pub fn with_config(config: JsRuntimeConfig) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<JsCommand>();
        let console_output = Arc::new(RwLock::new(Vec::<String>::new()));
        let mutations = Arc::new(RwLock::new(Vec::<DomMutation>::new()));

        // Spawn JS thread
        let console_output_clone = console_output.clone();
        let mutations_clone = mutations.clone();
        let viewport = (config.viewport_width, config.viewport_height);
        let user_agent = config.user_agent.clone();
        let _local_storage = Arc::new(RwLock::new(HashMap::<String, String>::new()));
        std::thread::Builder::new()
            .name("oxibrowser-js".into())
            .spawn(move || {
                js_thread_loop(
                    cmd_rx,
                    console_output_clone,
                    mutations_clone,
                    viewport,
                    None,
                    user_agent,
                );
            })
            .expect("failed to spawn JS thread");

        Self {
            cmd_tx,
            console_output,
            mutations,
            globals: RwLock::new(HashMap::new()),
            config,
            fetch_tx: None,
            pending_fonts: Vec::new(),
        }
    }

    /// Stage @font-face webfont bytes for the next `set_document_with_scripts`
    /// call (consumed on send). Call before navigating to a page whose inline
    /// `<style>` declares `@font-face` rules.
    pub fn set_pending_fonts(&mut self, fonts: Vec<Vec<u8>>) {
        self.pending_fonts = fonts;
    }

    /// Set the channels for fetch: the request sender and the shared response
    /// receiver. Must be called before JS can use `fetch()`/`XMLHttpRequest`.
    pub fn set_fetch_channel(
        &mut self,
        request_tx: std::sync::mpsc::Sender<FetchRequestMsg>,
        response_rx: std::sync::mpsc::Receiver<FetchResponseMsg>,
    ) {
        self.fetch_tx = Some(request_tx.clone());
        let (ack_tx, ack_rx) = mpsc::channel::<JsResponse>();
        if let Err(e) = self.cmd_tx.send(JsCommand::SetFetchChannel {
            request_tx,
            response_rx,
            response_tx: ack_tx,
        }) {
            tracing::error!(error = %e, "failed to send SetFetchChannel: JS thread has died");
            return;
        }
        let _ = ack_rx.recv();
    }
    /// Set the WebSocket channels: the request sender and the shared event
    /// receiver. Must be called before JS can use `WebSocket`.
    pub fn set_ws_channel(
        &mut self,
        request_tx: std::sync::mpsc::Sender<WsReqMsg>,
        event_rx: std::sync::mpsc::Receiver<WsEvent>,
    ) {
        let (ack_tx, ack_rx) = mpsc::channel::<JsResponse>();
        if let Err(e) = self.cmd_tx.send(JsCommand::SetWsChannel {
            request_tx,
            response_rx: event_rx,
            response_tx: ack_tx,
        }) {
            tracing::error!(error = %e, "failed to send SetWsChannel: JS thread has died");
            return;
        }
        let _ = ack_rx.recv();
    }

    /// Install the CoreEvent sink so console / exception / fetch / WebSocket /
    /// dialog events flow to an observer (typically the CDP layer). No-op
    /// pushes until this is called.
    pub fn set_event_sink(&mut self, tx: std::sync::mpsc::Sender<CoreEvent>) {
        let (response_tx, response_rx) = mpsc::channel::<JsResponse>();
        if let Err(e) = self
            .cmd_tx
            .send(JsCommand::SetEventSink { tx, response_tx })
        {
            tracing::error!(error = %e, "failed to send SetEventSink: JS thread has died");
            return;
        }
        let _ = response_rx.recv();
    }

    /// Install the shared dialog-resolution gate. Must be called for blocking
    /// `alert`/`confirm`/`prompt` to be resolvable by `Page.handleJavaScriptDialog`.
    pub fn set_dialog_gate(&mut self, gate: DialogGate) {
        let (response_tx, response_rx) = mpsc::channel::<JsResponse>();
        if let Err(e) = self
            .cmd_tx
            .send(JsCommand::SetDialogGate { gate, response_tx })
        {
            tracing::error!(error = %e, "failed to send SetDialogGate: JS thread has died");
            return;
        }
        let _ = response_rx.recv();
    }

    /// Set the channel for localStorage sync.
    pub fn set_local_storage_channel(&mut self, tx: std::sync::mpsc::Sender<LocalStorageMsg>) {
        let (response_tx, response_rx) = mpsc::channel::<JsResponse>();
        if let Err(e) = self
            .cmd_tx
            .send(JsCommand::SetLocalStorageChannel { tx, response_tx })
        {
            tracing::error!(error = %e, "failed to send SetLocalStorageChannel: JS thread has died");
            return;
        }
        let _ = response_rx.recv();
    }

    /// Evaluate a JavaScript expression and return the result.
    pub async fn evaluate(&mut self, expression: &str) -> Result<JsEvalResult> {
        self.evaluate_with_timeout(expression, None).await
    }

    /// Evaluate a JavaScript expression, optionally awaiting Promise resolution.
    pub async fn evaluate_with_await(
        &mut self,
        expression: &str,
        await_promise: bool,
    ) -> Result<JsEvalResult> {
        self.evaluate_with_timeout_and_await(expression, None, await_promise)
            .await
    }

    /// Evaluate a JavaScript expression with an explicit timeout override.
    pub async fn evaluate_with_timeout(
        &mut self,
        expression: &str,
        timeout_ms: Option<u64>,
    ) -> Result<JsEvalResult> {
        self.evaluate_with_timeout_and_await(expression, timeout_ms, false)
            .await
    }

    /// Evaluate a JavaScript expression with timeout and optional Promise awaiting.
    pub async fn evaluate_with_timeout_and_await(
        &mut self,
        expression: &str,
        timeout_ms: Option<u64>,
        await_promise: bool,
    ) -> Result<JsEvalResult> {
        self.console_output.write().clear();
        tracing::debug!(
            expr_len = expression.len(),
            timeout_ms = timeout_ms.unwrap_or(self.config.timeout_ms),
            "JS evaluation started"
        );
        let (response_tx, response_rx) = mpsc::channel::<JsResponse>();
        self.cmd_tx
            .send(JsCommand::Eval {
                context_id: 1,
                expression: expression.to_string(),
                timeout_ms: Some(timeout_ms.unwrap_or(self.config.timeout_ms)),
                max_loop_iterations: Some(self.config.max_loop_iterations),
                max_recursion: Some(self.config.max_recursion),
                max_stack_size: Some(self.config.max_stack_size),
                await_promise,
                response_tx,
            })
            .map_err(|_| CoreError::JsError("JS thread has died".into()))?;
        let resp = tokio::task::spawn_blocking(move || response_rx.recv())
            .await
            .map_err(|_| CoreError::JsError("JS eval recv task panicked".into()))?
            .map_err(|_| CoreError::JsError("JS thread has died".into()))?;
        match resp {
            JsResponse::EvalResult {
                value,
                exception,
                console_output,
                timed_out,
            } => {
                if timed_out {
                    tracing::warn!(
                        timeout_ms = timeout_ms.unwrap_or(self.config.timeout_ms),
                        "JS evaluation timed out — context reset"
                    );
                    return Err(CoreError::JsTimeout(
                        timeout_ms.unwrap_or(self.config.timeout_ms),
                    ));
                }
                Ok(JsEvalResult {
                    value,
                    exception,
                    console_output,
                    timed_out: false,
                })
            }
            _ => Err(CoreError::JsError(
                "unexpected response from JS thread".into(),
            )),
        }
    }

    /// Evaluate a script (multiple statements, no return value needed).
    pub async fn execute(&mut self, script: &str) -> Result<JsEvalResult> {
        self.evaluate(script).await
    }

    /// Get captured console output from the last eval.
    pub fn console_output(&self) -> Vec<String> {
        self.console_output.read().clone()
    }

    /// Clear captured console output.
    pub fn clear_console(&mut self) {
        self.console_output.write().clear();
    }

    /// Drain all pending DOM mutations collected by JS execution.
    pub fn drain_mutations(&self) -> Vec<DomMutation> {
        let mut guard = self.mutations.write();
        std::mem::take(&mut *guard)
    }

    /// Set a global variable — injected into the persistent JS Context.
    pub fn set_global(&mut self, name: impl Into<String>, value: Value) {
        let name = name.into();
        self.globals.write().insert(name.clone(), value.clone());
        let (response_tx, response_rx) = mpsc::channel::<JsResponse>();
        if let Err(e) = self.cmd_tx.send(JsCommand::SetGlobal {
            name,
            value,
            response_tx,
        }) {
            tracing::error!(error = %e, "failed to send SetGlobal: JS thread has died");
            return;
        }
        let _ = response_rx.recv();
    }

    /// Set the DOM snapshot (called after navigate).
    pub fn set_dom_snapshot(&mut self, snapshot: Option<DomSnapshot>) {
        self.mutations.write().clear();
        let (response_tx, response_rx) = mpsc::channel::<JsResponse>();
        if let Err(e) = self.cmd_tx.send(JsCommand::SetDom {
            snapshot: Box::new(snapshot),
            response_tx,
        }) {
            tracing::error!(error = %e, "failed to send SetDom: JS thread has died");
            return;
        }
        let _ = response_rx.recv();
    }

    /// Set the CookieJar so document.cookie reads/writes real cookies.
    pub fn set_cookie_jar(&mut self, jar: Arc<RwLock<CookieJar>>) -> Result<()> {
        let (response_tx, response_rx) = mpsc::channel::<JsResponse>();
        self.cmd_tx
            .send(JsCommand::SetCookieJar { jar, response_tx })
            .map_err(|_| CoreError::JsError("JS thread has died".into()))?;
        let resp = response_rx
            .recv()
            .map_err(|_| CoreError::JsError("JS thread has died".into()))?;
        match resp {
            JsResponse::Done => Ok(()),
            _ => Err(CoreError::JsError("unexpected response".into())),
        }
    }

    /// Update the page URL (used for window.location).
    pub fn set_page_url(&mut self, url: &str) {
        let (response_tx, response_rx) = mpsc::channel::<JsResponse>();
        if let Err(e) = self.cmd_tx.send(JsCommand::SetPageUrl {
            url: url.to_string(),
            response_tx,
        }) {
            tracing::error!(error = %e, "failed to send SetPageUrl: JS thread has died");
            return;
        }
        let _ = response_rx.recv();
    }

    // ── Render façades (ship a command to the JS thread, await a response) ────
    //
    // These reach the `!Send` `RenderDocument` that lives on the JS thread —
    // the single source of truth for the DOM after unification. The JS thread
    // builds/captures/queries it synchronously between JS ticks.

    /// Build (or replace) the renderable document on the JS thread from HTML.
    ///
    /// No page scripts are executed (legacy behavior). Use
    /// [`set_document_with_scripts`](Self::set_document_with_scripts) to run
    /// the page's `<script>` tags during navigation.
    pub async fn set_document(
        &mut self,
        html: &str,
        base_url: Option<&str>,
        viewport: (u32, u32),
    ) -> Result<()> {
        self.set_document_with_scripts(html, base_url, viewport, Vec::new())
            .await
    }

    /// Build the render document AND execute the supplied page `<script>`
    /// sources in document order, then fire `DOMContentLoaded`/`load` and
    /// settle the timer/microtask queue — mirroring a real browser's
    /// navigation. External scripts must already be fetched (`source` filled);
    /// entries with an empty `source` and a `src_url` are skipped with a warn.
    pub async fn set_document_with_scripts(
        &mut self,
        html: &str,
        base_url: Option<&str>,
        viewport: (u32, u32),
        scripts: Vec<ScriptSource>,
    ) -> Result<()> {
        let (response_tx, response_rx) = mpsc::channel::<JsResponse>();
        self.cmd_tx
            .send(JsCommand::SetDocument {
                html: html.to_string(),
                base_url: base_url.map(|s| s.to_string()),
                viewport,
                fonts: std::mem::take(&mut self.pending_fonts),
                scripts,
                nav_loop_limit: self.config.nav_script_max_loop_iterations,
                nav_recursion_limit: self.config.nav_script_max_recursion,
                nav_stack_limit: self.config.nav_script_max_stack_size,
                nav_timeout_ms: self.config.nav_script_timeout_ms,
                response_tx,
            })
            .map_err(|_| CoreError::JsError("JS thread has died".into()))?;
        let resp = tokio::task::spawn_blocking(move || response_rx.recv())
            .await
            .map_err(|_| CoreError::JsError("JS document recv task panicked".into()))?
            .map_err(|_| CoreError::JsError("JS thread has died".into()))?;
        match resp {
            JsResponse::Done => Ok(()),
            JsResponse::Error { message } => Err(CoreError::ScreenshotError(message)),
            _ => Err(CoreError::JsError("unexpected response".into())),
        }
    }

    /// Capture a full-page PNG screenshot of the current render document.
    ///
    /// The render runs synchronously on the JS thread, so the captured frame
    /// is a consistent snapshot (no half-applied JS mutations).
    pub async fn capture_png(&mut self, opts: CaptureOpts) -> Result<Vec<u8>> {
        let (response_tx, response_rx) = mpsc::channel::<JsResponse>();
        self.cmd_tx
            .send(JsCommand::Capture { opts, response_tx })
            .map_err(|_| CoreError::JsError("JS thread has died".into()))?;
        let resp = response_rx
            .recv()
            .map_err(|_| CoreError::JsError("JS thread has died".into()))?;
        match resp {
            JsResponse::CaptureResult { png } => Ok(png),
            JsResponse::Error { message } => Err(CoreError::ScreenshotError(message)),
            _ => Err(CoreError::JsError("unexpected response".into())),
        }
    }

    /// Query all nodes matching a CSS selector against the render document.
    ///
    /// Returns serializable [`NodeInfo`] (the async side never touches the
    /// `!Send` `RenderDocument` directly).
    pub async fn query_selector_all(&mut self, selector: &str) -> Result<Vec<NodeInfo>> {
        let (response_tx, response_rx) = mpsc::channel::<JsResponse>();
        self.cmd_tx
            .send(JsCommand::Query {
                selector: selector.to_string(),
                response_tx,
            })
            .map_err(|_| CoreError::JsError("JS thread has died".into()))?;
        let resp = response_rx
            .recv()
            .map_err(|_| CoreError::JsError("JS thread has died".into()))?;
        match resp {
            JsResponse::QueryResult { nodes } => Ok(nodes),
            JsResponse::Error { message } => Err(CoreError::ScreenshotError(message)),
            _ => Err(CoreError::JsError("unexpected response".into())),
        }
    }

    /// Serialize the live RenderDocument to a [`DomSnapshot`].
    ///
    /// The snapshot reflects every JS mutation (it is built from the
    /// `RenderDocument` on the JS thread, not a navigate-time copy), so CDP
    /// DOM/OXI and `extract` reads stay correct after JS interaction.
    pub async fn dom_snapshot(
        &mut self,
        url: &str,
    ) -> Result<crate::js::dom_snapshot::DomSnapshot> {
        let (response_tx, response_rx) = mpsc::channel::<JsResponse>();
        self.cmd_tx
            .send(JsCommand::GetDocumentSnapshot {
                url: url.to_string(),
                response_tx,
            })
            .map_err(|_| CoreError::JsError("JS thread has died".into()))?;
        let resp = response_rx
            .recv()
            .map_err(|_| CoreError::JsError("JS thread has died".into()))?;
        match resp {
            JsResponse::Snapshot(snap) => Ok(*snap),
            JsResponse::Error { message } => Err(CoreError::ScreenshotError(message)),
            _ => Err(CoreError::JsError("unexpected response".into())),
        }
    }

    // ── Phase 8: per-frame execution contexts ────────────────────────────

    /// Evaluate a JS expression in a specific frame's execution context.
    /// `context_id: 1` is the main frame (equivalent to `evaluate`).
    pub async fn evaluate_in_context(
        &mut self,
        expression: &str,
        context_id: u32,
        await_promise: bool,
    ) -> Result<JsEvalResult> {
        let (response_tx, response_rx) = mpsc::channel::<JsResponse>();
        self.cmd_tx
            .send(JsCommand::Eval {
                context_id,
                expression: expression.to_string(),
                timeout_ms: Some(self.config.timeout_ms),
                max_loop_iterations: Some(self.config.max_loop_iterations),
                max_recursion: Some(self.config.max_recursion),
                max_stack_size: Some(self.config.max_stack_size),
                await_promise,
                response_tx,
            })
            .map_err(|_| CoreError::JsError("JS thread has died".into()))?;
        let resp = tokio::task::spawn_blocking(move || response_rx.recv())
            .await
            .map_err(|_| CoreError::JsError("JS eval recv task panicked".into()))?
            .map_err(|_| CoreError::JsError("JS thread has died".into()))?;
        match resp {
            JsResponse::EvalResult {
                value,
                exception,
                console_output,
                timed_out,
            } => {
                if timed_out {
                    return Err(CoreError::JsTimeout(self.config.timeout_ms));
                }
                Ok(JsEvalResult {
                    value,
                    exception,
                    console_output,
                    timed_out: false,
                })
            }
            JsResponse::Error { message } => Err(CoreError::JsError(message)),
            _ => Err(CoreError::JsError(
                "unexpected response from JS thread".into(),
            )),
        }
    }

    /// Build a child frame's execution context from HTML + scripts (Phase 8).
    /// Creates a new `Context` + `RenderDocument` on the JS thread under the
    /// given `context_id` (must be ≥ 2).
    pub async fn set_frame_document(
        &mut self,
        context_id: u32,
        html: &str,
        base_url: &str,
        viewport: (u32, u32),
        scripts: Vec<ScriptSource>,
    ) -> Result<()> {
        let (response_tx, response_rx) = mpsc::channel::<JsResponse>();
        self.cmd_tx
            .send(JsCommand::SetFrameDocument {
                context_id,
                html: html.to_string(),
                base_url: base_url.to_string(),
                viewport,
                scripts,
                nav_loop_limit: self.config.nav_script_max_loop_iterations,
                nav_recursion_limit: self.config.nav_script_max_recursion,
                nav_stack_limit: self.config.nav_script_max_stack_size,
                nav_timeout_ms: self.config.nav_script_timeout_ms,
                response_tx,
            })
            .map_err(|_| CoreError::JsError("JS thread has died".into()))?;
        let resp = tokio::task::spawn_blocking(move || response_rx.recv())
            .await
            .map_err(|_| CoreError::JsError("JS frame doc recv task panicked".into()))?
            .map_err(|_| CoreError::JsError("JS thread has died".into()))?;
        match resp {
            JsResponse::Done => Ok(()),
            JsResponse::Error { message } => Err(CoreError::ScreenshotError(message)),
            _ => Err(CoreError::JsError("unexpected response".into())),
        }
    }

    /// Drop all child-frame execution contexts (context_id > 1). Called on
    /// navigation before building new child frames.
    pub fn clear_child_contexts(&mut self) {
        let (response_tx, response_rx) = mpsc::channel::<JsResponse>();
        if let Err(e) = self
            .cmd_tx
            .send(JsCommand::ClearChildContexts { response_tx })
        {
            tracing::error!(error = %e, "failed to send ClearChildContexts: JS thread has died");
            return;
        }
        let _ = response_rx.recv();
    }
}

impl Default for JsRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for JsRuntime {
    fn drop(&mut self) {
        // Signal the JS thread to shut down — no response needed for Shutdown
        let _ = self.cmd_tx.send(JsCommand::Shutdown);
    }
}

// ---------------------------------------------------------------------------
// JS thread
// ---------------------------------------------------------------------------

/// A child frame's JS execution context (Phase 8). Each iframe gets its own
/// `Context` + `RenderDocument`, co-located on the JS thread alongside the
/// main frame's context. The main frame (context_id=1) is NOT stored here —
/// it lives directly in `js_thread_loop` as `ctx`/`render_doc_cell`.
struct ChildFrame {
    ctx: Context,
    job_queue: Rc<TokioJobQueue>,
    render_doc_cell: Rc<RefCell<Option<RenderDocument>>>,
    dom_snapshot_arc: Arc<RwLock<Option<DomSnapshot>>>,
}

/// Outcome of `run_eval`: either a response to send back, or a timeout signal
/// (the caller recreates the context).
enum EvalOutcome {
    Response(JsResponse),
    TimedOut {
        console: Vec<String>,
        elapsed_ms: u128,
    },
}

/// Core eval logic shared by the main frame and child frames. Does everything
/// except context recreation on timeout (the caller handles that, since main
/// and child frames recreate differently).
#[allow(clippy::too_many_arguments)]
fn run_eval(
    ctx: &mut Context,
    job_queue: &Rc<TokioJobQueue>,
    console_output: &Arc<RwLock<Vec<String>>>,
    mutations: &Arc<RwLock<Vec<DomMutation>>>,
    expression: &str,
    timeout_ms: Option<u64>,
    max_loop_iterations: Option<u64>,
    max_recursion: Option<usize>,
    max_stack_size: Option<usize>,
    await_promise: bool,
) -> EvalOutcome {
    console_output.write().clear();
    mutations.write().clear();

    let loop_limit = max_loop_iterations.unwrap_or(100_000);
    let recursion_limit = max_recursion.unwrap_or(100);
    let stack_limit = max_stack_size.unwrap_or(1024);

    {
        let limits = ctx.runtime_limits_mut();
        limits.set_loop_iteration_limit(loop_limit);
        limits.set_recursion_limit(recursion_limit);
        limits.set_stack_size_limit(stack_limit);
    }

    let timeout = timeout_ms.unwrap_or(5000);
    let start = std::time::Instant::now();
    let source = Source::from_bytes(expression);
    let result = ctx.eval(source);

    ctx.run_jobs();
    drain_timers(job_queue, ctx);

    let act = active_context_id();
    if PENDING_FETCH.with(|m| m.borrow().values().any(|p| p.context_id() == act))
        || PENDING_WS.with(|m| m.borrow().values().any(|s| s.context_id() == Some(act)))
    {
        settle_to_idle(ctx, job_queue, start, Duration::from_millis(timeout));
    }

    let elapsed = start.elapsed();
    let console = console_output.read().clone();

    if elapsed.as_millis() > timeout as u128 {
        return EvalOutcome::TimedOut {
            console,
            elapsed_ms: elapsed.as_millis(),
        };
    }

    match result {
        Ok(value) => {
            let final_value = if await_promise {
                await_promise_value(value, ctx, job_queue)
            } else {
                value
            };
            let json_value = js_value_to_json(&final_value, ctx);
            EvalOutcome::Response(JsResponse::EvalResult {
                value: Some(json_value),
                exception: None,
                console_output: console,
                timed_out: false,
            })
        }
        Err(err) => {
            let (msg, err_name, err_stack) = error_sink_details(&err, ctx);
            push_event(CoreEvent::Exception {
                message: msg.clone(),
                name: err_name,
                stack: err_stack,
                timestamp: now_ms(),
            });
            EvalOutcome::Response(JsResponse::EvalResult {
                value: None,
                exception: Some(msg),
                console_output: console,
                timed_out: false,
            })
        }
    }
}

/// Best-effort human-readable message from a panic payload.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string())
}

/// Main loop for the JS thread.
///
/// Creates a single `Context`, registers globals, and processes commands
/// until a `Shutdown` is received.
fn js_thread_loop(
    cmd_rx: Receiver<JsCommand>,
    console_output: Arc<RwLock<Vec<String>>>,
    mutations: Arc<RwLock<Vec<DomMutation>>>,
    viewport: (u32, u32),
    _fetch_tx: Option<std::sync::mpsc::Sender<FetchRequestMsg>>,
    user_agent: String,
) {
    let fetch_tx_arc: Arc<RwLock<Option<std::sync::mpsc::Sender<FetchRequestMsg>>>> =
        Arc::new(RwLock::new(None));
    let local_storage_tx_arc: Arc<RwLock<Option<std::sync::mpsc::Sender<LocalStorageMsg>>>> =
        Arc::new(RwLock::new(None));
    let cookie_jar_arc: Arc<RwLock<Option<Arc<RwLock<CookieJar>>>>> = Arc::new(RwLock::new(None));
    let dom_snapshot: Arc<RwLock<Option<DomSnapshot>>> = Arc::new(RwLock::new(None));
    // The Blitz-backed render document. `BaseDocument` is effectively `!Send`,
    // so it lives here on the JS thread (co-located with boa's `Context`),
    // mirroring a real browser's main thread. Shared (via `Rc<RefCell>`) with
    // the JS DOM bindings so JS mutates it directly; set via `SetDocument`,
    // captured/queried via `Capture`/`Query`.
    let render_doc_cell: Rc<RefCell<Option<RenderDocument>>> = Rc::new(RefCell::new(None));
    let (mut ctx, mut job_queue) = create_context(
        &console_output,
        &dom_snapshot,
        &mutations,
        viewport,
        "",
        &user_agent,
        &fetch_tx_arc,
        &cookie_jar_arc,
        &render_doc_cell,
    );

    // Per-iframe execution contexts (Phase 8). Keyed by context_id (≥ 2).
    // The main frame (context_id=1) uses `ctx`/`render_doc_cell` above.
    let mut child_frames: HashMap<u32, ChildFrame> = HashMap::new();

    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            JsCommand::Eval {
                context_id,
                expression,
                timeout_ms,
                max_loop_iterations,
                max_recursion,
                max_stack_size,
                await_promise,
                response_tx,
            } => {
                ACTIVE_CONTEXT_ID.set(context_id);

                // A native binding that panics must not unwind out of this
                // loop — that killed the JS thread, and every later command
                // failed with "JS thread has died" (issue #2). Contain the
                // panic here and answer with a proper error. The context is
                // deliberately kept: most panics originate in the render-doc
                // bindings before boa state is touched, and recreating it
                // would drop all page JS state.
                let eval_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if context_id == 1 {
                        Some(run_eval(
                            &mut ctx,
                            &job_queue,
                            &console_output,
                            &mutations,
                            &expression,
                            timeout_ms,
                            max_loop_iterations,
                            max_recursion,
                            max_stack_size,
                            await_promise,
                        ))
                    } else {
                        child_frames.get_mut(&context_id).map(|cf| {
                            run_eval(
                                &mut cf.ctx,
                                &cf.job_queue,
                                &console_output,
                                &mutations,
                                &expression,
                                timeout_ms,
                                max_loop_iterations,
                                max_recursion,
                                max_stack_size,
                                await_promise,
                            )
                        })
                    }
                }));

                let outcome = match eval_result {
                    Ok(Some(outcome)) => outcome,
                    Ok(None) => {
                        let _ = response_tx.send(JsResponse::Error {
                            message: format!("unknown execution context id {context_id}"),
                        });
                        ACTIVE_CONTEXT_ID.set(1);
                        continue;
                    }
                    Err(payload) => {
                        let detail = panic_message(payload.as_ref());
                        let _ = response_tx.send(JsResponse::Error {
                            message: format!("native binding panicked during evaluation: {detail}"),
                        });
                        ACTIVE_CONTEXT_ID.set(1);
                        continue;
                    }
                };

                match outcome {
                    EvalOutcome::Response(resp) => {
                        let _ = response_tx.send(resp);
                    }
                    EvalOutcome::TimedOut {
                        console,
                        elapsed_ms,
                    } => {
                        // Recreate the timed-out context.
                        if context_id == 1 {
                            let (new_ctx, new_queue) = create_context(
                                &console_output,
                                &dom_snapshot,
                                &mutations,
                                viewport,
                                "",
                                &user_agent,
                                &fetch_tx_arc,
                                &cookie_jar_arc,
                                &render_doc_cell,
                            );
                            ctx = new_ctx;
                            job_queue = new_queue;
                        } else if let Some(cf) = child_frames.get_mut(&context_id) {
                            let (new_ctx, new_queue) = create_context(
                                &console_output,
                                &cf.dom_snapshot_arc,
                                &mutations,
                                viewport,
                                "",
                                &user_agent,
                                &fetch_tx_arc,
                                &cookie_jar_arc,
                                &cf.render_doc_cell,
                            );
                            cf.ctx = new_ctx;
                            cf.job_queue = new_queue;
                        }
                        let _ = response_tx.send(JsResponse::EvalResult {
                            value: None,
                            exception: Some(format!(
                                "JS execution timed out after {elapsed_ms}ms — context was reset, previous state lost"
                            )),
                            console_output: console,
                            timed_out: true,
                        });
                    }
                }
                ACTIVE_CONTEXT_ID.set(1);
            }
            JsCommand::SetGlobal {
                name,
                value,
                response_tx,
            } => {
                let js_val = json_to_js_value(&value, &mut ctx);
                let _ = ctx.register_global_property(
                    JsString::from(name.as_str()),
                    js_val,
                    Attribute::all(),
                );
                let _ = response_tx.send(JsResponse::Done);
            }
            JsCommand::SetDom {
                snapshot,
                response_tx,
            } => {
                *dom_snapshot.write() = *snapshot;
                // Update document title/URL in the JS context
                let snap = dom_snapshot.read();
                if let Some(ref s) = *snap {
                    let _ = ctx.register_global_property(
                        js_string!("__domTitle"),
                        JsValue::from(JsString::from(s.title.as_str())),
                        Attribute::all(),
                    );
                    let _ = ctx.register_global_property(
                        js_string!("__domUrl"),
                        JsValue::from(JsString::from(s.url.as_str())),
                        Attribute::all(),
                    );
                }
                let _ = response_tx.send(JsResponse::Done);
            }
            JsCommand::SetPageUrl { url, response_tx } => {
                set_current_origin(&url);
                // Re-register window.location with the new URL
                let snap = dom_snapshot.read();
                let dom_snapshot_ref = dom_snapshot.clone();
                drop(snap);
                register_window_globals(
                    &mut ctx,
                    &dom_snapshot_ref,
                    &mutations,
                    viewport,
                    &url,
                    &user_agent,
                    &fetch_tx_arc,
                    &render_doc_cell,
                );
                // Preserve localStorage across URL changes.
                // TODO(#sop): Check same-origin before preserving localStorage.
                // Currently preserves across all navigations, including cross-origin.
                // In a production browser, localStorage should be scoped per-origin.
                //
                // Only re-register localStorage if it hasn't been registered yet;
                // otherwise the existing JS-side storage object persists across navigations
                // (same-origin policy would be checked in a full implementation).
                // Previously this always re-registered with an empty HashMap, wiping storage.
                let existing_ls = ctx
                    .global_object()
                    .get(js_string!("localStorage"), &mut ctx)
                    .ok();
                if existing_ls
                    .as_ref()
                    .is_none_or(|v| v.is_undefined() || v.is_null())
                {
                    // First time — register fresh
                    let empty = std::collections::HashMap::new();
                    register_local_storage(
                        &mut ctx,
                        empty,
                        &dom_snapshot_ref,
                        local_storage_tx_arc.clone(),
                    );
                }
                // else: localStorage already exists, preserve it across navigation
                let _ = response_tx.send(JsResponse::Done);
            }
            JsCommand::SetLocalStorageChannel { tx, response_tx } => {
                *local_storage_tx_arc.write() = Some(tx);
                let _ = response_tx.send(JsResponse::Done);
            }
            JsCommand::SetFetchChannel {
                request_tx,
                response_rx,
                response_tx,
            } => {
                *fetch_tx_arc.write() = Some(request_tx);
                RESPONSE_RX.with(|cell| *cell.borrow_mut() = Some(response_rx));
                let _ = response_tx.send(JsResponse::Done);
            }
            JsCommand::SetWsChannel {
                request_tx,
                response_rx,
                response_tx,
            } => {
                WS_REQ_TX.with(|cell| *cell.borrow_mut() = Some(request_tx));
                WS_EVENT_RX.with(|cell| *cell.borrow_mut() = Some(response_rx));
                let _ = response_tx.send(JsResponse::Done);
            }
            JsCommand::SetCookieJar { jar, response_tx } => {
                *cookie_jar_arc.write() = Some(jar);
                let _ = response_tx.send(JsResponse::Done);
            }
            JsCommand::SetEventSink { tx, response_tx } => {
                EVENT_TX.with(|cell| *cell.borrow_mut() = Some(tx));
                let _ = response_tx.send(JsResponse::Done);
            }
            JsCommand::SetDialogGate { gate, response_tx } => {
                DIALOG_GATE.with(|cell| *cell.borrow_mut() = Some(gate));
                let _ = response_tx.send(JsResponse::Done);
            }
            JsCommand::SetDocument {
                html,
                base_url,
                viewport,
                fonts,
                scripts,
                nav_loop_limit,
                nav_recursion_limit,
                nav_stack_limit,
                nav_timeout_ms,
                response_tx,
            } => {
                // The base_url (document URL) drives the page origin for CORS/Referer.
                if let Some(ref bu) = base_url {
                    set_current_origin(bu);
                }
                let vp = Viewport {
                    width: viewport.0.max(64),
                    height: viewport.1.max(64),
                    scale: 1.0,
                };
                match RenderDocument::from_html_with_fonts(&html, base_url.as_deref(), vp, &fonts) {
                    Ok(doc) => {
                        *render_doc_cell.borrow_mut() = Some(doc);
                        // Shadow roots are per-document; drop stale entries.
                        crate::js::dom_snapshot::clear_shadow_roots();
                        // Attach declarative shadow roots
                        // (`<template shadowrootmode>`) from the parsed
                        // document before scripts run, so the page sees them.
                        {
                            let mut guard = render_doc_cell.borrow_mut();
                            if let Some(doc) = guard.as_mut() {
                                process_declarative_shadow_dom(doc);
                            }
                        }
                        // Build the DomSnapshot from the render doc so its node
                        // ids stay consistent with the Taffy layout boxes (used
                        // by layout-based hit-testing and getBoundingClientRect).
                        {
                            let guard = render_doc_cell.borrow();
                            if let Some(doc) = guard.as_ref() {
                                let title = doc
                                    .query_selector("title")
                                    .map(|id| doc.node_text(id))
                                    .unwrap_or_default();
                                let snap =
                                    crate::js::dom_snapshot::DomSnapshot::from_render_document(
                                        doc,
                                        base_url.as_deref().unwrap_or(""),
                                        &title,
                                    );
                                *dom_snapshot.write() = Some(snap);
                            }
                        }
                        // Contained like the Eval arm: a panic in a page
                        // script's native binding must not kill the JS thread
                        // — set_document reports it instead.
                        let nav = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            run_navigation_scripts(
                                &mut ctx,
                                &job_queue,
                                &scripts,
                                nav_loop_limit,
                                nav_recursion_limit,
                                nav_stack_limit,
                                nav_timeout_ms,
                            );
                        }));
                        match nav {
                            Ok(()) => {
                                let _ = response_tx.send(JsResponse::Done);
                            }
                            Err(payload) => {
                                let _ = response_tx.send(JsResponse::Error {
                                    message: format!(
                                        "native binding panicked during navigation scripts: {}",
                                        panic_message(payload.as_ref())
                                    ),
                                });
                            }
                        }
                    }
                    Err(e) => {
                        let _ = response_tx.send(JsResponse::Error {
                            message: e.to_string(),
                        });
                    }
                }
            }
            JsCommand::Capture { opts, response_tx } => {
                let mut guard = render_doc_cell.borrow_mut();
                let result = match guard.as_mut() {
                    Some(doc) => capture_png_composed(doc, &opts),
                    None => Err(RenderError::Render("no render document set".into())),
                };
                drop(guard);
                match result {
                    Ok(bytes) => {
                        let _ = response_tx.send(JsResponse::CaptureResult { png: bytes });
                    }
                    Err(e) => {
                        let _ = response_tx.send(JsResponse::Error {
                            message: e.to_string(),
                        });
                    }
                }
            }
            JsCommand::Query {
                selector,
                response_tx,
            } => {
                let nodes = match render_doc_cell.borrow().as_ref() {
                    Some(doc) => doc
                        .query_selector_all(&selector)
                        .into_iter()
                        .map(|id| NodeInfo {
                            id,
                            tag: doc.tag_name(id),
                            text: doc.node_text(id),
                            attributes: doc.node_attributes(id),
                        })
                        .collect(),
                    None => Vec::new(),
                };
                let _ = response_tx.send(JsResponse::QueryResult { nodes });
            }
            JsCommand::GetDocumentSnapshot { url, response_tx } => {
                let snap = {
                    let guard = render_doc_cell.borrow();
                    guard.as_ref().map(|doc| {
                        let title = doc
                            .query_selector("title")
                            .map(|id| doc.node_text(id))
                            .unwrap_or_default();
                        crate::js::dom_snapshot::DomSnapshot::from_render_document(
                            doc, &url, &title,
                        )
                    })
                };
                match snap {
                    Some(s) => {
                        let _ = response_tx.send(JsResponse::Snapshot(Box::new(s)));
                    }
                    None => {
                        let _ = response_tx.send(JsResponse::Error {
                            message: "no render document set".into(),
                        });
                    }
                }
            }
            JsCommand::SetFrameDocument {
                context_id,
                html,
                base_url,
                viewport: vp,
                scripts,
                nav_loop_limit,
                nav_recursion_limit,
                nav_stack_limit,
                nav_timeout_ms,
                response_tx,
            } => {
                ACTIVE_CONTEXT_ID.set(context_id);
                let child_dom_snapshot: Arc<RwLock<Option<DomSnapshot>>> =
                    Arc::new(RwLock::new(None));
                let child_render_doc: Rc<RefCell<Option<RenderDocument>>> =
                    Rc::new(RefCell::new(None));
                let (mut child_ctx, child_jq) = create_context(
                    &console_output,
                    &child_dom_snapshot,
                    &mutations,
                    vp,
                    &base_url,
                    &user_agent,
                    &fetch_tx_arc,
                    &cookie_jar_arc,
                    &child_render_doc,
                );
                // Build the child frame's RenderDocument + run scripts.
                let boar_vp = Viewport {
                    width: vp.0.max(64),
                    height: vp.1.max(64),
                    scale: 1.0,
                };
                match RenderDocument::from_html(&html, Some(&base_url), boar_vp) {
                    Ok(doc) => {
                        *child_render_doc.borrow_mut() = Some(doc);
                        crate::js::dom_snapshot::clear_shadow_roots();
                        {
                            let mut guard = child_render_doc.borrow_mut();
                            if let Some(doc) = guard.as_mut() {
                                process_declarative_shadow_dom(doc);
                            }
                        }
                        {
                            let guard = child_render_doc.borrow();
                            if let Some(doc) = guard.as_ref() {
                                let title = doc
                                    .query_selector("title")
                                    .map(|id| doc.node_text(id))
                                    .unwrap_or_default();
                                let snap =
                                    crate::js::dom_snapshot::DomSnapshot::from_render_document(
                                        doc, &base_url, &title,
                                    );
                                *child_dom_snapshot.write() = Some(snap);
                            }
                        }
                        // Contained like the Eval arm (same panic class).
                        let nav = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            run_navigation_scripts(
                                &mut child_ctx,
                                &child_jq,
                                &scripts,
                                nav_loop_limit,
                                nav_recursion_limit,
                                nav_stack_limit,
                                nav_timeout_ms,
                            );
                        }));
                        // Register the frame even when scripts panicked: the
                        // document itself is intact and the context id stays
                        // resolvable for later commands.
                        child_frames.insert(
                            context_id,
                            ChildFrame {
                                ctx: child_ctx,
                                job_queue: child_jq,
                                render_doc_cell: child_render_doc,
                                dom_snapshot_arc: child_dom_snapshot,
                            },
                        );
                        match nav {
                            Ok(()) => {
                                let _ = response_tx.send(JsResponse::Done);
                            }
                            Err(payload) => {
                                let _ = response_tx.send(JsResponse::Error {
                                    message: format!(
                                        "native binding panicked during navigation scripts: {}",
                                        panic_message(payload.as_ref())
                                    ),
                                });
                            }
                        }
                    }
                    Err(e) => {
                        let _ = response_tx.send(JsResponse::Error {
                            message: e.to_string(),
                        });
                    }
                }
                ACTIVE_CONTEXT_ID.set(1);
            }
            JsCommand::ClearChildContexts { response_tx } => {
                child_frames.clear();
                let _ = response_tx.send(JsResponse::Done);
            }
            JsCommand::Shutdown => {
                break;
            }
        }
    }
}

// Create a fresh boa_engine Context with console.log/warn/error/info
// and `document` object registered.
// ---------------------------------------------------------------------------
// Timer drain
// ---------------------------------------------------------------------------

/// Drain all due timers from the job queue and execute their callbacks.
///
/// For interval timers, the callback is re-scheduled with the original interval.
/// After each batch of timer callbacks, we also drain any microtasks they
/// enqueued. Repeats until no more due timers remain (up to a safety limit).
/// When `Runtime.evaluate` is called with `awaitPromise: true`, this function:
/// 1. Checks if the value is a Promise (has a `.then` method)
/// 2. Attaches `.then()`/`.catch()` handlers to capture the settled value
/// 3. Drains the microtask queue repeatedly until the Promise settles
/// 4. Returns the resolved value (or the rejection error as a string)
///
/// If the value is not a Promise, returns it unchanged.
fn await_promise_value(
    value: JsValue,
    ctx: &mut Context,
    job_queue: &Rc<TokioJobQueue>,
) -> JsValue {
    // Check if the value is thenable (has a .then method)
    let is_thenable = value.as_object().is_some_and(|obj| {
        obj.get(js_string!("then"), ctx)
            .ok()
            .and_then(|v| v.as_object().map(|o| o.is_callable()))
            .unwrap_or(false)
    });

    if !is_thenable {
        return value;
    }

    // Set up __promiseResult / __promiseSettled globals
    // then attach .then() and .catch() handlers
    let setup_result = ctx.eval(Source::from_bytes(
        "globalThis.__promiseResult = undefined; globalThis.__promiseSettled = false; globalThis.__promiseError = null;"
    ));
    if setup_result.is_err() {
        return value; // fallback: return the Promise object
    }

    // Store the promise and attach handlers via eval
    let _ = ctx.register_global_property(
        js_string!("__pendingPromise"),
        value.clone(),
        boa_engine::property::Attribute::all(),
    );

    let handler_code = r#"
        (function() {
            var p = globalThis.__pendingPromise;
            p.then(
                function(v) { globalThis.__promiseResult = v; globalThis.__promiseSettled = true; },
                function(e) { globalThis.__promiseError = e instanceof Error ? e.message : String(e); globalThis.__promiseSettled = true; }
            );
        })()
    "#;
    let _ = ctx.eval(Source::from_bytes(handler_code));

    // Drain microtasks repeatedly until the Promise settles
    // (up to 50 iterations to prevent infinite loops)
    for _ in 0..50 {
        ctx.run_jobs();
        drain_timers(job_queue, ctx);

        let settled = ctx
            .global_object()
            .get(js_string!("__promiseSettled"), ctx)
            .ok()
            .and_then(|v| v.as_boolean())
            .unwrap_or(false);

        if settled {
            break;
        }
    }

    // Read the settled value
    let error = ctx
        .global_object()
        .get(js_string!("__promiseError"), ctx)
        .ok();
    let has_error = error
        .as_ref()
        .and_then(|v| v.as_string())
        .map(|s| !s.to_std_string_escaped().is_empty())
        .unwrap_or(false);

    if has_error {
        // Return the error as a string value — the CDP handler will detect
        // this as an exception-like result
        error.unwrap_or(JsValue::undefined())
    } else {
        ctx.global_object()
            .get(js_string!("__promiseResult"), ctx)
            .unwrap_or(value)
    }
}

/// Execute the page's `<script>` tags in document order, fire the load
/// lifecycle, and settle the timer/microtask queue — the Phase 1 keystone.
///
/// Runs entirely on the JS thread against the just-built `RenderDocument`.
/// Uses dedicated (high) nav-script limits, NOT `evaluate()`'s caps, so real
/// SPA bundles are not silently skipped. On budget exhaustion we stop running
/// further scripts but keep partial state (no context reset).
fn run_navigation_scripts(
    ctx: &mut Context,
    job_queue: &Rc<TokioJobQueue>,
    scripts: &[ScriptSource],
    loop_limit: u64,
    recursion_limit: usize,
    stack_limit: usize,
    timeout_ms: u64,
) {
    // No scripts → document is immediately complete (legacy set_document path).
    if scripts.is_empty() {
        DOC_READY_STATE.with(|c| c.set("complete"));
        return;
    }

    let budget = Duration::from_millis(timeout_ms);
    let start = Instant::now();

    // readyState = "interactive": DOM is built, scripts about to run.
    DOC_READY_STATE.with(|c| c.set("interactive"));

    let apply_limits = |ctx: &mut Context| {
        let limits = ctx.runtime_limits_mut();
        limits.set_loop_iteration_limit(loop_limit);
        limits.set_recursion_limit(recursion_limit);
        limits.set_stack_size_limit(stack_limit);
    };

    for script in scripts {
        if start.elapsed() >= budget {
            tracing::warn!(
                elapsed_ms = start.elapsed().as_millis() as u64,
                budget_ms = timeout_ms,
                "nav-script budget exhausted; stopping script execution (state kept)"
            );
            break;
        }
        // External scripts must be fetched by the caller; an empty source with
        // a src_url means it was not retrieved — skip (mirrors onerror).
        if script.source.is_empty() {
            if let Some(src) = &script.src_url {
                tracing::warn!(src, "external script not fetched; skipping");
            }
            continue;
        }
        // Re-arm limits per script: boa's loop counter can accumulate across
        // evals, and a prior script's runaway would otherwise starve siblings.
        apply_limits(ctx);
        if let Err(err) = ctx.eval(Source::from_bytes(script.source.as_str())) {
            let (msg, err_name, err_stack) = error_sink_details(&err, ctx);
            push_event(CoreEvent::Exception {
                message: msg.clone(),
                name: err_name,
                stack: err_stack,
                timestamp: now_ms(),
            });
            tracing::warn!(error = %msg, src = ?script.src_url, "page script threw; continuing");
        }
        // Drain microtasks + already-due timers queued by this script.
        ctx.run_jobs();
        drain_timers(job_queue, ctx);
    }

    // Lifecycle events. DOMContentLoaded fires while readyState is still
    // "interactive"; load fires after it becomes "complete".
    fire_lifecycle_event(ctx, "DOMContentLoaded");
    DOC_READY_STATE.with(|c| c.set("complete"));
    fire_lifecycle_event(ctx, "load");

    // Bootstrap pump: settle timers/microtasks to idle, waiting for the next
    // timer deadline (capped at remaining budget) so e.g. setTimeout(50) fires.
    settle_to_idle(ctx, job_queue, start, budget);
}

/// Dispatch a lifecycle event on `document` and `window` defensively. A
/// missing `dispatchEvent` or thrown handler never propagates to the caller.
fn fire_lifecycle_event(ctx: &mut Context, event_type: &str) {
    let snippet = format!(
        r#"(function () {{
  try {{ document.dispatchEvent(new Event({evt:?})); }} catch (e) {{}}
  try {{ if (window.dispatchEvent) window.dispatchEvent(new Event({evt:?})); }} catch (e) {{}}
}})();"#,
        evt = event_type
    );
    let _ = ctx.eval(Source::from_bytes(snippet.as_str()));
    ctx.run_jobs();
}

/// Pump microtasks and timers until the page is idle: no pending timers and no
/// pending microtasks. If timers are scheduled for the future, sleep on the JS
/// thread until the nearest deadline (capped at `budget` remaining from
/// `start`), then loop. Bounded by 200 passes to guard interval storms.
fn settle_to_idle(
    ctx: &mut Context,
    job_queue: &Rc<TokioJobQueue>,
    start: Instant,
    budget: Duration,
) {
    for _pass in 0..200u32 {
        ctx.run_jobs();
        drain_timers(job_queue, ctx);

        let pending_timers = job_queue.timer_count();
        let pending_microtasks = job_queue.microtask_count();
        let act = active_context_id();
        let pending_fetch =
            PENDING_FETCH.with(|m| m.borrow().values().any(|p| p.context_id() == act));
        let pending_ws =
            PENDING_WS.with(|m| m.borrow().values().any(|s| s.context_id() == Some(act)));
        if pending_timers == 0 && pending_microtasks == 0 && !pending_fetch && !pending_ws {
            return;
        }
        // Only microtasks pending — run_jobs on the next pass drains them.
        if pending_timers == 0 && pending_microtasks > 0 {
            continue;
        }
        let now = Instant::now();
        let remaining = budget.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            tracing::warn!(
                pending_timers,
                pending_fetch,
                "settle budget exhausted; async work remains"
            );
            return;
        }
        // Wait until the next timer deadline, or — with no timers but pending
        // background fetches — poll briefly for their responses.
        let wait = if pending_timers > 0 {
            match job_queue.next_timer_deadline() {
                Some(dl) if dl > now => dl - now,
                _ => Duration::ZERO,
            }
        } else {
            Duration::from_millis(2)
        };
        if !wait.is_zero() {
            std::thread::sleep(wait.min(remaining));
        }
    }
}
/// Settle a pending fetch `Promise` from a background response. Builds the
/// Response object (status/headers/text/json/arrayBuffer) and resolves it —
/// or rejects on a transport error.
fn settle_fetch(resolvers: ResolvingFunctions, resp: FetchResponseMsg, ctx: &mut Context) {
    if let Some(err) = resp.error {
        let err_json =
            serde_json::to_string(&err).unwrap_or_else(|_| "\"fetch failed\"".to_string());
        let err = ctx
            .eval(Source::from_bytes(
                format!("new Error({})", err_json).as_str(),
            ))
            .unwrap_or(JsValue::undefined());
        let _ = resolvers.reject.call(&JsValue::undefined(), &[err], ctx);
        return;
    }

    let text_fn = unsafe {
        NativeFunction::from_closure(move |this, _args, ctx| {
            let body = this
                .as_object()
                .and_then(|o| o.get(js_string!("__body"), ctx).ok())
                .unwrap_or(JsValue::undefined());
            let _ = ctx.register_global_property(js_string!("__text_body"), body, Attribute::all());
            ctx.eval(Source::from_bytes(
                "(() => { const v = __text_body; delete globalThis.__text_body; return Promise.resolve(v); })()",
            ))
        })
    };

    let json_fn = unsafe {
        NativeFunction::from_closure(move |this, _args, ctx| {
            let body = this
                .as_object()
                .and_then(|o| o.get(js_string!("__body"), ctx).ok())
                .unwrap_or(JsValue::undefined());
            let _ = ctx.register_global_property(js_string!("__json_body"), body, Attribute::all());
            ctx.eval(Source::from_bytes(
                "(() => { const v = __json_body; delete globalThis.__json_body; return Promise.resolve(JSON.parse(v)); })()",
            ))
        })
    };

    let array_buffer_fn = unsafe {
        NativeFunction::from_closure(move |this, _args, ctx| {
            let body_owned = {
                if let Some(obj) = this.as_object()
                    && let Ok(v) = obj.get(js_string!("__body"), ctx)
                    && let Some(s) = v.as_string()
                {
                    s.to_std_string_escaped()
                } else {
                    String::new()
                }
            };
            let bytes_json =
                serde_json::to_string(body_owned.as_bytes()).unwrap_or_else(|_| String::from("[]"));
            ctx.eval(Source::from_bytes(
                format!("Promise.resolve(new Uint8Array({}))", bytes_json).as_str(),
            ))
        })
    };

    let headers_obj = boa_engine::object::ObjectInitializer::new(ctx).build();
    for (k, v) in &resp.headers {
        let _ = headers_obj.set(
            JsString::from(k.as_str()),
            JsValue::from(JsString::from(v.as_str())),
            true,
            ctx,
        );
    }

    let response_obj = boa_engine::object::ObjectInitializer::new(ctx)
        .property(
            js_string!("status"),
            JsValue::from(resp.status),
            Attribute::all(),
        )
        .property(
            js_string!("statusText"),
            JsValue::from(JsString::from(resp.status_text.as_str())),
            Attribute::all(),
        )
        .property(
            js_string!("ok"),
            JsValue::from(resp.status < 400),
            Attribute::all(),
        )
        .property(
            js_string!("url"),
            JsValue::from(JsString::from(resp.url.as_str())),
            Attribute::all(),
        )
        .property(
            js_string!("bodyUsed"),
            JsValue::from(false),
            Attribute::all(),
        )
        .property(
            js_string!("type"),
            JsValue::from(JsString::from("basic")),
            Attribute::all(),
        )
        .property(
            js_string!("__body"),
            JsValue::from(JsString::from(resp.body.as_str())),
            Attribute::all(),
        )
        .property(
            js_string!("headers"),
            JsValue::from(headers_obj),
            Attribute::all(),
        )
        .function(text_fn, js_string!("text"), 0)
        .function(json_fn, js_string!("json"), 0)
        .function(array_buffer_fn, js_string!("arrayBuffer"), 0)
        .build();

    let _ = resolvers
        .resolve
        .call(&JsValue::undefined(), &[JsValue::from(response_obj)], ctx);
}

/// Call a JS callback stored in a shared slot, if it is set and callable.
fn fire_callback(slot: &Arc<RwLock<Option<JsValue>>>, ctx: &mut Context) {
    let cb = slot.read().clone();
    if let Some(v) = cb
        && let Some(cb_obj) = v.as_object()
        && cb_obj.is_callable()
    {
        let _ = cb_obj.call(&JsValue::undefined(), &[], ctx);
    }
}

/// Settle a pending XHR: drive the shared state cells through LOADING (3) →
/// DONE (4), firing `onreadystatechange` at each transition and `onload` (or
/// `onerror`) at DONE.
#[allow(clippy::too_many_arguments)]
fn settle_xhr(
    ready_state: Arc<RwLock<f64>>,
    status: Arc<RwLock<f64>>,
    resp_body: Arc<RwLock<String>>,
    resp_hdrs: Arc<RwLock<String>>,
    onload: Arc<RwLock<Option<JsValue>>>,
    onerror: Arc<RwLock<Option<JsValue>>>,
    onrsc: Arc<RwLock<Option<JsValue>>>,
    resp: FetchResponseMsg,
    ctx: &mut Context,
) {
    // LOADING (3)
    *ready_state.write() = 3.0;
    *status.write() = resp.status as f64;
    *resp_body.write() = resp.body.clone();
    let mut hdr_str = String::new();
    for (k, v) in &resp.headers {
        hdr_str.push_str(&format!("{}: {}\r\n", k, v));
    }
    *resp_hdrs.write() = hdr_str;
    fire_callback(&onrsc, ctx);

    // DONE (4)
    *ready_state.write() = 4.0;
    if resp.error.is_none() {
        fire_callback(&onload, ctx);
    } else {
        *status.write() = 0.0;
        fire_callback(&onerror, ctx);
    }
    fire_callback(&onrsc, ctx);
}

/// Drain all currently-available fetch/XHR responses from the shared channel
/// and settle their pending entries (Phase 3). Called at every event-loop pump
/// site so responses resolve during script execution, the bootstrap pump, and
/// top-level `evaluate()`. Responses are collected before settling to release
/// the channel borrow before re-entering boa (settling may issue more JS/fetch).
fn drain_pending_fetch_responses(ctx: &mut Context) {
    let active = active_context_id();
    // Abort pass: reject any pending fetch whose AbortSignal has fired since
    // the last pump. Only checks the active context's fetches — other frames'
    // aborts are handled when they next pump (Phase 8).
    let aborted_ids: Vec<u64> = PENDING_FETCH
        .with(|m| {
            m.borrow()
                .iter()
                .filter(|(_, pf)| pf.context_id() == active)
                .filter_map(|(id, pf)| match pf {
                    PendingFetch::Fetch { signal, .. } => signal.clone().map(|s| (*id, s)),
                    PendingFetch::Xhr { .. } => None,
                })
                .collect::<Vec<_>>()
        })
        .into_iter()
        .filter_map(|(id, sig)| {
            sig.get(js_string!("aborted"), ctx)
                .ok()
                .and_then(|v| v.as_boolean())
                .filter(|&b| b)
                .map(|_| id)
        })
        .collect();
    for id in aborted_ids {
        if let Some(PendingFetch::Fetch { resolvers, .. }) =
            PENDING_FETCH.with(|m| m.borrow_mut().remove(&id))
        {
            let err = ctx
                .eval(Source::from_bytes(
                    "(()=>{const e=new Error('The user aborted a request.');\
                     e.name='AbortError';return e;})()",
                ))
                .unwrap_or(JsValue::undefined());
            let _ = resolvers.reject.call(&JsValue::undefined(), &[err], ctx);
        }
    }

    // Collect deferred responses (from a previous context's pump) + fresh
    // channel responses, then settle only those belonging to the active
    // context. Others are re-deferred for their owning context's next pump.
    let mut responses: Vec<FetchResponseMsg> =
        DEFERRED_RESPONSES.with(|d| d.borrow_mut().drain(..).collect());
    responses.extend(RESPONSE_RX.with(|cell| {
        let mut borrowed = cell.borrow_mut();
        let Some(rx) = borrowed.as_mut() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        while let Ok(resp) = rx.try_recv() {
            out.push(resp);
        }
        out
    }));

    let mut to_defer = Vec::new();
    for resp in responses {
        // Check whether this response belongs to the active context.
        let belongs_here = PENDING_FETCH
            .with(|m| m.borrow().get(&resp.id).map(|p| p.context_id()) == Some(active));
        if !belongs_here {
            // Entry missing → stale, discard. Entry for another context → defer.
            if PENDING_FETCH.with(|m| m.borrow().contains_key(&resp.id)) {
                to_defer.push(resp);
            } else {
                tracing::trace!(id = resp.id, "stale fetch response — no pending entry");
            }
            continue;
        }
        // Emit Network.responseReceived + loadingFinished before `resp` is
        // moved into settle_fetch/settle_xhr.
        if event_sink_attached() {
            let rid = cdp_request_id(resp.id);
            push_event(CoreEvent::FetchResponse {
                request_id: rid.clone(),
                url: resp.url.clone(),
                status: resp.status,
                mime_type: content_type_mime(&resp.headers),
                timestamp: now_ms(),
            });
            push_event(CoreEvent::FetchLoadingFinished {
                request_id: rid,
                timestamp: now_ms(),
            });
        }
        let entry = PENDING_FETCH.with(|m| m.borrow_mut().remove(&resp.id));
        match entry {
            Some(PendingFetch::Fetch { resolvers, .. }) => settle_fetch(resolvers, resp, ctx),
            Some(PendingFetch::Xhr {
                ready_state,
                status,
                resp_body,
                resp_hdrs,
                onload,
                onerror,
                onrsc,
                ..
            }) => settle_xhr(
                ready_state,
                status,
                resp_body,
                resp_hdrs,
                onload,
                onerror,
                onrsc,
                resp,
                ctx,
            ),
            None => {
                tracing::trace!(id = resp.id, "stale fetch response — no pending entry");
            }
        }
    }
    if !to_defer.is_empty() {
        DEFERRED_RESPONSES.with(|d| d.borrow_mut().extend(to_defer));
    }
    // Flush microtasks scheduled by abort/response settlement (the `.catch`/
    // `.then` reactions) within this pump. Without it, an abort that empties
    // `pending_fetch` suppresses the settle_to_idle loop and leaves the
    // rejection reaction un-drained until the next evaluate.
    ctx.run_jobs();
}
/// Drain all available WebSocket events and fire the matching JS callbacks.
/// Collects into a Vec first to release the `WS_EVENT_RX` borrow before
/// re-entering boa (settling may issue more JS / WS / fetch).
fn drain_ws_events(ctx: &mut Context) {
    let active = active_context_id();
    // Collect deferred events + fresh channel events.
    let mut events: Vec<WsEvent> = DEFERRED_WS_EVENTS.with(|d| d.borrow_mut().drain(..).collect());
    events.extend(WS_EVENT_RX.with(|cell| {
        let mut borrowed = cell.borrow_mut();
        let Some(rx) = borrowed.as_mut() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }));
    let mut to_defer = Vec::new();
    for ev in events {
        // Only settle WS events whose owning socket belongs to the active
        // context. Others are deferred for their owning context's next pump.
        let id = match &ev {
            WsEvent::Open { id, .. }
            | WsEvent::Message { id, .. }
            | WsEvent::Close { id, .. }
            | WsEvent::Error { id, .. } => *id,
        };
        let belongs_here =
            PENDING_WS.with(|m| m.borrow().get(&id).and_then(|s| s.context_id()) == Some(active));
        if !belongs_here {
            // Socket not found (stale) → discard; belongs to another context → defer.
            if PENDING_WS.with(|m| m.borrow().contains_key(&id)) {
                to_defer.push(ev);
            }
            continue;
        }
        match ev {
            WsEvent::Open {
                id,
                protocol,
                extensions,
            } => settle_ws_open(id, protocol, extensions, ctx),
            WsEvent::Message { id, data } => settle_ws_message(id, data, ctx),
            WsEvent::Close {
                id,
                code,
                reason,
                was_clean,
            } => settle_ws_close(id, code, reason, was_clean, ctx),
            WsEvent::Error { id, message } => settle_ws_error(id, message, ctx),
        }
    }
    if !to_defer.is_empty() {
        DEFERRED_WS_EVENTS.with(|d| d.borrow_mut().extend(to_defer));
    }
}

/// Fire the on-property + the hidden `__listeners_<type>` array for one event.
fn ws_fire(obj: &JsObject, type_name: &str, event: JsValue, ctx: &mut Context) {
    let type_key = JsString::from(format!("on{type_name}").as_str());
    // on-property
    if let Ok(cb) = obj.get(type_key.clone(), ctx)
        && !cb.is_null()
        && !cb.is_undefined()
        && let Some(f) = cb.as_object()
        && f.is_callable()
    {
        let _ = f.call(&JsValue::undefined(), std::slice::from_ref(&event), ctx);
    }
    // listener vec
    let lkey = JsString::from(format!("__listeners_{type_name}").as_str());
    if let Ok(arr_val) = obj.get(lkey, ctx)
        && let Some(arr_obj) = arr_val.as_object()
        && let Ok(arr) = JsArray::from_object(arr_obj.clone())
        && let Ok(len) = arr.length(ctx)
    {
        for i in 0..len {
            if let Ok(cb) = arr.get(i, ctx)
                && let Some(f) = cb.as_object()
                && f.is_callable()
            {
                let _ = f.call(&JsValue::undefined(), std::slice::from_ref(&event), ctx);
            }
        }
    }
}
fn settle_ws_open(id: u64, protocol: String, extensions: String, ctx: &mut Context) {
    let obj = PENDING_WS.with(|m| {
        m.borrow().get(&id).and_then(|s| match s {
            WsState::Live { obj, .. } => Some(obj.clone()),
            _ => None,
        })
    });
    let Some(obj) = obj else {
        return;
    };
    let _ = obj.set(js_string!("readyState"), JsValue::from(1), true, ctx);
    let _ = obj.set(
        js_string!("protocol"),
        JsValue::from(JsString::from(protocol.as_str())),
        true,
        ctx,
    );
    let _ = obj.set(
        js_string!("extensions"),
        JsValue::from(JsString::from(extensions.as_str())),
        true,
        ctx,
    );
    let event = boa_engine::object::ObjectInitializer::new(ctx)
        .property(
            js_string!("type"),
            JsValue::from(JsString::from("open")),
            Attribute::all(),
        )
        .property(
            js_string!("target"),
            JsValue::from(obj.clone()),
            Attribute::all(),
        )
        .build();
    ws_fire(&obj, "open", event.into(), ctx);
}

fn settle_ws_message(id: u64, data: WsData, ctx: &mut Context) {
    let obj = PENDING_WS.with(|m| {
        m.borrow().get(&id).and_then(|s| match s {
            WsState::Live { obj, .. } => Some(obj.clone()),
            _ => None,
        })
    });
    let Some(obj) = obj else {
        return;
    };
    if event_sink_attached() {
        let (opcode, payload) = match &data {
            WsData::Text(t) => (1u8, t.clone()),
            WsData::Binary(b) => (2u8, base64::engine::general_purpose::STANDARD.encode(b)),
        };
        push_event(CoreEvent::WsFrame {
            direction: WsDirection::Received,
            request_id: format!("oxi-ws-{id}"),
            opcode,
            data: payload,
            timestamp: now_ms(),
        });
    }
    let data_val = match data {
        WsData::Text(t) => JsValue::from(JsString::from(t.as_str())),
        WsData::Binary(b) => {
            // Binary arrives as an array of byte values (binaryType-driven
            // ArrayBuffer construction is refined in the binary test task).
            let arr = JsArray::new(ctx);
            for &byte in &b {
                let _ = arr.push(JsValue::from(byte), ctx);
            }
            JsValue::from(arr)
        }
    };
    let event = boa_engine::object::ObjectInitializer::new(ctx)
        .property(
            js_string!("type"),
            JsValue::from(JsString::from("message")),
            Attribute::all(),
        )
        .property(js_string!("data"), data_val, Attribute::all())
        .property(
            js_string!("target"),
            JsValue::from(obj.clone()),
            Attribute::all(),
        )
        .build();
    ws_fire(&obj, "message", event.into(), ctx);
}

/// Best-effort extraction of a byte buffer from a JS value, for WebSocket
/// binary `send()`. Recognises ArrayBuffer / TypedArray / a plain `Array` of
/// numbers — anything exposing a numeric `length` (or `byteLength`) whose
/// indexed elements are numbers. Returns `None` for strings/other values.
fn extract_binary_bytes(v: &JsValue, ctx: &mut Context) -> Option<Vec<u8>> {
    let obj = v.as_object()?;
    let len = obj
        .get(js_string!("length"), ctx)
        .ok()
        .and_then(|l| l.as_number())
        .or_else(|| {
            obj.get(js_string!("byteLength"), ctx)
                .ok()
                .and_then(|l| l.as_number())
        })?;
    let n = len as usize;
    if n == 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(n);
    for i in 0..n {
        let val = obj.get(i as u32, ctx).ok()?;
        bytes.push(val.as_number()? as u8);
    }
    Some(bytes)
}

/// Normalize a fetch/XHR body argument into bytes + an optional auto Content-Type.
/// Plain strings → UTF-8 bytes; `FormData` → `multipart/form-data`; `Blob` → raw
/// bytes with the blob's `type`. Returns `None` for no body.
fn normalize_fetch_body(value: &JsValue, ctx: &mut Context) -> Option<(Vec<u8>, Option<String>)> {
    if value.is_undefined() || value.is_null() {
        return None;
    }
    if let Some(s) = value.as_string() {
        return Some((s.to_std_string_escaped().into_bytes(), None));
    }
    // FormData / Blob via the JS serializer installed by FORMDATA_BLOB_BOOTSTRAP.
    let serializer = ctx
        .global_object()
        .get(js_string!("__oxi_serialize_body"), ctx)
        .ok()?;
    let callable = serializer.as_callable()?;
    let res = callable
        .call(&JsValue::undefined(), std::slice::from_ref(value), ctx)
        .ok()?;
    let obj = res.as_object()?;
    let content_type = match obj.get(js_string!("contentType"), ctx) {
        Ok(v) => match v.as_string() {
            Some(s) if !s.is_empty() => Some(s.to_std_string_escaped()),
            _ => None,
        },
        Err(_) => None,
    };
    let bytes_val = obj
        .get(js_string!("bytes"), ctx)
        .unwrap_or(JsValue::undefined());
    let bytes = extract_binary_bytes(&bytes_val, ctx).or_else(|| {
        let tv = obj.get(js_string!("text"), ctx).ok()?;
        let s = tv.as_string()?;
        Some(s.to_std_string_escaped().into_bytes())
    })?;
    Some((bytes, content_type))
}

/// Upgrade a freshly-created element via the `globalThis.__oxi_upgrade_custom`
/// helper installed by `WEB_COMPONENTS_BOOTSTRAP`: if its tag is a registered
/// custom element, apply the constructor's prototype + body. No-op otherwise.
fn upgrade_custom_element(value: JsValue, ctx: &mut Context) -> JsValue {
    if let Ok(helper) = ctx
        .global_object()
        .get(js_string!("__oxi_upgrade_custom"), ctx)
        && let Some(callable) = helper.as_callable()
        && let Ok(upgraded) =
            callable.call(&JsValue::undefined(), std::slice::from_ref(&value), ctx)
    {
        return upgraded;
    }
    value
}

/// Call a `globalThis.__oxi_*` helper (installed by a bootstrap) with `args`,
/// swallowing any error. Used to fire custom-element lifecycle callbacks
/// (`__oxi_fire_connected` / `__oxi_fire_disconnected` / `__oxi_fire_attr_changed`)
/// from the native appendChild / remove / setAttribute hooks.
fn call_global_helper(ctx: &mut Context, name: &str, args: &[JsValue]) {
    if let Ok(helper) = ctx.global_object().get(JsString::from(name), ctx)
        && let Some(callable) = helper.as_callable()
    {
        let _ = callable.call(&JsValue::undefined(), args, ctx);
    }
}

fn settle_ws_close(id: u64, code: u16, reason: String, was_clean: bool, ctx: &mut Context) {
    let obj = PENDING_WS.with(|m| {
        let mut borrowed = m.borrow_mut();
        let obj = borrowed.get(&id).and_then(|s| match s {
            WsState::Live { obj, .. } => Some(obj.clone()),
            _ => None,
        });
        borrowed.insert(id, WsState::Closed);
        obj
    });
    let Some(obj) = obj else {
        return;
    };
    let _ = obj.set(js_string!("readyState"), JsValue::from(3), true, ctx);
    let event = boa_engine::object::ObjectInitializer::new(ctx)
        .property(
            js_string!("type"),
            JsValue::from(JsString::from("close")),
            Attribute::all(),
        )
        .property(js_string!("code"), JsValue::from(code), Attribute::all())
        .property(
            js_string!("reason"),
            JsValue::from(JsString::from(reason.as_str())),
            Attribute::all(),
        )
        .property(
            js_string!("wasClean"),
            JsValue::from(was_clean),
            Attribute::all(),
        )
        .property(
            js_string!("target"),
            JsValue::from(obj.clone()),
            Attribute::all(),
        )
        .build();
    ws_fire(&obj, "close", event.into(), ctx);
}

fn settle_ws_error(id: u64, _message: String, ctx: &mut Context) {
    let obj = PENDING_WS.with(|m| {
        m.borrow().get(&id).and_then(|s| match s {
            WsState::Live { obj, .. } => Some(obj.clone()),
            _ => None,
        })
    });
    let Some(obj) = obj else {
        return;
    };
    let event = boa_engine::object::ObjectInitializer::new(ctx)
        .property(
            js_string!("type"),
            JsValue::from(JsString::from("error")),
            Attribute::all(),
        )
        .property(
            js_string!("target"),
            JsValue::from(obj.clone()),
            Attribute::all(),
        )
        .build();
    ws_fire(&obj, "error", event.into(), ctx);
}
fn drain_timers(queue: &Rc<TokioJobQueue>, ctx: &mut Context) {
    // Settle any fetch/XHR responses first — they may enqueue microtasks or
    // timers that the rest of this drain must then process (Phase 3).
    drain_pending_fetch_responses(ctx);
    drain_ws_events(ctx);
    let mut iterations = 0u32;
    loop {
        let due = queue.pop_due_timers();
        if due.is_empty() {
            break;
        }

        for timer in due {
            let _ = timer.callback.call(&JsValue::undefined(), &timer.args, ctx);

            // Re-schedule interval timers
            if timer.is_interval {
                let interval_ms = timer.interval_ms.unwrap_or(0).max(1);
                let deadline = Instant::now() + Duration::from_millis(interval_ms);
                queue.schedule_timer(
                    deadline,
                    timer.callback,
                    timer.args,
                    true,
                    Some(interval_ms),
                );
            }
        }

        // Timer callbacks may have queued microtasks — drain those too
        ctx.run_jobs();

        iterations += 1;
        if iterations > 100 {
            // Safety limit to prevent infinite timer loops
            break;
        }
    }
}

/// Push a mutation record to all active MutationObservers.
fn notify_mutation_observers(ctx: &mut Context, mutation_type: &str, target_id: u32) {
    let registry = ctx.global_object().get(js_string!("__moRegistry"), ctx);
    if let Ok(reg_val) = registry
        && let Some(reg_obj) = reg_val.as_object()
        && let Ok(reg_arr) = JsArray::from_object(reg_obj.clone())
        && let Ok(len) = reg_arr.length(ctx)
    {
        for i in 0..len {
            if let Ok(observer_val) = reg_arr.at(i as i64, ctx)
                && let Some(obs_obj) = observer_val.as_object()
            {
                let observing = obs_obj
                    .get(js_string!("__observing"), ctx)
                    .ok()
                    .and_then(|v| v.as_boolean())
                    .unwrap_or(false);
                if observing {
                    // Create MutationRecord
                    let record = boa_engine::object::ObjectInitializer::new(ctx)
                        .property(
                            js_string!("type"),
                            JsValue::from(JsString::from(mutation_type)),
                            Attribute::all(),
                        )
                        .property(
                            js_string!("target"),
                            JsValue::from(target_id),
                            Attribute::all(),
                        )
                        .build();
                    // Push to __records
                    let records_val = obs_obj
                        .get(js_string!("__records"), ctx)
                        .unwrap_or(JsValue::Null);
                    if let Some(rec_obj) = records_val.as_object()
                        && let Ok(rec_arr) = JsArray::from_object(rec_obj.clone())
                    {
                        let _ = rec_arr.push(JsValue::from(record), ctx);
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn create_context(
    output: &Arc<RwLock<Vec<String>>>,
    dom_snapshot: &Arc<RwLock<Option<DomSnapshot>>>,
    mutations: &Arc<RwLock<Vec<DomMutation>>>,
    viewport: (u32, u32),
    page_url: &str,
    user_agent: &str,
    fetch_tx_arc: &Arc<RwLock<Option<std::sync::mpsc::Sender<FetchRequestMsg>>>>,
    cookie_jar_arc: &Arc<RwLock<Option<Arc<RwLock<CookieJar>>>>>,
    render_doc_cell: &Rc<RefCell<Option<RenderDocument>>>,
) -> (Context, Rc<TokioJobQueue>) {
    let job_queue = Rc::new(TokioJobQueue::new());
    let mut context = Context::builder()
        .job_queue(job_queue.clone())
        .build()
        .expect("failed to build boa Engine context");

    // --- Console functions ---

    macro_rules! console_fn {
        ($out:expr, $level:expr) => {
            unsafe {
                NativeFunction::from_closure(
                    move |_this: &JsValue, args: &[JsValue], ctx: &mut Context| {
                        let mut strs: Vec<String> = Vec::with_capacity(args.len());
                        let mut typed: Vec<ConsoleArg> = Vec::with_capacity(args.len());
                        for arg in args.iter() {
                            let s = arg
                                .to_string(ctx)
                                .map(|s| s.to_std_string_escaped())
                                .unwrap_or_else(|_| "undefined".to_string());
                            typed.push(classify_console_arg(arg, &s, ctx));
                            strs.push(s);
                        }
                        let line = strs.join(" ");
                        {
                            let mut guard = $out.write();
                            guard.push(line);
                        }
                        // Mirror to the CoreEvent sink (CDP Runtime.consoleAPICalled /
                        // Log.entryAdded). No-op when no observer is attached.
                        push_event(CoreEvent::Console {
                            level: $level,
                            args: typed,
                            timestamp: now_ms(),
                        });
                        Ok(JsValue::undefined())
                    },
                )
            }
        };
    }

    let out_log = output.clone();
    let out_warn = output.clone();
    let out_error = output.clone();
    let out_info = output.clone();

    let log_fn = console_fn!(out_log, ConsoleLevel::Log);

    // Register standalone `log(...)` function
    let _ = context.register_global_callable(js_string!("log"), 1, log_fn.clone());

    // Build console object
    let console = boa_engine::object::ObjectInitializer::new(&mut context)
        .function(log_fn, js_string!("log"), 1)
        .function(
            console_fn!(out_warn, ConsoleLevel::Warn),
            js_string!("warn"),
            1,
        )
        .function(
            console_fn!(out_error, ConsoleLevel::Error),
            js_string!("error"),
            1,
        )
        .function(
            console_fn!(out_info, ConsoleLevel::Info),
            js_string!("info"),
            1,
        )
        .build();

    let _ = context.register_global_property(js_string!("console"), console, Attribute::all());

    // --- Timer functions (scheduled via TokioJobQueue) ---
    //
    // setTimeout(fn, delay, ...args) — schedules callback via schedule_timer().
    //   The callback fires on the next timer drain (after eval returns).
    // setInterval(fn, delay)        — same, but re-schedules after each firing.
    // clearTimeout / clearInterval   — cancels the timer by ID.

    let timer_queue_st = job_queue.clone();
    let set_timeout_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            if args.is_empty() {
                return Ok(JsValue::undefined());
            }
            let callback = args[0].clone();
            let delay_ms = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as u64;
            let cb_args: Vec<JsValue> = args[2..].to_vec();

            if let Some(func) = callback.as_object().cloned()
                && func.is_callable()
            {
                let deadline = Instant::now() + Duration::from_millis(delay_ms);
                let id = timer_queue_st.schedule_timer(deadline, func, cb_args, false, None);
                return Ok(JsValue::from(id as f64));
            }
            Ok(JsValue::undefined())
        })
    };

    let timer_queue_si = job_queue.clone();
    let set_interval_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            if args.is_empty() {
                return Ok(JsValue::undefined());
            }
            let callback = args[0].clone();
            let delay_ms = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as u64;
            let cb_args: Vec<JsValue> = args[2..].to_vec();

            if let Some(func) = callback.as_object().cloned()
                && func.is_callable()
            {
                let deadline = Instant::now() + Duration::from_millis(delay_ms);
                let id =
                    timer_queue_si.schedule_timer(deadline, func, cb_args, true, Some(delay_ms));
                return Ok(JsValue::from(id as f64));
            }
            Ok(JsValue::undefined())
        })
    };

    let timer_queue_ct = job_queue.clone();
    let clear_timer_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            if let Some(id) = args.first().and_then(|v| v.as_number()) {
                timer_queue_ct.cancel_timer(id as u64);
            }
            Ok(JsValue::undefined())
        })
    };

    let _ = context.register_global_callable(js_string!("setTimeout"), 2, set_timeout_fn);
    let _ = context.register_global_callable(js_string!("setInterval"), 2, set_interval_fn);
    let _ = context.register_global_callable(js_string!("clearTimeout"), 1, clear_timer_fn.clone());
    let _ = context.register_global_callable(js_string!("clearInterval"), 1, clear_timer_fn);

    // --- fetch() implementation ---
    //
    // Makes real HTTP requests via the HttpClient in the main session.
    // The JS thread sends FetchRequestMsg to the main thread via channel,
    // then blocks waiting for the response.
    //
    // Returns a JS Promise that resolves with the Response object.

    let fetch_tx_inner = fetch_tx_arc.clone();

    let fetch_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            // Get URL and options from arguments
            let url = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();

            // Extract method and options from second argument
            let mut method = String::from("GET");
            let mut headers: Vec<(String, String)> = Vec::new();
            if let Some(opts) = args.get(1).and_then(|v| v.as_object())
                && let Ok(hdrs) = opts.get(js_string!("headers"), ctx)
                && let Some(hdr_obj) = hdrs.as_object()
            {
                for &key in &[
                    "content-type",
                    "accept",
                    "authorization",
                    "user-agent",
                    "cookie",
                ] {
                    if let Ok(val) = hdr_obj.get(js_string!(key), ctx)
                        && !val.is_undefined()
                        && !val.is_null()
                        && let Some(s) = val.as_string()
                    {
                        headers.push((key.to_string(), s.to_std_string_escaped()));
                    }
                }
            }

            let mut body: Option<Vec<u8>> = None;
            let mut _timeout_ms: Option<u64> = None;

            if args.len() > 1
                && let Some(opts) = args[1].as_object()
            {
                // method
                if let Ok(m) = opts.get(js_string!("method"), ctx)
                    && let Some(s) = m.as_string()
                {
                    method = s.to_std_string_escaped().to_uppercase();
                }
                // headers (simplified — just extract common ones)
                // Full header iteration via enumerate() skipped for simplicity
                // since boa 0.20's JsIterator API requires careful handling
                // body: FormData/Blob via normalize, else a plain string.
                if let Ok(b) = opts.get(js_string!("body"), ctx)
                    && !b.is_undefined()
                    && !b.is_null()
                    && let Some((bytes, ct)) = normalize_fetch_body(&b, ctx)
                {
                    body = Some(bytes);
                    if let Some(ct) = ct
                        && !headers
                            .iter()
                            .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                    {
                        headers.push(("content-type".to_string(), ct));
                    }
                }
                // timeout
                if let Ok(t) = opts.get(js_string!("timeout"), ctx)
                    && let Some(n) = t.as_number()
                {
                    _timeout_ms = Some(n as u64);
                }
            }
            // AbortSignal: if `signal` is present and already aborted, reject
            // the returned promise with an AbortError WITHOUT dispatching the
            // request (the standard "aborted at call time" path).
            let signal_obj: Option<JsObject> = args
                .get(1)
                .and_then(|v| v.as_object())
                .and_then(|opts| opts.get(js_string!("signal"), ctx).ok())
                .and_then(|v| v.as_object().cloned());
            if let Some(sig) = &signal_obj
                && let Ok(aborted) = sig.get(js_string!("aborted"), ctx)
                && aborted.as_boolean() == Some(true)
            {
                let (promise, resolvers) = JsPromise::new_pending(ctx);
                let err = ctx
                    .eval(Source::from_bytes(
                        "(()=>{const e=new Error('The user aborted a request.');\
                         e.name='AbortError';return e;})()",
                    ))
                    .unwrap_or(JsValue::undefined());
                let _ = resolvers.reject.call(&JsValue::undefined(), &[err], ctx);
                return Ok(promise.into());
            }

            // Dispatch asynchronously (Phase 3): return a pending Promise now
            // and settle it on the event loop when the response arrives. The JS
            // thread never blocks on the network.
            let (promise, resolvers) = JsPromise::new_pending(ctx);
            let id = next_fetch_id();
            // Emit Network.requestWillBeSent to the CoreEvent sink. Gated to
            // avoid cloning the (potentially large) body when no observer is
            // attached (e.g. the CLI `fetch` path).
            if event_sink_attached() {
                push_event(CoreEvent::FetchRequest {
                    request_id: cdp_request_id(id),
                    url: url.clone(),
                    method: method.clone(),
                    headers: headers.clone(),
                    post_data: body.clone(),
                    timestamp: now_ms(),
                });
            }

            let request = FetchRequestMsg {
                id,
                url,
                method,
                headers,
                body,
                origin: current_origin(),
            };

            let tx = {
                let guard = fetch_tx_inner.read();
                match guard.as_ref() {
                    Some(t) => t.clone(),
                    None => {
                        // No fetch channel — reject the real pending Promise.
                        let msg = "fetch() is not available — channel not set";
                        let msg_json = serde_json::to_string(msg)
                            .unwrap_or_else(|_| "\"fetch unavailable\"".to_string());
                        let err = ctx
                            .eval(Source::from_bytes(
                                format!("new Error({})", msg_json).as_str(),
                            ))
                            .unwrap_or(JsValue::undefined());
                        let _ = resolvers.reject.call(&JsValue::undefined(), &[err], ctx);
                        return Ok(promise.into());
                    }
                }
            };

            if let Err(e) = tx.send(request) {
                // Channel closed — reject the pending Promise.
                let msg = format!("fetch channel error: {}", e);
                let msg_json = serde_json::to_string(&msg)
                    .unwrap_or_else(|_| "\"fetch channel error\"".to_string());
                let err = ctx
                    .eval(Source::from_bytes(
                        format!("new Error({})", msg_json).as_str(),
                    ))
                    .unwrap_or(JsValue::undefined());
                let _ = resolvers.reject.call(&JsValue::undefined(), &[err], ctx);
                return Ok(promise.into());
            }

            // Register the resolver; `settle_fetch` resolves/rejects it when the
            // response arrives on the shared channel (drained by the pump).
            PENDING_FETCH.with(|m| {
                m.borrow_mut().insert(
                    id,
                    PendingFetch::Fetch {
                        resolvers,
                        signal: signal_obj,
                        context_id: active_context_id(),
                    },
                );
            });

            Ok(promise.into())
        })
    };

    let _ = context.register_global_callable(js_string!("fetch"), 2, fetch_fn);

    // --- XMLHttpRequest ---
    let xhr_fetch_tx = fetch_tx_arc.clone();
    let xhr_ctor = unsafe {
        NativeFunction::from_closure(move |_this, _args, ctx| {
            let open_method: Arc<RwLock<String>> = Arc::new(RwLock::new("GET".to_string()));
            let open_url: Arc<RwLock<String>> = Arc::new(RwLock::new(String::new()));
            let open_async: Arc<RwLock<bool>> = Arc::new(RwLock::new(true));
            let ready_state: Arc<RwLock<f64>> = Arc::new(RwLock::new(0.0)); // UNSENT
            let status_val: Arc<RwLock<f64>> = Arc::new(RwLock::new(0.0));
            let response_text: Arc<RwLock<String>> = Arc::new(RwLock::new(String::new()));
            let response_headers: Arc<RwLock<String>> = Arc::new(RwLock::new(String::new()));

            // Event handler callbacks
            let onload_cb: Arc<RwLock<Option<JsValue>>> = Arc::new(RwLock::new(None));
            let onerror_cb: Arc<RwLock<Option<JsValue>>> = Arc::new(RwLock::new(None));
            let onreadystatechange_cb: Arc<RwLock<Option<JsValue>>> = Arc::new(RwLock::new(None));

            // onload setter
            let onload_set = onload_cb.clone();
            let onload_setter = {
                NativeFunction::from_closure(move |_this, args, _ctx| {
                    if let Some(v) = args.first() {
                        *onload_set.write() = Some(v.clone());
                    }
                    Ok(JsValue::undefined())
                })
            };
            let onload_setter_fn = FunctionObjectBuilder::new(ctx.realm(), onload_setter)
                .name(js_string!("set onload"))
                .build();

            // onerror setter
            let onerror_set = onerror_cb.clone();
            let onerror_setter = {
                NativeFunction::from_closure(move |_this, args, _ctx| {
                    if let Some(v) = args.first() {
                        *onerror_set.write() = Some(v.clone());
                    }
                    Ok(JsValue::undefined())
                })
            };
            let onerror_setter_fn = FunctionObjectBuilder::new(ctx.realm(), onerror_setter)
                .name(js_string!("set onerror"))
                .build();

            // onreadystatechange setter
            let onrsc_set = onreadystatechange_cb.clone();
            let onrsc_setter = {
                NativeFunction::from_closure(move |_this, args, _ctx| {
                    if let Some(v) = args.first() {
                        *onrsc_set.write() = Some(v.clone());
                    }
                    Ok(JsValue::undefined())
                })
            };
            let onrsc_setter_fn = FunctionObjectBuilder::new(ctx.realm(), onrsc_setter)
                .name(js_string!("set onreadystatechange"))
                .build();

            // .open(method, url, async)
            let om = open_method.clone();
            let ou = open_url.clone();
            let oa = open_async.clone();
            let rs = ready_state.clone();
            let open_fn = {
                NativeFunction::from_closure(move |_this, args, ctx| {
                    let method = args
                        .first()
                        .and_then(|v| v.to_string(ctx).ok())
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    let url = args
                        .get(1)
                        .and_then(|v| v.to_string(ctx).ok())
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    let async_flag = args.get(2).and_then(|v| v.as_boolean()).unwrap_or(true);
                    *om.write() = method;
                    *ou.write() = url;
                    *oa.write() = async_flag;
                    *rs.write() = 1.0; // OPENED
                    Ok(JsValue::undefined())
                })
            };

            // .send(body?)
            let send_method = open_method.clone();
            let send_url = open_url.clone();
            let send_async = open_async.clone();
            let send_rs = ready_state.clone();
            let send_status = status_val.clone();
            let send_resp = response_text.clone();
            let send_hdrs = response_headers.clone();
            let send_onload = onload_cb.clone();
            let send_onerror = onerror_cb.clone();
            let send_onrsc = onreadystatechange_cb.clone();
            let send_tx = xhr_fetch_tx.clone();
            let send_fn = {
                NativeFunction::from_closure(move |_this, args, ctx| {
                    let (body, content_type) = match args.first() {
                        Some(v) => match normalize_fetch_body(v, ctx) {
                            Some((bytes, ct)) => (Some(bytes), ct),
                            None => (None, None),
                        },
                        None => (None, None),
                    };
                    let mut headers: Vec<(String, String)> = Vec::new();
                    if let Some(ct) = content_type {
                        headers.push(("content-type".to_string(), ct));
                    }
                    let method = send_method.read().clone();
                    let url = send_url.read().clone();
                    let _is_async = *send_async.read();

                    *send_rs.write() = 2.0; // HEADERS_RECEIVED

                    let tx = {
                        let guard = send_tx.read();
                        guard.as_ref().cloned()
                    };

                    if let Some(tx) = tx {
                        let id = next_fetch_id();
                        if event_sink_attached() {
                            push_event(CoreEvent::FetchRequest {
                                request_id: cdp_request_id(id),
                                url: url.clone(),
                                method: method.clone(),
                                headers: headers.clone(),
                                post_data: body.clone(),
                                timestamp: now_ms(),
                            });
                        }
                        let request = FetchRequestMsg {
                            id,
                            url: url.clone(),
                            method: method.clone(),
                            headers,
                            body,
                            origin: current_origin(),
                        };
                        if tx.send(request).is_ok() {
                            // Non-blocking (Phase 3): register the XHR's shared
                            // state cells + callbacks; `settle_xhr` mutates them
                            // and fires callbacks when the response arrives on
                            // the shared channel (drained by the pump).
                            PENDING_FETCH.with(|m| {
                                m.borrow_mut().insert(
                                    id,
                                    PendingFetch::Xhr {
                                        ready_state: send_rs.clone(),
                                        status: send_status.clone(),
                                        resp_body: send_resp.clone(),
                                        resp_hdrs: send_hdrs.clone(),
                                        onload: send_onload.clone(),
                                        onerror: send_onerror.clone(),
                                        onrsc: send_onrsc.clone(),
                                        context_id: active_context_id(),
                                    },
                                );
                            });
                        } else {
                            *send_rs.write() = 4.0;
                            *send_status.write() = 0.0;
                        }
                    }

                    Ok(JsValue::undefined())
                })
            };

            // .setRequestHeader(name, value) — noop for now
            let set_req_header_fn = {
                NativeFunction::from_closure(move |_this, _args, _ctx| Ok(JsValue::undefined()))
            };

            // .getResponseHeader(name)
            let get_hdr_rs = response_headers.clone();
            let get_header_fn = {
                NativeFunction::from_closure(move |_this, args, ctx| {
                    let name = args
                        .first()
                        .and_then(|v| v.to_string(ctx).ok())
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    let hdrs = get_hdr_rs.read();
                    for line in hdrs.lines() {
                        if let Some(eq) = line.find(':') {
                            let key = line[..eq].trim();
                            if key.eq_ignore_ascii_case(&name) {
                                return Ok(JsValue::from(JsString::from(line[eq + 1..].trim())));
                            }
                        }
                    }
                    Ok(JsValue::null())
                })
            };

            // .abort() — reset state
            let abort_rs = ready_state.clone();
            let abort_fn = {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    *abort_rs.write() = 0.0;
                    Ok(JsValue::undefined())
                })
            };

            // Build object
            let rs_clone = ready_state.clone();
            let rs_getter = {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    Ok(JsValue::from(*rs_clone.read()))
                })
            };
            let rs_getter_fn = FunctionObjectBuilder::new(ctx.realm(), rs_getter)
                .name(js_string!("get readyState"))
                .build();

            let st_clone = status_val.clone();
            let st_getter = {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    Ok(JsValue::from(*st_clone.read()))
                })
            };
            let st_getter_fn = FunctionObjectBuilder::new(ctx.realm(), st_getter)
                .name(js_string!("get status"))
                .build();

            let rt_clone = response_text.clone();
            let rt_getter = {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    Ok(JsValue::from(JsString::from(rt_clone.read().as_str())))
                })
            };
            let rt_getter_fn = FunctionObjectBuilder::new(ctx.realm(), rt_getter)
                .name(js_string!("get responseText"))
                .build();

            let ol_clone = onload_cb.clone();
            let ol_getter = {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    Ok(ol_clone.read().clone().unwrap_or(JsValue::null()))
                })
            };
            let ol_getter_fn = FunctionObjectBuilder::new(ctx.realm(), ol_getter)
                .name(js_string!("get onload"))
                .build();

            let oe_clone = onerror_cb.clone();
            let oe_getter = {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    Ok(oe_clone.read().clone().unwrap_or(JsValue::null()))
                })
            };
            let oe_getter_fn = FunctionObjectBuilder::new(ctx.realm(), oe_getter)
                .name(js_string!("get onerror"))
                .build();

            let obj = boa_engine::object::ObjectInitializer::new(ctx)
                .accessor(
                    js_string!("readyState"),
                    Some(rs_getter_fn),
                    None,
                    Attribute::all(),
                )
                .accessor(
                    js_string!("status"),
                    Some(st_getter_fn),
                    None,
                    Attribute::all(),
                )
                .accessor(
                    js_string!("responseText"),
                    Some(rt_getter_fn),
                    None,
                    Attribute::all(),
                )
                .accessor(
                    js_string!("onload"),
                    Some(ol_getter_fn),
                    Some(onload_setter_fn),
                    Attribute::all(),
                )
                .accessor(
                    js_string!("onerror"),
                    Some(oe_getter_fn),
                    Some(onerror_setter_fn),
                    Attribute::all(),
                )
                .accessor(
                    js_string!("onreadystatechange"),
                    None,
                    Some(onrsc_setter_fn),
                    Attribute::all(),
                )
                .property(
                    js_string!("responseType"),
                    JsValue::from(JsString::from("")),
                    Attribute::all(),
                )
                .property(js_string!("timeout"), JsValue::from(0), Attribute::all())
                .property(
                    js_string!("withCredentials"),
                    JsValue::from(false),
                    Attribute::all(),
                )
                .function(open_fn, js_string!("open"), 3)
                .function(send_fn, js_string!("send"), 1)
                .function(set_req_header_fn, js_string!("setRequestHeader"), 2)
                .function(get_header_fn, js_string!("getResponseHeader"), 1)
                .function(abort_fn, js_string!("abort"), 0)
                .build();

            Ok(JsValue::from(obj))
        })
    };
    let _ = context.register_global_callable(js_string!("XMLHttpRequest"), 0, xhr_ctor);
    // --- WebSocket (Phase 4) ---
    let ws_ctor = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let url = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            let protocols: Vec<String> = match args.get(1) {
                Some(JsValue::String(s)) => vec![s.to_std_string_escaped()],
                _ => vec![],
            };

            let id = next_ws_id();

            // ws.send(data): text from a JS string, binary otherwise (best-effort).
            let send_id = id;
            let send_fn = NativeFunction::from_closure(move |_this, args, ctx| {
                let data = match args.first() {
                    Some(JsValue::String(s)) => WsData::Text(s.to_std_string_escaped()),
                    Some(v) => match extract_binary_bytes(v, ctx) {
                        Some(bytes) => WsData::Binary(bytes),
                        None => WsData::Text(format!("{}", v.display())),
                    },
                    None => return Ok(JsValue::undefined()),
                };
                if event_sink_attached() {
                    let (opcode, payload) = match &data {
                        WsData::Text(t) => (1u8, t.clone()),
                        WsData::Binary(b) => {
                            (2u8, base64::engine::general_purpose::STANDARD.encode(b))
                        }
                    };
                    push_event(CoreEvent::WsFrame {
                        direction: WsDirection::Sent,
                        request_id: format!("oxi-ws-{send_id}"),
                        opcode,
                        data: payload,
                        timestamp: now_ms(),
                    });
                }
                let _ = WS_REQ_TX.with(|c| {
                    c.borrow()
                        .as_ref()
                        .map(|tx| tx.send(WsReqMsg::Send { id: send_id, data }))
                });
                Ok(JsValue::undefined())
            });
            // ws.close(code?, reason?)
            let close_id = id;
            let close_fn = NativeFunction::from_closure(move |_this, args, _| {
                let code = args.first().and_then(|v| v.as_number()).map(|n| n as u16);
                let reason = args
                    .get(1)
                    .and_then(|v| v.as_string())
                    .map(|s| s.to_std_string_escaped());
                let _ = WS_REQ_TX.with(|c| {
                    c.borrow().as_ref().map(|tx| {
                        tx.send(WsReqMsg::Close {
                            id: close_id,
                            code,
                            reason,
                        })
                    })
                });
                Ok(JsValue::undefined())
            });
            // ws.addEventListener(type, cb): store on a hidden array on the
            // live object, read back at settle time alongside the on* property.
            let ael_id = id;
            let ael_fn = NativeFunction::from_closure(move |_this, args, ctx| {
                let (t, cb) = match (args.first(), args.get(1)) {
                    (Some(t), Some(cb)) => (t, cb),
                    _ => return Ok(JsValue::undefined()),
                };
                let t = t
                    .as_string()
                    .map(|s| s.to_std_string_escaped())
                    .unwrap_or_default();
                PENDING_WS.with(|m| {
                    let borrowed = m.borrow();
                    if let Some(WsState::Live { obj, .. }) = borrowed.get(&ael_id) {
                        let key = JsString::from(format!("__listeners_{t}").as_str());
                        let existing = obj.get(key.clone(), ctx).unwrap_or(JsValue::null());
                        let arr = if let Some(o) = existing.as_object() {
                            JsArray::from_object(o.clone()).unwrap_or_else(|_| JsArray::new(ctx))
                        } else {
                            JsArray::new(ctx)
                        };
                        let _ = arr.push(cb.clone(), ctx);
                        let _ = obj.set(key, JsValue::from(arr), false, ctx);
                    }
                });
                Ok(JsValue::undefined())
            });
            let obj = boa_engine::object::ObjectInitializer::new(ctx)
                .property(
                    js_string!("url"),
                    JsValue::from(JsString::from(url.as_str())),
                    Attribute::all(),
                )
                .property(js_string!("readyState"), JsValue::from(0), Attribute::all())
                .property(
                    js_string!("protocol"),
                    JsValue::from(JsString::from("")),
                    Attribute::all(),
                )
                .property(
                    js_string!("extensions"),
                    JsValue::from(JsString::from("")),
                    Attribute::all(),
                )
                .property(
                    js_string!("binaryType"),
                    JsValue::from(JsString::from("arraybuffer")),
                    Attribute::all(),
                )
                .property(
                    js_string!("bufferedAmount"),
                    JsValue::from(0),
                    Attribute::all(),
                )
                .property(js_string!("onopen"), JsValue::null(), Attribute::all())
                .property(js_string!("onmessage"), JsValue::null(), Attribute::all())
                .property(js_string!("onclose"), JsValue::null(), Attribute::all())
                .property(js_string!("onerror"), JsValue::null(), Attribute::all())
                .function(send_fn, js_string!("send"), 1)
                .function(close_fn, js_string!("close"), 0)
                .function(ael_fn, js_string!("addEventListener"), 2)
                .build();

            PENDING_WS.with(|m| {
                m.borrow_mut().insert(
                    id,
                    WsState::Live {
                        obj: obj.clone(),
                        context_id: active_context_id(),
                    },
                );
            });
            let _ = WS_REQ_TX.with(|cell| {
                cell.borrow().as_ref().map(|tx| {
                    tx.send(WsReqMsg::Connect {
                        id,
                        url: url.clone(),
                        protocols: protocols.clone(),
                    })
                })
            });

            Ok(obj.into())
        })
    };
    let _ = context.register_global_callable(js_string!("WebSocket"), 1, ws_ctor);

    // --- MutationObserver ---
    let mo_ctor = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let callback = args.first().cloned().unwrap_or(JsValue::undefined());

            // Observation state stored in JS object properties
            // __callback: the MutationCallback
            // __observing: boolean flag
            // __records: array of MutationRecord objects

            let disconnect_fn = {
                NativeFunction::from_closure(move |_this, _args, ctx| {
                    if let Some(obj) = _this.as_object() {
                        let _ = obj.set(js_string!("__observing"), JsValue::from(false), true, ctx);
                        let empty_arr = JsArray::new(ctx);
                        let _ =
                            obj.set(js_string!("__records"), JsValue::from(empty_arr), true, ctx);
                    }
                    Ok(JsValue::undefined())
                })
            };

            let observe_fn = {
                NativeFunction::from_closure(move |_this, args, ctx| {
                    let _target = args.first();
                    let _options = args.get(1);

                    if let Some(obj) = _this.as_object() {
                        let _ = obj.set(js_string!("__observing"), JsValue::from(true), true, ctx);

                        // Register in global __moRegistry
                        let registry = ctx
                            .global_object()
                            .get(js_string!("__moRegistry"), ctx)
                            .unwrap_or(JsValue::Null);
                        if let Some(reg_obj) = registry.as_object()
                            && let Ok(reg_arr) = JsArray::from_object(reg_obj.clone())
                        {
                            let _ = reg_arr.push(JsValue::from(obj.clone()), ctx);
                        }
                    }
                    Ok(JsValue::undefined())
                })
            };

            let take_records_fn = {
                NativeFunction::from_closure(move |_this, _args, ctx| {
                    if let Some(obj) = _this.as_object() {
                        let records = obj
                            .get(js_string!("__records"), ctx)
                            .unwrap_or(JsValue::Null);
                        // Clear records
                        let empty_arr = JsArray::new(ctx);
                        let _ =
                            obj.set(js_string!("__records"), JsValue::from(empty_arr), true, ctx);
                        return Ok(records);
                    }
                    let arr = JsArray::new(ctx);
                    Ok(JsValue::from(arr))
                })
            };

            let empty_arr = JsArray::new(ctx);
            let obj = boa_engine::object::ObjectInitializer::new(ctx)
                .property(js_string!("__callback"), callback, Attribute::all())
                .property(
                    js_string!("__observing"),
                    JsValue::from(false),
                    Attribute::all(),
                )
                .property(
                    js_string!("__records"),
                    JsValue::from(empty_arr),
                    Attribute::all(),
                )
                .function(observe_fn, js_string!("observe"), 2)
                .function(disconnect_fn, js_string!("disconnect"), 0)
                .function(take_records_fn, js_string!("takeRecords"), 0)
                .build();

            Ok(JsValue::from(obj))
        })
    };
    let _ = context.register_global_callable(js_string!("MutationObserver"), 1, mo_ctor);

    // Global MutationObserver registry — tracks all active observers
    let mo_registry = JsArray::new(&mut context);
    let _ = context.register_global_property(
        js_string!("__moRegistry"),
        JsValue::from(mo_registry),
        Attribute::all(),
    );

    // --- Document object ---

    register_document_object(
        &mut context,
        dom_snapshot,
        mutations,
        cookie_jar_arc,
        render_doc_cell,
    );

    // --- Window global ---

    register_window_globals(
        &mut context,
        dom_snapshot,
        mutations,
        viewport,
        page_url,
        user_agent,
        fetch_tx_arc,
        render_doc_cell,
    );

    // --- atob / btoa (Base64) ---
    let atob_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let encoded = args
                .first()
                .and_then(|v| v.to_string(ctx).ok())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            // Decode base64
            let decoded = base64::engine::general_purpose::STANDARD.decode(&encoded);
            match decoded {
                Ok(bytes) => {
                    let s = String::from_utf8_lossy(&bytes).to_string();
                    Ok(JsValue::from(JsString::from(s.as_str())))
                }
                Err(_) => {
                    // Try URL-safe base64
                    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&encoded);
                    match decoded {
                        Ok(bytes) => {
                            let s = String::from_utf8_lossy(&bytes).to_string();
                            Ok(JsValue::from(JsString::from(s.as_str())))
                        }
                        Err(_) => Ok(JsValue::undefined()),
                    }
                }
            }
        })
    };
    let _ = context.register_global_callable(js_string!("atob"), 1, atob_fn);

    let btoa_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let decoded = args
                .first()
                .and_then(|v| v.to_string(ctx).ok())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            // Encode base64
            let encoded = base64::engine::general_purpose::STANDARD.encode(decoded.as_bytes());
            Ok(JsValue::from(JsString::from(encoded.as_str())))
        })
    };
    let _ = context.register_global_callable(js_string!("btoa"), 1, btoa_fn);

    // --- URLSearchParams (minimal) ---
    // URLSearchParams is typically used as: new URLSearchParams("foo=bar&baz=1")
    // We create a class-like constructor that returns an object with
    // get, set, append, delete, has, keys, values, entries, forEach methods.
    let search_params_ctor = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let query_string = args
                .first()
                .and_then(|v| v.to_string(ctx).ok())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();

            // Parse query string into HashMap
            let map: std::collections::HashMap<String, Vec<String>> =
                std::collections::HashMap::new();
            let storage = std::cell::RefCell::new(map);

            for pair in query_string.split('&') {
                if let Some(eq) = pair.find('=') {
                    let key = pair[..eq].to_string();
                    let val = pair[eq + 1..].to_string();
                    let mut s = storage.borrow_mut();
                    s.entry(key).or_default().push(val);
                } else if !pair.is_empty() {
                    let mut s = storage.borrow_mut();
                    s.entry(pair.to_string()).or_default();
                }
            }

            let storage_arc = std::sync::Arc::new(storage);
            let _sp_storage = storage_arc.clone();

            // --- get ---
            let get_sp = storage_arc.clone();
            let get_fn = {
                NativeFunction::from_closure(move |_this, _args, ctx| {
                    let key = _args
                        .first()
                        .and_then(|v| v.to_string(ctx).ok())
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    let val = get_sp
                        .borrow()
                        .get(&key)
                        .and_then(|v| v.first())
                        .cloned()
                        .unwrap_or_default();
                    Ok(JsValue::from(JsString::from(val.as_str())))
                })
            };

            // --- set ---
            let set_sp = storage_arc.clone();
            let set_fn = {
                NativeFunction::from_closure(move |_this, _args, ctx| {
                    let key = _args
                        .first()
                        .and_then(|v| v.to_string(ctx).ok())
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    let val = _args
                        .get(1)
                        .and_then(|v| v.to_string(ctx).ok())
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    set_sp.borrow_mut().insert(key, vec![val]);
                    Ok(JsValue::undefined())
                })
            };

            // --- append ---
            let app_sp = storage_arc.clone();
            let app_fn = {
                NativeFunction::from_closure(move |_this, _args, ctx| {
                    let key = _args
                        .first()
                        .and_then(|v| v.to_string(ctx).ok())
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    let val = _args
                        .get(1)
                        .and_then(|v| v.to_string(ctx).ok())
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    app_sp.borrow_mut().entry(key).or_default().push(val);
                    Ok(JsValue::undefined())
                })
            };

            // --- delete ---
            let del_sp = storage_arc.clone();
            let del_fn = {
                NativeFunction::from_closure(move |_this, _args, ctx| {
                    let key = _args
                        .first()
                        .and_then(|v| v.to_string(ctx).ok())
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    del_sp.borrow_mut().remove(&key);
                    Ok(JsValue::undefined())
                })
            };

            // --- has ---
            let has_sp = storage_arc.clone();
            let has_fn = {
                NativeFunction::from_closure(move |_this, _args, ctx| {
                    let key = _args
                        .first()
                        .and_then(|v| v.to_string(ctx).ok())
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    let result = has_sp.borrow().contains_key(&key);
                    Ok(JsValue::from(result))
                })
            };

            // --- forEach ---
            let foreach_sp = storage_arc.clone();
            let foreach_fn = {
                NativeFunction::from_closure(move |_this, _args, ctx| {
                    if let Some(callback) = _args.first()
                        && let Some(cb_obj) = callback.as_object()
                        && cb_obj.is_callable()
                    {
                        for (key, values) in foreach_sp.borrow().iter() {
                            for val in values {
                                let cb_args = &[
                                    JsValue::from(JsString::from(val.as_str())),
                                    JsValue::from(JsString::from(key.as_str())),
                                    JsValue::undefined(),
                                ];
                                let _ = cb_obj.call(&JsValue::undefined(), cb_args, ctx);
                            }
                        }
                    }
                    Ok(JsValue::undefined())
                })
            };

            // --- toString ---
            let str_sp = storage_arc.clone();
            let str_fn = {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    let mut parts = Vec::new();
                    for (key, values) in str_sp.borrow().iter() {
                        for val in values {
                            parts.push(format!("{}={}", key, val));
                        }
                    }
                    Ok(JsValue::from(JsString::from(parts.join("&").as_str())))
                })
            };

            // Build the URLSearchParams object
            let sp_obj = boa_engine::object::ObjectInitializer::new(ctx)
                .function(get_fn, js_string!("get"), 1)
                .function(set_fn, js_string!("set"), 2)
                .function(app_fn, js_string!("append"), 2)
                .function(del_fn, js_string!("delete"), 1)
                .function(has_fn, js_string!("has"), 1)
                .function(foreach_fn, js_string!("forEach"), 1)
                .function(str_fn, js_string!("toString"), 0)
                .build();

            Ok(JsValue::from(sp_obj))
        })
    };
    let _ = context.register_global_callable(js_string!("URLSearchParams"), 1, search_params_ctor);

    // --- URL class (stub) ---
    // new URL(url) — basic URL parsing with protocol, host, pathname, search
    let url_ctor = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let url_str = args
                .first()
                .and_then(|v| v.to_string(ctx).ok())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();

            let parsed = url::Url::parse(&url_str);

            // Storage object for URL properties
            let url_storage =
                std::sync::Arc::new(std::cell::RefCell::new(std::collections::HashMap::new()));

            // url_storage holds parsed values as strings
            {
                let mut s = url_storage.borrow_mut();
                match &parsed {
                    Ok(u) => {
                        s.insert("href".to_string(), u.to_string());
                        s.insert("origin".to_string(), u.origin().ascii_serialization());
                        s.insert("protocol".to_string(), format!("{}:", u.scheme()));
                        s.insert(
                            "host".to_string(),
                            u.host().map(|h| h.to_string()).unwrap_or_default(),
                        );
                        s.insert(
                            "hostname".to_string(),
                            u.host().map(|h| h.to_string()).unwrap_or_default(),
                        );
                        s.insert("pathname".to_string(), u.path().to_string());
                        s.insert(
                            "search".to_string(),
                            u.query().map(|q| format!("?{}", q)).unwrap_or_default(),
                        );
                        s.insert(
                            "hash".to_string(),
                            u.fragment().map(|f| format!("#{}", f)).unwrap_or_default(),
                        );
                        s.insert(
                            "port".to_string(),
                            u.port().map(|p| p.to_string()).unwrap_or_default(),
                        );
                        s.insert("username".to_string(), u.username().to_string());
                        s.insert(
                            "password".to_string(),
                            u.password().map(|p| p.to_string()).unwrap_or_default(),
                        );
                        s.insert("searchParams".to_string(), "URLSearchParams".to_string());
                        // marker
                    }
                    Err(_) => {
                        s.insert("href".to_string(), url_str.clone());
                        s.insert("origin".to_string(), url_str);
                        s.insert("protocol".to_string(), String::new());
                        s.insert("host".to_string(), String::new());
                        s.insert("hostname".to_string(), String::new());
                        s.insert("pathname".to_string(), String::new());
                        s.insert("search".to_string(), String::new());
                        s.insert("hash".to_string(), String::new());
                        s.insert("port".to_string(), String::new());
                        s.insert("username".to_string(), String::new());
                        s.insert("password".to_string(), String::new());
                    }
                }
            }

            let us_storage = url_storage.clone();
            let href_storage = url_storage.clone();
            let us_storage2 = url_storage.clone();
            let us_storage3 = url_storage.clone();
            let us_storage4 = url_storage.clone();
            let us_storage5 = url_storage.clone();
            let us_storage6 = url_storage.clone();
            let us_storage7 = url_storage.clone();
            let _us_storage8 = url_storage.clone();

            // href getter
            let href_fn = {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    let href = href_storage
                        .borrow()
                        .get("href")
                        .cloned()
                        .unwrap_or_default();
                    Ok(JsValue::from(JsString::from(href.as_str())))
                })
            };

            // origin getter
            let origin_fn = {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    let origin = us_storage
                        .borrow()
                        .get("origin")
                        .cloned()
                        .unwrap_or_default();
                    Ok(JsValue::from(JsString::from(origin.as_str())))
                })
            };

            // protocol getter
            let proto_fn = {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    let proto = us_storage2
                        .borrow()
                        .get("protocol")
                        .cloned()
                        .unwrap_or_default();
                    Ok(JsValue::from(JsString::from(proto.as_str())))
                })
            };

            // host getter
            let host_fn = {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    let host = us_storage3
                        .borrow()
                        .get("host")
                        .cloned()
                        .unwrap_or_default();
                    Ok(JsValue::from(JsString::from(host.as_str())))
                })
            };

            // pathname getter
            let path_fn = {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    let path = us_storage4
                        .borrow()
                        .get("pathname")
                        .cloned()
                        .unwrap_or_default();
                    Ok(JsValue::from(JsString::from(path.as_str())))
                })
            };

            // search getter
            let search_fn = {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    let search = us_storage5
                        .borrow()
                        .get("search")
                        .cloned()
                        .unwrap_or_default();
                    Ok(JsValue::from(JsString::from(search.as_str())))
                })
            };

            // hash getter
            let hash_fn = {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    let hash = us_storage6
                        .borrow()
                        .get("hash")
                        .cloned()
                        .unwrap_or_default();
                    Ok(JsValue::from(JsString::from(hash.as_str())))
                })
            };

            // searchParams getter (returns URLSearchParams-like object)
            let sp_storage = us_storage7.clone();
            let sp_fn = {
                NativeFunction::from_closure(move |_this, _args, ctx| {
                    let search = sp_storage
                        .borrow()
                        .get("search")
                        .cloned()
                        .unwrap_or_default();
                    let query = search.trim_start_matches('?').to_string();

                    // Parse query string into key-value pairs
                    let params: Vec<(String, String)> = if query.is_empty() {
                        Vec::new()
                    } else {
                        query
                            .split('&')
                            .filter_map(|pair| {
                                let mut kv = pair.splitn(2, '=');
                                let key = kv.next().unwrap_or("").to_string();
                                let val = kv.next().unwrap_or("").to_string();
                                if !key.is_empty() {
                                    Some((key, val))
                                } else {
                                    None
                                }
                            })
                            .collect()
                    };

                    // Build a JS object that acts like URLSearchParams
                    let _sp_get_fn = {
                        NativeFunction::from_closure(move |_this, _args, _ctx| {
                            Ok(JsValue::undefined())
                        })
                    };

                    // Store params in an array for methods to use
                    let params_arr = JsArray::new(ctx);
                    for (k, v) in &params {
                        let entry = boa_engine::object::ObjectInitializer::new(ctx)
                            .property(
                                js_string!("0"),
                                JsValue::from(JsString::from(k.as_str())),
                                Attribute::all(),
                            )
                            .property(
                                js_string!("1"),
                                JsValue::from(JsString::from(v.as_str())),
                                Attribute::all(),
                            )
                            .build();
                        let _ = params_arr.push(JsValue::from(entry), ctx);
                    }

                    // get(name) — returns first value for the key
                    let get_params = params.clone();
                    let sp_get = {
                        NativeFunction::from_closure(move |_this, args, _ctx| {
                            let key = args
                                .first()
                                .and_then(|v| v.as_string())
                                .map(|s| s.to_std_string_escaped())
                                .unwrap_or_default();
                            for (k, v) in &get_params {
                                if k == &key {
                                    return Ok(JsValue::from(JsString::from(v.as_str())));
                                }
                            }
                            Ok(JsValue::null())
                        })
                    };

                    // has(name)
                    let has_params = params.clone();
                    let sp_has = {
                        NativeFunction::from_closure(move |_this, args, _ctx| {
                            let key = args
                                .first()
                                .and_then(|v| v.as_string())
                                .map(|s| s.to_std_string_escaped())
                                .unwrap_or_default();
                            Ok(JsValue::from(has_params.iter().any(|(k, _)| k == &key)))
                        })
                    };

                    // toString()
                    let to_str_query = query.clone();
                    let sp_to_string = {
                        NativeFunction::from_closure(move |_this, _args, _ctx| {
                            Ok(JsValue::from(JsString::from(to_str_query.as_str())))
                        })
                    };

                    // getAll(name)
                    let getall_params = params.clone();
                    let sp_get_all = {
                        NativeFunction::from_closure(move |_this, args, ctx2| {
                            let key = args
                                .first()
                                .and_then(|v| v.as_string())
                                .map(|s| s.to_std_string_escaped())
                                .unwrap_or_default();
                            let vals: Vec<JsValue> = getall_params
                                .iter()
                                .filter(|(k, _)| k == &key)
                                .map(|(_, v)| JsValue::from(JsString::from(v.as_str())))
                                .collect();
                            Ok(JsValue::from(JsArray::from_iter(vals, ctx2)))
                        })
                    };

                    let sp_obj = boa_engine::object::ObjectInitializer::new(ctx)
                        .function(sp_get, js_string!("get"), 1)
                        .function(sp_has, js_string!("has"), 1)
                        .function(sp_get_all, js_string!("getAll"), 1)
                        .function(sp_to_string, js_string!("toString"), 0)
                        .build();
                    Ok(JsValue::from(sp_obj))
                })
            };

            // Build URL object — convert NativeFunction to JsFunction via FunctionObjectBuilder
            let href_getter = FunctionObjectBuilder::new(ctx.realm(), href_fn)
                .name("get href")
                .build();
            let origin_getter = FunctionObjectBuilder::new(ctx.realm(), origin_fn)
                .name("get origin")
                .build();
            let proto_getter = FunctionObjectBuilder::new(ctx.realm(), proto_fn)
                .name("get protocol")
                .build();
            let host_getter = FunctionObjectBuilder::new(ctx.realm(), host_fn)
                .name("get host")
                .build();
            let path_getter = FunctionObjectBuilder::new(ctx.realm(), path_fn)
                .name("get pathname")
                .build();
            let search_getter = FunctionObjectBuilder::new(ctx.realm(), search_fn)
                .name("get search")
                .build();
            let hash_getter = FunctionObjectBuilder::new(ctx.realm(), hash_fn)
                .name("get hash")
                .build();
            let sp_getter = FunctionObjectBuilder::new(ctx.realm(), sp_fn)
                .name("get searchParams")
                .build();

            let url_obj = boa_engine::object::ObjectInitializer::new(ctx)
                .accessor(
                    js_string!("href"),
                    Some(href_getter),
                    None,
                    Attribute::all(),
                )
                .accessor(
                    js_string!("origin"),
                    Some(origin_getter),
                    None,
                    Attribute::all(),
                )
                .accessor(
                    js_string!("protocol"),
                    Some(proto_getter),
                    None,
                    Attribute::all(),
                )
                .accessor(
                    js_string!("host"),
                    Some(host_getter),
                    None,
                    Attribute::all(),
                )
                .accessor(
                    js_string!("pathname"),
                    Some(path_getter),
                    None,
                    Attribute::all(),
                )
                .accessor(
                    js_string!("search"),
                    Some(search_getter),
                    None,
                    Attribute::all(),
                )
                .accessor(
                    js_string!("hash"),
                    Some(hash_getter),
                    None,
                    Attribute::all(),
                )
                .accessor(
                    js_string!("searchParams"),
                    Some(sp_getter),
                    None,
                    Attribute::all(),
                )
                .build();

            Ok(JsValue::from(url_obj))
        })
    };
    let _ = context.register_global_callable(js_string!("URL"), 1, url_ctor);

    // URL.createObjectURL / revokeObjectURL (static methods on the URL
    // constructor). Minimal: mints a `blob:` URL; revoke is a no-op.
    let cou_fn = unsafe {
        NativeFunction::from_closure(move |_this, _args, _ctx| {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let url = format!("blob:https://oxibrowser.local/{id}");
            Ok(JsValue::from(JsString::from(url.as_str())))
        })
    };
    let rou_fn =
        unsafe { NativeFunction::from_closure(move |_this, _args, _ctx| Ok(JsValue::undefined())) };
    let cou_built = FunctionObjectBuilder::new(context.realm(), cou_fn)
        .name(js_string!("createObjectURL"))
        .build();
    let rou_built = FunctionObjectBuilder::new(context.realm(), rou_fn)
        .name(js_string!("revokeObjectURL"))
        .build();
    {
        let globals = context.global_object().clone();
        if let Ok(url_val) = globals.get(js_string!("URL"), &mut context)
            && let Some(url_obj) = url_val.as_object().cloned()
        {
            let _ = url_obj.set(
                js_string!("createObjectURL"),
                JsValue::from(cou_built),
                true,
                &mut context,
            );
            let _ = url_obj.set(
                js_string!("revokeObjectURL"),
                JsValue::from(rou_built),
                true,
                &mut context,
            );
        }
    }
    // --- crypto.getRandomValues (CSPRNG) ---
    // Supports both JsArray and TypedArray (Uint8Array, Int32Array, etc.)
    // by using object.get("length") + object.set(index, value) directly.
    let get_random_values_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let arr = args.first().cloned().unwrap_or(JsValue::undefined());
            if let Some(arr_obj) = arr.as_object() {
                // Try to get length from the object (works for both JsArray and TypedArray)
                if let Ok(len_val) = arr_obj.get(js_string!("length"), ctx)
                    && let Some(len) = len_val.as_number()
                {
                    let arr_len = (len as usize).min(65536);
                    let mut buf = vec![0u8; arr_len];
                    // Use real CSPRNG instead of predictable time-based PRNG
                    let _ = getrandom::fill(&mut buf);
                    for (i, val) in buf.iter().enumerate().take(arr_len) {
                        let _ = arr_obj.set(i as u32, JsValue::from(*val as i32), true, ctx);
                    }
                }
            }
            Ok(arr)
        })
    };
    let crypto_obj = boa_engine::object::ObjectInitializer::new(&mut context)
        .function(get_random_values_fn, js_string!("getRandomValues"), 1)
        .build();
    let _ = context.register_global_property(
        js_string!("crypto"),
        JsValue::from(crypto_obj),
        Attribute::all(),
    );

    // --- TextEncoder ---
    let _te_encode_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let input = args
                .first()
                .and_then(|v| v.to_string(ctx).ok())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            let bytes = input.as_bytes();
            let arr = JsArray::new(ctx);
            for &b in bytes {
                let _ = arr.push(JsValue::from(b), ctx);
            }
            let _obj = boa_engine::object::ObjectInitializer::new(ctx)
                .property(
                    js_string!("encoding"),
                    JsValue::from(JsString::from("utf-8")),
                    Attribute::all(),
                )
                .build();
            // Return Uint8Array-like object
            let result = boa_engine::object::ObjectInitializer::new(ctx)
                .property(
                    js_string!("buffer"),
                    JsValue::from(arr.clone()),
                    Attribute::all(),
                )
                .build();
            Ok(JsValue::from(result))
        })
    };
    // Avoid recursive closure — use a simpler approach
    let te_ctor = unsafe {
        NativeFunction::from_closure(move |_this, _args, ctx| {
            let encode_fn = {
                NativeFunction::from_closure(move |_this2, args2, ctx2| {
                    let input = args2
                        .first()
                        .and_then(|v| v.to_string(ctx2).ok())
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    let bytes = input.as_bytes();
                    let arr = JsArray::new(ctx2);
                    for &b in bytes {
                        let _ = arr.push(JsValue::from(b), ctx2);
                    }
                    Ok(JsValue::from(arr))
                })
            };
            let obj = boa_engine::object::ObjectInitializer::new(ctx)
                .property(
                    js_string!("encoding"),
                    JsValue::from(JsString::from("utf-8")),
                    Attribute::all(),
                )
                .function(encode_fn, js_string!("encode"), 1)
                .build();
            Ok(JsValue::from(obj))
        })
    };
    let _ = context.register_global_callable(js_string!("TextEncoder"), 0, te_ctor);

    // --- TextDecoder ---
    let td_ctor = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let decode_fn = {
                NativeFunction::from_closure(move |_this2, args2, ctx2| {
                    // Decode buffer/array back to string
                    let input = args2.first().cloned().unwrap_or(JsValue::undefined());
                    if let Some(arr_obj) = input.as_object()
                        && let Ok(arr) = JsArray::from_object(arr_obj.clone())
                        && let Ok(len) = arr.length(ctx2)
                    {
                        let mut bytes = Vec::with_capacity(len as usize);
                        for i in 0..len {
                            if let Ok(v) = arr.at(i as i64, ctx2)
                                && let Some(n) = v.as_number()
                            {
                                bytes.push(n as u8);
                            }
                        }
                        let s = String::from_utf8_lossy(&bytes).to_string();
                        return Ok(JsValue::from(JsString::from(s.as_str())));
                    }
                    Ok(JsValue::from(JsString::from("")))
                })
            };
            let encoding = args
                .first()
                .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                .unwrap_or_else(|| "utf-8".to_string());
            let obj = boa_engine::object::ObjectInitializer::new(ctx)
                .property(
                    js_string!("encoding"),
                    JsValue::from(JsString::from(encoding.as_str())),
                    Attribute::all(),
                )
                .function(decode_fn, js_string!("decode"), 1)
                .build();
            Ok(JsValue::from(obj))
        })
    };
    let _ = context.register_global_callable(js_string!("TextDecoder"), 0, td_ctor);

    // --- Array.from() polyfill ---
    // boa_engine doesn't expose Array.from yet, so we inject it.
    let array_from_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let source = args.first().cloned().unwrap_or(JsValue::undefined());

            // Case 1: Already an array → shallow copy
            if let Some(obj) = source.as_object() {
                if let Ok(arr) = JsArray::from_object(obj.clone())
                    && let Ok(len) = arr.length(ctx)
                {
                    let items: Vec<JsValue> = (0..len)
                        .filter_map(|i| arr.at(i as i64, ctx).ok())
                        .collect();
                    return Ok(JsArray::from_iter(items, ctx).into());
                }

                // Case 2: Array-like object (has .length + indexed props)
                if let Ok(len_val) = obj.get(js_string!("length"), ctx)
                    && let Some(len) = len_val.as_number()
                {
                    let items: Vec<JsValue> = (0..len as u32)
                        .filter_map(|i| obj.get(i, ctx).ok())
                        .collect();
                    return Ok(JsArray::from_iter(items, ctx).into());
                }
            }

            // Case 3: Single value → wrap in array
            if !source.is_undefined() {
                return Ok(JsArray::from_iter([source], ctx).into());
            }

            Ok(JsArray::new(ctx).into())
        })
    };
    let _ = context.register_global_callable(js_string!("ArrayFrom"), 1, array_from_fn);
    let _ = context.eval(Source::from_bytes(
        "if (typeof Array.from === 'undefined') { Array.from = ArrayFrom; delete globalThis.ArrayFrom; }"
    ));

    // --- requestAnimationFrame ---
    //
    // Mirrors setTimeout's schedule_timer pattern: clone the job_queue into the closure,
    // pick the callback from args[0], fire after ~16ms (~60fps), and pass a
    // DOMHighResTimeStamp (ms since Unix epoch) as the callback's argument.
    let raf_queue = job_queue.clone();
    let raf_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            let Some(callback) = args.first().cloned() else {
                return Ok(JsValue::undefined());
            };
            if let Some(func) = callback.as_object().cloned()
                && func.is_callable()
            {
                let deadline = Instant::now() + Duration::from_millis(16);
                let timestamp_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs_f64() * 1000.0)
                    .unwrap_or(0.0);
                let cb_args: Vec<JsValue> = vec![JsValue::from(timestamp_ms)];
                let id = raf_queue.schedule_timer(deadline, func, cb_args, false, None);
                return Ok(JsValue::from(id as f64));
            }
            Ok(JsValue::undefined())
        })
    };
    let _ = context.register_global_callable(js_string!("requestAnimationFrame"), 1, raf_fn);

    // --- cancelAnimationFrame ---
    //
    // Cancels a previously scheduled rAF by its handle ID — same shape as clearTimeout.
    let cancel_raf_queue = job_queue.clone();
    let cancel_raf_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            if let Some(id) = args.first().and_then(|v| v.as_number()) {
                cancel_raf_queue.cancel_timer(id as u64);
            }
            Ok(JsValue::undefined())
        })
    };
    let _ = context.register_global_callable(js_string!("cancelAnimationFrame"), 1, cancel_raf_fn);

    // --- Event constructor ---

    // ── Event init-dict helpers ──────────────────────────────────────────────
    /// Copy known properties from args[1] (init dict) onto a freshly-built event object.
    /// Keys that exist in the init dict override the default; keys missing from the
    /// dict keep the default. This is used by every built-in event constructor
    /// because boa_engine's ObjectInitializer doesn't support dynamic property
    /// injection at build time.
    fn apply_init_dict(args: &[JsValue], obj: &JsObject, ctx: &mut Context, keys: &[&str]) {
        let Some(init) = args.get(1).and_then(|v| v.as_object()) else {
            return;
        };
        for &key in keys {
            if let Ok(val) = init.get(js_string!(key), ctx)
                && !val.is_undefined()
            {
                let _ = obj.set(js_string!(key), val, true, ctx);
            }
        }
    }

    /// Add Event.prototype methods as own properties on every event object.
    /// This is needed because boa_engine's register_global_callable does not
    /// create a .prototype property on the constructor, making prototype-based
    /// inheritance unavailable.
    fn setup_event_object(obj: &JsObject, ctx: &mut Context) {
        // preventDefault
        let prevent_fn = unsafe {
            NativeFunction::from_closure(move |_this, _args, ctx| {
                if let Some(o) = _this.as_object() {
                    let _ = o.set(
                        js_string!("defaultPrevented"),
                        JsValue::from(true),
                        true,
                        ctx,
                    );
                }
                Ok(JsValue::undefined())
            })
        };
        let _ = obj.set(
            js_string!("preventDefault"),
            FunctionObjectBuilder::new(ctx.realm(), prevent_fn)
                .name(js_string!("preventDefault"))
                .build(),
            true,
            ctx,
        );

        // stopPropagation
        let stop_fn = unsafe {
            NativeFunction::from_closure(move |_this, _args, ctx| {
                if let Some(o) = _this.as_object() {
                    let _ = o.set(
                        js_string!("__stopPropagation"),
                        JsValue::from(true),
                        true,
                        ctx,
                    );
                }
                Ok(JsValue::undefined())
            })
        };
        let _ = obj.set(
            js_string!("stopPropagation"),
            FunctionObjectBuilder::new(ctx.realm(), stop_fn)
                .name(js_string!("stopPropagation"))
                .build(),
            true,
            ctx,
        );

        let stop_imm_fn = unsafe {
            NativeFunction::from_closure(move |_this, _args, ctx| {
                if let Some(o) = _this.as_object() {
                    let _ = o.set(
                        js_string!("__stopPropagation"),
                        JsValue::from(true),
                        true,
                        ctx,
                    );
                    let _ = o.set(
                        js_string!("__stopImmediatePropagation"),
                        JsValue::from(true),
                        true,
                        ctx,
                    );
                }
                Ok(JsValue::undefined())
            })
        };
        let _ = obj.set(
            js_string!("stopImmediatePropagation"),
            FunctionObjectBuilder::new(ctx.realm(), stop_imm_fn)
                .name(js_string!("stopImmediatePropagation"))
                .build(),
            true,
            ctx,
        );
    }
    const EVENT_INIT_KEYS: &[&str] = &["bubbles", "cancelable"];
    const MOUSE_INIT_KEYS: &[&str] = &[
        "bubbles",
        "cancelable",
        "clientX",
        "clientY",
        "button",
        "buttons",
        "screenX",
        "screenY",
        "ctrlKey",
        "shiftKey",
        "altKey",
        "metaKey",
        "relatedTarget",
        "view",
        "detail",
    ];
    const KEYBOARD_INIT_KEYS: &[&str] = &[
        "key",
        "code",
        "keyCode",
        "charCode",
        "which",
        "location",
        "ctrlKey",
        "shiftKey",
        "altKey",
        "metaKey",
        "repeat",
        "isComposing",
    ];
    const FOCUS_INIT_KEYS: &[&str] = &["bubbles", "cancelable", "relatedTarget"];
    let event_ctor = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let event_type = args
                .first()
                .and_then(|v| v.to_string(ctx).ok())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            let obj = boa_engine::object::ObjectInitializer::new(ctx)
                .property(
                    js_string!("type"),
                    JsValue::from(JsString::from(event_type.as_str())),
                    Attribute::all(),
                )
                .property(
                    js_string!("bubbles"),
                    JsValue::from(false),
                    Attribute::all(),
                )
                .property(
                    js_string!("cancelable"),
                    JsValue::from(false),
                    Attribute::all(),
                )
                .property(
                    js_string!("defaultPrevented"),
                    JsValue::from(false),
                    Attribute::all(),
                )
                .property(js_string!("target"), JsValue::null(), Attribute::all())
                .property(
                    js_string!("currentTarget"),
                    JsValue::null(),
                    Attribute::all(),
                )
                .property(js_string!("eventPhase"), JsValue::from(0), Attribute::all())
                .build();
            apply_init_dict(args, &obj, ctx, EVENT_INIT_KEYS);
            setup_event_object(&obj, ctx);
            Ok(JsValue::from(obj))
        })
    };
    let _ = context.register_global_callable(js_string!("Event"), 1, event_ctor);

    // Event.prototype methods — needed by dispatchEvent logic
    let _ = context.eval(Source::from_bytes(
        r#"
        Event.prototype.preventDefault = function() {
            this.defaultPrevented = true;
        };
        Event.prototype.stopPropagation = function() {
            this.__stopPropagation = true;
        };
        Event.prototype.stopImmediatePropagation = function() {
            this.__stopImmediatePropagation = true;
            this.__stopPropagation = true;
        };
    "#,
    ));

    // --- MouseEvent constructor ---
    let mouse_event_ctor = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let event_type = args
                .first()
                .and_then(|v| v.to_string(ctx).ok())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            let obj = boa_engine::object::ObjectInitializer::new(ctx)
                .property(
                    js_string!("type"),
                    JsValue::from(JsString::from(event_type.as_str())),
                    Attribute::all(),
                )
                .property(js_string!("bubbles"), JsValue::from(true), Attribute::all())
                .property(
                    js_string!("cancelable"),
                    JsValue::from(true),
                    Attribute::all(),
                )
                .property(js_string!("clientX"), JsValue::from(0), Attribute::all())
                .property(js_string!("clientY"), JsValue::from(0), Attribute::all())
                .property(js_string!("button"), JsValue::from(0), Attribute::all())
                .property(js_string!("buttons"), JsValue::from(0), Attribute::all())
                .property(js_string!("screenX"), JsValue::from(0), Attribute::all())
                .property(js_string!("screenY"), JsValue::from(0), Attribute::all())
                .property(
                    js_string!("ctrlKey"),
                    JsValue::from(false),
                    Attribute::all(),
                )
                .property(
                    js_string!("shiftKey"),
                    JsValue::from(false),
                    Attribute::all(),
                )
                .property(js_string!("altKey"), JsValue::from(false), Attribute::all())
                .property(
                    js_string!("metaKey"),
                    JsValue::from(false),
                    Attribute::all(),
                )
                .property(
                    js_string!("relatedTarget"),
                    JsValue::null(),
                    Attribute::all(),
                )
                .property(js_string!("view"), JsValue::null(), Attribute::all())
                .property(js_string!("detail"), JsValue::from(0), Attribute::all())
                .build();
            apply_init_dict(args, &obj, ctx, MOUSE_INIT_KEYS);
            setup_event_object(&obj, ctx);
            Ok(JsValue::from(obj))
        })
    };
    let _ = context.register_global_callable(js_string!("MouseEvent"), 1, mouse_event_ctor);

    // --- KeyboardEvent constructor ---
    let keyboard_event_ctor = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let event_type = args
                .first()
                .and_then(|v| v.to_string(ctx).ok())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            let obj = boa_engine::object::ObjectInitializer::new(ctx)
                .property(
                    js_string!("type"),
                    JsValue::from(JsString::from(event_type.as_str())),
                    Attribute::all(),
                )
                .property(
                    js_string!("key"),
                    JsValue::from(JsString::from("")),
                    Attribute::all(),
                )
                .property(
                    js_string!("code"),
                    JsValue::from(JsString::from("")),
                    Attribute::all(),
                )
                .property(js_string!("keyCode"), JsValue::from(0), Attribute::all())
                .property(js_string!("charCode"), JsValue::from(0), Attribute::all())
                .property(js_string!("which"), JsValue::from(0), Attribute::all())
                .property(js_string!("location"), JsValue::from(0), Attribute::all())
                .property(
                    js_string!("ctrlKey"),
                    JsValue::from(false),
                    Attribute::all(),
                )
                .property(
                    js_string!("shiftKey"),
                    JsValue::from(false),
                    Attribute::all(),
                )
                .property(js_string!("altKey"), JsValue::from(false), Attribute::all())
                .property(
                    js_string!("metaKey"),
                    JsValue::from(false),
                    Attribute::all(),
                )
                .property(js_string!("repeat"), JsValue::from(false), Attribute::all())
                .property(
                    js_string!("isComposing"),
                    JsValue::from(false),
                    Attribute::all(),
                )
                .build();
            apply_init_dict(args, &obj, ctx, KEYBOARD_INIT_KEYS);
            setup_event_object(&obj, ctx);
            Ok(JsValue::from(obj))
        })
    };
    let _ = context.register_global_callable(js_string!("KeyboardEvent"), 1, keyboard_event_ctor);
    let focus_event_ctor = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let event_type = args
                .first()
                .and_then(|v| v.to_string(ctx).ok())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            let obj = boa_engine::object::ObjectInitializer::new(ctx)
                .property(
                    js_string!("type"),
                    JsValue::from(JsString::from(event_type.as_str())),
                    Attribute::all(),
                )
                .property(
                    js_string!("bubbles"),
                    JsValue::from(false),
                    Attribute::all(),
                )
                .property(
                    js_string!("cancelable"),
                    JsValue::from(false),
                    Attribute::all(),
                )
                .property(
                    js_string!("relatedTarget"),
                    JsValue::null(),
                    Attribute::all(),
                )
                .build();
            apply_init_dict(args, &obj, ctx, FOCUS_INIT_KEYS);
            setup_event_object(&obj, ctx);
            Ok(JsValue::from(obj))
        })
    };
    let _ = context.register_global_callable(js_string!("FocusEvent"), 1, focus_event_ctor);

    // --- DragEvent constructor (extends MouseEvent init keys) ---
    let drag_event_ctor = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let event_type = args
                .first()
                .and_then(|v| v.to_string(ctx).ok())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            let obj = boa_engine::object::ObjectInitializer::new(ctx)
                .property(
                    js_string!("type"),
                    JsValue::from(JsString::from(event_type.as_str())),
                    Attribute::all(),
                )
                .property(js_string!("bubbles"), JsValue::from(true), Attribute::all())
                .property(
                    js_string!("cancelable"),
                    JsValue::from(true),
                    Attribute::all(),
                )
                .property(js_string!("clientX"), JsValue::from(0), Attribute::all())
                .property(js_string!("clientY"), JsValue::from(0), Attribute::all())
                .property(js_string!("button"), JsValue::from(0), Attribute::all())
                .property(js_string!("buttons"), JsValue::from(0), Attribute::all())
                .property(js_string!("screenX"), JsValue::from(0), Attribute::all())
                .property(js_string!("screenY"), JsValue::from(0), Attribute::all())
                .property(
                    js_string!("ctrlKey"),
                    JsValue::from(false),
                    Attribute::all(),
                )
                .property(
                    js_string!("shiftKey"),
                    JsValue::from(false),
                    Attribute::all(),
                )
                .property(js_string!("altKey"), JsValue::from(false), Attribute::all())
                .property(
                    js_string!("metaKey"),
                    JsValue::from(false),
                    Attribute::all(),
                )
                .property(
                    js_string!("dataTransfer"),
                    JsValue::null(),
                    Attribute::all(),
                )
                .build();
            apply_init_dict(args, &obj, ctx, MOUSE_INIT_KEYS);
            setup_event_object(&obj, ctx);
            Ok(JsValue::from(obj))
        })
    };
    let _ = context.register_global_callable(js_string!("DragEvent"), 1, drag_event_ctor);

    // --- document.createDocumentFragment (via eval) ---
    // A fragment tracks its children in a JS array and carries NO __nodeId:
    // the render-doc appendChild splices those children into the document
    // (DOM spec). The old stub carried a bogus __nodeId which panicked Blitz
    // and permanently killed the JS thread on insert (issue #2).
    let _ = context.eval(Source::from_bytes(
        r#"
        document.createDocumentFragment = function() {
            var frag = {
                nodeType: 11,
                childNodes: [],
                appendChild: function(child) {
                    if (child === frag) {
                        // DOM spec: a fragment cannot contain itself — the
                        // flatten below pushes onto the array being iterated
                        // and would never terminate.
                        throw new TypeError("cannot append a fragment to itself");
                    }
                    if (child == null) {
                        throw new TypeError("appendChild requires a Node argument");
                    }
                    if (child.nodeType === 11) {
                        // Splice a nested fragment's children in — fragments
                        // never nest (DOM spec flattening).
                        for (var i = 0; i < child.childNodes.length; i++) {
                            frag.childNodes.push(child.childNodes[i]);
                        }
                        child.childNodes.length = 0;
                    } else {
                        frag.childNodes.push(child);
                    }
                    return child;
                }
            };
            return frag;
        };
    "#,
    ));

    // --- WebAssembly support ---
    // Registers the `WebAssembly` global namespace (compile, instantiate,
    // validate, Module/Instance/Memory/Table). All WASM ops run synchronously
    // on this JS thread (wasmi handles are not Send). See `js::wasm`.
    if let Err(e) = crate::js::wasm::register_wasm_globals(&mut context) {
        tracing::warn!(error = %e, "WebAssembly globals registration failed");
    }

    (context, job_queue)
}

// ---------------------------------------------------------------------------
// Document object registration
// ---------------------------------------------------------------------------

/// Build a JS element object backed by a node in the [`RenderDocument`].
///
/// The object exposes the Web API subset that mutates the render document
/// *directly* (no `DomMutation` log): `setAttribute`/`getAttribute`/
/// `removeAttribute`, `appendChild`, `remove`, `style.setProperty`,
/// `textContent`, `id`, `className`. Each method takes a short borrow of the
/// shared `Rc<RefCell<Option<RenderDocument>>>` — never held across a JS
/// callback — so the document stays borrowable for the next operation.
fn create_render_element_object(
    ctx: &mut Context,
    render_doc: Rc<RefCell<Option<RenderDocument>>>,
    node_id: usize,
) -> JsValue {
    let tag = render_doc
        .borrow()
        .as_ref()
        .and_then(|d| d.tag_name(node_id))
        .unwrap_or_default();
    let tag_upper = tag.to_uppercase();

    // ── attribute methods ──
    let rd_set = render_doc.clone();
    let set_attr_fn = unsafe {
        NativeFunction::from_closure(move |this: &JsValue, args, ctx| {
            let name = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            let value = args
                .get(1)
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            let old_val = rd_set
                .borrow()
                .as_ref()
                .and_then(|d| d.node_attr(node_id, &name));
            if let Some(doc) = rd_set.borrow_mut().as_mut() {
                doc.set_attribute(node_id, &name, &value);
            }
            // Fire attributeChangedCallback for custom elements (gated by
            // observedAttributes inside the helper).
            let old_js = old_val
                .map(|v| JsValue::from(JsString::from(v.as_str())))
                .unwrap_or(JsValue::null());
            call_global_helper(
                ctx,
                "__oxi_fire_attr_changed",
                &[
                    this.clone(),
                    JsValue::from(JsString::from(name.as_str())),
                    old_js,
                    JsValue::from(JsString::from(value.as_str())),
                ],
            );
            Ok(JsValue::undefined())
        })
    };

    let rd_get = render_doc.clone();
    let get_attr_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            let name = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            let val = rd_get
                .borrow()
                .as_ref()
                .and_then(|d| d.node_attr(node_id, &name));
            match val {
                Some(v) => Ok(JsValue::from(JsString::from(v.as_str()))),
                None => Ok(JsValue::null()),
            }
        })
    };

    let rd_rm = render_doc.clone();
    let remove_attr_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            let name = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            if let Some(doc) = rd_rm.borrow_mut().as_mut() {
                doc.remove_attribute(node_id, &name);
            }
            Ok(JsValue::undefined())
        })
    };

    // ── appendChild / remove ──
    let rd_ac = render_doc.clone();
    let append_child_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let child = args.first().cloned().unwrap_or(JsValue::undefined());
            // A DocumentFragment (nodeType 11) is never inserted itself: per
            // spec its children are spliced into the parent and the fragment
            // is left empty. The fragment object carries no __nodeId —
            // forwarding one to Blitz panics and kills the JS thread.
            fn node_id_of(node: &JsValue, ctx: &mut Context) -> Option<usize> {
                node.as_object()
                    .and_then(|o| o.get(js_string!("__nodeId"), ctx).ok())
                    .and_then(|v| v.as_number().map(|n| n as usize))
            }
            let is_fragment = child
                .as_object()
                .and_then(|o| o.get(js_string!("nodeType"), ctx).ok())
                .and_then(|v| v.as_number())
                .map(|n| n as u32 == 11)
                .unwrap_or(false);

            // Nodes to insert: the fragment's childNodes for a splice,
            // otherwise the child itself. Nodes without a __nodeId cannot
            // live in the render document; createElement/createTextNode
            // always mint one, so hitting this means foreign objects were
            // pushed into childNodes by hand — skip them observably.
            let mut skipped = 0usize;
            let insert_nodes: Vec<JsValue> = if is_fragment {
                let frag_children = child
                    .as_object()
                    .and_then(|o| o.get(js_string!("childNodes"), ctx).ok())
                    .and_then(|v| v.as_object().cloned())
                    .map(JsArray::from_object);
                let children: Vec<JsValue> = match frag_children {
                    Some(Ok(arr)) => arr
                        .length(ctx)
                        .map(|len| {
                            (0..len)
                                .filter_map(|i| arr.get(i as i32, ctx).ok())
                                .collect()
                        })
                        .unwrap_or_default(),
                    _ => Vec::new(),
                };
                let (with_id, without): (Vec<_>, Vec<_>) = children
                    .into_iter()
                    .partition(|n| node_id_of(n, ctx).is_some());
                skipped = without.len();
                with_id
            } else if node_id_of(&child, ctx).is_some() {
                vec![child.clone()]
            } else {
                skipped = 1;
                Vec::new()
            };
            if skipped > 0 {
                tracing::warn!("appendChild: skipped {skipped} node(s) without a __nodeId");
            }
            let insert_ids: Vec<usize> = insert_nodes
                .iter()
                .filter_map(|n| node_id_of(n, ctx))
                .collect();

            let mut appended = false;
            if let Some(doc) = rd_ac.borrow_mut().as_mut() {
                for id in &insert_ids {
                    doc.append_child(node_id, *id);
                }
                appended = !insert_ids.is_empty();
                if appended {
                    notify_mutation_observers(ctx, "childList", node_id as u32);
                }
            }

            // Fire connectedCallback OUTSIDE the render-doc borrow, so a
            // callback that touches the DOM (setAttribute/appendChild) can
            // re-borrow the RefCell without panicking. Gated on `appended`:
            // when the render doc is absent nothing was inserted, and custom
            // elements must not observe a false "connected".
            if appended {
                for node in &insert_nodes {
                    call_global_helper(ctx, "__oxi_fire_connected", std::slice::from_ref(node));
                }
            }

            // Splice semantics: the fragment is left empty after insertion.
            if is_fragment
                && appended
                && let Some(frag_obj) = child.as_object()
                && let Err(e) = frag_obj.set(
                    js_string!("childNodes"),
                    JsValue::from(JsArray::new(ctx)),
                    true,
                    ctx,
                )
            {
                tracing::warn!("failed to empty fragment after splice: {e}");
            }
            Ok(child)
        })
    };

    let rd_rem = render_doc.clone();
    let remove_fn = unsafe {
        NativeFunction::from_closure(move |this: &JsValue, _args, ctx| {
            let removed = rd_rem
                .borrow_mut()
                .as_mut()
                .map(|doc| {
                    doc.remove_node(node_id);
                })
                .is_some();
            // Fire disconnectedCallback OUTSIDE the render-doc borrow (see
            // connectedCallback above).
            if removed {
                call_global_helper(ctx, "__oxi_fire_disconnected", std::slice::from_ref(this));
            }
            Ok(JsValue::undefined())
        })
    };

    // ── style accessor (returns a CSSStyleDeclaration-like object) ──
    let rd_style = render_doc.clone();
    let style_fn = unsafe {
        NativeFunction::from_closure(move |_this, _args, ctx| {
            let sp = rd_style.clone();
            let set_prop = NativeFunction::from_closure(move |_this, args, _ctx| {
                let prop = args
                    .first()
                    .and_then(|v| v.as_string())
                    .map(|s| s.to_std_string_escaped())
                    .unwrap_or_default();
                let val = args
                    .get(1)
                    .and_then(|v| v.as_string())
                    .map(|s| s.to_std_string_escaped())
                    .unwrap_or_default();
                if let Some(doc) = sp.borrow_mut().as_mut() {
                    doc.set_inline_style(node_id, &prop, &val);
                }
                Ok(JsValue::undefined())
            });
            let gp = rd_style.clone();
            let get_prop = NativeFunction::from_closure(move |_this, _args, _ctx| {
                // Inline-style reads aren't tracked separately; return "".
                let _ = gp;
                Ok(JsValue::from(JsString::from("")))
            });
            let style_obj = boa_engine::object::ObjectInitializer::new(ctx)
                .function(set_prop, js_string!("setProperty"), 2)
                .function(get_prop, js_string!("getPropertyValue"), 1)
                .build();
            Ok(JsValue::from(style_obj))
        })
    };
    let style_getter_fn = FunctionObjectBuilder::new(ctx.realm(), style_fn)
        .name(js_string!("get style"))
        .build();

    // ── textContent getter/setter ──
    let rd_tc_get = render_doc.clone();
    let text_get_fn = unsafe {
        NativeFunction::from_closure(move |_this, _args, _ctx| {
            let text = rd_tc_get
                .borrow()
                .as_ref()
                .map(|d| d.node_text(node_id))
                .unwrap_or_default();
            Ok(JsValue::from(JsString::from(text.as_str())))
        })
    };
    let text_getter_fn = FunctionObjectBuilder::new(ctx.realm(), text_get_fn)
        .name(js_string!("get textContent"))
        .build();

    let rd_tc_set = render_doc.clone();
    let text_set_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            let text = args
                .first()
                .and_then(|v| v.to_string(_ctx).ok())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            if let Some(doc) = rd_tc_set.borrow_mut().as_mut() {
                doc.set_text(node_id, &text);
            }
            Ok(JsValue::undefined())
        })
    };
    let text_setter_fn = FunctionObjectBuilder::new(ctx.realm(), text_set_fn)
        .name(js_string!("set textContent"))
        .build();

    // ── id / className (backed by attributes) ──
    let rd_id_get = render_doc.clone();
    let id_get_fn = unsafe {
        NativeFunction::from_closure(move |_this, _args, _ctx| {
            let v = rd_id_get
                .borrow()
                .as_ref()
                .and_then(|d| d.node_attr(node_id, "id"))
                .unwrap_or_default();
            Ok(JsValue::from(JsString::from(v.as_str())))
        })
    };
    let id_getter_fn = FunctionObjectBuilder::new(ctx.realm(), id_get_fn)
        .name(js_string!("get id"))
        .build();
    let rd_id_set = render_doc.clone();
    let id_set_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            let v = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            if let Some(doc) = rd_id_set.borrow_mut().as_mut() {
                doc.set_attribute(node_id, "id", &v);
            }
            Ok(JsValue::undefined())
        })
    };
    let id_setter_fn = FunctionObjectBuilder::new(ctx.realm(), id_set_fn)
        .name(js_string!("set id"))
        .build();

    let rd_cls_get = render_doc.clone();
    let cls_get_fn = unsafe {
        NativeFunction::from_closure(move |_this, _args, _ctx| {
            let v = rd_cls_get
                .borrow()
                .as_ref()
                .and_then(|d| d.node_attr(node_id, "class"))
                .unwrap_or_default();
            Ok(JsValue::from(JsString::from(v.as_str())))
        })
    };
    let cls_getter_fn = FunctionObjectBuilder::new(ctx.realm(), cls_get_fn)
        .name(js_string!("get className"))
        .build();
    let rd_cls_set = render_doc.clone();
    let cls_set_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            let v = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            if let Some(doc) = rd_cls_set.borrow_mut().as_mut() {
                doc.set_attribute(node_id, "class", &v);
            }
            Ok(JsValue::undefined())
        })
    };
    let cls_setter_fn = FunctionObjectBuilder::new(ctx.realm(), cls_set_fn)
        .name(js_string!("set className"))
        .build();

    let click_fn = unsafe {
        NativeFunction::from_closure(move |_this, _args, ctx| {
            // Fire any registered click listeners directly (no mutation log).
            for cb in registry_get(node_id as u32, "click") {
                let evt = boa_engine::object::ObjectInitializer::new(ctx)
                    .property(
                        js_string!("type"),
                        JsValue::from(JsString::from("click")),
                        Attribute::all(),
                    )
                    .build();
                let _ = cb.call(&JsValue::undefined(), &[JsValue::from(evt)], ctx);
            }
            Ok(JsValue::undefined())
        })
    };

    // ── layout + events + form helpers ──
    let rd_rect = render_doc.clone();
    let get_bounding_client_rect_fn = unsafe {
        NativeFunction::from_closure(move |_this, _args, ctx| {
            let (x, y, w, h) = rd_rect
                .borrow()
                .as_ref()
                .map(|d| d.node_layout_rect(node_id))
                .unwrap_or((0.0, 0.0, 0.0, 0.0));
            let obj = boa_engine::object::ObjectInitializer::new(ctx)
                .property(js_string!("left"), JsValue::from(x), Attribute::all())
                .property(js_string!("top"), JsValue::from(y), Attribute::all())
                .property(js_string!("right"), JsValue::from(x + w), Attribute::all())
                .property(js_string!("bottom"), JsValue::from(y + h), Attribute::all())
                .property(js_string!("width"), JsValue::from(w), Attribute::all())
                .property(js_string!("height"), JsValue::from(h), Attribute::all())
                .property(js_string!("x"), JsValue::from(x), Attribute::all())
                .property(js_string!("y"), JsValue::from(y), Attribute::all())
                .build();
            Ok(JsValue::from(obj))
        })
    };

    let ael_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            let event_type = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            if let Some(cb) = args.get(1).and_then(|v| v.as_object().cloned()) {
                registry_add(node_id as u32, &event_type, cb);
            }
            Ok(JsValue::undefined())
        })
    };

    let rel_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            let event_type = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            registry_remove(node_id as u32, &event_type);
            Ok(JsValue::undefined())
        })
    };

    let dispatch_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let event = args.first().cloned().unwrap_or(JsValue::undefined());
            let event_type = event
                .as_object()
                .and_then(|o| o.get(js_string!("type"), ctx).ok())
                .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                .unwrap_or_default();
            if !event_type.is_empty() {
                for cb in registry_get(node_id as u32, &event_type) {
                    let _ = cb.call(&JsValue::undefined(), std::slice::from_ref(&event), ctx);
                }
            }
            Ok(JsValue::from(true))
        })
    };

    let make_noop = || unsafe {
        NativeFunction::from_closure(move |_this, _args, _ctx| Ok(JsValue::undefined()))
    };
    // value / checked / href / src (attribute-backed)
    let attr_getter = |rd: Rc<RefCell<Option<RenderDocument>>>, name: &'static str| unsafe {
        NativeFunction::from_closure(move |_this, _args, _ctx| {
            let v = rd
                .borrow()
                .as_ref()
                .and_then(|d| d.node_attr(node_id, name))
                .unwrap_or_default();
            Ok(JsValue::from(JsString::from(v.as_str())))
        })
    };
    let val_get_fn = attr_getter(render_doc.clone(), "value");
    let val_getter_fn = FunctionObjectBuilder::new(ctx.realm(), val_get_fn)
        .name(js_string!("get value"))
        .build();
    let rd_val_set = render_doc.clone();
    let val_set_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            let v = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            if let Some(doc) = rd_val_set.borrow_mut().as_mut() {
                doc.set_attribute(node_id, "value", &v);
            }
            Ok(JsValue::undefined())
        })
    };
    let val_setter_fn = FunctionObjectBuilder::new(ctx.realm(), val_set_fn)
        .name(js_string!("set value"))
        .build();

    let href_get_fn = attr_getter(render_doc.clone(), "href");
    let href_getter_fn = FunctionObjectBuilder::new(ctx.realm(), href_get_fn)
        .name(js_string!("get href"))
        .build();
    let src_get_fn = attr_getter(render_doc.clone(), "src");
    let src_getter_fn = FunctionObjectBuilder::new(ctx.realm(), src_get_fn)
        .name(js_string!("get src"))
        .build();

    // slot.assignedNodes() / assignedElements(): the light-DOM children
    // distributed into this <slot>. Refreshes the compose view from the live
    // tree, then reads the slot-assignment registry. Returns [] for non-slot
    // nodes (a harmless no-op on ordinary elements).
    let an_rd = render_doc.clone();
    let assigned_nodes_fn = unsafe {
        NativeFunction::from_closure(move |_this, _args, ctx| {
            let assigned = refresh_slot_assignments(&an_rd, node_id);
            let objs: Vec<JsValue> = assigned
                .into_iter()
                .map(|cid| create_render_element_object(ctx, an_rd.clone(), cid as usize))
                .collect();
            Ok(JsArray::from_iter(objs, ctx).into())
        })
    };
    let ae_rd = render_doc.clone();
    let assigned_elements_fn = unsafe {
        NativeFunction::from_closure(move |_this, _args, ctx| {
            let assigned = refresh_slot_assignments(&ae_rd, node_id);
            let objs: Vec<JsValue> = assigned
                .into_iter()
                .filter(|cid| {
                    ae_rd
                        .borrow()
                        .as_ref()
                        .and_then(|d| d.tag_name(*cid as usize))
                        .is_some()
                })
                .map(|cid| create_render_element_object(ctx, ae_rd.clone(), cid as usize))
                .collect();
            Ok(JsArray::from_iter(objs, ctx).into())
        })
    };
    // node.assignedSlot: the <slot> this node was distributed into (open trees
    // only; slots in closed roots yield null), or null.
    let as_rd = render_doc.clone();
    let assigned_slot_get_fn = unsafe {
        NativeFunction::from_closure(move |_this, _args, ctx| {
            {
                let guard = as_rd.borrow();
                if let Some(d) = guard.as_ref() {
                    let _ = crate::js::dom_snapshot::DomSnapshot::from_render_document(d, "", "");
                }
            }
            match crate::js::dom_snapshot::assigned_slot_of(node_id as u32) {
                Some(slot_id) => Ok(create_render_element_object(
                    ctx,
                    as_rd.clone(),
                    slot_id as usize,
                )),
                None => Ok(JsValue::null()),
            }
        })
    };
    let assigned_slot_getter_fn = FunctionObjectBuilder::new(ctx.realm(), assigned_slot_get_fn)
        .name(js_string!("get assignedSlot"))
        .build();

    let obj = boa_engine::object::ObjectInitializer::new(ctx)
        .property(
            js_string!("tagName"),
            JsValue::from(JsString::from(tag_upper.as_str())),
            Attribute::all(),
        )
        .property(
            js_string!("nodeName"),
            JsValue::from(JsString::from(tag_upper.as_str())),
            Attribute::all(),
        )
        .property(js_string!("nodeType"), JsValue::from(1), Attribute::all())
        .property(
            js_string!("__nodeId"),
            JsValue::from(node_id),
            Attribute::all(),
        )
        .function(get_attr_fn, js_string!("getAttribute"), 1)
        .function(set_attr_fn, js_string!("setAttribute"), 2)
        .function(remove_attr_fn, js_string!("removeAttribute"), 1)
        .function(append_child_fn, js_string!("appendChild"), 1)
        .function(remove_fn, js_string!("remove"), 0)
        .function(click_fn, js_string!("click"), 0)
        .function(
            get_bounding_client_rect_fn,
            js_string!("getBoundingClientRect"),
            0,
        )
        .function(ael_fn, js_string!("addEventListener"), 2)
        .function(rel_fn, js_string!("removeEventListener"), 2)
        .function(dispatch_fn, js_string!("dispatchEvent"), 1)
        .function(make_noop(), js_string!("focus"), 0)
        .function(make_noop(), js_string!("blur"), 0)
        .function(make_noop(), js_string!("scrollIntoView"), 0)
        .accessor(
            js_string!("style"),
            Some(style_getter_fn),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("textContent"),
            Some(text_getter_fn),
            Some(text_setter_fn),
            Attribute::all(),
        )
        .accessor(
            js_string!("id"),
            Some(id_getter_fn),
            Some(id_setter_fn),
            Attribute::all(),
        )
        .accessor(
            js_string!("className"),
            Some(cls_getter_fn),
            Some(cls_setter_fn),
            Attribute::all(),
        )
        .accessor(
            js_string!("value"),
            Some(val_getter_fn),
            Some(val_setter_fn),
            Attribute::all(),
        )
        .accessor(
            js_string!("href"),
            Some(href_getter_fn),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("src"),
            Some(src_getter_fn),
            None,
            Attribute::all(),
        )
        .function(assigned_nodes_fn, js_string!("assignedNodes"), 0)
        .function(assigned_elements_fn, js_string!("assignedElements"), 0)
        .accessor(
            js_string!("assignedSlot"),
            Some(assigned_slot_getter_fn),
            None,
            Attribute::all(),
        )
        .build();
    JsValue::from(obj)
}

/// Register the `document` global object with DOM query methods.
fn register_document_object(
    ctx: &mut Context,
    dom_snapshot: &Arc<RwLock<Option<DomSnapshot>>>,
    mutations: &Arc<RwLock<Vec<DomMutation>>>,
    cookie_jar_arc: &Arc<RwLock<Option<Arc<RwLock<CookieJar>>>>>,
    render_doc_rc: &Rc<RefCell<Option<RenderDocument>>>,
) {
    let dom_capture_title = dom_snapshot.clone();
    let title_getter = unsafe {
        NativeFunction::from_closure(move |_this, _args, _ctx| {
            let dom = dom_capture_title.read();
            if let Some(ref s) = *dom {
                Ok(JsValue::from(JsString::from(s.title.as_str())))
            } else {
                Ok(JsValue::from(JsString::from("")))
            }
        })
    };
    let title_getter_fn = FunctionObjectBuilder::new(ctx.realm(), title_getter)
        .name(js_string!("get title"))
        .build();

    let dom_capture_url = dom_snapshot.clone();
    let url_getter: NativeFunction = unsafe {
        NativeFunction::from_closure(move |_this, _args, _ctx| {
            let dom = dom_capture_url.read();
            if let Some(ref s) = *dom {
                Ok(JsValue::from(JsString::from(s.url.as_str())))
            } else {
                Ok(JsValue::from(JsString::from("")))
            }
        })
    };
    let url_getter_fn = FunctionObjectBuilder::new(ctx.realm(), url_getter)
        .name(js_string!("get URL"))
        .build();

    let cookie_jar_for_get = cookie_jar_arc.clone();
    let dom_for_cookie = dom_snapshot.clone();
    let cookie_getter: NativeFunction = unsafe {
        NativeFunction::from_closure(move |_this, _args, _ctx| {
            let dom = dom_for_cookie.read();
            if let Some(ref s) = *dom
                && let Ok(url) = url::Url::parse(&s.url)
            {
                let guard = cookie_jar_for_get.read();
                if let Some(ref jar) = *guard {
                    let cookies = jar.read().cookies_for_js(&url);
                    return Ok(JsValue::from(JsString::from(cookies.as_str())));
                }
            }
            Ok(JsValue::from(JsString::from("")))
        })
    };
    let cookie_getter_fn = FunctionObjectBuilder::new(ctx.realm(), cookie_getter)
        .name(js_string!("get cookie"))
        .build();

    let cookie_jar_for_set = cookie_jar_arc.clone();
    let dom_for_cookie_set = dom_snapshot.clone();
    let cookie_setter: NativeFunction = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            if let Some(cookie_str) = args.first().and_then(|v| v.as_string()) {
                let cookie_string = cookie_str.to_std_string_escaped();
                let dom = dom_for_cookie_set.read();
                if let Some(ref s) = *dom
                    && let Ok(url) = url::Url::parse(&s.url)
                {
                    let guard = cookie_jar_for_set.read();
                    if let Some(ref jar) = *guard {
                        jar.write().store(&url, &cookie_string);
                    }
                }
            }
            Ok(JsValue::undefined())
        })
    };
    let cookie_setter_fn = FunctionObjectBuilder::new(ctx.realm(), cookie_setter)
        .name(js_string!("set cookie"))
        .build();

    // querySelector(selector)
    let dom_capture_qs = dom_snapshot.clone();
    let mutations_capture_qs = mutations.clone();
    let rd_qs = render_doc_rc.clone();
    let query_selector_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let selector = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();

            // Render-document path (single source of truth when set).
            let nid_opt = {
                let guard = rd_qs.borrow();
                guard.as_ref().and_then(|doc| doc.query_selector(&selector))
            };
            if let Some(nid) = nid_opt {
                return Ok(create_render_element_object(ctx, rd_qs.clone(), nid));
            }

            let dom = dom_capture_qs.read();
            if let Some(ref snapshot) = *dom
                && let Some(node_id) = snapshot.query_selector(&selector)
                && let Some(node) = snapshot.nodes.get(&node_id)
            {
                return Ok(create_element_object(
                    snapshot,
                    node,
                    ctx,
                    &mutations_capture_qs,
                    &dom_capture_qs,
                ));
            }
            Ok(JsValue::null())
        })
    };

    // querySelectorAll(selector)
    let dom_capture_qsa = dom_snapshot.clone();
    let mutations_capture_qsa = mutations.clone();
    let rd_qsa = render_doc_rc.clone();
    let query_selector_all_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let selector = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();

            // Render-document path.
            let ids: Vec<usize> = {
                let guard = rd_qsa.borrow();
                guard
                    .as_ref()
                    .map(|doc| doc.query_selector_all(&selector))
                    .unwrap_or_default()
            };
            if !ids.is_empty() {
                let js_values: Vec<JsValue> = ids
                    .into_iter()
                    .map(|nid| create_render_element_object(ctx, rd_qsa.clone(), nid))
                    .collect();
                let arr = JsArray::from_iter(js_values, ctx);
                return Ok(arr.into());
            }

            let dom = dom_capture_qsa.read();
            if let Some(ref snapshot) = *dom {
                let ids = snapshot.query_selector_all(&selector);
                let js_values: Vec<JsValue> = ids
                    .iter()
                    .filter_map(|&id| {
                        snapshot.nodes.get(&id).map(|node| {
                            create_element_object(
                                snapshot,
                                node,
                                ctx,
                                &mutations_capture_qsa,
                                &dom_capture_qsa,
                            )
                        })
                    })
                    .collect();
                let arr = JsArray::from_iter(js_values, ctx);
                return Ok(arr.into());
            }
            let arr = JsArray::from_iter(Vec::<JsValue>::new(), ctx);
            Ok(arr.into())
        })
    };

    // getElementById(id)
    let dom_capture_gbi = dom_snapshot.clone();
    let mutations_capture_gbi = mutations.clone();
    let rd_gbi = render_doc_rc.clone();
    let get_element_by_id_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let id = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();

            // Render-document path.
            let nid_opt = {
                let guard = rd_gbi.borrow();
                guard
                    .as_ref()
                    .and_then(|doc| doc.query_selector(&format!("#{id}")))
            };
            if let Some(nid) = nid_opt {
                return Ok(create_render_element_object(ctx, rd_gbi.clone(), nid));
            }

            let dom = dom_capture_gbi.read();
            if let Some(ref snapshot) = *dom
                && let Some(node_id) = snapshot.get_element_by_id(&id)
                && let Some(node) = snapshot.nodes.get(&node_id)
            {
                return Ok(create_element_object(
                    snapshot,
                    node,
                    ctx,
                    &mutations_capture_gbi,
                    &dom_capture_gbi,
                ));
            }
            Ok(JsValue::null())
        })
    };

    // getElementsByTagName(tag)
    let dom_capture_gtn = dom_snapshot.clone();
    let mutations_capture_gtn = mutations.clone();
    let rd_gtn = render_doc_rc.clone();
    let get_elements_by_tag_name_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let tag = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();

            // Render-document path.
            let ids: Vec<usize> = {
                let guard = rd_gtn.borrow();
                guard
                    .as_ref()
                    .map(|doc| doc.query_selector_all(&tag))
                    .unwrap_or_default()
            };
            if !ids.is_empty() {
                let js_values: Vec<JsValue> = ids
                    .into_iter()
                    .map(|nid| create_render_element_object(ctx, rd_gtn.clone(), nid))
                    .collect();
                let arr = JsArray::from_iter(js_values, ctx);
                return Ok(arr.into());
            }

            let dom = dom_capture_gtn.read();
            if let Some(ref snapshot) = *dom {
                let ids = snapshot.get_elements_by_tag_name(&tag);
                let js_values: Vec<JsValue> = ids
                    .iter()
                    .filter_map(|&id| {
                        snapshot.nodes.get(&id).map(|node| {
                            create_element_object(
                                snapshot,
                                node,
                                ctx,
                                &mutations_capture_gtn,
                                &dom_capture_gtn,
                            )
                        })
                    })
                    .collect();
                let arr = JsArray::from_iter(js_values, ctx);
                return Ok(arr.into());
            }
            let arr = JsArray::from_iter(Vec::<JsValue>::new(), ctx);
            Ok(arr.into())
        })
    };

    // getElementsByClassName(class)
    let dom_capture_gcn = dom_snapshot.clone();
    let mutations_capture_gcn = mutations.clone();
    let rd_gcn = render_doc_rc.clone();
    let get_elements_by_class_name_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let class = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();

            // Render-document path.
            let ids: Vec<usize> = {
                let guard = rd_gcn.borrow();
                guard
                    .as_ref()
                    .map(|doc| doc.query_selector_all(&format!(".{class}")))
                    .unwrap_or_default()
            };
            if !ids.is_empty() {
                let js_values: Vec<JsValue> = ids
                    .into_iter()
                    .map(|nid| create_render_element_object(ctx, rd_gcn.clone(), nid))
                    .collect();
                let arr = JsArray::from_iter(js_values, ctx);
                return Ok(arr.into());
            }

            let dom = dom_capture_gcn.read();
            if let Some(ref snapshot) = *dom {
                let ids = snapshot.get_elements_by_class_name(&class);
                let js_values: Vec<JsValue> = ids
                    .iter()
                    .filter_map(|&id| {
                        snapshot.nodes.get(&id).map(|node| {
                            create_element_object(
                                snapshot,
                                node,
                                ctx,
                                &mutations_capture_gcn,
                                &dom_capture_gcn,
                            )
                        })
                    })
                    .collect();
                let arr = JsArray::from_iter(js_values, ctx);
                return Ok(arr.into());
            }
            let arr = JsArray::from_iter(Vec::<JsValue>::new(), ctx);
            Ok(arr.into())
        })
    };

    // EventTarget methods for document — uses __listeners property on the document object
    let doc_add_event_listener_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let event_type = args
                .first()
                .and_then(|v| v.to_string(ctx).ok())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            let callback = args.get(1).cloned().unwrap_or(JsValue::undefined());

            if callback.is_undefined() || callback.is_null() {
                return Ok(JsValue::undefined());
            }

            let this_obj = match _this.as_object() {
                Some(o) => o,
                None => return Ok(JsValue::undefined()),
            };

            // Get or create __listeners object
            let listeners_val = this_obj
                .get(js_string!("__listeners"), ctx)
                .unwrap_or(JsValue::Null);
            // Create __listeners if missing
            if listeners_val.as_object().is_none() {
                let obj = boa_engine::object::ObjectInitializer::new(ctx).build();
                let _ = this_obj.set(js_string!("__listeners"), JsValue::from(obj), true, ctx);
            }
            let lv2 = this_obj
                .get(js_string!("__listeners"), ctx)
                .unwrap_or(JsValue::Null);
            let listeners_obj = match lv2.as_object() {
                Some(o) => o,
                None => return Ok(JsValue::undefined()),
            };

            // Ensure array for this event type
            let arr_key = JsString::from(event_type.as_str());
            let ev = listeners_obj
                .get(arr_key.clone(), ctx)
                .unwrap_or(JsValue::Null);
            if ev.as_object().is_none() {
                let a: JsValue = JsValue::from(JsArray::new(ctx));
                let _ = listeners_obj.set(arr_key.clone(), a, true, ctx);
            }
            let arr_val = listeners_obj.get(arr_key, ctx).unwrap_or(JsValue::Null);
            if let Some(arr_obj) = arr_val.as_object()
                && let Ok(arr) = JsArray::from_object(arr_obj.clone())
            {
                let _ = arr.push(callback, ctx);
            }

            Ok(JsValue::undefined())
        })
    };

    let doc_remove_event_listener_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let event_type = args
                .first()
                .and_then(|v| v.to_string(ctx).ok())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();

            if let Some(this_obj) = _this.as_object()
                && let Ok(l_val) = this_obj.get(js_string!("__listeners"), ctx)
                && let Some(l_obj) = l_val.as_object()
            {
                let _ = l_obj.set(
                    JsString::from(event_type.as_str()),
                    JsValue::Null,
                    true,
                    ctx,
                );
            }
            Ok(JsValue::undefined())
        })
    };

    let doc_dispatch_event_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let event = args.first().cloned().unwrap_or(JsValue::undefined());

            let event_type = if let Some(evt_obj) = event.as_object() {
                evt_obj
                    .get(js_string!("type"), ctx)
                    .ok()
                    .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                    .unwrap_or_default()
            } else if let Some(s) = event.as_string() {
                s.to_std_string_escaped()
            } else {
                return Ok(JsValue::from(true));
            };

            if let Some(this_obj) = _this.as_object()
                && let Ok(l_val) = this_obj.get(js_string!("__listeners"), ctx)
                && let Some(l_obj) = l_val.as_object()
            {
                let arr_val = l_obj
                    .get(JsString::from(event_type.as_str()), ctx)
                    .unwrap_or(JsValue::Null);
                if let Some(arr_obj) = arr_val.as_object()
                    && let Ok(arr) = JsArray::from_object(arr_obj.clone())
                    && let Ok(len) = arr.length(ctx)
                {
                    for i in 0..len {
                        if let Ok(cb) = arr.at(i as i64, ctx)
                            && let Some(cb_obj) = cb.as_object()
                            && cb_obj.is_callable()
                        {
                            let _ = cb_obj.call(_this, std::slice::from_ref(&event), ctx);
                        }
                    }
                }
            }

            Ok(JsValue::from(true))
        })
    };

    // document.body / document.head / document.documentElement getters
    let rd_body = render_doc_rc.clone();
    let dom_snap_body = dom_snapshot.clone();
    let dom_snap_body_clone = dom_snapshot.clone();
    let body_getter_fn = {
        let mutations_clone = mutations.clone();
        let getter: NativeFunction = unsafe {
            NativeFunction::from_closure(move |_this, _args, ctx| {
                // Render-document path.
                let nid_opt = {
                    let guard = rd_body.borrow();
                    guard.as_ref().and_then(|doc| doc.query_selector("body"))
                };
                if let Some(nid) = nid_opt {
                    return Ok(create_render_element_object(ctx, rd_body.clone(), nid));
                }

                let snap = dom_snap_body.read();
                if let Some(ref s) = *snap
                    && let Some(bid) = s.body_id
                    && let Some(node) = s.nodes.get(&bid)
                {
                    return Ok(create_element_object(
                        s,
                        node,
                        ctx,
                        &mutations_clone,
                        &dom_snap_body_clone,
                    ));
                }
                Ok(JsValue::null())
            })
        };
        FunctionObjectBuilder::new(ctx.realm(), getter)
            .name(js_string!("get body"))
            .build()
    };

    let rd_head = render_doc_rc.clone();
    let dom_snap_head = dom_snapshot.clone();
    let dom_snap_head_clone = dom_snapshot.clone();
    let head_getter_fn = {
        let mutations_clone = mutations.clone();
        let getter: NativeFunction = unsafe {
            NativeFunction::from_closure(move |_this, _args, ctx| {
                // Render-document path.
                let nid_opt = {
                    let guard = rd_head.borrow();
                    guard.as_ref().and_then(|doc| doc.query_selector("head"))
                };
                if let Some(nid) = nid_opt {
                    return Ok(create_render_element_object(ctx, rd_head.clone(), nid));
                }

                let snap = dom_snap_head.read();
                if let Some(ref s) = *snap
                    && let Some(hid) = s.head_id
                    && let Some(node) = s.nodes.get(&hid)
                {
                    return Ok(create_element_object(
                        s,
                        node,
                        ctx,
                        &mutations_clone,
                        &dom_snap_head_clone,
                    ));
                }
                Ok(JsValue::null())
            })
        };
        FunctionObjectBuilder::new(ctx.realm(), getter)
            .name(js_string!("get head"))
            .build()
    };

    let rd_de = render_doc_rc.clone();
    let dom_snap_de = dom_snapshot.clone();
    let document_element_getter_fn = {
        let mutations_clone = mutations.clone();
        let getter: NativeFunction = unsafe {
            NativeFunction::from_closure(move |_this, _args, ctx| {
                // Render-document path: root element (<html>).
                let nid_opt = {
                    let guard = rd_de.borrow();
                    guard.as_ref().map(|doc| doc.root_element_id())
                };
                if let Some(nid) = nid_opt {
                    return Ok(create_render_element_object(ctx, rd_de.clone(), nid));
                }

                let snap = dom_snap_de.read();
                if let Some(ref s) = *snap {
                    // document.documentElement should be the <html> element,
                    // which is a child of the root Document node.
                    let html_node = s.nodes.get(&s.root_id).and_then(|root| {
                        root.children.iter().find_map(|&child_id| {
                            s.nodes.get(&child_id).and_then(|n| {
                                if n.tag == "html" {
                                    Some((child_id, n))
                                } else {
                                    None
                                }
                            })
                        })
                    });
                    if let Some((_, node)) = html_node {
                        return Ok(create_element_object(
                            s,
                            node,
                            ctx,
                            &mutations_clone,
                            &dom_snap_de,
                        ));
                    }
                }
                Ok(JsValue::null())
            })
        };
        FunctionObjectBuilder::new(ctx.realm(), getter)
            .name(js_string!("get documentElement"))
            .build()
    };

    // === document.write() ===
    let dw_snap = dom_snapshot.clone();
    let dw_mut = mutations.clone();
    let doc_write_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let html = args
                .first()
                .and_then(|v| v.to_string(ctx).ok())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            if html.is_empty() {
                return Ok(JsValue::undefined());
            }

            // Parse HTML fragment into text nodes and append to body
            let mut dom = dw_snap.write();
            if let Some(ref mut snap) = *dom {
                let body_id = match snap.body_id {
                    Some(id) => id,
                    None => return Ok(JsValue::undefined()),
                };

                // Generate a new node ID
                let max_id = snap.nodes.keys().max().copied().unwrap_or(0);
                let new_id = max_id + 1;

                // Create a text node with the raw HTML content
                // (Full HTML fragment parsing requires html5ever tree builder —
                //  for now, insert as a single text node)
                let node = DomNode {
                    id: new_id,
                    tag: String::new(),
                    attributes: HashMap::new(),
                    text_content: html.clone(),
                    children: Vec::new(),
                    parent: Some(body_id),
                    node_type: 3, // TEXT_NODE
                };
                snap.nodes.insert(new_id, node);

                // Append to body's children
                if let Some(body) = snap.nodes.get_mut(&body_id) {
                    body.children.push(new_id);
                }

                dw_mut.write().push(DomMutation::AppendChild {
                    parent_id: body_id,
                    child_id: new_id,
                });
            }

            Ok(JsValue::undefined())
        })
    };

    // === DOM Mutation: createElement ===
    let dom_snap_ce = dom_snapshot.clone();
    let mutations_ce = mutations.clone();
    let rd_ce = render_doc_rc.clone();
    let create_element_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let tag = args
                .first()
                .and_then(|v| v.to_string(ctx).ok())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            if tag.is_empty() {
                return Ok(JsValue::undefined());
            }

            // Render-document path: create the element directly in the
            // RenderDocument (detached until appendChild attaches it).
            let nid_opt = {
                let mut guard = rd_ce.borrow_mut();
                guard.as_mut().map(|doc| doc.create_element(&tag))
            };
            if let Some(nid) = nid_opt {
                return Ok(upgrade_custom_element(
                    create_render_element_object(ctx, rd_ce.clone(), nid),
                    ctx,
                ));
            }

            // Generate a unique node ID using an atomic counter (avoids collisions in tight loops)
            let new_id = NEXT_NODE_ID.fetch_add(1, Ordering::Relaxed) as u32;

            let tag_upper = tag.to_uppercase();

            // Create DomNode in snapshot
            {
                let mut dom = dom_snap_ce.write();
                if let Some(ref mut snap) = *dom {
                    let node = DomNode {
                        id: new_id,
                        tag: tag.clone(),
                        attributes: HashMap::new(),
                        text_content: String::new(),
                        children: Vec::new(),
                        parent: None,
                        node_type: 1,
                    };
                    snap.nodes.insert(new_id, node);
                }
            }

            // Record mutation
            mutations_ce.write().push(DomMutation::CreateElement {
                node_id: new_id,
                tag: tag.clone(),
            });

            // Build a JS element object
            let tag_for_obj = tag_upper.clone();
            let id_for_obj = new_id;
            // Shared attribute map so getAttribute sees setAttribute mutations
            let attrs_map: Arc<parking_lot::RwLock<HashMap<String, String>>> =
                Arc::new(parking_lot::RwLock::new(HashMap::new()));
            let dom_snap_el = dom_snap_ce.clone();
            let mutations_el = mutations_ce.clone();

            // setAttribute for this element
            let mut_set_attr = mutations_el.clone();
            let mut_set_id = id_for_obj;
            let attrs_for_set = attrs_map.clone();
            let dom_snap_for_setattr = dom_snap_el.clone();
            let set_attr_fn = {
                NativeFunction::from_closure(move |_this, args, _ctx| {
                    let name = args
                        .first()
                        .and_then(|v| v.as_string())
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    let value = args
                        .get(1)
                        .and_then(|v| v.as_string())
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    // Update shared attribute map so getAttribute sees the change
                    attrs_for_set.write().insert(name.clone(), value.clone());
                    // Sync to snapshot so querySelector can find the attribute
                    {
                        let mut dom = dom_snap_for_setattr.write();
                        if let Some(ref mut snap) = *dom
                            && let Some(node) = snap.nodes.get_mut(&mut_set_id)
                        {
                            node.attributes.insert(name.clone(), value.clone());
                        }
                    }
                    mut_set_attr.write().push(DomMutation::SetAttribute {
                        node_id: mut_set_id,
                        name,
                        value,
                    });
                    Ok(JsValue::undefined())
                })
            };

            // getAttribute for this element — reads from shared Arc<RwLock<HashMap>>
            let attrs_for_get = attrs_map.clone();
            let get_attr_fn = {
                NativeFunction::from_closure(move |_this, args, _ctx| {
                    let name = args
                        .first()
                        .and_then(|v| v.as_string())
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    match attrs_for_get.read().get(&name) {
                        Some(v) => Ok(JsValue::from(JsString::from(v.as_str()))),
                        None => Ok(JsValue::null()),
                    }
                })
            };

            // click for this element
            let mut_click = mutations_el.clone();
            let click_id = id_for_obj;
            let click_fn = {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    mut_click
                        .write()
                        .push(DomMutation::ClickElement { node_id: click_id });
                    Ok(JsValue::undefined())
                })
            };

            // appendChild for this element
            let dom_snap_ac = dom_snap_el.clone();
            let parent_id_ac = id_for_obj;
            let append_child_fn = {
                NativeFunction::from_closure(move |_this, args, ctx| {
                    let child = args.first().cloned().unwrap_or(JsValue::undefined());
                    let child_id = child
                        .as_object()
                        .and_then(|o| o.get(js_string!("__nodeId"), ctx).ok())
                        .and_then(|v| v.as_number().map(|n| n as u32));

                    if let Some(cid) = child_id {
                        // Update snapshot
                        {
                            let mut dom = dom_snap_ac.write();
                            if let Some(ref mut snap) = *dom {
                                // Add child to parent's children list
                                if let Some(parent) = snap.nodes.get_mut(&parent_id_ac)
                                    && !parent.children.contains(&cid)
                                {
                                    parent.children.push(cid);
                                }
                                // Set child's parent
                                if let Some(child_node) = snap.nodes.get_mut(&cid) {
                                    child_node.parent = Some(parent_id_ac);
                                }
                            }
                        }
                        // Notify MutationObservers
                        notify_mutation_observers(ctx, "childList", parent_id_ac);
                    }

                    Ok(child)
                })
            };

            // 생성된 노드를 snapshot에서 찾아 create_element_object로 완전한 요소 생성
            // 이렇게 하면 새 요소도 style, classList, cloneNode, remove 등 모든 메서드를 가짐
            let dom = dom_snap_ce.read();
            if let Some(ref snap) = *dom
                && let Some(new_node) = snap.nodes.get(&new_id)
            {
                return Ok(create_element_object(
                    snap,
                    new_node,
                    ctx,
                    &mutations_ce,
                    &dom_snap_ce,
                ));
            }
            // fallback: snapshot에서 못 찾으면 기본 객체 반환
            let obj = boa_engine::object::ObjectInitializer::new(ctx)
                .property(
                    js_string!("tagName"),
                    JsValue::from(JsString::from(tag_for_obj.as_str())),
                    Attribute::all(),
                )
                .property(
                    js_string!("nodeName"),
                    JsValue::from(JsString::from(tag_for_obj.as_str())),
                    Attribute::all(),
                )
                .property(
                    js_string!("textContent"),
                    JsValue::from(JsString::from("")),
                    Attribute::all(),
                )
                .property(
                    js_string!("id"),
                    JsValue::from(JsString::from("")),
                    Attribute::all(),
                )
                .property(
                    js_string!("className"),
                    JsValue::from(JsString::from("")),
                    Attribute::all(),
                )
                .property(
                    js_string!("__nodeId"),
                    JsValue::from(id_for_obj),
                    Attribute::all(),
                )
                .function(get_attr_fn, js_string!("getAttribute"), 1)
                .function(set_attr_fn, js_string!("setAttribute"), 2)
                .function(click_fn, js_string!("click"), 0)
                .function(append_child_fn, js_string!("appendChild"), 1)
                .build();
            Ok(upgrade_custom_element(JsValue::from(obj), ctx))
        })
    };

    // === DOM Mutation: createTextNode ===
    let dom_snap_ct = dom_snapshot.clone();
    let mutations_ct = mutations.clone();
    let rd_ct = render_doc_rc.clone();
    let create_text_node_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let text = args
                .first()
                .and_then(|v| v.to_string(ctx).ok())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();

            // Render-document path.
            let nid_opt = {
                let mut guard = rd_ct.borrow_mut();
                guard.as_mut().map(|doc| doc.create_text_node(&text))
            };
            if let Some(nid) = nid_opt {
                let obj = boa_engine::object::ObjectInitializer::new(ctx)
                    .property(
                        js_string!("textContent"),
                        JsValue::from(JsString::from(text.as_str())),
                        Attribute::all(),
                    )
                    .property(js_string!("nodeType"), JsValue::from(3), Attribute::all())
                    .property(js_string!("__nodeId"), JsValue::from(nid), Attribute::all())
                    .build();
                return Ok(JsValue::from(obj));
            }

            let new_id = NEXT_NODE_ID.fetch_add(1, Ordering::Relaxed) as u32;

            {
                let mut dom = dom_snap_ct.write();
                if let Some(ref mut snap) = *dom {
                    let node = DomNode {
                        id: new_id,
                        tag: String::new(),
                        attributes: HashMap::new(),
                        text_content: text.clone(),
                        children: Vec::new(),
                        parent: None,
                        node_type: 3,
                    };
                    snap.nodes.insert(new_id, node);
                }
            }

            mutations_ct.write().push(DomMutation::CreateTextNode {
                node_id: new_id,
                text: text.clone(),
            });

            let obj = boa_engine::object::ObjectInitializer::new(ctx)
                .property(
                    js_string!("textContent"),
                    JsValue::from(JsString::from(text.as_str())),
                    Attribute::all(),
                )
                .property(js_string!("nodeType"), JsValue::from(3), Attribute::all())
                .property(
                    js_string!("__nodeId"),
                    JsValue::from(new_id),
                    Attribute::all(),
                )
                .build();

            Ok(JsValue::from(obj))
        })
    };

    let ready_state_getter_fn = FunctionObjectBuilder::new(ctx.realm(), unsafe {
        NativeFunction::from_closure(move |_this, _args, _ctx| {
            let s = DOC_READY_STATE.with(|c| c.get());
            Ok(JsValue::from(js_string!(s)))
        })
    })
    .name(js_string!("get readyState"))
    .build();
    let document_obj = boa_engine::object::ObjectInitializer::new(ctx)
        .accessor(
            js_string!("title"),
            Some(title_getter_fn),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("URL"),
            Some(url_getter_fn),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("cookie"),
            Some(cookie_getter_fn),
            Some(cookie_setter_fn),
            Attribute::all(),
        )
        .function(query_selector_fn, js_string!("querySelector"), 1)
        .function(query_selector_all_fn, js_string!("querySelectorAll"), 1)
        .function(get_element_by_id_fn, js_string!("getElementById"), 1)
        .function(
            get_elements_by_tag_name_fn,
            js_string!("getElementsByTagName"),
            1,
        )
        .function(
            get_elements_by_class_name_fn,
            js_string!("getElementsByClassName"),
            1,
        )
        .function(doc_add_event_listener_fn, js_string!("addEventListener"), 2)
        .function(
            doc_remove_event_listener_fn,
            js_string!("removeEventListener"),
            2,
        )
        .function(doc_dispatch_event_fn, js_string!("dispatchEvent"), 1)
        .function(create_element_fn, js_string!("createElement"), 1)
        .function(create_text_node_fn, js_string!("createTextNode"), 1)
        .function(doc_write_fn, js_string!("write"), 1)
        // DOM tree accessors
        .accessor(
            js_string!("body"),
            Some(body_getter_fn.clone()),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("head"),
            Some(head_getter_fn),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("documentElement"),
            Some(document_element_getter_fn),
            None,
            Attribute::all(),
        )
        // activeElement — same as body (no real focus tracking yet)
        .accessor(
            js_string!("activeElement"),
            Some(body_getter_fn),
            None,
            Attribute::all(),
        )
        // elementFromPoint(x, y) — returns element at viewport coordinates.
        // Approximation: finds the Nth visible element by DOM order.
        // We approximate Y positions using estimated line heights.
        .function(
            {
                let snap_efp = dom_snapshot.clone();
                let mutations_efp = mutations.clone();
                let dom_efp = dom_snapshot.clone();
                let rd_efp = render_doc_rc.clone();
                unsafe {
                    let fn_ptr: NativeFunction =
                        NativeFunction::from_closure(move |_this, args, ctx| {
                            // elementFromPoint(x, y) — layout-based hit test.
                            //
                            // Uses the RenderDocument's Taffy layout boxes to find the
                            // deepest element whose laid-out box contains (x, y). Falls
                            // back to null when no rendered document is available.
                            let x = args
                                .first()
                                .and_then(|v| v.to_number(ctx).ok())
                                .unwrap_or(0.0);
                            let y = args
                                .get(1)
                                .and_then(|v| v.to_number(ctx).ok())
                                .unwrap_or(0.0);
                            // Layout-based hit test against the live DomSnapshot
                            // (kept consistent with the render doc's Taffy boxes)
                            // + RenderDocument layout rects.
                            let snap = snap_efp.read();
                            if let Some(ref s) = *snap {
                                let el = rd_efp
                                    .borrow()
                                    .as_ref()
                                    .and_then(|d| hit_test_element(d, s, x, y))
                                    .and_then(|id| s.nodes.get(&id));
                                if let Some(el) = el {
                                    return Ok(create_element_object(
                                        s,
                                        el,
                                        ctx,
                                        &mutations_efp,
                                        &dom_efp,
                                    ));
                                }
                            }
                            Ok(JsValue::null())
                        });
                    fn_ptr
                }
            },
            js_string!("elementFromPoint"),
            2,
        )
        .accessor(
            js_string!("readyState"),
            Some(ready_state_getter_fn),
            None,
            Attribute::all(),
        )
        .build();

    let _ = ctx.register_global_property(js_string!("document"), document_obj, Attribute::all());
}

/// Create a JS element object from a DomNode.
fn create_element_object(
    snapshot: &DomSnapshot,
    node: &DomNode,
    ctx: &mut Context,
    mutations: &Arc<RwLock<Vec<DomMutation>>>,
    dom_snapshot_arc: &Arc<RwLock<Option<DomSnapshot>>>,
) -> JsValue {
    let tag_upper = node.tag.to_uppercase();
    let href_val = node
        .attributes
        .get("href")
        .map(|s| s.as_str())
        .unwrap_or("");
    let src_val = node.attributes.get("src").map(|s| s.as_str()).unwrap_or("");

    // Inject data-oxi-node-id into attributes so that
    // Runtime.callFunctionOn can resolve nodes via querySelector.
    // We add it to the cloned attribute map so getAttribute/hasAttribute
    // can also see it.
    let mut enriched_attrs: HashMap<String, String> = node.attributes.clone();
    enriched_attrs.insert("data-oxi-node-id".to_string(), node.id.to_string());

    // getAttribute(name)
    // getAttribute(name) — reads from live snapshot (reflects setAttribute mutations)
    let dom_snap_ga = dom_snapshot_arc.clone();
    let node_id_ga = node.id;
    let get_attribute_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            let name = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            // 읽기 전용 snapshot에서 attribute 조회 (setAttribute가 snapshot에 반영됨)
            let dom = dom_snap_ga.read();
            if let Some(ref snap) = *dom
                && let Some(n) = snap.nodes.get(&node_id_ga)
                && let Some(val) = n.attributes.get(&name)
            {
                return Ok(JsValue::from(JsString::from(val.as_str())));
            }
            Ok(JsValue::null())
        })
    };

    // hasAttribute(name) — reads from live snapshot
    let dom_snap_ha = dom_snapshot_arc.clone();
    let node_id_ha = node.id;
    let has_attribute_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            let name = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            let dom = dom_snap_ha.read();
            if let Some(ref snap) = *dom
                && let Some(n) = snap.nodes.get(&node_id_ha)
            {
                return Ok(JsValue::from(n.attributes.contains_key(&name)));
            }
            Ok(JsValue::from(false))
        })
    };

    // addEventListener — stores callback by event type on the JS object itself.
    // We use a hidden `__listeners` property: { "click": [fn1, fn2], "DOMContentLoaded": [fn3] }
    let node_id_ael = node.id;
    let add_event_listener_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let event_type = args
                .first()
                .and_then(|v| v.to_string(ctx).ok())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            let callback = args.get(1).cloned().unwrap_or(JsValue::undefined());

            if callback.is_undefined() || callback.is_null() {
                return Ok(JsValue::undefined());
            }

            let this_obj = match _this.as_object() {
                Some(o) => o,
                None => return Ok(JsValue::undefined()),
            };
            // Ensure __listeners exists
            let lv = this_obj
                .get(js_string!("__listeners"), ctx)
                .unwrap_or(JsValue::Null);
            if lv.as_object().is_none() {
                let obj = boa_engine::object::ObjectInitializer::new(ctx).build();
                let _ = this_obj.set(js_string!("__listeners"), JsValue::from(obj), true, ctx);
            }
            let listeners_val2 = this_obj
                .get(js_string!("__listeners"), ctx)
                .unwrap_or(JsValue::Null);
            let listeners_obj = match listeners_val2.as_object() {
                Some(o) => o,
                None => return Ok(JsValue::undefined()),
            };
            // Ensure array for this event type
            let arr_key = JsString::from(event_type.as_str());
            let ev = listeners_obj
                .get(arr_key.clone(), ctx)
                .unwrap_or(JsValue::Null);
            if ev.as_object().is_none() {
                let a: JsValue = JsValue::from(JsArray::new(ctx));
                let _ = listeners_obj.set(arr_key.clone(), a, true, ctx);
            }
            let arr_val = listeners_obj.get(arr_key, ctx).unwrap_or(JsValue::Null);
            if let Some(arr_obj) = arr_val.as_object()
                && let Ok(arr) = JsArray::from_object(arr_obj.clone())
            {
                let _ = arr.push(callback.clone(), ctx);
            }

            // Also store in the nodeId-keyed registry so bubbling can find
            // listeners registered through any element object instance.
            if let Some(cb_obj) = callback.as_object() {
                registry_add(node_id_ael, &event_type, cb_obj.clone());
            }

            Ok(JsValue::undefined())
        })
    };

    // removeEventListener — removes callback from __listeners
    let node_id_rel = node.id;
    let remove_event_listener_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let event_type = args
                .first()
                .and_then(|v| v.to_string(ctx).ok())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            let _callback = args.get(1);

            let this_obj = _this.as_object().unwrap();
            let listeners = this_obj.get(js_string!("__listeners"), ctx);
            if let Ok(l_val) = listeners
                && let Some(l_obj) = l_val.as_object()
            {
                let _ = l_obj.set(
                    JsString::from(event_type.as_str()),
                    JsValue::Null,
                    true,
                    ctx,
                );
            }

            // Also remove from the nodeId-keyed registry.
            registry_remove(node_id_rel, &event_type);

            Ok(JsValue::undefined())
        })
    };

    // dispatchEvent — calls all registered callbacks for the event type
    let node_id_disp = node.id;
    let snap_disp = dom_snapshot_arc.clone();
    let dispatch_event_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let event = args.first().cloned().unwrap_or(JsValue::undefined());

            // Need a valid event object with a "type" property
            let Some(evt_obj) = event.as_object() else {
                return Ok(JsValue::from(true));
            };

            let event_type = evt_obj
                .get(js_string!("type"), ctx)
                .ok()
                .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                .unwrap_or_default();

            // Set target/currentTarget before any listener runs
            let _ = evt_obj.set(js_string!("target"), _this.clone(), true, ctx);
            let _ = evt_obj.set(js_string!("currentTarget"), _this.clone(), true, ctx);

            let this_obj = _this.as_object().unwrap();
            let listeners = this_obj.get(js_string!("__listeners"), ctx);
            if let Ok(l_val) = listeners
                && let Some(l_obj) = l_val.as_object()
            {
                let arr_val = l_obj
                    .get(JsString::from(event_type.as_str()), ctx)
                    .unwrap_or(JsValue::Null);
                if let Some(arr_obj) = arr_val.as_object()
                    && let Ok(arr) = JsArray::from_object(arr_obj.clone())
                    && let Ok(len) = arr.length(ctx)
                {
                    for i in 0..len {
                        // Check stopImmediatePropagation before each callback
                        if evt_obj
                            .get(js_string!("__stopImmediatePropagation"), ctx)
                            .ok()
                            .and_then(|v| v.as_boolean())
                            .unwrap_or(false)
                        {
                            break;
                        }

                        if let Ok(cb) = arr.at(i as i64, ctx)
                            && let Some(cb_obj) = cb.as_object()
                            && cb_obj.is_callable()
                        {
                            let evt_arg = event.clone();
                            let _ = cb_obj.call(_this, &[evt_arg], ctx);
                        }
                    }
                }
            }

            // === Bubbling phase ===
            // Walk the DomSnapshot parent chain (not JS parentNode stubs,
            // which lack __listeners). For each ancestor nodeId, look up
            // listeners in the thread-local registry.
            let bubbles = evt_obj
                .get(js_string!("bubbles"), ctx)
                .ok()
                .and_then(|v| v.as_boolean())
                .unwrap_or(true);
            if bubbles {
                // Build the ancestor nodeId chain from the snapshot.
                let mut ancestor_ids: Vec<u32> = Vec::new();
                let snap = snap_disp.read();
                if let Some(ref s) = *snap {
                    let mut current = s.nodes.get(&node_id_disp).and_then(|n| n.parent);
                    while let Some(pid) = current {
                        // Stop at document nodes (type 9) — they use a
                        // separate listener system on the document object.
                        match s.nodes.get(&pid) {
                            Some(pn) if pn.node_type == 1 => {
                                ancestor_ids.push(pid);
                                current = pn.parent;
                            }
                            _ => break,
                        }
                    }
                }
                drop(snap);

                for aid in ancestor_ids {
                    // Check stopPropagation
                    if evt_obj
                        .get(js_string!("__stopPropagation"), ctx)
                        .ok()
                        .and_then(|v| v.as_boolean())
                        .unwrap_or(false)
                    {
                        break;
                    }

                    // Look up listeners from the registry (NOT from JS
                    // __listeners, which lives on a different object instance).
                    let callbacks = registry_get(aid, &event_type);
                    if callbacks.is_empty() {
                        continue;
                    }

                    // Build a minimal ancestor JS object for currentTarget.
                    let ancestor_obj = boa_engine::object::ObjectInitializer::new(ctx)
                        .property(js_string!("__nodeId"), JsValue::from(aid), Attribute::all())
                        .build();
                    let _ = evt_obj.set(
                        js_string!("currentTarget"),
                        JsValue::from(ancestor_obj.clone()),
                        true,
                        ctx,
                    );

                    let ancestor_val = JsValue::from(ancestor_obj);
                    for cb in &callbacks {
                        if evt_obj
                            .get(js_string!("__stopImmediatePropagation"), ctx)
                            .ok()
                            .and_then(|v| v.as_boolean())
                            .unwrap_or(false)
                        {
                            break;
                        }
                        let _ = cb.call(&ancestor_val, std::slice::from_ref(&event), ctx);
                    }
                }
            }

            let prevented = evt_obj
                .get(js_string!("defaultPrevented"), ctx)
                .ok()
                .and_then(|v| v.as_boolean())
                .unwrap_or(false);

            Ok(JsValue::from(!prevented))
        })
    };

    // click() → fires JS event handlers + records DomMutation::ClickElement
    let node_id_click = node.id;
    let mutations_click = mutations.clone();
    let click_fn = unsafe {
        NativeFunction::from_closure(move |_this, _args, ctx| {
            // 1. mutation 기록
            mutations_click.write().push(DomMutation::ClickElement {
                node_id: node_id_click,
            });
            // 2. __listeners에서 click 핸들러 찾아서 실행
            if let Some(this_obj) = _this.as_object()
                && let Ok(listeners_val) = this_obj.get(js_string!("__listeners"), ctx)
                && let Some(listeners_obj) = listeners_val.as_object()
                && let Ok(arr_val) = listeners_obj.get(js_string!("click"), ctx)
                && let Some(arr_js) = arr_val.as_object()
                && let Ok(arr) = JsArray::from_object(arr_js.clone())
            {
                let len = arr.length(ctx).unwrap_or(0) as usize;
                let event_obj = boa_engine::object::ObjectInitializer::new(ctx)
                    .property(
                        js_string!("type"),
                        JsValue::from(JsString::from("click")),
                        Attribute::all(),
                    )
                    .property(js_string!("target"), _this.clone(), Attribute::all())
                    .property(js_string!("currentTarget"), _this.clone(), Attribute::all())
                    .property(js_string!("bubbles"), JsValue::from(true), Attribute::all())
                    .build();
                for i in 0..len {
                    if let Ok(cb) = arr.get(i as u64, ctx)
                        && let Some(cb_obj) = cb.as_object()
                    {
                        let _ = cb_obj.call(_this, &[JsValue::from(event_obj.clone())], ctx);
                    }
                }
            }
            Ok(JsValue::undefined())
        })
    };

    // setAttribute(name, value) → records DomMutation::SetAttribute
    let node_id_sa = node.id;
    let mutations_sa = mutations.clone();
    let dom_snap_sa = dom_snapshot_arc.clone();
    let set_attribute_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            let name = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            let value = args
                .get(1)
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            // Sync to snapshot so querySelector can find the attribute
            {
                let mut dom = dom_snap_sa.write();
                if let Some(ref mut snap) = *dom
                    && let Some(node) = snap.nodes.get_mut(&node_id_sa)
                {
                    node.attributes.insert(name.clone(), value.clone());
                }
            }
            mutations_sa.write().push(DomMutation::SetAttribute {
                node_id: node_id_sa,
                name,
                value,
            });
            Ok(JsValue::undefined())
        })
    };

    // appendChild — update DomSnapshot parent/child relationships
    let node_id_ac = node.id;
    let dom_snap_ac = dom_snapshot_arc.clone();
    let mutations_ac = mutations.clone();
    let append_child_obj_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let child = args.first().cloned().unwrap_or(JsValue::undefined());
            let child_id = child
                .as_object()
                .and_then(|o| o.get(js_string!("__nodeId"), ctx).ok())
                .and_then(|v| v.as_number().map(|n| n as u32));

            if let Some(cid) = child_id {
                {
                    let mut dom = dom_snap_ac.write();
                    if let Some(ref mut snap) = *dom {
                        if let Some(parent) = snap.nodes.get_mut(&node_id_ac)
                            && !parent.children.contains(&cid)
                        {
                            parent.children.push(cid);
                        }
                        if let Some(child_node) = snap.nodes.get_mut(&cid) {
                            child_node.parent = Some(node_id_ac);
                        }
                    }
                }
                mutations_ac.write().push(DomMutation::AppendChild {
                    parent_id: node_id_ac,
                    child_id: cid,
                });
                // Notify MutationObservers
                notify_mutation_observers(ctx, "childList", node_id_ac);
            }
            Ok(child)
        })
    };

    // removeChild — remove child from parent
    let node_id_rc = node.id;
    let dom_snap_rc = dom_snapshot_arc.clone();
    let mutations_rc = mutations.clone();
    let remove_child_obj_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let child = args.first().cloned().unwrap_or(JsValue::undefined());
            let child_id = child
                .as_object()
                .and_then(|o| o.get(js_string!("__nodeId"), ctx).ok())
                .and_then(|v| v.as_number().map(|n| n as u32));

            if let Some(cid) = child_id {
                {
                    let mut dom = dom_snap_rc.write();
                    if let Some(ref mut snap) = *dom {
                        if let Some(parent) = snap.nodes.get_mut(&node_id_rc) {
                            parent.children.retain(|&id| id != cid);
                        }
                        if let Some(child_node) = snap.nodes.get_mut(&cid) {
                            child_node.parent = None;
                        }
                    }
                }
                mutations_rc.write().push(DomMutation::RemoveChild {
                    parent_id: node_id_rc,
                    child_id: cid,
                });
                // Notify MutationObservers
                notify_mutation_observers(ctx, "childList", node_id_rc);
            }
            Ok(child)
        })
    };

    // element.querySelector(selector)
    let qs_dom = dom_snapshot_arc.clone();
    let qs_mutations = mutations.clone();
    let qs_root_id = node.id;
    let element_qs_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let selector = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();

            let dom = qs_dom.read();
            if let Some(ref snapshot) = *dom
                && let Some(match_id) = snapshot.query_selector_from(qs_root_id, &selector)
                && let Some(match_node) = snapshot.nodes.get(&match_id)
            {
                return Ok(create_element_object(
                    snapshot,
                    match_node,
                    ctx,
                    &qs_mutations,
                    &qs_dom,
                ));
            }
            Ok(JsValue::null())
        })
    };

    // element.querySelectorAll(selector)
    let qsa_dom = dom_snapshot_arc.clone();
    let qsa_mutations = mutations.clone();
    let qsa_root_id = node.id;
    let element_qsa_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let selector = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();

            let dom = qsa_dom.read();
            if let Some(ref snapshot) = *dom {
                let ids = snapshot.query_selector_all_from(qsa_root_id, &selector);
                let js_values: Vec<JsValue> = ids
                    .iter()
                    .filter_map(|&id| {
                        snapshot.nodes.get(&id).map(|n| {
                            create_element_object(snapshot, n, ctx, &qsa_mutations, &qsa_dom)
                        })
                    })
                    .collect();
                let arr = JsArray::from_iter(js_values, ctx);
                return Ok(arr.into());
            }
            let arr = JsArray::from_iter(Vec::<JsValue>::new(), ctx);
            Ok(arr.into())
        })
    };
    // element.matches(selector)
    let m_dom = dom_snapshot_arc.clone();
    let m_root_id = node.id;
    let element_matches_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            let selector = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            let dom = m_dom.read();
            if let Some(ref snapshot) = *dom {
                return Ok(JsValue::from(
                    snapshot.element_matches(m_root_id, &selector),
                ));
            }
            Ok(JsValue::from(false))
        })
    };

    // element.closest(selector)
    let c_dom = dom_snapshot_arc.clone();
    let c_mutations = mutations.clone();
    let c_root_id = node.id;
    let element_closest_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let selector = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            let dom = c_dom.read();
            if let Some(ref snapshot) = *dom
                && let Some(match_id) = snapshot.element_closest(c_root_id, &selector)
                && let Some(match_node) = snapshot.nodes.get(&match_id)
            {
                return Ok(create_element_object(
                    snapshot,
                    match_node,
                    ctx,
                    &c_mutations,
                    &c_dom,
                ));
            }
            Ok(JsValue::null())
        })
    };

    // ── 트리 탐색 접근자 (firstChild, lastChild, nextSibling, previousSibling) ──

    let snap_fc = dom_snapshot_arc.clone();
    let nid_fc = node.id;
    let mut_fc = mutations.clone();
    let first_child_getter = unsafe {
        NativeFunction::from_closure(move |_this, _args, ctx| {
            let dom = snap_fc.read();
            if let Some(ref s) = *dom
                && let Some(fid) = s.first_child(nid_fc)
                && let Some(c) = s.nodes.get(&fid)
            {
                return Ok(create_element_object(s, c, ctx, &mut_fc, &snap_fc));
            }
            Ok(JsValue::null())
        })
    };
    let first_child_getter_fn = FunctionObjectBuilder::new(ctx.realm(), first_child_getter)
        .name(js_string!("get firstChild"))
        .build();

    let snap_lc = dom_snapshot_arc.clone();
    let nid_lc = node.id;
    let mut_lc = mutations.clone();
    let last_child_getter = unsafe {
        NativeFunction::from_closure(move |_this, _args, ctx| {
            let dom = snap_lc.read();
            if let Some(ref s) = *dom
                && let Some(lid) = s.last_child(nid_lc)
                && let Some(c) = s.nodes.get(&lid)
            {
                return Ok(create_element_object(s, c, ctx, &mut_lc, &snap_lc));
            }
            Ok(JsValue::null())
        })
    };
    let last_child_getter_fn = FunctionObjectBuilder::new(ctx.realm(), last_child_getter)
        .name(js_string!("get lastChild"))
        .build();

    let snap_ns = dom_snapshot_arc.clone();
    let nid_ns = node.id;
    let mut_ns = mutations.clone();
    let next_sibling_getter = unsafe {
        NativeFunction::from_closure(move |_this, _args, ctx| {
            let dom = snap_ns.read();
            if let Some(ref s) = *dom
                && let Some(nid) = s.next_sibling(nid_ns)
                && let Some(c) = s.nodes.get(&nid)
            {
                return Ok(create_element_object(s, c, ctx, &mut_ns, &snap_ns));
            }
            Ok(JsValue::null())
        })
    };
    let next_sibling_getter_fn = FunctionObjectBuilder::new(ctx.realm(), next_sibling_getter)
        .name(js_string!("get nextSibling"))
        .build();

    let snap_ps = dom_snapshot_arc.clone();
    let nid_ps = node.id;
    let mut_ps = mutations.clone();
    let prev_sibling_getter = unsafe {
        NativeFunction::from_closure(move |_this, _args, ctx| {
            let dom = snap_ps.read();
            if let Some(ref s) = *dom
                && let Some(pid) = s.previous_sibling(nid_ps)
                && let Some(c) = s.nodes.get(&pid)
            {
                return Ok(create_element_object(s, c, ctx, &mut_ps, &snap_ps));
            }
            Ok(JsValue::null())
        })
    };
    let prev_sibling_getter_fn = FunctionObjectBuilder::new(ctx.realm(), prev_sibling_getter)
        .name(js_string!("get previousSibling"))
        .build();

    // ── 트리 조작 메서드 (insertBefore, replaceChild, removeAttribute, cloneNode, remove) ──

    let snap_ib = dom_snapshot_arc.clone();
    let nid_ib = node.id;
    let mut_ib = mutations.clone();
    let insert_before_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let new_child = args.first().cloned().unwrap_or(JsValue::undefined());
            let ref_child = args.get(1).cloned().unwrap_or(JsValue::null());
            let new_id = new_child
                .as_object()
                .and_then(|o| o.get(js_string!("__nodeId"), ctx).ok())
                .and_then(|v| v.as_number().map(|n| n as u32));
            let ref_id = if ref_child.is_null() || ref_child.is_undefined() {
                None
            } else {
                ref_child
                    .as_object()
                    .and_then(|o| o.get(js_string!("__nodeId"), ctx).ok())
                    .and_then(|v| v.as_number().map(|n| n as u32))
            };
            if let Some(nid) = new_id {
                let mut dom = snap_ib.write();
                if let Some(ref mut s) = *dom {
                    // 기존 부모에서 제거
                    if let Some(old_parent) = s.nodes.get(&nid).and_then(|n| n.parent)
                        && old_parent != nid_ib
                        && let Some(p) = s.nodes.get_mut(&old_parent)
                    {
                        p.children.retain(|&c| c != nid);
                    }
                    // ref_id 위치에 삽입 또는 맨 뒤에 append
                    let children = s
                        .nodes
                        .get(&nid_ib)
                        .map(|p| p.children.clone())
                        .unwrap_or_default();
                    if let Some(rid) = ref_id {
                        if let Some(pos) = children.iter().position(|&c| c == rid)
                            && let Some(p) = s.nodes.get_mut(&nid_ib)
                        {
                            p.children.retain(|&c| c != nid);
                            p.children.insert(pos, nid);
                        }
                    } else {
                        if let Some(p) = s.nodes.get_mut(&nid_ib) {
                            p.children.retain(|&c| c != nid);
                            p.children.push(nid);
                        }
                    }
                    if let Some(c) = s.nodes.get_mut(&nid) {
                        c.parent = Some(nid_ib);
                    }
                    mut_ib.write().push(DomMutation::AppendChild {
                        parent_id: nid_ib,
                        child_id: nid,
                    });
                }
            }
            Ok(new_child)
        })
    };

    let snap_rc = dom_snapshot_arc.clone();
    let nid_rc = node.id;
    let mut_rc = mutations.clone();
    let replace_child_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let new_child = args.first().cloned().unwrap_or(JsValue::undefined());
            let old_child = args.get(1).cloned().unwrap_or(JsValue::undefined());
            let new_id = new_child
                .as_object()
                .and_then(|o| o.get(js_string!("__nodeId"), ctx).ok())
                .and_then(|v| v.as_number().map(|n| n as u32));
            let old_id = old_child
                .as_object()
                .and_then(|o| o.get(js_string!("__nodeId"), ctx).ok())
                .and_then(|v| v.as_number().map(|n| n as u32));
            if let (Some(nid), Some(oid)) = (new_id, old_id) {
                let mut dom = snap_rc.write();
                if let Some(ref mut s) = *dom {
                    if let Some(p) = s.nodes.get_mut(&nid_rc) {
                        p.children.retain(|&c| c != oid);
                        if let Some(pos) = p.children.iter().position(|&c| c == oid) {
                            p.children.insert(pos, nid);
                        } else {
                            p.children.push(nid);
                        }
                    }
                    if let Some(c) = s.nodes.get_mut(&nid) {
                        c.parent = Some(nid_rc);
                    }
                    if let Some(o) = s.nodes.get_mut(&oid) {
                        o.parent = None;
                    }
                    mut_rc.write().push(DomMutation::RemoveChild {
                        parent_id: nid_rc,
                        child_id: oid,
                    });
                    mut_rc.write().push(DomMutation::AppendChild {
                        parent_id: nid_rc,
                        child_id: nid,
                    });
                }
            }
            Ok(new_child)
        })
    };

    let snap_ra = dom_snapshot_arc.clone();
    let nid_ra = node.id;
    let mut_ra = mutations.clone();
    let remove_attr_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            let name = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            if !name.is_empty() {
                let mut dom = snap_ra.write();
                if let Some(ref mut s) = *dom
                    && let Some(n) = s.nodes.get_mut(&nid_ra)
                {
                    n.attributes.remove(&name);
                }
                mut_ra.write().push(DomMutation::SetAttribute {
                    node_id: nid_ra,
                    name,
                    value: String::new(),
                });
            }
            Ok(JsValue::undefined())
        })
    };

    let snap_rm = dom_snapshot_arc.clone();
    let nid_rm = node.id;
    let mut_rm = mutations.clone();
    let remove_fn = unsafe {
        NativeFunction::from_closure(move |_this, _args, _ctx| {
            let mut dom = snap_rm.write();
            if let Some(ref mut s) = *dom {
                let pid = s.nodes.get(&nid_rm).and_then(|n| n.parent);
                if let Some(pid2) = pid {
                    if let Some(p) = s.nodes.get_mut(&pid2) {
                        p.children.retain(|&c| c != nid_rm);
                    }
                    mut_rm.write().push(DomMutation::RemoveChild {
                        parent_id: pid2,
                        child_id: nid_rm,
                    });
                }
                if let Some(n) = s.nodes.get_mut(&nid_rm) {
                    n.parent = None;
                }
            }
            Ok(JsValue::undefined())
        })
    };

    let snap_cl = dom_snapshot_arc.clone();
    let nid_cl = node.id;
    let mut_cl = mutations.clone();
    let clone_node_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let deep = args.first().and_then(|v| v.as_boolean()).unwrap_or(false);
            let new_id = NEXT_NODE_ID.fetch_add(1, Ordering::Relaxed) as u32;
            // 먼저 필요한 값들을 clone (borrow 해제 후 insert)
            let (tag, attrs, text, ntype) = {
                let dom = snap_cl.read();
                if let Some(ref s) = *dom {
                    if let Some(src) = s.nodes.get(&nid_cl) {
                        (
                            src.tag.clone(),
                            if deep {
                                src.attributes.clone()
                            } else {
                                HashMap::new()
                            },
                            if deep {
                                src.text_content.clone()
                            } else {
                                String::new()
                            },
                            src.node_type,
                        )
                    } else {
                        return Ok(JsValue::null());
                    }
                } else {
                    return Ok(JsValue::null());
                }
            };
            let cloned = DomNode {
                id: new_id,
                tag: tag.clone(),
                attributes: attrs,
                text_content: text,
                children: Vec::new(),
                parent: None,
                node_type: ntype,
            };
            {
                let mut dom = snap_cl.write();
                if let Some(ref mut s) = *dom {
                    s.nodes.insert(new_id, cloned);
                }
            }
            mut_cl.write().push(DomMutation::CreateElement {
                node_id: new_id,
                tag: tag.clone(),
            });
            let dom = snap_cl.read();
            if let Some(ref s) = *dom
                && let Some(n) = s.nodes.get(&new_id)
            {
                return Ok(create_element_object(s, n, ctx, &mut_cl, &snap_cl));
            }
            Ok(JsValue::null())
        })
    };

    // ── 스타일/클래스 접근자 (style, classList) ──
    // .function()으로 등록 — 호출 시 객체 반환

    let snap_st = dom_snapshot_arc.clone();
    let nid_st = node.id;
    let style_fn = unsafe {
        NativeFunction::from_closure(move |_this, _args, ctx| {
            let sp_arc = snap_st.clone();
            let sp_id = nid_st;
            let set_fn = {
                NativeFunction::from_closure(move |_this2, args2, _ctx2| {
                    let prop = args2
                        .first()
                        .and_then(|v| v.as_string())
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    let val = args2
                        .get(1)
                        .and_then(|v| v.as_string())
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    if !prop.is_empty() {
                        let mut dom = sp_arc.write();
                        if let Some(ref mut s) = *dom
                            && let Some(n) = s.nodes.get_mut(&sp_id)
                        {
                            n.attributes.insert(format!("style:{}", prop), val);
                        }
                    }
                    Ok(JsValue::undefined())
                })
            };
            let gp_arc = snap_st.clone();
            let gp_id = nid_st;
            let get_fn = {
                NativeFunction::from_closure(move |_this2, args2, _ctx2| {
                    let prop = args2
                        .first()
                        .and_then(|v| v.as_string())
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    let dom = gp_arc.read();
                    if let Some(ref s) = *dom
                        && let Some(n) = s.nodes.get(&gp_id)
                    {
                        let key = format!("style:{}", prop);
                        if let Some(v) = n.attributes.get(&key) {
                            return Ok(JsValue::from(JsString::from(v.as_str())));
                        }
                    }
                    Ok(JsValue::from(JsString::from("")))
                })
            };
            let rp_arc = snap_st.clone();
            let rp_id = nid_st;
            let rm_fn = {
                NativeFunction::from_closure(move |_this2, args2, _ctx2| {
                    let prop = args2
                        .first()
                        .and_then(|v| v.as_string())
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    if !prop.is_empty() {
                        let mut dom = rp_arc.write();
                        if let Some(ref mut s) = *dom
                            && let Some(n) = s.nodes.get_mut(&rp_id)
                        {
                            let key = format!("style:{}", prop);
                            n.attributes.remove(&key);
                        }
                    }
                    Ok(JsValue::undefined())
                })
            };
            let style_obj = boa_engine::object::ObjectInitializer::new(ctx)
                .function(set_fn, js_string!("setProperty"), 2)
                .function(get_fn, js_string!("getPropertyValue"), 1)
                .function(rm_fn, js_string!("removeProperty"), 1)
                .build();
            Ok(JsValue::from(style_obj))
        })
    };

    let snap_cls = dom_snapshot_arc.clone();
    let nid_cls = node.id;
    let classlist_fn = unsafe {
        NativeFunction::from_closure(move |_this, _args, ctx| {
            // 현재 class 속성 읽기
            let current = {
                let dom = snap_cls.read();
                if let Some(ref s) = *dom {
                    if let Some(n) = s.nodes.get(&nid_cls) {
                        n.attributes.get("class").cloned().unwrap_or_default()
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                }
            };
            let count = current.split_whitespace().count() as i32;

            let ca_arc = snap_cls.clone();
            let ca_id = nid_cls;
            let add_fn = {
                NativeFunction::from_closure(move |_this2, args2, _ctx2| {
                    let cls = args2
                        .first()
                        .and_then(|v| v.as_string())
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    if !cls.is_empty() {
                        let mut dom = ca_arc.write();
                        if let Some(ref mut s) = *dom
                            && let Some(n) = s.nodes.get_mut(&ca_id)
                        {
                            let cur = n.attributes.get("class").cloned().unwrap_or_default();
                            if !cur.split_whitespace().any(|c| c == cls) {
                                let new_cls = if cur.is_empty() {
                                    cls.clone()
                                } else {
                                    format!("{} {}", cur, cls)
                                };
                                n.attributes.insert("class".to_string(), new_cls);
                            }
                        }
                    }
                    Ok(JsValue::undefined())
                })
            };

            let cr_arc = snap_cls.clone();
            let cr_id = nid_cls;
            let rm_cls_fn = {
                NativeFunction::from_closure(move |_this2, args2, _ctx2| {
                    let cls = args2
                        .first()
                        .and_then(|v| v.as_string())
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    if !cls.is_empty() {
                        let mut dom = cr_arc.write();
                        if let Some(ref mut s) = *dom
                            && let Some(n) = s.nodes.get_mut(&cr_id)
                        {
                            let cur = n.attributes.get("class").cloned().unwrap_or_default();
                            let new_cls = cur
                                .split_whitespace()
                                .filter(|c| *c != cls)
                                .collect::<Vec<_>>()
                                .join(" ");
                            n.attributes.insert("class".to_string(), new_cls);
                        }
                    }
                    Ok(JsValue::undefined())
                })
            };

            let ch_arc = snap_cls.clone();
            let ch_id = nid_cls;
            let has_fn = {
                NativeFunction::from_closure(move |_this2, args2, _ctx2| {
                    let cls = args2
                        .first()
                        .and_then(|v| v.as_string())
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    let dom = ch_arc.read();
                    if let Some(ref s) = *dom
                        && let Some(n) = s.nodes.get(&ch_id)
                    {
                        let cur = n.attributes.get("class").cloned().unwrap_or_default();
                        return Ok(JsValue::from(cur.split_whitespace().any(|c| c == cls)));
                    }
                    Ok(JsValue::from(false))
                })
            };

            let ct_arc = snap_cls.clone();
            let ct_id = nid_cls;
            let toggle_fn = {
                NativeFunction::from_closure(move |_this2, args2, _ctx2| {
                    let cls = args2
                        .first()
                        .and_then(|v| v.as_string())
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    let dom = ct_arc.read();
                    let mut found = false;
                    if let Some(ref s) = *dom
                        && let Some(n) = s.nodes.get(&ct_id)
                    {
                        let cur = n.attributes.get("class").cloned().unwrap_or_default();
                        found = cur.split_whitespace().any(|c| c == cls);
                    }
                    drop(dom);
                    if !cls.is_empty() {
                        let mut dom2 = ct_arc.write();
                        if let Some(ref mut s) = *dom2
                            && let Some(n) = s.nodes.get_mut(&ct_id)
                        {
                            let cur = n.attributes.get("class").cloned().unwrap_or_default();
                            let new_cls = if found {
                                cur.split_whitespace()
                                    .filter(|c| *c != cls)
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            } else {
                                if cur.is_empty() {
                                    cls.clone()
                                } else {
                                    format!("{} {}", cur, cls)
                                }
                            };
                            n.attributes.insert("class".to_string(), new_cls);
                        }
                    }
                    Ok(JsValue::from(!found))
                })
            };

            let cl_obj = boa_engine::object::ObjectInitializer::new(ctx)
                .property(js_string!("length"), JsValue::from(count), Attribute::all())
                .function(add_fn, js_string!("add"), 1)
                .function(rm_cls_fn, js_string!("remove"), 1)
                .function(has_fn, js_string!("contains"), 1)
                .function(toggle_fn, js_string!("toggle"), 1)
                .build();
            Ok(JsValue::from(cl_obj))
        })
    };

    // style/classList를 accessor로 사용하기 위해 FunctionObjectBuilder로 변환
    // (ObjectInitializer::new(ctx)가 ctx를 mutable borrow하므로 미리 변환 필요)
    let style_getter_fn = FunctionObjectBuilder::new(ctx.realm(), style_fn)
        .name(js_string!("get style"))
        .build();
    let classlist_getter_fn = FunctionObjectBuilder::new(ctx.realm(), classlist_fn)
        .name(js_string!("get classList"))
        .build();

    // ── getBoundingClientRect ──
    let gbr_dom = dom_snapshot_arc.clone();
    let gbr_id = node.id;
    let get_bounding_client_rect_fn = unsafe {
        NativeFunction::from_closure(move |_this, _args, _ctx| {
            let dom = gbr_dom.read();
            let snapshot = match dom.as_ref() {
                Some(s) => s,
                None => return Ok(JsValue::null()),
            };
            let rect = LayoutEngine::compute_rect(snapshot, gbr_id);
            let obj = boa_engine::object::ObjectInitializer::new(_ctx)
                .property(js_string!("x"), JsValue::from(rect.x), Attribute::all())
                .property(js_string!("y"), JsValue::from(rect.y), Attribute::all())
                .property(
                    js_string!("width"),
                    JsValue::from(rect.width),
                    Attribute::all(),
                )
                .property(
                    js_string!("height"),
                    JsValue::from(rect.height),
                    Attribute::all(),
                )
                .property(js_string!("top"), JsValue::from(rect.top), Attribute::all())
                .property(
                    js_string!("right"),
                    JsValue::from(rect.right),
                    Attribute::all(),
                )
                .property(
                    js_string!("bottom"),
                    JsValue::from(rect.bottom),
                    Attribute::all(),
                )
                .property(
                    js_string!("left"),
                    JsValue::from(rect.left),
                    Attribute::all(),
                )
                .build();
            Ok(JsValue::from(obj))
        })
    };

    // ── offsetWidth / offsetHeight ──
    let ow_dom = dom_snapshot_arc.clone();
    let ow_id = node.id;
    let offset_width_getter_raw = unsafe {
        NativeFunction::from_closure(move |_this, _args, _ctx| {
            let dom = ow_dom.read();
            if let Some(ref snap) = *dom {
                let rect = LayoutEngine::compute_rect(snap, ow_id);
                return Ok(JsValue::from(rect.width));
            }
            Ok(JsValue::from(0.0))
        })
    };
    let offset_width_getter = FunctionObjectBuilder::new(ctx.realm(), offset_width_getter_raw)
        .name(js_string!("get offsetWidth"))
        .build();
    let oh_dom = dom_snapshot_arc.clone();
    let oh_id = node.id;
    let offset_height_getter_raw = unsafe {
        NativeFunction::from_closure(move |_this, _args, _ctx| {
            let dom = oh_dom.read();
            if let Some(ref snap) = *dom {
                let rect = LayoutEngine::compute_rect(snap, oh_id);
                return Ok(JsValue::from(rect.height));
            }
            Ok(JsValue::from(0.0))
        })
    };
    let offset_height_getter = FunctionObjectBuilder::new(ctx.realm(), offset_height_getter_raw)
        .name(js_string!("get offsetHeight"))
        .build();

    // ── 포커스/폼 (noop) ──

    let focus_fn =
        unsafe { NativeFunction::from_closure(move |_this, _args, _ctx| Ok(JsValue::undefined())) };
    let blur_fn =
        unsafe { NativeFunction::from_closure(move |_this, _args, _ctx| Ok(JsValue::undefined())) };
    let submit_fn =
        unsafe { NativeFunction::from_closure(move |_this, _args, _ctx| Ok(JsValue::undefined())) };

    // value getter
    // value getter — reads from live snapshot (reflects value setter)
    let dom_snap_vg = dom_snapshot_arc.clone();
    let node_id_vg = node.id;
    let value_getter = unsafe {
        NativeFunction::from_closure(move |_this, _args, _ctx| {
            let dom = dom_snap_vg.read();
            if let Some(ref snap) = *dom
                && let Some(n) = snap.nodes.get(&node_id_vg)
            {
                return Ok(JsValue::from(JsString::from(
                    n.attributes.get("value").map(|s| s.as_str()).unwrap_or(""),
                )));
            }
            Ok(JsValue::from(JsString::from("")))
        })
    };
    let value_getter_fn = FunctionObjectBuilder::new(ctx.realm(), value_getter)
        .name(js_string!("get value"))
        .build();

    // value setter → updates snapshot + records DomMutation::InputElement
    let node_id_vs = node.id;
    let mutations_vs = mutations.clone();
    let dom_snap_vs = dom_snapshot_arc.clone();
    let value_setter = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            let val = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            // snapshot에 value attribute 업데이트 (getter가 즉시 반영)
            {
                let mut dom = dom_snap_vs.write();
                if let Some(ref mut snap) = *dom
                    && let Some(n) = snap.nodes.get_mut(&node_id_vs)
                {
                    n.attributes.insert("value".to_string(), val.clone());
                }
            }
            mutations_vs.write().push(DomMutation::InputElement {
                node_id: node_id_vs,
                value: val,
            });
            Ok(JsValue::undefined())
        })
    };
    let value_setter_fn = FunctionObjectBuilder::new(ctx.realm(), value_setter)
        .name(js_string!("set value"))
        .build();

    // textContent getter — reads from live snapshot
    let dom_snap_tcg = dom_snapshot_arc.clone();
    let nid_tcg = node.id;
    let text_content_getter = unsafe {
        NativeFunction::from_closure(move |_this, _args, _ctx| {
            let dom = dom_snap_tcg.read();
            if let Some(ref s) = *dom
                && let Some(n) = s.nodes.get(&nid_tcg)
            {
                return Ok(JsValue::from(JsString::from(n.text_content.as_str())));
            }
            Ok(JsValue::from(JsString::from("")))
        })
    };
    let text_content_getter_fn = FunctionObjectBuilder::new(ctx.realm(), text_content_getter)
        .name(js_string!("get textContent"))
        .build();

    // textContent setter — updates snapshot + records mutation
    let dom_snap_tcs = dom_snapshot_arc.clone();
    let nid_tcs = node.id;
    let mut_tcs = mutations.clone();
    let text_content_setter = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            let text = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            {
                let mut dom = dom_snap_tcs.write();
                if let Some(ref mut s) = *dom
                    && let Some(n) = s.nodes.get_mut(&nid_tcs)
                {
                    n.text_content = text.clone();
                }
            }
            mut_tcs.write().push(DomMutation::SetTextContent {
                node_id: nid_tcs,
                text,
            });
            Ok(JsValue::undefined())
        })
    };
    let text_content_setter_fn = FunctionObjectBuilder::new(ctx.realm(), text_content_setter)
        .name(js_string!("set textContent"))
        .build();

    // innerHTML getter — serializes the node's children from the live snapshot.
    let dom_snap_ihg = dom_snapshot_arc.clone();
    let nid_ihg = node.id;
    let inner_html_getter = unsafe {
        NativeFunction::from_closure(move |_this, _args, _ctx| {
            let dom = dom_snap_ihg.read();
            let mut buf = String::new();
            if let Some(s) = &*dom
                && let Some(n) = s.nodes.get(&nid_ihg)
            {
                crate::js::dom_serializer::serialize_children(n, s, &mut buf);
                return Ok(JsValue::from(JsString::from(buf.as_str())));
            }
            Ok(JsValue::from(JsString::from("")))
        })
    };
    let inner_html_getter_fn = FunctionObjectBuilder::new(ctx.realm(), inner_html_getter)
        .name(js_string!("get innerHTML"))
        .build();

    // innerHTML setter — updates snapshot + records mutation
    let dom_snap_ihs = dom_snapshot_arc.clone();
    let nid_ihs = node.id;
    let mut_ihs = mutations.clone();
    let inner_html_setter = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            let html = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            {
                let mut dom = dom_snap_ihs.write();
                if let Some(s) = &mut *dom {
                    s.set_inner_html(nid_ihs, &html);
                    s.rebuild_indices();
                }
            }
            mut_ihs.write().push(DomMutation::SetInnerHtml {
                node_id: nid_ihs,
                html,
            });
            Ok(JsValue::undefined())
        })
    };
    let inner_html_setter_fn = FunctionObjectBuilder::new(ctx.realm(), inner_html_setter)
        .name(js_string!("set innerHTML"))
        .build();

    // outerHTML getter — serializes the node itself (tag + attrs + children).
    // Read-only; matches browser semantics (no setter).
    let dom_snap_ohg = dom_snapshot_arc.clone();
    let nid_ohg = node.id;
    let outer_html_getter = unsafe {
        NativeFunction::from_closure(move |_this, _args, _ctx| {
            let dom = dom_snap_ohg.read();
            let mut buf = String::new();
            if let Some(s) = &*dom
                && let Some(n) = s.nodes.get(&nid_ohg)
            {
                crate::js::dom_serializer::serialize_node(n, s, &mut buf);
                return Ok(JsValue::from(JsString::from(buf.as_str())));
            }
            Ok(JsValue::from(JsString::from("")))
        })
    };
    let outer_html_getter_fn = FunctionObjectBuilder::new(ctx.realm(), outer_html_getter)
        .name(js_string!("get outerHTML"))
        .build();

    // children → [element children IDs as lightweight objects]
    // Note: we avoid recursively calling create_element_object for children/parentNode
    // to prevent stack overflow on deeply nested DOMs. Instead, children get a
    // minimal stub with tagName and id.
    let child_ids = node
        .children
        .iter()
        .filter(|&&c| {
            snapshot
                .nodes
                .get(&c)
                .map(|n| n.node_type == 1)
                .unwrap_or(false)
        })
        .copied()
        .collect::<Vec<u32>>();
    let children_js: Vec<JsValue> = child_ids
        .iter()
        .filter_map(|&cid| {
            snapshot.nodes.get(&cid).map(|child| {
                let child_obj = boa_engine::object::ObjectInitializer::new(ctx)
                    .property(
                        js_string!("tagName"),
                        JsValue::from(JsString::from(child.tag.to_uppercase().as_str())),
                        Attribute::all(),
                    )
                    .property(
                        js_string!("id"),
                        JsValue::from(JsString::from(
                            child.attributes.get("id").map(|s| s.as_str()).unwrap_or(""),
                        )),
                        Attribute::all(),
                    )
                    .build();
                child_obj.into()
            })
        })
        .collect();
    let children_arr = JsArray::from_iter(children_js, ctx);

    // parentNode — stub (avoid recursion)
    let parent_val: JsValue = match node.parent {
        Some(pid) => match snapshot.nodes.get(&pid) {
            Some(pnode) if pnode.node_type == 1 => {
                let parent_obj = boa_engine::object::ObjectInitializer::new(ctx)
                    .property(
                        js_string!("tagName"),
                        JsValue::from(JsString::from(pnode.tag.to_uppercase().as_str())),
                        Attribute::all(),
                    )
                    .property(
                        js_string!("id"),
                        JsValue::from(JsString::from(
                            pnode.attributes.get("id").map(|s| s.as_str()).unwrap_or(""),
                        )),
                        Attribute::all(),
                    )
                    .build();
                parent_obj.into()
            }
            _ => JsValue::null(),
        },
        None => JsValue::null(),
    };

    // id — accessor that reads/writes from live DomSnapshot
    let snap_id = dom_snapshot_arc.clone();
    let nid_id = node.id;
    let mut_id = mutations.clone();
    let id_getter_fn = {
        let snap = snap_id.clone();
        let nid = nid_id;
        let getter = unsafe {
            NativeFunction::from_closure(move |_this, _args, _ctx| {
                let dom = snap.read();
                if let Some(ref s) = *dom
                    && let Some(n) = s.nodes.get(&nid)
                {
                    return Ok(JsValue::from(JsString::from(
                        n.attributes.get("id").map(|s| s.as_str()).unwrap_or(""),
                    )));
                }
                Ok(JsValue::from(JsString::from("")))
            })
        };
        FunctionObjectBuilder::new(ctx.realm(), getter)
            .name(js_string!("get id"))
            .build()
    };
    let id_setter_fn = {
        let snap = snap_id.clone();
        let nid = nid_id;
        let m = mut_id.clone();
        let setter = unsafe {
            NativeFunction::from_closure(move |_this, args, _ctx| {
                let value = args
                    .first()
                    .and_then(|v| v.as_string())
                    .map(|s| s.to_std_string_escaped())
                    .unwrap_or_default();
                {
                    let mut dom = snap.write();
                    if let Some(ref mut s) = *dom
                        && let Some(n) = s.nodes.get_mut(&nid)
                    {
                        n.attributes.insert("id".to_string(), value.clone());
                    }
                }
                m.write().push(DomMutation::SetAttribute {
                    node_id: nid,
                    name: "id".to_string(),
                    value,
                });
                Ok(JsValue::undefined())
            })
        };
        FunctionObjectBuilder::new(ctx.realm(), setter)
            .name(js_string!("set id"))
            .build()
    };

    // className — accessor
    let snap_cn = dom_snapshot_arc.clone();
    let nid_cn = node.id;
    let mut_cn = mutations.clone();
    let class_getter_fn = {
        let snap = snap_cn.clone();
        let nid = nid_cn;
        let getter = unsafe {
            NativeFunction::from_closure(move |_this, _args, _ctx| {
                let dom = snap.read();
                if let Some(ref s) = *dom
                    && let Some(n) = s.nodes.get(&nid)
                {
                    return Ok(JsValue::from(JsString::from(
                        n.attributes.get("class").map(|s| s.as_str()).unwrap_or(""),
                    )));
                }
                Ok(JsValue::from(JsString::from("")))
            })
        };
        FunctionObjectBuilder::new(ctx.realm(), getter)
            .name(js_string!("get className"))
            .build()
    };
    let class_setter_fn = {
        let snap = snap_cn.clone();
        let nid = nid_cn;
        let m = mut_cn.clone();
        let setter = unsafe {
            NativeFunction::from_closure(move |_this, args, _ctx| {
                let value = args
                    .first()
                    .and_then(|v| v.as_string())
                    .map(|s| s.to_std_string_escaped())
                    .unwrap_or_default();
                {
                    let mut dom = snap.write();
                    if let Some(ref mut s) = *dom
                        && let Some(n) = s.nodes.get_mut(&nid)
                    {
                        n.attributes.insert("class".to_string(), value.clone());
                    }
                }
                m.write().push(DomMutation::SetAttribute {
                    node_id: nid,
                    name: "class".to_string(),
                    value,
                });
                Ok(JsValue::undefined())
            })
        };
        FunctionObjectBuilder::new(ctx.realm(), setter)
            .name(js_string!("set className"))
            .build()
    };

    let obj = boa_engine::object::ObjectInitializer::new(ctx)
        .property(
            js_string!("tagName"),
            JsValue::from(JsString::from(tag_upper.as_str())),
            Attribute::all(),
        )
        .accessor(
            js_string!("textContent"),
            Some(text_content_getter_fn.clone()),
            Some(text_content_setter_fn),
            Attribute::all(),
        )
        .accessor(
            js_string!("innerText"),
            Some(text_content_getter_fn),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("innerHTML"),
            Some(inner_html_getter_fn),
            Some(inner_html_setter_fn),
            Attribute::all(),
        )
        .accessor(
            js_string!("outerHTML"),
            Some(outer_html_getter_fn),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("id"),
            Some(id_getter_fn),
            Some(id_setter_fn),
            Attribute::all(),
        )
        .accessor(
            js_string!("className"),
            Some(class_getter_fn),
            Some(class_setter_fn),
            Attribute::all(),
        )
        .property(
            js_string!("href"),
            JsValue::from(JsString::from(href_val)),
            Attribute::all(),
        )
        .property(
            js_string!("src"),
            JsValue::from(JsString::from(src_val)),
            Attribute::all(),
        )
        .property(
            js_string!("children"),
            JsValue::from(children_arr),
            Attribute::all(),
        )
        .property(js_string!("parentNode"), parent_val, Attribute::all())
        .function(get_attribute_fn, js_string!("getAttribute"), 1)
        .function(has_attribute_fn, js_string!("hasAttribute"), 1)
        .function(add_event_listener_fn, js_string!("addEventListener"), 2)
        .function(
            remove_event_listener_fn,
            js_string!("removeEventListener"),
            2,
        )
        .function(dispatch_event_fn, js_string!("dispatchEvent"), 1)
        .function(click_fn, js_string!("click"), 0)
        .function(set_attribute_fn, js_string!("setAttribute"), 2)
        .function(append_child_obj_fn, js_string!("appendChild"), 1)
        .function(remove_child_obj_fn, js_string!("removeChild"), 1)
        .function(element_qs_fn, js_string!("querySelector"), 1)
        .function(element_qsa_fn, js_string!("querySelectorAll"), 1)
        .function(element_matches_fn, js_string!("matches"), 1)
        .function(element_closest_fn, js_string!("closest"), 1)
        .function(
            {
                let snap_cn = dom_snapshot_arc.clone();
                let nid_cn = node.id;
                let mut_cn = mutations.clone();
                unsafe {
                    NativeFunction::from_closure(move |_this, _args, ctx| {
                        let dom = snap_cn.read();
                        if let Some(ref snap) = *dom
                            && let Some(cur) = snap.nodes.get(&nid_cn)
                        {
                            let items: Vec<JsValue> = cur
                                .children
                                .iter()
                                .filter_map(|&cid| snap.nodes.get(&cid))
                                .map(|child| {
                                    create_element_object(snap, child, ctx, &mut_cn, &snap_cn)
                                })
                                .collect();
                            let arr = JsArray::from_iter(items, ctx);
                            return Ok(arr.into());
                        }
                        let arr = JsArray::new(ctx);
                        Ok(arr.into())
                    })
                }
            },
            js_string!("childNodes"),
            0,
        )
        // ── 트리 탐색 접근자 ──
        .accessor(
            js_string!("firstChild"),
            Some(first_child_getter_fn),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("lastChild"),
            Some(last_child_getter_fn),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("nextSibling"),
            Some(next_sibling_getter_fn),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("previousSibling"),
            Some(prev_sibling_getter_fn),
            None,
            Attribute::all(),
        )
        // ── 트리 조작 메서드 ──
        .function(insert_before_fn, js_string!("insertBefore"), 2)
        .function(replace_child_fn, js_string!("replaceChild"), 2)
        .function(remove_attr_fn, js_string!("removeAttribute"), 1)
        .function(clone_node_fn, js_string!("cloneNode"), 1)
        .function(remove_fn, js_string!("remove"), 0)
        // ── 스타일/클래스 (함수 — 호출 시 객체 반환) ──
        // style/classList accessors — el.style (not el.style())
        .accessor(
            js_string!("style"),
            Some(style_getter_fn),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("classList"),
            Some(classlist_getter_fn),
            None,
            Attribute::all(),
        )
        // ── 레이아웃 평가 ──
        .function(
            get_bounding_client_rect_fn,
            js_string!("getBoundingClientRect"),
            0,
        )
        .accessor(
            js_string!("offsetWidth"),
            Some(offset_width_getter),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("offsetHeight"),
            Some(offset_height_getter),
            None,
            Attribute::all(),
        )
        // ── _visible / _interactive — methods that read live from DomSnapshot ──
        .function(
            {
                let vis_dom = dom_snapshot_arc.clone();
                let vis_id = node.id;
                unsafe {
                    NativeFunction::from_closure(move |_this, _args, _ctx| {
                        let dom = vis_dom.read();
                        if let Some(ref snap) = *dom
                            && let Some(cs) = LayoutEngine::compute_style(snap, vis_id)
                        {
                            return Ok(JsValue::from(cs.visible));
                        }
                        Ok(JsValue::from(true))
                    })
                }
            },
            js_string!("_visible"),
            0,
        )
        .function(
            {
                let int_dom = dom_snapshot_arc.clone();
                let int_id = node.id;
                unsafe {
                    NativeFunction::from_closure(move |_this, _args, _ctx| {
                        let dom = int_dom.read();
                        if let Some(ref snap) = *dom
                            && let Some(cs) = LayoutEngine::compute_style(snap, int_id)
                        {
                            return Ok(JsValue::from(cs.interactive));
                        }
                        Ok(JsValue::from(false))
                    })
                }
            },
            js_string!("_interactive"),
            0,
        )
        // ── 포커스/폼 ──
        .function(focus_fn, js_string!("focus"), 0)
        .function(blur_fn, js_string!("blur"), 0)
        .function(submit_fn, js_string!("submit"), 0)
        .property(
            js_string!("__nodeId"),
            JsValue::from(node.id),
            Attribute::all(),
        )
        .accessor(
            js_string!("value"),
            Some(value_getter_fn),
            Some(value_setter_fn),
            Attribute::all(),
        )
        .build();

    // ── _visible / _interactive — live computed visibility from DomSnapshot ──
    // Define after .build() to avoid borrow conflicts with ObjectInitializer::new(ctx)
    obj.into()
}

// ---------------------------------------------------------------------------
// JsValue ↔ serde_json::Value conversions
// ---------------------------------------------------------------------------

/// Convert a serde_json Value to a boa_engine JsValue.
fn json_to_js_value(value: &Value, context: &mut Context) -> JsValue {
    use std::ops::Deref;

    match value {
        Value::Null => JsValue::null(),
        Value::Bool(b) => JsValue::from(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                JsValue::from(i as f64)
            } else {
                JsValue::from(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => JsValue::from(JsString::from(s.as_str())),
        Value::Array(arr) => {
            let js_values: Vec<JsValue> =
                arr.iter().map(|v| json_to_js_value(v, context)).collect();
            let js_arr = JsArray::from_iter(js_values, context);
            js_arr.deref().clone().into()
        }
        Value::Object(map) => {
            let pairs: Vec<(String, JsValue)> = map
                .iter()
                .map(|(k, v)| (k.clone(), json_to_js_value(v, context)))
                .collect();
            let mut obj = boa_engine::object::ObjectInitializer::new(context);
            for (k, v) in pairs {
                obj.property(JsString::from(k.as_str()), v, Attribute::all());
            }
            obj.build().into()
        }
    }
}

/// Convert a boa_engine JsValue to serde_json::Value.
fn js_value_to_json(value: &JsValue, context: &mut Context) -> Value {
    match value {
        JsValue::Null | JsValue::Undefined => Value::Null,
        JsValue::Boolean(b) => Value::Bool(*b),
        JsValue::Integer(n) => Value::Number(serde_json::Number::from(*n)),
        JsValue::Rational(n) => serde_json::Number::from_f64(*n)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        JsValue::String(s) => Value::String(s.to_std_string_escaped()),
        JsValue::Symbol(_) => Value::String("[symbol]".to_string()),
        JsValue::BigInt(_) => {
            let s = value
                .to_string(context)
                .unwrap_or_else(|_| JsString::from("0n"));
            Value::String(s.to_std_string_escaped())
        }
        JsValue::Object(obj) => {
            if obj.is_array()
                && let Ok(arr) = JsArray::from_object(obj.clone())
            {
                let len = arr.length(context).unwrap_or(0) as usize;
                let mut vec = Vec::with_capacity(len);
                for i in 0..len {
                    match arr.at(i as i64, context) {
                        Ok(elem) => vec.push(js_value_to_json(&elem, context)),
                        Err(_) => vec.push(Value::Null),
                    }
                }
                return Value::Array(vec);
            }
            object_to_json_via_stringify(obj, context)
        }
    }
}

/// Convert a JS object to JSON via `JSON.stringify`.
fn object_to_json_via_stringify(obj: &boa_engine::JsObject, context: &mut Context) -> Value {
    let json_global = context
        .global_object()
        .get(js_string!("JSON"), context)
        .unwrap_or_else(|_| JsValue::undefined());

    if let Some(json_obj) = json_global.as_object()
        && let Ok(stringify_fn) = json_obj.get(js_string!("stringify"), context)
        && stringify_fn.is_callable()
        && let Some(obj_inner) = stringify_fn.as_object()
        && let Ok(result) = obj_inner.call(&JsValue::undefined(), &[obj.clone().into()], context)
        && let Some(s) = result.as_string()
    {
        let json_str = s.to_std_string_escaped();
        if let Ok(parsed) = serde_json::from_str::<Value>(&json_str) {
            return parsed;
        }
        return Value::String(json_str);
    }

    if let Ok(s) = JsValue::from(obj.clone()).to_string(context) {
        let s = s.to_std_string_escaped();
        if s != "[object Object]" {
            return Value::String(s);
        }
    }

    Value::Object(serde_json::Map::new())
}

// ---------------------------------------------------------------------------
// localStorage
// ---------------------------------------------------------------------------

/// Register the `localStorage` global object (Storage interface).
///
/// localStorage is a simple key-value store with synchronous getItem/setItem.
/// Changes are propagated back to the Session via read-only Arc (since JS thread
/// can't mutate Session directly).
fn register_local_storage(
    ctx: &mut Context,
    storage: std::collections::HashMap<String, String>,
    _dom_snapshot: &Arc<RwLock<Option<DomSnapshot>>>,
    local_storage_tx: Arc<RwLock<Option<std::sync::mpsc::Sender<LocalStorageMsg>>>>,
) {
    // Build a JS object with Storage interface methods
    // We store the HashMap in a RefCell so JS can mutate it.
    use std::cell::RefCell;
    let storage_arc = Arc::new(RefCell::new(storage));
    let _storage_for_methods = storage_arc.clone();

    // --- getItem ---
    let get_storage = storage_arc.clone();
    let get_item_fn = unsafe {
        NativeFunction::from_closure(
            move |_this: &JsValue, args: &[JsValue], ctx: &mut Context| {
                let key = args
                    .first()
                    .and_then(|v| v.to_string(ctx).ok())
                    .map(|s| s.to_std_string_escaped())
                    .unwrap_or_default();
                let val = get_storage.borrow().get(&key).cloned();
                match val {
                    Some(v) => Ok(JsValue::from(JsString::from(v.as_str()))),
                    None => Ok(JsValue::null()),
                }
            },
        )
    };

    // --- setItem ---
    let set_storage = storage_arc.clone();
    let set_ls_tx = local_storage_tx.clone();
    let set_item_fn = unsafe {
        NativeFunction::from_closure(
            move |_this: &JsValue, args: &[JsValue], ctx: &mut Context| {
                if args.len() >= 2 {
                    let key = args[0]
                        .to_string(ctx)
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    let val = args[1]
                        .to_string(ctx)
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    set_storage.borrow_mut().insert(key.clone(), val.clone());
                    // Sync to Session
                    let tx_opt = { set_ls_tx.read().as_ref().cloned() };
                    if let Some(tx) = tx_opt {
                        let _ = tx.send(LocalStorageMsg::SetItem(key, val));
                    }
                }
                Ok(JsValue::undefined())
            },
        )
    };

    // --- removeItem ---
    let rem_storage = storage_arc.clone();
    let rem_ls_tx = local_storage_tx.clone();
    let remove_item_fn = unsafe {
        NativeFunction::from_closure(
            move |_this: &JsValue, args: &[JsValue], ctx: &mut Context| {
                if let Some(key_arg) = args.first() {
                    let key = key_arg
                        .to_string(ctx)
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    rem_storage.borrow_mut().remove(&key);
                    // Sync to Session
                    let tx_opt = { rem_ls_tx.read().as_ref().cloned() };
                    if let Some(tx) = tx_opt {
                        let _ = tx.send(LocalStorageMsg::RemoveItem(key));
                    }
                }
                Ok(JsValue::undefined())
            },
        )
    };

    // --- clear ---
    let clear_storage = storage_arc.clone();
    let clear_ls_tx = local_storage_tx.clone();
    let clear_fn = unsafe {
        NativeFunction::from_closure(
            move |_this: &JsValue, _args: &[JsValue], _ctx: &mut Context| {
                clear_storage.borrow_mut().clear();
                // Sync to Session
                let tx_opt = { clear_ls_tx.read().as_ref().cloned() };
                if let Some(tx) = tx_opt {
                    let _ = tx.send(LocalStorageMsg::Clear);
                }
                Ok(JsValue::undefined())
            },
        )
    };

    // --- key ---
    let key_storage = storage_arc.clone();
    let key_fn = unsafe {
        NativeFunction::from_closure(
            move |_this: &JsValue, args: &[JsValue], ctx: &mut Context| {
                if let Some(idx_arg) = args.first() {
                    let idx = idx_arg.to_index(ctx).unwrap_or(0) as usize;
                    let keys: Vec<_> = key_storage.borrow().keys().cloned().collect();
                    match keys.get(idx) {
                        Some(k) => Ok(JsValue::from(JsString::from(k.as_str()))),
                        None => Ok(JsValue::null()),
                    }
                } else {
                    Ok(JsValue::null())
                }
            },
        )
    };

    // --- get length (snapshot) ---
    let len_storage = storage_arc.clone();
    let _len_fn = unsafe {
        NativeFunction::from_closure(
            move |_this: &JsValue, _args: &[JsValue], _ctx: &mut Context| {
                Ok(JsValue::from(len_storage.borrow().len() as i32))
            },
        )
    };

    // Build localStorage object (Storage interface)
    let local_storage_obj = boa_engine::object::ObjectInitializer::new(ctx)
        .function(get_item_fn, js_string!("getItem"), 1)
        .function(set_item_fn, js_string!("setItem"), 2)
        .function(remove_item_fn, js_string!("removeItem"), 1)
        .function(clear_fn, js_string!("clear"), 0)
        .function(key_fn, js_string!("key"), 1)
        .build();

    let _ = ctx.register_global_property(
        js_string!("localStorage"),
        local_storage_obj,
        Attribute::all(),
    );

    // --- sessionStorage object ---
    //
    // Identical to localStorage but separate storage (same origin, different storage area).
    // In a real browser, localStorage persists and sessionStorage is per-tab.
    // For our implementation, both use an empty HashMap (synced from Session).
    let empty_session = std::collections::HashMap::new();
    register_storage_obj(ctx, js_string!("sessionStorage"), empty_session);
}

/// Register a Storage interface object (localStorage / sessionStorage pattern).
///
/// Creates a JS object with getItem/setItem/removeItem/clear/key/length methods,
/// backed by a RefCell<HashMap<String, String>>.
fn register_storage_obj(
    ctx: &mut Context,
    name: boa_engine::JsString,
    storage: std::collections::HashMap<String, String>,
) {
    use std::cell::RefCell;
    let storage_arc = Arc::new(RefCell::new(storage));

    // getItem
    let get_s = storage_arc.clone();
    let get_item_fn = unsafe {
        NativeFunction::from_closure(
            move |_this: &JsValue, args: &[JsValue], ctx: &mut Context| {
                let key = args
                    .first()
                    .and_then(|v| v.to_string(ctx).ok())
                    .map(|s| s.to_std_string_escaped())
                    .unwrap_or_default();
                match get_s.borrow().get(&key).cloned() {
                    Some(v) => Ok(JsValue::from(JsString::from(v.as_str()))),
                    None => Ok(JsValue::null()),
                }
            },
        )
    };

    // setItem
    let set_s = storage_arc.clone();
    let set_item_fn = unsafe {
        NativeFunction::from_closure(
            move |_this: &JsValue, args: &[JsValue], ctx: &mut Context| {
                if args.len() >= 2 {
                    let key = args[0]
                        .to_string(ctx)
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    let val = args[1]
                        .to_string(ctx)
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    set_s.borrow_mut().insert(key, val);
                }
                Ok(JsValue::undefined())
            },
        )
    };

    // removeItem
    let rem_s = storage_arc.clone();
    let remove_item_fn = unsafe {
        NativeFunction::from_closure(
            move |_this: &JsValue, args: &[JsValue], ctx: &mut Context| {
                if let Some(k) = args.first() {
                    let key = k
                        .to_string(ctx)
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    rem_s.borrow_mut().remove(&key);
                }
                Ok(JsValue::undefined())
            },
        )
    };

    // clear
    let clr_s = storage_arc.clone();
    let clear_fn = unsafe {
        NativeFunction::from_closure(
            move |_this: &JsValue, _args: &[JsValue], _ctx: &mut Context| {
                clr_s.borrow_mut().clear();
                Ok(JsValue::undefined())
            },
        )
    };

    // key
    let key_s = storage_arc.clone();
    let key_fn = unsafe {
        NativeFunction::from_closure(
            move |_this: &JsValue, args: &[JsValue], ctx: &mut Context| {
                if let Some(idx_arg) = args.first() {
                    let idx = idx_arg.to_index(ctx).unwrap_or(0) as usize;
                    let keys: Vec<_> = key_s.borrow().keys().cloned().collect();
                    match keys.get(idx) {
                        Some(k) => Ok(JsValue::from(JsString::from(k.as_str()))),
                        None => Ok(JsValue::null()),
                    }
                } else {
                    Ok(JsValue::null())
                }
            },
        )
    };

    // length (dynamic getter that reads current storage size)
    let len_s = storage_arc.clone();
    let len_getter_fn = {
        let getter: NativeFunction = unsafe {
            NativeFunction::from_closure(move |_this, _args, _ctx| {
                Ok(JsValue::from(len_s.borrow().len() as i32))
            })
        };
        FunctionObjectBuilder::new(ctx.realm(), getter)
            .name(js_string!("get length"))
            .build()
    };
    let storage_obj = boa_engine::object::ObjectInitializer::new(ctx)
        .function(get_item_fn, js_string!("getItem"), 1)
        .function(set_item_fn, js_string!("setItem"), 2)
        .function(remove_item_fn, js_string!("removeItem"), 1)
        .function(clear_fn, js_string!("clear"), 0)
        .function(key_fn, js_string!("key"), 1)
        .accessor(
            js_string!("length"),
            Some(len_getter_fn),
            None,
            Attribute::all(),
        )
        .build();

    let _ = ctx.register_global_property(name, storage_obj, Attribute::all());
}

// ---------------------------------------------------------------------------
// Error formatting
// ---------------------------------------------------------------------------

fn format_js_error(err: &boa_engine::JsError, context: &mut Context) -> String {
    if let Some(native) = err.as_native() {
        let kind = format!("{:?}", native.kind).to_lowercase();
        let msg = native.message();
        if msg.is_empty() {
            return kind;
        }
        return format!("{}: {}", kind, msg);
    }

    if let Some(opaque) = err.as_opaque() {
        if let Ok(s) = opaque.to_string(context) {
            let s = s.to_std_string_escaped();
            if !s.is_empty() && s != "undefined" {
                return s;
            }
        }
        if let Some(obj) = opaque.as_object()
            && let Ok(msg_val) = obj.get(js_string!("message"), context)
            && let Some(msg) = msg_val.as_string()
        {
            let msg_str = msg.to_std_string_escaped();
            if !msg_str.is_empty() {
                if let Ok(name_val) = obj.get(js_string!("name"), context)
                    && let Some(name) = name_val.as_string()
                {
                    return format!("{}: {}", name.to_std_string_escaped(), msg_str);
                }
                return msg_str;
            }
        }
        return format!("Error: {:?}", opaque);
    }

    "Unknown JavaScript error".to_string()
}

/// Extract `(message, name, stack)` from a [`boa_engine::JsError`] for the
/// `CoreEvent::Exception` sink.
///
/// `name` is the error's constructor name (e.g. `TypeError`) when recoverable,
/// else `Error`. `stack` is the thrown object's `.stack` string if present —
/// best-effort, since boa 0.20 does not populate real source locations on
/// `JsNativeError` or `Error.stack` (the synthetic trace comes from the
/// `Error.prototype.stack` polyfill).
fn error_sink_details(
    err: &boa_engine::JsError,
    context: &mut Context,
) -> (String, String, Option<String>) {
    let message = format_js_error(err, context);
    let mut name = "Error".to_string();
    let mut stack: Option<String> = None;
    if let Some(opaque) = err.as_opaque()
        && let Some(obj) = opaque.as_object()
    {
        if let Ok(nv) = obj.get(js_string!("name"), context)
            && let Some(ns) = nv.as_string()
        {
            let s = ns.to_std_string_escaped();
            if !s.is_empty() {
                name = s;
            }
        }
        if let Ok(sv) = obj.get(js_string!("stack"), context)
            && let Some(ss) = sv.as_string()
        {
            let s = ss.to_std_string_escaped();
            if !s.is_empty() && s != "undefined" {
                stack = Some(s);
            }
        }
    }
    (message, name, stack)
}

// ---------------------------------------------------------------------------
// `window` global object
#[allow(clippy::too_many_arguments)]
/// Register `window` global object with browser property stubs.
///
/// This makes `typeof window === 'object'` true and provides common
/// properties that most JS libraries expect.
fn register_window_globals(
    ctx: &mut Context,
    dom_snapshot: &Arc<RwLock<Option<DomSnapshot>>>,
    mutations: &Arc<RwLock<Vec<DomMutation>>>,
    viewport: (u32, u32),
    page_url: &str,
    user_agent: &str,
    fetch_tx_arc: &Arc<RwLock<Option<std::sync::mpsc::Sender<FetchRequestMsg>>>>,
    render_doc_cell: &Rc<RefCell<Option<RenderDocument>>>,
) {
    let _ = fetch_tx_arc; // suppress unused warning
    let url_owned = page_url.to_string();
    let ua_owned = user_agent.to_string();
    let (vp_w, vp_h) = viewport;

    // --- document.body / head / documentElement getters ---
    // We re-register `document` as a getter-based object that resolves these
    // from the DomSnapshot dynamically.
    let snap_body = dom_snapshot.clone();
    let mutations_body = mutations.clone();
    let body_getter = unsafe {
        NativeFunction::from_closure(move |_this, _args, ctx| {
            let snap = snap_body.read();
            if let Some(ref s) = *snap
                && let Some(bid) = s.body_id
                && let Some(node) = s.nodes.get(&bid)
            {
                return Ok(create_element_object(
                    s,
                    node,
                    ctx,
                    &mutations_body,
                    &snap_body,
                ));
            }
            Ok(JsValue::null())
        })
    };

    let snap_head = dom_snapshot.clone();
    let mutations_head = mutations.clone();
    let head_getter = unsafe {
        NativeFunction::from_closure(move |_this, _args, ctx| {
            let snap = snap_head.read();
            if let Some(ref s) = *snap
                && let Some(hid) = s.head_id
                && let Some(node) = s.nodes.get(&hid)
            {
                return Ok(create_element_object(
                    s,
                    node,
                    ctx,
                    &mutations_head,
                    &snap_head,
                ));
            }
            Ok(JsValue::null())
        })
    };

    let snap_de = dom_snapshot.clone();
    let mutations_de = mutations.clone();
    let document_element_getter = unsafe {
        NativeFunction::from_closure(move |_this, _args, ctx| {
            let snap = snap_de.read();
            if let Some(ref s) = *snap {
                let html_node = s.nodes.get(&s.root_id).and_then(|root| {
                    root.children.iter().find_map(|&child_id| {
                        s.nodes.get(&child_id).and_then(|n| {
                            if n.tag == "html" {
                                Some((child_id, n))
                            } else {
                                None
                            }
                        })
                    })
                });
                if let Some((_, node)) = html_node {
                    return Ok(create_element_object(s, node, ctx, &mutations_de, &snap_de));
                }
            }
            Ok(JsValue::null())
        })
    };

    // Register document.body, document.head, document.documentElement as
    // callable getters (not accessors, since boa 0.20 ObjectInitializer
    // doesn't support adding accessors to existing objects easily).
    // These appear as methods but act like properties: document.body()
    // We also register proper property-like access via a wrapper.
    //
    // Simpler approach: register them as methods on the global document
    // and also register a $body / $head / $documentElement that return
    // the element directly.
    //
    // Actually, the simplest working approach for boa 0.20: register them
    // as regular functions called body(), head(), documentElement().
    // Most JS code does document.body (property), not document.body().
    //
    // To make document.body work as a property, we need to build the
    // document object with accessors from the start. Let's modify
    // the existing document construction.

    // We'll add these functions to the global document object by
    // registering global getters that the JS code can use.
    // For now, register as document_get_body() etc. and also
    // as window.document properties via a wrapper.

    // --- Pre-build values that need ctx (to avoid double borrow) ---
    let languages_arr: JsValue = JsArray::from_iter(
        [
            JsValue::from(js_string!("en-US")),
            JsValue::from(js_string!("en")),
        ],
        ctx,
    )
    .into();

    // `navigator.platform` — via the stealth profile so it always agrees with
    // the WebGL renderer and userAgentData.platform (single source of truth).
    let nav_platform = crate::js::stealth::ChromeProfile::platform_for(&ua_owned);

    // window.navigator
    let nav_obj = boa_engine::object::ObjectInitializer::new(ctx)
        .property(
            js_string!("userAgent"),
            JsValue::from(js_string!(ua_owned.as_str())),
            Attribute::all(),
        )
        .property(
            js_string!("language"),
            JsValue::from(js_string!("en-US")),
            Attribute::all(),
        )
        .property(js_string!("languages"), languages_arr, Attribute::all())
        .property(
            js_string!("platform"),
            JsValue::from(js_string!(nav_platform)),
            Attribute::all(),
        )
        .property(
            js_string!("vendor"),
            JsValue::from(js_string!("Google Inc.")),
            Attribute::all(),
        )
        .property(
            js_string!("appName"),
            JsValue::from(js_string!("Netscape")),
            Attribute::all(),
        )
        .property(
            js_string!("appVersion"),
            JsValue::from(js_string!(ua_owned.as_str())),
            Attribute::all(),
        )
        .property(
            js_string!("webdriver"),
            JsValue::from(false),
            Attribute::all(),
        )
        .property(
            js_string!("hardwareConcurrency"),
            JsValue::from(8),
            Attribute::all(),
        )
        .property(
            js_string!("deviceMemory"),
            JsValue::from(8),
            Attribute::all(),
        )
        .property(
            js_string!("maxTouchPoints"),
            JsValue::from(0),
            Attribute::all(),
        )
        .property(js_string!("doNotTrack"), JsValue::null(), Attribute::all())
        .property(
            js_string!("cookieEnabled"),
            JsValue::from(true),
            Attribute::all(),
        )
        .property(js_string!("onLine"), JsValue::from(true), Attribute::all())
        .property(
            js_string!("pdfViewerEnabled"),
            JsValue::from(true),
            Attribute::all(),
        )
        .property(
            js_string!("product"),
            JsValue::from(js_string!("Gecko")),
            Attribute::all(),
        )
        .property(
            js_string!("productSub"),
            JsValue::from(js_string!("20030107")),
            Attribute::all(),
        )
        .property(
            js_string!("vendorSub"),
            JsValue::from(js_string!("")),
            Attribute::all(),
        )
        .build();

    // Level-1 stealth surface: navigator.plugins/mimeTypes/userAgentData/
    // permissions/connection (attached here) plus window.chrome and WebGL
    // constructors (wired into window_final / globals below). Attached before
    // `nav_obj` is cloned to window.navigator + the global navigator, so both
    // see the surface. See `js::stealth` for scope and limitations.
    let stealth = crate::js::stealth::build(ctx, &ua_owned);
    let _ = crate::js::stealth::attach_to_navigator(ctx, &nav_obj, &stealth);

    // navigator.geolocation — backed by the Emulation override. With no
    // override, getCurrentPosition reports POSITION_UNAVAILABLE (a headless
    // client has no real location source).
    let geo_obj = build_geolocation_object(ctx);
    let _ = nav_obj.set(js_string!("geolocation"), geo_obj, true, ctx);

    // window.location
    let parsed_url = url::Url::parse(&url_owned);
    let loc_href = url_owned.clone();
    let loc_origin = parsed_url
        .as_ref()
        .map(|u| u.origin().ascii_serialization())
        .unwrap_or_default();
    let loc_protocol = parsed_url
        .as_ref()
        .map(|u| u.scheme().to_string() + ":")
        .unwrap_or_default();
    let loc_hostname = parsed_url
        .as_ref()
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_default();
    let loc_pathname = parsed_url
        .as_ref()
        .ok()
        .map(|u| u.path().to_string())
        .unwrap_or_default();

    let location_obj = boa_engine::object::ObjectInitializer::new(ctx)
        .property(
            js_string!("href"),
            JsValue::from(js_string!(loc_href.as_str())),
            Attribute::all(),
        )
        .property(
            js_string!("origin"),
            JsValue::from(js_string!(loc_origin.as_str())),
            Attribute::all(),
        )
        .property(
            js_string!("protocol"),
            JsValue::from(js_string!(loc_protocol.as_str())),
            Attribute::all(),
        )
        .property(
            js_string!("hostname"),
            JsValue::from(js_string!(loc_hostname.as_str())),
            Attribute::all(),
        )
        .property(
            js_string!("pathname"),
            JsValue::from(js_string!(loc_pathname.as_str())),
            Attribute::all(),
        )
        .build();

    // window.performance
    let perf_obj = boa_engine::object::ObjectInitializer::new(ctx)
        .function(
            unsafe {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    let ms = js_sys_helpers::now_ms();
                    Ok(JsValue::from(ms))
                })
            },
            js_string!("now"),
            0,
        )
        .build();

    // Build final window by combining all sub-objects
    // Since boa 0.20 doesn't have with_object, we register properties
    // on a fresh object that includes everything.
    // Clone objects before using them in window_final (they get moved)
    let nav_obj_for_window = nav_obj.clone();
    let location_obj_for_window = location_obj.clone();
    let perf_obj_for_window = perf_obj.clone();
    let global_doc = ctx
        .global_object()
        .get(js_string!("document"), ctx)
        .unwrap_or(JsValue::undefined());
    let global_console = ctx
        .global_object()
        .get(js_string!("console"), ctx)
        .unwrap_or(JsValue::undefined());

    // ── getComputedStyle(element) ──
    let gcs_dom = dom_snapshot.clone();
    let get_computed_style_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let element = args.first().cloned().unwrap_or(JsValue::null());
            let node_id = element
                .as_object()
                .and_then(|o| o.get(js_string!("__nodeId"), ctx).ok())
                .and_then(|v| v.as_number().map(|n| n as u32));

            let node_id = match node_id {
                Some(id) => id,
                None => return Ok(JsValue::null()),
            };

            let dom = gcs_dom.read();
            let snapshot = match dom.as_ref() {
                Some(s) => s,
                None => return Ok(JsValue::null()),
            };

            let cs = match LayoutEngine::compute_style(snapshot, node_id) {
                Some(c) => c,
                None => return Ok(JsValue::null()),
            };

            let gcs_obj = boa_engine::object::ObjectInitializer::new(ctx)
                .property(js_string!("display"), JsValue::from(JsString::from(cs.display.clone())), Attribute::all())
                .property(js_string!("visibility"), JsValue::from(JsString::from(cs.visibility.clone())), Attribute::all())
                .property(js_string!("opacity"), JsValue::from(cs.opacity), Attribute::all())
                .property(js_string!("color"), JsValue::from(JsString::from(cs.color.clone())), Attribute::all())
                .property(js_string!("backgroundColor"), JsValue::from(JsString::from(cs.background_color.clone())), Attribute::all())
                .property(js_string!("fontSize"), JsValue::from(JsString::from(format!("{}px", cs.font_size))), Attribute::all())
                .property(js_string!("fontWeight"), JsValue::from(JsString::from(cs.font_weight.clone())), Attribute::all())
                .property(js_string!("textAlign"), JsValue::from(JsString::from(cs.text_align.clone())), Attribute::all())
                .property(js_string!("overflow"), JsValue::from(JsString::from(cs.overflow.clone())), Attribute::all())
                .property(js_string!("pointerEvents"), JsValue::from(JsString::from(cs.pointer_events.clone())), Attribute::all())
                .property(js_string!("position"), JsValue::from(JsString::from(cs.position.clone())), Attribute::all())
                .property(js_string!("width"), cs.width.map(|w| JsValue::from(JsString::from(format!("{}px", w)))).unwrap_or(JsValue::from(JsString::from("auto"))), Attribute::all())
                .property(js_string!("height"), cs.height.map(|h| JsValue::from(JsString::from(format!("{}px", h)))).unwrap_or(JsValue::from(JsString::from("auto"))), Attribute::all())
                .property(js_string!("zIndex"), cs.z_index.map(|z| JsValue::from(JsString::from(z.to_string()))).unwrap_or(JsValue::from(JsString::from("auto"))), Attribute::all())
                .property(js_string!("_visible"), JsValue::from(cs.visible), Attribute::all())
                .property(js_string!("_interactive"), JsValue::from(cs.interactive), Attribute::all())
                // getPropertyValue(name) — look up property by camelCase name
                .function({
                    let props = serde_json::json!({
                        "display": cs.display,
                        "visibility": cs.visibility,
                        "opacity": cs.opacity,
                        "color": cs.color,
                        "backgroundColor": cs.background_color,
                        "fontSize": format!("{}px", cs.font_size),
                        "fontWeight": cs.font_weight,
                        "textAlign": cs.text_align,
                        "overflow": cs.overflow,
                        "position": cs.position,
                        "pointerEvents": cs.pointer_events,
                        "width": cs.width.map(|w| format!("{}px", w)).unwrap_or_else(|| "auto".to_string()),
                        "height": cs.height.map(|h| format!("{}px", h)).unwrap_or_else(|| "auto".to_string()),
                        "zIndex": cs.z_index.map(|z| z.to_string()).unwrap_or_else(|| "auto".to_string()),
                    });
                    {
                        NativeFunction::from_closure(move |_this, args, _ctx| {
                            let name = args.first()
                                .and_then(|v| v.as_string())
                                .map(|s| s.to_std_string_escaped())
                                .unwrap_or_default();
                            let val = props.get(&name)
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            Ok(JsValue::from(JsString::from(val)))
                        })
                    }
                }, js_string!("getPropertyValue"), 1)
                .build();

            Ok(JsValue::from(gcs_obj))
        })
    };

    let window_final = boa_engine::object::ObjectInitializer::new(ctx)
        // Copy viewport props
        .property(
            js_string!("innerWidth"),
            JsValue::from(vp_w as f64),
            Attribute::all(),
        )
        .property(
            js_string!("innerHeight"),
            JsValue::from(vp_h as f64),
            Attribute::all(),
        )
        .property(
            js_string!("outerWidth"),
            JsValue::from(vp_w as f64),
            Attribute::all(),
        )
        .property(
            js_string!("outerHeight"),
            JsValue::from(vp_h as f64),
            Attribute::all(),
        )
        .property(
            js_string!("devicePixelRatio"),
            JsValue::from(1.0),
            Attribute::all(),
        )
        .property(
            js_string!("name"),
            JsValue::from(js_string!("")),
            Attribute::all(),
        )
        .property(js_string!("length"), JsValue::from(0), Attribute::all())
        .property(js_string!("closed"), JsValue::from(false), Attribute::all())
        // Sub-objects
        .property(js_string!("document"), global_doc, Attribute::all())
        .property(js_string!("console"), global_console, Attribute::all())
        .property(
            js_string!("navigator"),
            JsValue::from(nav_obj_for_window),
            Attribute::all(),
        )
        .property(
            js_string!("location"),
            JsValue::from(location_obj_for_window),
            Attribute::all(),
        )
        .property(
            js_string!("performance"),
            JsValue::from(perf_obj_for_window),
            Attribute::all(),
        )
        // DOM shortcuts (as functions since boa 0.20 doesn't support
        // adding accessors to pre-existing objects)
        .function(body_getter, js_string!("getBody"), 0)
        .function(head_getter, js_string!("getHead"), 0)
        .function(document_element_getter, js_string!("getDocumentElement"), 0)
        .function(get_computed_style_fn, js_string!("getComputedStyle"), 1)
        .property(
            js_string!("chrome"),
            stealth.chrome.clone(),
            Attribute::all(),
        )
        .build();

    let _ = ctx.register_global_property(
        js_string!("window"),
        JsValue::from(window_final.clone()),
        Attribute::all(),
    );
    // Register getComputedStyle as a standalone global before moving window_final
    let gcs_fn_val = window_final
        .get(js_string!("getComputedStyle"), ctx)
        .unwrap_or(JsValue::undefined());
    let _ =
        ctx.register_global_property(js_string!("getComputedStyle"), gcs_fn_val, Attribute::all());
    let _ = ctx.register_global_property(
        js_string!("self"),
        JsValue::from(window_final),
        Attribute::all(),
    );

    // Also register navigator and location as standalone globals (browser spec)
    let _ = ctx.register_global_property(
        js_string!("navigator"),
        JsValue::from(nav_obj.clone()),
        Attribute::all(),
    );
    let _ = ctx.register_global_property(
        js_string!("location"),
        JsValue::from(location_obj.clone()),
        Attribute::all(),
    );
    let _ = ctx.register_global_property(
        js_string!("performance"),
        JsValue::from(perf_obj.clone()),
        Attribute::all(),
    );
    // crypto global (for window.crypto)
    let crypto_get_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            // crypto.getRandomValues — fill ArrayBuffer/TypedArray with random bytes
            // For now, just return the buffer as-is (full impl would copy random bytes)
            if let Some(arg) = args.first() {
                Ok(arg.clone())
            } else {
                Ok(JsValue::undefined())
            }
        })
    };
    let crypto_obj = boa_engine::object::ObjectInitializer::new(ctx)
        .function(crypto_get_fn, js_string!("getRandomValues"), 1)
        .build();
    let _ = ctx.register_global_property(
        js_string!("crypto"),
        JsValue::from(crypto_obj),
        Attribute::all(),
    );
    // Stealth globals: real Chrome exposes `chrome`, `WebGLRenderingContext`,
    // and `WebGL2RenderingContext` as top-level globals (not only on `window`).
    let _ = ctx.register_global_property(
        js_string!("chrome"),
        stealth.chrome.clone(),
        Attribute::all(),
    );
    let _ = ctx.register_global_property(
        js_string!("WebGLRenderingContext"),
        stealth.webgl1.clone(),
        Attribute::all(),
    );
    let _ = ctx.register_global_property(
        js_string!("WebGL2RenderingContext"),
        stealth.webgl2.clone(),
        Attribute::all(),
    );
    // ── SPA routing: history + location navigation ──
    // Native triggers push `DomMutation::Navigate`/`Reload`, which `Session`
    // drains and executes as real (async) navigations. The `history`/`location`
    // surface itself is installed by a JS bootstrap below (real JS getters and
    // closures), seeded idempotently so client-side routing survives navigation.
    let nav_mut = mutations.clone();
    let navigate_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            if let Some(v) = args.first()
                && let Some(s) = v.as_string()
            {
                let url = s.to_std_string_escaped();
                if !url.is_empty() {
                    nav_mut.write().push(DomMutation::Navigate { url });
                }
            }
            Ok(JsValue::undefined())
        })
    };
    let _ = ctx.register_global_callable(js_string!("__oxiNavigate"), 1, navigate_fn);
    let rld_mut = mutations.clone();
    let reload_fn = unsafe {
        NativeFunction::from_closure(move |_this, _args, _ctx| {
            rld_mut.write().push(DomMutation::Reload);
            Ok(JsValue::undefined())
        })
    };
    let _ = ctx.register_global_callable(js_string!("__oxiReload"), 0, reload_fn);

    let page_url_json = serde_json::to_string(page_url).unwrap_or_else(|_| "\"\"".to_string());
    let bootstrap = HISTORY_LOCATION_BOOTSTRAP.replace("/*PAGE_URL*/", &page_url_json);
    if let Err(e) = ctx.eval(Source::from_bytes(&bootstrap)) {
        tracing::warn!(error = %e, "history/location bootstrap failed");
    }
    if let Err(e) = ctx.eval(Source::from_bytes(OBSERVER_BOOTSTRAP)) {
        tracing::warn!(error = %e, "observer bootstrap failed");
    }
    {
        let tz =
            serde_json::to_string(&effective_timezone()).unwrap_or_else(|_| "\"UTC\"".to_string());
        let parity = V8_PARITY_BOOTSTRAP
            .replace("/*TZ*/", &tz)
            .replace("/*LOCALE*/", "\"en-US\"");
        if let Err(e) = ctx.eval(Source::from_bytes(&parity)) {
            tracing::warn!(error = %e, "v8 parity bootstrap failed");
        }
    }
    // Native ShadowRoot backing: `__oxi_attach_shadow(hostId)` creates a real
    // shadow tree in the DomSnapshot registry and returns a JS shadow-root
    // object whose appendChild records child node ids for the compose pass
    // (see dom_snapshot::compose_shadow_trees). The JS `Element.prototype.
    // attachShadow` (WEB_COMPONENTS_BOOTSTRAP) delegates to this.
    let rd_for_inner_html = render_doc_cell.clone();
    let attach_shadow_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let host_id = args
                .first()
                .and_then(|v| v.as_number())
                .map(|n| n as u32)
                .unwrap_or(0);
            // mode: 'closed' → Closed, anything else → Open (default).
            let mode = args
                .get(1)
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .map(|m| {
                    if m.eq_ignore_ascii_case("closed") {
                        crate::js::dom_snapshot::ShadowMode::Closed
                    } else {
                        crate::js::dom_snapshot::ShadowMode::Open
                    }
                })
                .unwrap_or_default();
            crate::js::dom_snapshot::register_shadow_host(host_id, mode);
            let host_val = args.first().cloned().unwrap_or(JsValue::undefined());
            // appendChild: record the child node id as a shadow child of host.
            let append_child_host = host_id;
            let append_child_fn = NativeFunction::from_closure(move |_this, args, ctx| {
                let child = args.first().cloned().unwrap_or(JsValue::undefined());
                let child_id = child
                    .as_object()
                    .and_then(|o| o.get(js_string!("__nodeId"), ctx).ok())
                    .and_then(|v| v.as_number().map(|n| n as u32));
                if let Some(cid) = child_id {
                    crate::js::dom_snapshot::push_shadow_child(append_child_host, cid);
                }
                Ok(child)
            });
            // ParentNode.append: append every (element) argument to the shadow
            // root. Mirrors appendChild for the multi-arg case.
            let append_host = host_id;
            let append_fn = NativeFunction::from_closure(move |_this, args, ctx| {
                for a in args.iter() {
                    let cid = a
                        .as_object()
                        .and_then(|o| o.get(js_string!("__nodeId"), ctx).ok())
                        .and_then(|v| v.as_number().map(|n| n as u32));
                    if let Some(cid) = cid {
                        crate::js::dom_snapshot::push_shadow_child(append_host, cid);
                    }
                }
                Ok(JsValue::undefined())
            });
            let root = boa_engine::object::ObjectInitializer::new(ctx)
                .property(js_string!("nodeType"), JsValue::from(11), Attribute::all())
                .property(js_string!("host"), host_val, Attribute::all())
                .function(append_child_fn, js_string!("appendChild"), 1)
                .function(append_fn, js_string!("append"), 1)
                .build();
            Ok(JsValue::from(root))
        })
    };
    // __oxi_shadow_set_inner_html(hostId, html): parse `html` and append the
    // resulting nodes as shadow children of `hostId` (used by the
    // `shadowRoot.innerHTML` setter installed in WEB_COMPONENTS_BOOTSTRAP).
    let rd_ih = rd_for_inner_html.clone();
    let shadow_set_inner_html_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            let host_id = args
                .first()
                .and_then(|v| v.as_number())
                .map(|n| n as u32)
                .unwrap_or(0);
            let html = args
                .get(1)
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            append_html_fragment_to_shadow(&rd_ih, host_id, &html);
            Ok(JsValue::undefined())
        })
    };
    let _ = ctx.register_global_callable(
        js_string!("__oxi_shadow_set_inner_html"),
        2,
        shadow_set_inner_html_fn,
    );
    let _ = ctx.register_global_callable(js_string!("__oxi_attach_shadow"), 2, attach_shadow_fn);
    if let Err(e) = ctx.eval(Source::from_bytes(WEB_COMPONENTS_BOOTSTRAP)) {
        tracing::warn!(error = %e, "web components bootstrap failed");
    }
    const FORMDATA_BLOB_BOOTSTRAP: &str = r#"
(function () {
  function install(g) {
    if (typeof g.Blob === 'undefined') {
      function toBytes(parts) {
        parts = parts || [];
        var bytes = [];
        for (var i = 0; i < parts.length; i++) {
          var p = parts[i];
          if (typeof p === 'string') {
            for (var j = 0; j < p.length; j++) bytes.push(p.charCodeAt(j) & 0xff);
          } else if (typeof p === 'number') {
            bytes.push(p & 0xff);
          } else if (p && typeof p.length === 'number') {
            for (var j = 0; j < p.length; j++) bytes.push((p[j] || 0) & 0xff);
          } else if (p && p.__bytes && typeof p.__bytes.length === 'number') {
            for (var j = 0; j < p.__bytes.length; j++) bytes.push((p.__bytes[j] || 0) & 0xff);
          }
        }
        return bytes;
      }
      function Blob(parts, opts) {
        if (!(this instanceof Blob)) throw new TypeError("Failed to construct 'Blob'");
        var b = toBytes(parts);
        this.size = b.length;
        this.type = (opts && opts.type) ? String(opts.type) : '';
        this.__bytes = new Uint8Array(b);
        this.__isBlob = true;
        var src = b;
        this.arrayBuffer = function () { return Promise.resolve(new Uint8Array(src)); };
        this.text = function () { var s = ''; for (var i = 0; i < src.length; i++) s += String.fromCharCode(src[i]); return Promise.resolve(s); };
        this.slice = function () { return new Blob([]); };
      }
      g.Blob = Blob;
    }
    if (typeof g.FormData === 'undefined') {
      function FormData() {
        if (!(this instanceof FormData)) throw new TypeError("Failed to construct 'FormData'");
        this.__entries = [];
        this.__isFormData = true;
      }
      FormData.prototype.append = function (name, value, filename) {
        var isBlob = !!(value && value.__isBlob);
        var e = { name: String(name), value: value, isBlob: isBlob };
        if (isBlob) e.filename = (typeof filename === 'string') ? filename : 'blob';
        this.__entries.push(e);
      };
      FormData.prototype.set = function (name, value, filename) { this.delete(name); this.append(name, value, filename); };
      FormData.prototype.delete = function (name) {
        var n = String(name), kept = [];
        for (var i = 0; i < this.__entries.length; i++) if (this.__entries[i].name !== n) kept.push(this.__entries[i]);
        this.__entries = kept;
      };
      FormData.prototype.get = function (name) {
        var n = String(name);
        for (var i = 0; i < this.__entries.length; i++) if (this.__entries[i].name === n) return this.__entries[i].value;
        return null;
      };
      FormData.prototype.getAll = function (name) {
        var n = String(name), r = [];
        for (var i = 0; i < this.__entries.length; i++) if (this.__entries[i].name === n) r.push(this.__entries[i].value);
        return r;
      };
      FormData.prototype.has = function (name) {
        var n = String(name);
        for (var i = 0; i < this.__entries.length; i++) if (this.__entries[i].name === n) return true;
        return false;
      };
      FormData.prototype.entries = function () {
        var arr = [];
        for (var i = 0; i < this.__entries.length; i++) arr.push([this.__entries[i].name, this.__entries[i].value]);
        var idx = 0;
        return { next: function () { return idx < arr.length ? { value: arr[idx++], done: false } : { value: null, done: true }; } };
      };
      g.FormData = FormData;
    }
  }
  install(globalThis);
  if (globalThis.window) install(globalThis.window);
  if (typeof globalThis.__oxi_serialize_body === 'undefined') {
    globalThis.__oxi_serialize_body = function (body) {
      if (body == null) return null;
      if (typeof body === 'string') return { bytes: null, text: body, contentType: null };
      if (body.__isFormData) {
        var boundary = '----oxiformdata' + Math.random().toString(36).slice(2) + (Date.now ? Date.now().toString(36) : '0');
        var chunks = [];
        var entries = body.__entries || [];
        for (var i = 0; i < entries.length; i++) {
          var e = entries[i];
          if (e.isBlob) {
            chunks.push('--' + boundary + '\r\n');
            chunks.push('Content-Disposition: form-data; name="' + e.name + '"; filename="' + (e.filename || 'blob') + '"\r\n');
            chunks.push('Content-Type: ' + ((e.value && e.value.type) || 'application/octet-stream') + '\r\n\r\n');
            var vb = (e.value && e.value.__bytes) || [];
            for (var k = 0; k < vb.length; k++) chunks.push(String.fromCharCode(vb[k] & 0xff));
            chunks.push('\r\n');
          } else {
            chunks.push('--' + boundary + '\r\n');
            chunks.push('Content-Disposition: form-data; name="' + e.name + '"\r\n\r\n');
            chunks.push(String(e.value));
            chunks.push('\r\n');
          }
        }
        chunks.push('--' + boundary + '--\r\n');
        var text = chunks.join('');
        var out = [];
        for (var j = 0; j < text.length; j++) out.push(text.charCodeAt(j) & 0xff);
        return { bytes: new Uint8Array(out), text: null, contentType: 'multipart/form-data; boundary=' + boundary };
      }
      if (body.__isBlob) {
        var b = body.__bytes || [];
        var out = [];
        for (var j = 0; j < b.length; j++) out.push(b[j] & 0xff);
        return { bytes: new Uint8Array(out), text: null, contentType: body.type || null };
      }
      return { bytes: null, text: String(body), contentType: null };
    };
  }
})();
"#;
    if let Err(e) = ctx.eval(Source::from_bytes(FORMDATA_BLOB_BOOTSTRAP)) {
        tracing::warn!(error = %e, "formdata/blob bootstrap failed");
    }
    const CANVAS_BOOTSTRAP: &str = r#"
(function () {
  if (globalThis.__oxi_canvas_patched) return;
  globalThis.__oxi_canvas_patched = true;
  function makeContext2d(canvas) {
    return {
      canvas: canvas,
      fillStyle: '#000000', strokeStyle: '#000000', lineWidth: 1, font: '10px sans-serif',
      textAlign: 'start', textBaseline: 'alphabetic', globalAlpha: 1, lineCap: 'butt',
      lineJoin: 'miter', miterLimit: 10, shadowBlur: 0, shadowColor: 'rgba(0, 0, 0, 0)',
      shadowOffsetX: 0, shadowOffsetY: 0, globalCompositeOperation: 'source-over',
      imageSmoothingEnabled: true, imageSmoothingQuality: 'low', direction: 'ltr',
      letterSpacing: '0px', fontKerning: 'auto', filter: 'none',
      fillRect: function () {}, strokeRect: function () {}, clearRect: function () {},
      beginPath: function () {}, closePath: function () {}, moveTo: function () {}, lineTo: function () {},
      arc: function () {}, arcTo: function () {}, ellipse: function () {}, rect: function () {},
      bezierCurveTo: function () {}, quadraticCurveTo: function () {}, roundRect: function () {},
      fill: function () {}, stroke: function () {}, clip: function () {},
      fillText: function () {}, strokeText: function () {}, drawImage: function () {},
      save: function () {}, restore: function () {}, scale: function () {}, rotate: function () {},
      translate: function () {}, transform: function () {}, setTransform: function () {}, resetTransform: function () {},
      setLineDash: function () {}, getLineDash: function () { return []; }, lineDashOffset: 0,
      createLinearGradient: function () { return { addColorStop: function () {} }; },
      createRadialGradient: function () { return { addColorStop: function () {} }; },
      createConicGradient: function () { return { addColorStop: function () {} }; },
      createPattern: function () { return {}; },
      measureText: function (s) {
        var n = String(s == null ? '' : s).length;
        return { width: n * 6, actualBoundingBoxLeft: 0, actualBoundingBoxRight: n * 6,
                 actualBoundingBoxAscent: 8, actualBoundingBoxDescent: 2,
                 fontBoundingBoxAscent: 10, fontBoundingBoxDescent: 2 };
      },
      getImageData: function (x, y, w, h) {
        var sw = Math.max(0, (w | 0)), sh = Math.max(0, (h | 0));
        return { data: new Uint8ClampedArray(sw * sh * 4), width: sw, height: sh, colorSpace: 'srgb' };
      },
      putImageData: function () {},
      createImageData: function (w, h) {
        var sw, sh;
        if (h === undefined) { sw = (w && w.width) || 0; sh = (w && w.height) || 0; } else { sw = w | 0; sh = h | 0; }
        return { data: new Uint8ClampedArray(Math.max(0, sw * sh * 4)), width: sw, height: sh, colorSpace: 'srgb' };
      },
      isPointInPath: function () { return false; }, isPointInStroke: function () { return false; },
      getContextAttributes: function () { return { alpha: true, willReadFrequently: false }; },
      drawFocusIfNeeded: function () {}
    };
  }
  function makeWebGL(canvas) {
    var no = function () {};
    return {
      canvas: canvas, drawingBufferWidth: canvas.width || 300, drawingBufferHeight: canvas.height || 150,
      getParameter: function () { return null; }, getExtension: function () { return null; },
      getSupportedExtensions: function () { return []; },
      createShader: function () { return {}; }, shaderSource: no, compileShader: no, getShaderParameter: function () { return null; },
      createProgram: function () { return {}; }, attachShader: no, linkProgram: no, useProgram: no,
      getProgramParameter: function () { return null; }, getAttribLocation: function () { return -1; },
      getUniformLocation: function () { return null; },
      createBuffer: function () { return {}; }, bindBuffer: no, bufferData: no, deleteShader: no, deleteProgram: no, deleteBuffer: no,
      enableVertexAttribArray: no, disableVertexAttribArray: no, vertexAttribPointer: no,
      uniform1f: no, uniform2f: no, uniform3f: no, uniform4f: no, uniform1i: no, uniform2i: no, uniform3i: no, uniform4i: no,
      uniformMatrix2fv: no, uniformMatrix3fv: no, uniformMatrix4fv: no,
      viewport: no, drawArrays: no, drawElements: no, clearColor: no, clear: no, enable: no, disable: no,
      blendFunc: no, depthFunc: no, cullFace: no, frontFace: no, pixelStorei: no, hint: no,
      createContextAttributes: function () { return { alpha: true, antialias: true }; },
      getContextAttributes: function () { return { alpha: true, antialias: true }; }
    };
  }
  function patchCanvas(el) {
    if (el.__oxiCanvas) return el;
    el.__oxiCanvas = true;
    if (typeof el.width !== 'number') el.width = 300;
    if (typeof el.height !== 'number') el.height = 150;
    el.getContext = function (type) {
      var t = String(type || '');
      if (t === '2d') return makeContext2d(el);
      if (t === 'webgl' || t === 'webgl2' || t === 'experimental-webgl') return makeWebGL(el);
      return null;
    };
    el.toDataURL = function () { return 'data:,'; };
    el.toBlob = function (cb) { try { if (typeof cb === 'function') cb(new globalThis.Blob([])); } catch (e) {} };
    el.captureStream = function () { return { getTracks: function () { return []; }, getVideoTracks: function () { return []; } }; };
    el.transferControlToOffscreen = function () { return makeContext2d(el); };
    return el;
  }
  globalThis.__oxi_patchCanvas = patchCanvas;
  if (typeof document !== 'undefined' && typeof document.createElement === 'function' && !document.__oxiCeWrap) {
    document.__oxiCeWrap = true;
    var origCE = document.createElement.bind(document);
    document.createElement = function (tag) {
      var el = origCE(String(tag));
      try { if (String(tag).toLowerCase() === 'canvas') patchCanvas(el); } catch (e) {}
      return el;
    };
  }
})();
"#;
    if let Err(e) = ctx.eval(Source::from_bytes(CANVAS_BOOTSTRAP)) {
        tracing::warn!(error = %e, "canvas bootstrap failed");
    }
    // Native alert/confirm/prompt backing: pushes a CoreEvent::Dialog to the
    // sink and blocks (polling the shared DialogGate) until the CDP client
    // resolves it via Page.handleJavaScriptDialog, or the 30s timeout elapses.
    // When no sink is attached (CLI path), it default-dismisses immediately,
    // preserving the pre-event no-op semantics.
    let dialog_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            let dtype = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_else(|| "alert".to_string());
            let message = args
                .get(1)
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            let default_val = args
                .get(2)
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped());
            let dialog_type = match dtype.as_str() {
                "confirm" => DialogType::Confirm,
                "prompt" => DialogType::Prompt,
                _ => DialogType::Alert,
            };
            let has_sink = event_sink_attached();
            push_event(CoreEvent::Dialog {
                dialog_type,
                message: message.clone(),
                default_value: if dialog_type == DialogType::Prompt {
                    default_val.clone()
                } else {
                    None
                },
            });
            // Default resolution when dismissed / no observer: real browsers
            // dismiss an unhandled dialog (alert→undefined, confirm→false,
            // prompt→null). The CDP client overrides this via
            // Page.handleJavaScriptDialog before the 30s timeout.
            let default = DialogResult {
                accept: false,
                prompt_text: default_val.clone(),
            };
            let result = if has_sink {
                wait_dialog_resolution(default, Duration::from_secs(30))
            } else {
                default
            };
            Ok(match dialog_type {
                DialogType::Alert => JsValue::undefined(),
                DialogType::Confirm => JsValue::from(result.accept),
                DialogType::Prompt => {
                    if result.accept {
                        match result.prompt_text {
                            Some(t) => JsValue::from(JsString::from(t.as_str())),
                            None => JsValue::from(JsString::from("")),
                        }
                    } else {
                        JsValue::null()
                    }
                }
            })
        })
    };
    let _ = ctx.register_global_callable(js_string!("__oxi_dialog"), 3, dialog_fn);
    const DIALOG_BOOTSTRAP: &str = r#"
(function () {
  function install(g) {
    g.alert = function (m) { __oxi_dialog('alert', m, null); };
    g.confirm = function (m) { return __oxi_dialog('confirm', m, null); };
    g.prompt = function (m, d) { return __oxi_dialog('prompt', m, d != null ? d : null); };
    if (typeof g.print === 'undefined') g.print = function () {};
  }
  install(globalThis);
  if (globalThis.window) install(globalThis.window);
})();
"#;
    if let Err(e) = ctx.eval(Source::from_bytes(DIALOG_BOOTSTRAP)) {
        tracing::warn!(error = %e, "dialog bootstrap failed");
    }

    const OBSERVER_BOOTSTRAP: &str = r#"
(function () {
  // Headless: no layout-driven intersection, so use real-browser initial-fire
  // semantics — observe() invokes the callback once with isIntersecting:true.
  // This makes lazy-load + feature-detection code work while keeping the full
  // API surface (observe/unobserve/disconnect/takeRecords) present.
  function IO(cb, opts) {
    if (!(this instanceof IO)) return new IO(cb, opts);
    this.__cb = cb;
    this.root = (opts && opts.root) || null;
    this.rootMargin = (opts && opts.rootMargin) || '0px';
    var th = opts && opts.threshold != null ? opts.threshold : 0;
    this.thresholds = typeof th === 'number' ? [th] : [0];
  }
  IO.prototype.observe = function (t) {
    try {
      this.__cb([{
        target: t, isIntersecting: true, isVisible: true, intersectionRatio: 1,
        time: Date.now(), rootBounds: null,
        intersectionRect: { x: 0, y: 0, top: 0, left: 0, right: 0, bottom: 0, width: 1, height: 1 },
        boundingClientRect: { x: 0, y: 0, top: 0, left: 0, right: 0, bottom: 0, width: 1, height: 1 }
      }], this);
    } catch (e) {}
    return this;
  };
  IO.prototype.unobserve = function () { return this; };
  IO.prototype.disconnect = function () {};
  IO.prototype.takeRecords = function () { return []; };
  globalThis.IntersectionObserver = IO;

  function RO(cb) {
    if (!(this instanceof RO)) return new RO(cb);
    this.__cb = cb;
  }
  RO.prototype.observe = function (t) {
    try {
      this.__cb([{
        target: t,
        contentRect: { x: 0, y: 0, top: 0, left: 0, right: 0, bottom: 0, width: 1, height: 1 },
        borderBoxSize: [{ inlineSize: 1, blockSize: 1 }],
        contentBoxSize: [{ inlineSize: 1, blockSize: 1 }],
        devicePixelContentBoxSize: [{ inlineSize: 1, blockSize: 1 }]
      }], this);
    } catch (e) {}
    return this;
  };
  RO.prototype.unobserve = function () { return this; };
  RO.prototype.disconnect = function () {};
  globalThis.ResizeObserver = RO;

  // Feature-detection code uses `'IntersectionObserver' in window`, so mirror
  // the constructors onto the window object too (re-applied on every navigation).
  if (globalThis.window) {
    globalThis.window.IntersectionObserver = IO;
    globalThis.window.ResizeObserver = RO;
  }
})();
"#;
    const V8_PARITY_BOOTSTRAP: &str = r#"
(function () {
  function def(g, name, value) {
    try { if (typeof g[name] === 'undefined') g[name] = value; } catch (e) {}
  }
  // Intl: boa has none. Provide the fingerprint surface reads rely on —
  // DateTimeFormat/NumberFormat .resolvedOptions().{locale,timeZone,calendar,
  // numberingSystem} + Collator. Numeric/date formatting is best-effort.
  if (typeof globalThis.Intl === 'undefined') {
    var TZ = /*TZ*/;
    var LOCALE = /*LOCALE*/;
    function resolved(locale) {
      return { locale: locale || LOCALE, timeZone: TZ, calendar: 'gregory', numberingSystem: 'latn' };
    }
    function normLocale(l) { return (typeof l === 'string') ? l : (l && l.length ? l[0] : LOCALE); }
    function DT(locale, options) {
      if (!(this instanceof DT)) return new DT(locale, options);
      this.__l = normLocale(locale);
      this.__opts = options || {};
    }
    DT.prototype.resolvedOptions = function () {
      var o = resolved(this.__l);
      // Honor an explicitly requested timeZone (real browsers do); otherwise
      // resolvedOptions().timeZone is the system default TZ baked into `resolved`.
      if (this.__opts.timeZone) o.timeZone = this.__opts.timeZone;
      return o;
    };
    DT.prototype.format = function (d) {
      var n = (d instanceof Date) ? d : new Date();
      return n.getFullYear() + '/' + (n.getMonth() + 1) + '/' + n.getDate();
    };
    DT.supportedLocalesOf = function (l) { return (typeof l === 'string') ? [l] : (l && l.length ? [l[0]] : [LOCALE]); };
    function NF(locale, options) {
      if (!(this instanceof NF)) return new NF(locale, options);
      this.__l = normLocale(locale);
    }
    NF.prototype.format = function (n) { return String(n); };
    NF.prototype.resolvedOptions = function () { return { locale: this.__l, numberingSystem: 'latn' }; };
    NF.supportedLocalesOf = DT.supportedLocalesOf;
    function Col(locale) {
      if (!(this instanceof Col)) return new Col(locale);
      this.__l = (typeof locale === 'string') ? locale : LOCALE;
    }
    Col.prototype.compare = function (a, b) { a = String(a); b = String(b); return a < b ? -1 : (a > b ? 1 : 0); };
    Col.prototype.resolvedOptions = function () { return resolved(this.__l); };
    Col.supportedLocalesOf = DT.supportedLocalesOf;
    var IntlObj = {
      DateTimeFormat: DT, NumberFormat: NF, Collator: Col,
      getCanonicalLocales: function (l) { return (typeof l === 'string') ? [l] : l; }
    };
    globalThis.Intl = IntlObj;
  }
  if (globalThis.window) globalThis.window.Intl = globalThis.Intl;
  // Error.stack: boa leaves it undefined; give a V8-shaped trace so sandbox/
  // headless detectors reading `new Error().stack` see a Chrome frame, not
  // `undefined`. Exact line numbers are unknowable from JS — this passes the
  // common "is .stack a non-empty string starting with the error name" check.
  if (typeof Error.prototype.stack === 'undefined') {
    Object.defineProperty(Error.prototype, 'stack', {
      configurable: true,
      get: function () {
        var name = (this && this.constructor && this.constructor.name) ? this.constructor.name : 'Error';
        var msg = (this && typeof this.message === 'string' && this.message.length) ? (': ' + this.message) : '';
        return name + msg + '\n    at Object.<anonymous> (<anonymous>:1:1)';
      }
    });
  }
  // structuredClone: deep-clone plain data via JSON.
  def(globalThis, 'structuredClone', function (v) {
    if (v === null || typeof v !== 'object') return v;
    try { return JSON.parse(JSON.stringify(v)); } catch (e) { return v; }
  });
  // queueMicrotask: schedule on the microtask queue via Promise.
  def(globalThis, 'queueMicrotask', function (cb) { Promise.resolve().then(cb); });
  // FinalizationRegistry presence stub (WeakRef is already present in boa).
  if (typeof globalThis.FinalizationRegistry === 'undefined') {
    function FR(cb) { this.__cb = cb; }
    FR.prototype.register = function () { return this; };
    FR.prototype.unregister = function () { return false; };
    globalThis.FinalizationRegistry = FR;
  }
  if (globalThis.window) globalThis.window.FinalizationRegistry = globalThis.FinalizationRegistry;
  // Page-context booleans real Chrome exposes.
  def(globalThis, 'crossOriginIsolated', false);
  def(globalThis, 'isSecureContext', true);
  def(globalThis, 'originAgentCluster', false);
  if (globalThis.window) {
    def(globalThis.window, 'crossOriginIsolated', false);
    def(globalThis.window, 'isSecureContext', true);
  }
})();
"#;
    const WEB_COMPONENTS_BOOTSTRAP: &str = r#"
(function () {
  // DOM constructor presence: Element/HTMLElement/Node/ShadowRoot/
  // DocumentFragment/EventTarget. Real element objects here are plain object
  // literals (createElement returns {}), NOT instances of these — parsed
  // elements won't gain the prototype chain. These exist for feature-detection
  // ('attachShadow' in Element.prototype, typeof HTMLElement) and so
  // customElements.define can validate constructors.
  function defCtor(name, parent) {
    if (typeof globalThis[name] !== 'undefined') return;
    var P = parent || function () {};
    function C() { if (!(this instanceof C)) throw new TypeError("Failed to construct '" + name + "': Please use the 'new' operator"); }
    C.prototype = Object.create(P.prototype);
    C.prototype.constructor = C;
    globalThis[name] = C;
  }
  defCtor('EventTarget');
  defCtor('Node', globalThis.EventTarget);
  defCtor('Element', globalThis.Node);
  defCtor('HTMLElement', globalThis.Element);
  defCtor('DocumentFragment', globalThis.Node);
  defCtor('ShadowRoot', globalThis.DocumentFragment);
  // attachShadow / getRootNode / shadowRoot on Element.prototype.
  if (globalThis.Element && !globalThis.Element.prototype.attachShadow) {
    globalThis.Element.prototype.attachShadow = function (init) {
      if (this.__shadowRoot) throw new Error("Failed to execute 'attachShadow': Shadow root already attached");
      var mode = (init && init.mode) || 'open';
      var root = (typeof __oxi_attach_shadow === 'function')
        ? __oxi_attach_shadow(this.__nodeId, mode)
        : ((typeof document !== 'undefined' && document.createDocumentFragment) ? document.createDocumentFragment() : {});
      root.host = this; root.mode = mode;
      // shadowRoot.innerHTML setter: parse the fragment and append the nodes
      // as shadow children of this host (native __oxi_shadow_set_inner_html).
      if (typeof __oxi_shadow_set_inner_html === 'function') {
        var hostNode = this;
        Object.defineProperty(root, 'innerHTML', {
          configurable: true,
          get: function () { return ''; },
          set: function (html) { __oxi_shadow_set_inner_html(hostNode.__nodeId, String(html)); }
        });
      }
      this.__shadowRoot = root; return root;
    };
    Object.defineProperty(globalThis.Element.prototype, 'shadowRoot', {
      configurable: true,
      get: function () { return (this.__shadowRoot && this.__shadowRoot.mode === 'closed') ? null : (this.__shadowRoot || null); }
    });
    globalThis.Element.prototype.getRootNode = function () {
      var n = this, g = 0;
      while (n && n.parentNode && g < 100) { n = n.parentNode; g++; }
      return n || this;
    };
  }
  // customElements registry: define/get/whenDefined/upgrade. Full
  // upgrade-on-parse needs a parser hook (unavailable from JS); presence +
  // explicit registration + best-effort upgrade of existing matched nodes.
  if (typeof globalThis.customElements === 'undefined') {
    var registry = {}; var waiting = {};
    function valid(name) { return typeof name === 'string' && name.indexOf('-') > 0 && name === name.toLowerCase(); }
    var CE = {
      define: function (name, ctor) {
        if (!valid(name)) throw new TypeError("Failed to execute 'define': '" + name + "' is not a valid custom element name");
        if (registry[name]) throw new Error("Failed to execute 'define': this name has already been used: '" + name + "'");
        registry[name] = ctor;
        try { var ex = document.querySelectorAll(name); for (var i = 0; ex && i < ex.length; i++) CE.upgrade(ex[i], ctor); } catch (e) {}
        if (waiting[name]) { for (var j = 0; j < waiting[name].length; j++) { try { waiting[name][j](ctor); } catch (e) {} } delete waiting[name]; }
      },
      get: function (name) { return registry[name]; },
      whenDefined: function (name) {
        return new Promise(function (resolve) {
          if (registry[name]) resolve(registry[name]);
          else (waiting[name] = waiting[name] || []).push(resolve);
        });
      },
      upgrade: function (node, ctor) {
        ctor = ctor || (node && node.tagName && registry[node.tagName.toLowerCase()]);
        if (!ctor) return;
        try { if (ctor.prototype) Object.setPrototypeOf(node, ctor.prototype); } catch (e) {}
        try { ctor.call(node); } catch (e) {}
      }
    };
    globalThis.customElements = CE;
  }
  // Upgrade helper called from the native createElement path: if the element's
  // tag is a registered custom element, apply its prototype + constructor.
  // Lives on globalThis (not window) so it survives document rebuilds.
  globalThis.__oxi_upgrade_custom = function (node) {
    try {
      if (node && globalThis.customElements) {
        var tag = (node.tagName && node.tagName.toLowerCase) ? node.tagName.toLowerCase() : '';
        if (tag && typeof globalThis.customElements.upgrade === 'function') {
          globalThis.customElements.upgrade(node);
        }
      }
    } catch (e) {}
    return node;
  };
  // Lifecycle helpers called from the native appendChild / remove /
  // setAttribute hooks: fire the custom-element callback if present. Best-
  // effort subtree walk for connected/disconnected (render-doc elements lack
  // a children accessor, so only the appended node fires in that case).
  globalThis.__oxi_fire_connected = function (node) {
    var stack = [node];
    while (stack.length) {
      var n = stack.pop(); if (!n) continue;
      try { if (typeof n.connectedCallback === 'function') n.connectedCallback(); } catch (e) {}
      var kids = n.children || n.childNodes; if (kids) for (var i = 0; i < kids.length; i++) stack.push(kids[i]);
    }
  };
  globalThis.__oxi_fire_disconnected = function (node) {
    var stack = [node];
    while (stack.length) {
      var n = stack.pop(); if (!n) continue;
      try { if (typeof n.disconnectedCallback === 'function') n.disconnectedCallback(); } catch (e) {}
      var kids = n.children || n.childNodes; if (kids) for (var i = 0; i < kids.length; i++) stack.push(kids[i]);
    }
  };
  globalThis.__oxi_fire_attr_changed = function (node, name, oldVal, newVal) {
    try {
      if (node && typeof node.attributeChangedCallback === 'function') {
        var Ctor = node.constructor;
        var obs = (Ctor && Ctor.observedAttributes) || null;
        if (!obs || obs.indexOf(name) >= 0) node.attributeChangedCallback(name, oldVal, newVal);
      }
    } catch (e) {}
  };
  // window is rebuilt every navigation → sync unconditionally.
  if (globalThis.window) {
    var w = globalThis.window;
    var names = ['EventTarget','Node','Element','HTMLElement','DocumentFragment','ShadowRoot'];
    for (var k = 0; k < names.length; k++) { var nm = names[k]; if (globalThis[nm] && typeof w[nm] === 'undefined') w[nm] = globalThis[nm]; }
    w.customElements = globalThis.customElements;
  }
  // AbortController / AbortSignal (feature-detect + basic abort propagation).
  if (typeof globalThis.AbortController === 'undefined') {
    function AbortSignal() {
      this.aborted = false; this.reason = undefined; this.onabort = null;
      var listeners = [];
      this.addEventListener = function (t, cb) { if (t === 'abort' && typeof cb === 'function') listeners.push(cb); };
      this.removeEventListener = function (t, cb) { if (t === 'abort') listeners = listeners.filter(function (f) { return f !== cb; }); };
      this._abort = function (reason) {
        if (this.aborted) return;
        this.aborted = true; this.reason = reason;
        var ev = { type: 'abort', target: this };
        if (typeof this.onabort === 'function') { try { this.onabort(ev); } catch (e) {} }
        for (var i = 0; i < listeners.length; i++) { try { listeners[i](ev); } catch (e) {} }
      };
    }
    function AbortController() { this.signal = new AbortSignal(); }
    AbortController.prototype.abort = function (reason) { this.signal._abort(reason); };
    globalThis.AbortSignal = AbortSignal;
    globalThis.AbortController = AbortController;
    if (globalThis.window) { globalThis.window.AbortController = AbortController; globalThis.window.AbortSignal = AbortSignal; }
  }
})();
"#;

    const HISTORY_LOCATION_BOOTSTRAP: &str = r#"
(function () {
  var PAGE_URL = /*PAGE_URL*/;
  function isAbs(u) { return /^(https?:|data:|blob:|file:|ftp:)/i.test(u) || u.indexOf('//') === 0; }
  function originOf(u) { var m = /^(https?:\/\/[^\/#?]+)/i.exec(u); return m ? m[1] : ''; }
  if (!globalThis.__oxiHistoryInit) {
    globalThis.__oxiHistoryEntries = [{ url: PAGE_URL, state: null }];
    globalThis.__oxiHistoryIndex = 0;
    globalThis.__oxiPopstateListeners = [];
    globalThis.__oxiHistoryInit = true;
  } else {
    var top = globalThis.__oxiHistoryEntries[globalThis.__oxiHistoryIndex];
    if (PAGE_URL && (!top || top.url !== PAGE_URL)) {
      globalThis.__oxiHistoryEntries = globalThis.__oxiHistoryEntries.slice(0, globalThis.__oxiHistoryIndex + 1);
      globalThis.__oxiHistoryEntries.push({ url: PAGE_URL, state: null });
      globalThis.__oxiHistoryIndex = globalThis.__oxiHistoryEntries.length - 1;
    }
  }
  function cur() { return globalThis.__oxiHistoryEntries[globalThis.__oxiHistoryIndex] || { url: PAGE_URL, state: null }; }
  function resolveUrl(url) {
    if (!url) return cur().url;
    url = String(url);
    if (isAbs(url)) return url;
    var base = cur().url || PAGE_URL;
    if (url.charAt(0) === '#') return base.split('#')[0] + url;
    if (url.charAt(0) === '/') return originOf(base) + url;
    var b = base.split('#')[0].split('?')[0];
    var i = b.lastIndexOf('/');
    return (i >= 0 ? b.substring(0, i + 1) : b + '/') + url;
  }
  function firePopstate() {
    var ev = { type: 'popstate', state: cur().state };
    (globalThis.__oxiPopstateListeners || []).forEach(function (cb) { try { cb(ev); } catch (e) {} });
  }
  globalThis.history = {
    get length() { return globalThis.__oxiHistoryEntries.length; },
    get state() { var e = cur(); return e ? e.state : null; },
    scrollRestoration: 'auto',
    pushState: function (state, unused, url) {
      var abs = url ? resolveUrl(url) : cur().url;
      globalThis.__oxiHistoryEntries = globalThis.__oxiHistoryEntries.slice(0, globalThis.__oxiHistoryIndex + 1);
      globalThis.__oxiHistoryEntries.push({ url: abs, state: state });
      globalThis.__oxiHistoryIndex = globalThis.__oxiHistoryEntries.length - 1;
    },
    replaceState: function (state, unused, url) {
      var abs = url ? resolveUrl(url) : cur().url;
      globalThis.__oxiHistoryEntries[globalThis.__oxiHistoryIndex] = { url: abs, state: state };
    },
    back: function () { globalThis.history.go(-1); },
    forward: function () { globalThis.history.go(1); },
    go: function (delta) {
      delta = (typeof delta === 'number') ? delta : 0;
      var ni = globalThis.__oxiHistoryIndex + delta;
      if (ni < 0) ni = 0;
      if (ni >= globalThis.__oxiHistoryEntries.length) ni = globalThis.__oxiHistoryEntries.length - 1;
      if (ni === globalThis.__oxiHistoryIndex) return;
      globalThis.__oxiHistoryIndex = ni;
      firePopstate();
    },
  };
  globalThis.addEventListener = globalThis.addEventListener || function (type, cb) {
    if (typeof cb !== 'function') return;
    if (type === 'popstate') (globalThis.__oxiPopstateListeners = globalThis.__oxiPopstateListeners || []).push(cb);
    var m = (globalThis.__oxiWinListeners = globalThis.__oxiWinListeners || {});
    (m[type] = m[type] || []).push(cb);
  };
  globalThis.removeEventListener = globalThis.removeEventListener || function (type, cb) {
    if (type === 'popstate' && globalThis.__oxiPopstateListeners) {
      globalThis.__oxiPopstateListeners = globalThis.__oxiPopstateListeners.filter(function (x) { return x !== cb; });
    }
    var m = globalThis.__oxiWinListeners;
    if (m && m[type]) { m[type] = m[type].filter(function (x) { return x !== cb; }); }
  };
  globalThis.dispatchEvent = globalThis.dispatchEvent || function (ev) {
    var t = (ev && typeof ev === 'object') ? ev.type : ev;
    var cbs = globalThis.__oxiWinListeners && globalThis.__oxiWinListeners[t];
    if (cbs) { for (var i = 0; i < cbs.length; i++) { try { cbs[i].call(globalThis, ev); } catch (e) {} } }
    var onprop = 'on' + t;
    if (typeof globalThis[onprop] === 'function') { try { globalThis[onprop].call(globalThis, ev); } catch (e) {} }
    return true;
  };
  // Mirror the event-target methods onto `window`. `globalThis` and `window`
  // are distinct objects here, so without these copies `window.addEventListener`
  // throws while `globalThis.addEventListener` works. Mirrors the
  // `matchMedia` pattern below (line ~10695).
  if (globalThis.window) {
    globalThis.window.addEventListener = globalThis.window.addEventListener || globalThis.addEventListener;
    globalThis.window.removeEventListener = globalThis.window.removeEventListener || globalThis.removeEventListener;
    globalThis.window.dispatchEvent = globalThis.window.dispatchEvent || globalThis.dispatchEvent;
  }
   // window.matchMedia — minimal MediaQueryList. 'matches' is derived for
  // common min/max-width queries against the viewport; other queries
  // (prefers-color-scheme, hover, ...) default to false. Many SPAs only need
  // matchMedia to EXIST and not throw (responsive/CSS-in-JS feature checks).
  (function () {
    // window.matchMedia — minimal MediaQueryList. 'matches' is derived for
    // common min/max-width queries against the viewport; other queries
    // (prefers-color-scheme, hover, ...) default to false. Many SPAs only
    // need matchMedia to EXIST and not throw (responsive/CSS-in-JS checks).
    // NOTE: `window` is a distinct object from globalThis here, so install on
    // both so `window.matchMedia` and bare `matchMedia` both resolve.
    var mm = function (query) {
      var q = String(query);
      var w = (globalThis.innerWidth || (globalThis.window && globalThis.window.innerWidth) || 1280);
      var min = /min-width:\s*(\d+)/i.exec(q);
      var max = /max-width:\s*(\d+)/i.exec(q);
      var m = false;
      if (min || max) {
        m = true;
        if (min && w < parseInt(min[1], 10)) m = false;
        if (max && w > parseInt(max[1], 10)) m = false;
      }
      return { media: q, matches: m, onchange: null,
        addListener: function () {}, removeListener: function () {},
        addEventListener: function () {}, removeEventListener: function () {},
        dispatchEvent: function () { return true; } };
    };
    globalThis.matchMedia = globalThis.matchMedia || mm;
    if (globalThis.window) { globalThis.window.matchMedia = globalThis.window.matchMedia || mm; }
  })();
  function fireHashchange(oldURL, newURL) {
    var ev = { type: 'hashchange', oldURL: oldURL || '', newURL: newURL || '', isTrusted: false };
    var cbs = globalThis.__oxiHashchangeListeners || [];
    for (var i = 0; i < cbs.length; i++) { try { cbs[i].call(globalThis, ev); } catch (e) {} }
  }
  function augment(loc) {
    if (!loc) return;
    try {
      loc.assign = function (url) { __oxiNavigate(resolveUrl(url)); };
      loc.replace = function (url) { __oxiNavigate(resolveUrl(url)); };
      loc.reload = function () { __oxiReload(); };
      Object.defineProperty(loc, 'href', {
        configurable: true, enumerable: true,
        get: function () { return cur().url; },
        set: function (v) { __oxiNavigate(resolveUrl(v)); },
      });
      // Fire `hashchange` when JS sets `location.hash`. Best-effort: only the
      // setter route fires it (real-browser fires it on history navigation
      // too, which we don't model here).
      var lastHash = '';
      try {
        var curLoc = new URL(cur().url || PAGE_URL);
        lastHash = curLoc.hash || '';
      } catch (e) {}
      Object.defineProperty(loc, 'hash', {
        configurable: true, enumerable: true,
        get: function () {
          var u;
          try { u = new URL(cur().url); } catch (e) { return ''; }
          return u.hash || '';
        },
        set: function (v) {
          var u;
          try { u = new URL(cur().url); } catch (e) { return; }
          var prev = u.href;
          u.hash = (v == null ? '' : String(v));
          // Update history entry without navigating the page.
          var top = cur();
          top.url = u.href;
          globalThis.__oxiHistoryEntries[globalThis.__oxiHistoryIndex] = top;
          fireHashchange(prev, u.href);
        },
      });
    } catch (e) {}
  }
  function makeLocationProxy(target) {
    return new Proxy(target || {}, {
      get: function (t, p) {
        if (p === 'hash') {
          var u; try { u = new URL(cur().url); } catch (e) { return ''; }
          return u.hash || '';
        }
        var v = Reflect.get(t, p, t);
        return typeof v === 'function' ? v.bind(t) : v;
      },
      set: function (t, p, val) {
        if (p === 'hash') {
          var prev = (cur() && cur().url) || PAGE_URL;
          var curStr = String(prev);
          var hashStr = (val == null ? '' : String(val));
          if (hashStr.charAt(0) !== '#') hashStr = '#' + hashStr;
          var hashIdx = curStr.indexOf('#');
          var next = (hashIdx >= 0 ? curStr.substring(0, hashIdx) : curStr) + hashStr;
          var top = cur();
          top.url = next;
          if (globalThis.__oxiHistoryEntries) globalThis.__oxiHistoryEntries[globalThis.__oxiHistoryIndex] = top;
          fireHashchange(curStr, next);
          return true;
        }
        return Reflect.set(t, p, val, t);
      },
    });
  }
  function proxyLocation(loc) {
    if (!loc) return;
    try { return makeLocationProxy(loc); } catch (e) { return loc; }
  }
  augment(globalThis.location);
  if (globalThis.window && globalThis.window.location) augment(globalThis.window.location);
  // Replace `window.location` with a Proxy so setting `window.location.hash`
  // (and reading it) routes through our handler. The host Location's native
  // setter is non-configurable, so `Object.defineProperty` can't intercept
  // `hash =` — a Proxy around the host object is the simplest way to keep
  // the standard `location.hash = '#x'` syntax working.
  try {
    var wloc = globalThis.window && globalThis.window.location;
    if (wloc) globalThis.window = Object.assign({}, globalThis.window, { location: makeLocationProxy(wloc) });
  } catch (e) {}
  if (globalThis.window && globalThis.window.location) augment(globalThis.window.location);
  // Register a dedicated hashchange listener slot (regular
  // globalThis.addEventListener('hashchange', cb) also fires via the mirror
  // install above because the listener registry dispatches by `ev.type`).
  globalThis.__oxiHashchangeListeners = globalThis.__oxiHashchangeListeners || [];
  if (typeof globalThis.addEventListener === 'function') {
    // Hook into the addEventListener registry so the existing globalThis mirror
    // also lands hashchange callbacks here. The current bootstrap writes
    // listeners into __oxiWinListeners[type]; we double-tap into the same slot.
    var __oxiAddEv = globalThis.addEventListener;
    if (typeof __oxiAddEv === 'function' && !globalThis.__oxiHashAddTap) {
      globalThis.__oxiHashAddTap = true;
      globalThis.addEventListener = function (type, cb) {
        if (type === 'hashchange' && typeof cb === 'function') {
          (globalThis.__oxiHashchangeListeners = globalThis.__oxiHashchangeListeners || []).push(cb);
          return;
        }
        return __oxiAddEv.call(this, type, cb);
      };
      if (globalThis.window) globalThis.window.addEventListener = globalThis.addEventListener;
    }
  }
  // Fire an initial hashchange on first load if the page URL has a fragment.
  if (!globalThis.__oxiHashchangeInit) {
    globalThis.__oxiHashchangeInit = true;
    try {
      var u0 = new URL(PAGE_URL);
      if (u0.hash && u0.hash !== '') {
        // oldURL is the URL without the fragment (best-effort).
        var bare = u0.href.split('#')[0];
        fireHashchange(bare, u0.href);
      }
    } catch (e) {}
  }
  // page's base URL (real-browser behavior). The native binding expects an
  // absolute URL — passing '/api/x' yields a TypeError. resolveUrl joins
  // relative refs against PAGE_URL (origin-only for leading-`/`, doc
  // directory for relative).
  if (typeof globalThis.fetch === 'function') {
    var __oxiFetch = globalThis.fetch;
    var __oxiResolveForFetch = function (u) { try { return resolveUrl(u); } catch (_) { return u; } };
    globalThis.fetch = function (input, init) {
      if (input && typeof input === 'string') input = __oxiResolveForFetch(input);
      else if (input && typeof input === 'object' && typeof input.url === 'string')
        input = new (input.constructor || URL)(__oxiResolveForFetch(input.url));
      return __oxiFetch.call(globalThis, input, init);
    };
    if (globalThis.window) { globalThis.window.fetch = globalThis.fetch; }
  }
 })();
"#;
}

/// Simple time helper for performance.now().
mod js_sys_helpers {
    pub fn now_ms() -> f64 {
        use std::time::SystemTime;
        let duration = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        duration.as_millis() as f64
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Layout-based hit test (Phase 7): the deepest **element** node whose
/// laid-out box contains the viewport point `(x, y)`, or `None`.
///
/// Uses the `RenderDocument`'s Taffy layout boxes (`node_layout_rect`). A node
/// with a non-positive box is skipped (not painted). Children are searched
/// before the parent so the most specific (deepest) element wins. Falls back to
/// the parent element when the deepest match is a text node.
fn hit_test_element(doc: &RenderDocument, snap: &DomSnapshot, x: f64, y: f64) -> Option<u32> {
    fn is_element(snap: &DomSnapshot, id: u32) -> bool {
        snap.nodes
            .get(&id)
            .is_some_and(|n| n.node_type == 1 || !n.tag.is_empty())
    }

    fn contains(doc: &RenderDocument, snap: &DomSnapshot, id: u32, x: f64, y: f64) -> Option<u32> {
        let (rx, ry, rw, rh) = doc.node_layout_rect(id as usize);
        if rw <= 0.0 || rh <= 0.0 {
            return None;
        }
        if !(x >= rx && x < rx + rw && y >= ry && y < ry + rh) {
            return None;
        }
        // Recurse into children first — deepest match wins.
        if let Some(node) = snap.nodes.get(&id) {
            for &child_id in &node.children {
                if let Some(deep) = contains(doc, snap, child_id, x, y) {
                    return Some(deep);
                }
            }
        }
        Some(id)
    }

    let raw = snap
        .body_id
        .and_then(|bid| contains(doc, snap, bid, x, y))?;
    if is_element(snap, raw) {
        Some(raw)
    } else {
        // Text node — walk up to the enclosing element.
        snap.nodes
            .get(&raw)
            .and_then(|n| n.parent)
            .filter(|&p| is_element(snap, p))
    }
}

/// Build the `navigator.geolocation` object backed by the Emulation override.
fn build_geolocation_object(ctx: &mut Context) -> JsValue {
    let get_current_position = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let success = args.first().cloned().unwrap_or(JsValue::undefined());
            let error = args.get(1).cloned().unwrap_or(JsValue::undefined());
            if let Some((lat, lon, acc)) = geolocation_override() {
                let coords = boa_engine::object::ObjectInitializer::new(ctx)
                    .property(js_string!("latitude"), JsValue::from(lat), Attribute::all())
                    .property(
                        js_string!("longitude"),
                        JsValue::from(lon),
                        Attribute::all(),
                    )
                    .property(js_string!("accuracy"), JsValue::from(acc), Attribute::all())
                    .property(js_string!("altitude"), JsValue::null(), Attribute::all())
                    .property(
                        js_string!("altitudeAccuracy"),
                        JsValue::null(),
                        Attribute::all(),
                    )
                    .property(js_string!("heading"), JsValue::null(), Attribute::all())
                    .property(js_string!("speed"), JsValue::null(), Attribute::all())
                    .build();
                let pos = boa_engine::object::ObjectInitializer::new(ctx)
                    .property(
                        js_string!("coords"),
                        JsValue::from(coords),
                        Attribute::all(),
                    )
                    .property(
                        js_string!("timestamp"),
                        JsValue::from(now_ms()),
                        Attribute::all(),
                    )
                    .build();
                if let Some(cb) = success.as_object() {
                    let _ = cb.call(&JsValue::undefined(), &[JsValue::from(pos)], ctx);
                }
            } else if let Some(cb) = error.as_object() {
                let err = boa_engine::object::ObjectInitializer::new(ctx)
                    .property(js_string!("code"), JsValue::from(2u8), Attribute::all())
                    .property(
                        js_string!("message"),
                        JsValue::from(js_string!("Position unavailable")),
                        Attribute::all(),
                    )
                    .build();
                let _ = cb.call(&JsValue::undefined(), &[JsValue::from(err)], ctx);
            }
            Ok(JsValue::undefined())
        })
    };
    // watchPosition returns a (fake) watch id; no periodic updates in headless mode.
    let watch_position =
        unsafe { NativeFunction::from_closure(move |_this, _args, _ctx| Ok(JsValue::from(0i32))) };
    let clear_watch =
        unsafe { NativeFunction::from_closure(move |_this, _args, _ctx| Ok(JsValue::undefined())) };
    boa_engine::object::ObjectInitializer::new(ctx)
        .function(get_current_position, js_string!("getCurrentPosition"), 3)
        .function(watch_position, js_string!("watchPosition"), 3)
        .function(clear_watch, js_string!("clearWatch"), 1)
        .build()
        .into()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Frame;
    use crate::js::dom_snapshot::{ExecuteTiming, ScriptKind};
    use url::Url;

    // --- Render façades (RenderDocument on the JS thread) ---

    #[tokio::test]
    async fn test_set_document_then_capture_png() {
        // set_document builds a RenderDocument on the JS thread; capture_png
        // returns a valid PNG of the rendered HTML — no JS involved.
        let mut rt = JsRuntime::new();
        let html = concat!(
            "<!DOCTYPE html><html><head><style>",
            "body { margin: 0; } .box { width: 40px; height: 40px; background: red; }",
            "</style></head><body><div class=\"box\"></div></body></html>"
        );
        rt.set_document(html, Some("https://example.com/"), (400, 300))
            .await
            .expect("set_document should build the render doc");

        let png = rt
            .capture_png(CaptureOpts {
                full_page: true,
                ..Default::default()
            })
            .await
            .expect("capture_png should render a PNG");

        // PNG magic header.
        assert!(png.len() > 8, "PNG data should be more than 8 bytes");
        assert_eq!(&png[..8], &[137, 80, 78, 71, 13, 10, 26, 10]);

        // Decode and confirm real (non-blank) CSS-rendered content.
        let img = image::load_from_memory(&png).expect("decode captured png");
        let rgba = img.to_rgba8();
        let has_red = rgba.pixels().any(|p| p[0] > 200 && p[1] < 80 && p[2] < 80);
        assert!(has_red, "the red .box should be rendered");
    }

    // Regression test for GitHub issue #2: appending a DocumentFragment must
    // splice its children into the document and leave the JS thread alive.
    #[tokio::test]
    async fn test_document_fragment_append_child_splices_and_survives() {
        let mut rt = JsRuntime::new();
        rt.set_document(
            "<html><body><p id=\"out\">x</p></body></html>",
            Some("https://example.com/"),
            (400, 300),
        )
        .await
        .unwrap();

        // The exact 3-line repro from the issue.
        let insert = rt
            .evaluate(
                "var f = document.createDocumentFragment(); \
                 f.appendChild(document.createElement('i')); \
                 document.body.appendChild(f); 'inserted'",
            )
            .await
            .expect("fragment insert must not error");
        assert_eq!(insert.value, Some(Value::from("inserted")));

        // Splice semantics: children land in the document, fragment empties.
        let spliced = rt
            .evaluate(
                "JSON.stringify({ \
                    found: document.querySelector('i') !== null, \
                    fragEmpty: f.childNodes.length === 0 })",
            )
            .await
            .expect("JS thread must survive fragment appendChild");
        assert_eq!(
            spliced.value,
            Some(Value::from("{\"found\":true,\"fragEmpty\":true}"))
        );

        // And the thread is still healthy afterwards.
        rt.evaluate("1 + 1").await.expect("thread alive");
    }

    // Appending a fragment into a fragment splices (never nests); the outer
    // fragment then inserts the whole set in one call.
    #[tokio::test]
    async fn test_nested_fragment_append_flattens() {
        let mut rt = JsRuntime::new();
        rt.set_document(
            "<html><body></body></html>",
            Some("https://example.com/"),
            (400, 300),
        )
        .await
        .unwrap();

        let r = rt
            .evaluate(
                "var outer = document.createDocumentFragment(); \
                 var inner = document.createDocumentFragment(); \
                 inner.appendChild(document.createElement('b')); \
                 outer.appendChild(inner); \
                 outer.appendChild(document.createTextNode('t')); \
                 document.body.appendChild(outer); \
                 JSON.stringify({ innerEmpty: inner.childNodes.length === 0, \
                                  outerEmpty: outer.childNodes.length === 0, \
                                  foundB: document.querySelector('b') !== null })",
            )
            .await
            .expect("nested fragment append must not error");
        assert_eq!(
            r.value,
            Some(Value::from(
                "{\"innerEmpty\":true,\"outerEmpty\":true,\"foundB\":true}"
            ))
        );
    }

    // Regression for issue #2's full-page form: an inline page script that
    // appends a DocumentFragment must succeed during navigation (the old
    // stub crashed here too, killing the thread before any evaluate ran).
    #[tokio::test]
    async fn test_nav_script_fragment_append_survives() {
        let mut rt = JsRuntime::new();
        rt.set_document_with_scripts(
            NAV_HTML,
            Some("https://example.com/"),
            (400, 300),
            vec![classic(
                "var f = document.createDocumentFragment(); \
                 f.appendChild(document.createElement('i')); \
                 document.body.appendChild(f); window.__frag = 'ok';",
            )],
        )
        .await
        .expect("navigation with fragment append must not fail");
        let r = rt.evaluate("window.__frag").await.expect("thread alive");
        assert_eq!(r.value, Some(Value::from("ok")));
    }
    #[tokio::test]
    async fn test_capture_without_document_errors() {
        let mut rt = JsRuntime::new();
        let err = rt.capture_png(CaptureOpts::default()).await.unwrap_err();
        assert!(
            matches!(err, CoreError::ScreenshotError(_)),
            "capture without a document should be a screenshot error, got {err:?}"
        );
    }

    // --- Navigation script execution (Phase 1 keystone) ---

    fn classic(source: &str) -> ScriptSource {
        ScriptSource {
            source: source.to_string(),
            src_url: None,
            kind: ScriptKind::Classic,
            execute: ExecuteTiming::Defer,
        }
    }

    const NAV_HTML: &str =
        "<!DOCTYPE html><html><head></head><body><div id=\"app\"></div></body></html>";

    #[tokio::test]
    async fn test_nav_script_inline_executes() {
        let mut rt = JsRuntime::new();
        rt.set_document_with_scripts(
            NAV_HTML,
            Some("https://example.com/"),
            (400, 300),
            vec![classic("window.__t = 1;")],
        )
        .await
        .expect("set_document_with_scripts");
        let r = rt.evaluate("window.__t").await.expect("evaluate");
        assert_eq!(r.value, Some(serde_json::json!(1)), "inline script ran");
    }

    #[tokio::test]
    async fn test_nav_scripts_run_in_document_order() {
        let mut rt = JsRuntime::new();
        rt.set_document_with_scripts(
            NAV_HTML,
            Some("https://example.com/"),
            (400, 300),
            vec![
                classic("window.__order = (window.__order || '') + 'A';"),
                classic("window.__order = (window.__order || '') + 'B';"),
                classic("window.__order = (window.__order || '') + 'C';"),
            ],
        )
        .await
        .expect("set_document_with_scripts");
        let r = rt.evaluate("window.__order").await.expect("evaluate");
        assert_eq!(r.value, Some(serde_json::json!("ABC")), "ordered execution");
    }

    #[tokio::test]
    async fn test_nav_script_throw_does_not_abort_sibling() {
        let mut rt = JsRuntime::new();
        rt.set_document_with_scripts(
            NAV_HTML,
            Some("https://example.com/"),
            (400, 300),
            vec![
                classic("throw new Error('boom');"),
                classic("window.__survived = 1;"),
            ],
        )
        .await
        .expect("set_document_with_scripts");
        let r = rt.evaluate("window.__survived").await.expect("evaluate");
        assert_eq!(
            r.value,
            Some(serde_json::json!(1)),
            "later script ran despite throw"
        );
    }

    #[tokio::test]
    async fn test_nav_script_dom_content_loaded_fires() {
        let mut rt = JsRuntime::new();
        rt.set_document_with_scripts(
            NAV_HTML,
            Some("https://example.com/"),
            (400, 300),
            vec![classic(
                "document.addEventListener('DOMContentLoaded', function () {\
                     window.__dcl = 1;\
                 });",
            )],
        )
        .await
        .expect("set_document_with_scripts");
        let r = rt.evaluate("window.__dcl").await.expect("evaluate");
        assert_eq!(
            r.value,
            Some(serde_json::json!(1)),
            "DOMContentLoaded fired"
        );
    }

    #[tokio::test]
    async fn test_nav_script_ready_state_complete() {
        let mut rt = JsRuntime::new();
        rt.set_document_with_scripts(
            NAV_HTML,
            Some("https://example.com/"),
            (400, 300),
            vec![classic("window.__rs = document.readyState;")],
        )
        .await
        .expect("set_document_with_scripts");
        // The script captured readyState during execution ("interactive"); the
        // post-nav readyState must be "complete".
        let r = rt.evaluate("document.readyState").await.expect("evaluate");
        assert_eq!(
            r.value,
            Some(serde_json::json!("complete")),
            "readyState complete"
        );
    }

    #[tokio::test]
    async fn test_nav_script_settimeout_settles() {
        // A 50 ms timer must fire during the bootstrap pump (not require a
        // later evaluate()), proving the pump waits for due timers.
        let mut rt = JsRuntime::new();
        rt.set_document_with_scripts(
            NAV_HTML,
            Some("https://example.com/"),
            (400, 300),
            vec![classic(
                "setTimeout(function () { window.__to = 'fired'; }, 50);",
            )],
        )
        .await
        .expect("set_document_with_scripts");
        let r = rt.evaluate("window.__to").await.expect("evaluate");
        assert_eq!(
            r.value,
            Some(serde_json::json!("fired")),
            "setTimeout fired in pump"
        );
    }

    #[tokio::test]
    async fn test_nav_script_heavy_loop_runs_under_dedicated_limits() {
        // A loop exceeding the default evaluate() cap (100 000) must still run
        // under the nav-script limits — proving real bundles are not silently
        // skipped. Count is bounded by boa's interpreter throughput (a tight
        // loop runs at ~tens of k iter/s JIT-less); 250 000 is 2.5x the eval
        // cap and completes in a few seconds. NOTE: a single compute-bound
        // script is uninterruptible mid-eval except by the loop counter, so
        // pathological tight loops are a Phase 2 watchdog concern.
        let mut rt = JsRuntime::new();
        let t0 = std::time::Instant::now();
        rt.set_document_with_scripts(
            NAV_HTML,
            Some("https://example.com/"),
            (400, 300),
            vec![classic(
                "var s = 0; for (var i = 0; i < 250000; i++) { s++; } window.__heavy = s;",
            )],
        )
        .await
        .expect("set_document_with_scripts");
        let elapsed = t0.elapsed();
        eprintln!("250k-loop nav-script elapsed: {elapsed:?}");
        let r = rt.evaluate("window.__heavy").await.expect("evaluate");
        assert_eq!(
            r.value,
            Some(serde_json::json!(250_000)),
            "heavy loop completed (nav limits high enough), elapsed {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn test_match_media_min_max_width_and_default() {
        // matchMedia must exist and not throw (responsive/CSS-in-JS feature
        // checks call it at module init). min/max-width derive from viewport;
        // other queries default to matches=false.
        let mut rt = JsRuntime::new();
        rt.set_document("<html><body></body></html>", None, (1280, 720))
            .await
            .unwrap();
        let r = rt
            .evaluate(
                "({\
                   has: typeof window.matchMedia === 'function',\
                   small: window.matchMedia('(min-width: 100px)').matches,\
                   huge: window.matchMedia('(min-width: 99999px)').matches,\
                   maxok: window.matchMedia('(max-width: 5000px)').matches,\
                   dark: window.matchMedia('(prefers-color-scheme: dark)').matches\
                 })",
            )
            .await
            .expect("evaluate");
        if r.value.is_none() {
            panic!("matchMedia eval failed: {:?}", r.exception);
        }
        let obj = r.value.expect("json object");
        assert_eq!(
            obj["has"],
            serde_json::json!(true),
            "matchMedia is a function"
        );
        assert_eq!(obj["small"], serde_json::json!(true), "1280 >= 100");
        assert_eq!(obj["huge"], serde_json::json!(false), "1280 < 99999");
        assert_eq!(obj["maxok"], serde_json::json!(true), "1280 <= 5000");
        assert_eq!(
            obj["dark"],
            serde_json::json!(false),
            "non-width query defaults to false"
        );
    }

    #[tokio::test]
    async fn test_query_selector_all_returns_nodes() {
        let mut rt = JsRuntime::new();
        let html = "<!DOCTYPE html><html><body>\
                    <div class=\"item\" data-n=\"1\">one</div>\
                    <div class=\"item\" data-n=\"2\">two</div>\
                    <span class=\"item\">three</span>\
                    </body></html>";
        rt.set_document(html, None, (400, 300)).await.unwrap();

        let nodes = rt.query_selector_all(".item").await.unwrap();
        assert_eq!(nodes.len(), 3, "three .item elements");
        assert_eq!(nodes[0].tag.as_deref(), Some("div"));
        assert_eq!(nodes[0].text, "one");
        assert_eq!(
            nodes[0]
                .attributes
                .iter()
                .find(|(k, _)| k == "data-n")
                .map(|(_, v)| v.as_str()),
            Some("1"),
            "first item data-n attribute"
        );
        assert_eq!(nodes[2].tag.as_deref(), Some("span"));
    }

    #[tokio::test]
    async fn test_js_create_element_reflected_in_capture() {
        // JS mutates the RenderDocument directly (Task 2): createElement, inline
        // style, appendChild — then capture_png renders the live DOM.
        let mut rt = JsRuntime::new();
        let html = "<!DOCTYPE html><html><head></head><body></body></html>";
        rt.set_document(html, Some("https://example.com/"), (400, 300))
            .await
            .unwrap();

        rt.execute(
            "var el = document.createElement('div');\
             el.style.setProperty('background-color', 'red');\
             el.style.setProperty('width', '50px');\
             el.style.setProperty('height', '50px');\
             document.body.appendChild(el);",
        )
        .await
        .expect("JS createElement+appendChild should succeed");

        let png = rt
            .capture_png(CaptureOpts {
                full_page: true,
                ..Default::default()
            })
            .await
            .expect("capture_png should reflect the JS-created element");

        assert_eq!(&png[..8], &[137, 80, 78, 71, 13, 10, 26, 10]);
        let img = image::load_from_memory(&png).expect("decode captured png");
        let has_red = img
            .to_rgba8()
            .pixels()
            .any(|p| p[0] > 200 && p[1] < 80 && p[2] < 80);
        assert!(has_red, "the JS-created red div should render in capture");
    }

    #[tokio::test]
    async fn test_js_query_selector_reads_render_document() {
        // document.querySelector reads from the RenderDocument when set.
        let mut rt = JsRuntime::new();
        let html = "<!DOCTYPE html><html><body>\
                    <div id=\"target\" class=\"x\">hello</div>\
                    </body></html>";
        rt.set_document(html, None, (400, 300)).await.unwrap();

        let res = rt
            .evaluate("document.getElementById('target').textContent")
            .await
            .unwrap();
        assert!(res.is_ok(), "getElementById should find the node");
        assert_eq!(res.value, Some(Value::String("hello".into())));

        let res = rt
            .evaluate("document.querySelectorAll('.x').length")
            .await
            .unwrap();
        assert_eq!(res.value, Some(Value::Number(1.into())));
    }

    // --- Basic types ---

    #[tokio::test]
    async fn test_evaluate_literal() {
        let mut rt = JsRuntime::new();
        let result = rt.evaluate("42").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::Number(42.into())));
    }

    #[tokio::test]
    async fn test_evaluate_string() {
        let mut rt = JsRuntime::new();
        let result = rt.evaluate("\"hello\"").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::String("hello".into())));
    }

    #[tokio::test]
    async fn test_evaluate_boolean() {
        let mut rt = JsRuntime::new();
        let result = rt.evaluate("true").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::Bool(true)));
    }

    #[tokio::test]
    async fn test_evaluate_null() {
        let mut rt = JsRuntime::new();
        let result = rt.evaluate("null").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::Null));
    }

    #[tokio::test]
    async fn test_evaluate_undefined() {
        let mut rt = JsRuntime::new();
        let result = rt.evaluate("undefined").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::Null));
    }

    // --- Arithmetic & expressions ---

    #[tokio::test]
    async fn test_evaluate_arithmetic() {
        let mut rt = JsRuntime::new();
        let result = rt.evaluate("2 + 3 * 4").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::Number(14.into())));
    }

    #[tokio::test]
    async fn test_evaluate_expression() {
        let mut rt = JsRuntime::new();
        let result = rt.evaluate("'hello ' + 'world'").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::String("hello world".into())));
    }

    // --- Functions ---

    #[tokio::test]
    async fn test_evaluate_function() {
        let mut rt = JsRuntime::new();
        let result = rt
            .evaluate("function add(a, b) { return a + b; } add(1, 2)")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::Number(3.into())));
    }

    // --- Console ---

    #[tokio::test]
    async fn test_console_log_capture() {
        let mut rt = JsRuntime::new();
        let result = rt.evaluate("console.log('Hello, world!')").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.console_output, vec!["Hello, world!"]);
    }

    #[tokio::test]
    async fn test_console_pushes_core_event() {
        // console.log should mirror to the CoreEvent sink when one is attached.
        let mut rt = JsRuntime::new();
        let (tx, rx) = std::sync::mpsc::channel::<CoreEvent>();
        rt.set_event_sink(tx);
        let result = rt
            .evaluate("console.warn('watch', 42, true, null, {a:1})")
            .await
            .unwrap();
        assert!(result.is_ok());
        let ev = rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("expected a CoreEvent::Console");
        match ev {
            CoreEvent::Console { level, args, .. } => {
                assert_eq!(level, ConsoleLevel::Warn);
                assert_eq!(args.len(), 5, "5 console args: {args:?}");
                // Typed RemoteObjects: string / number / boolean / null / object.
                assert!(matches!(args[0], ConsoleArg::String(ref s) if s == "watch"));
                assert!(
                    matches!(args[1], ConsoleArg::Number(n) if (n - 42.0).abs() < 1e-9),
                    "numeric arg preserved as Number, got {:?}",
                    args[1]
                );
                assert!(matches!(args[2], ConsoleArg::Boolean(true)));
                assert!(matches!(args[3], ConsoleArg::Null));
                assert!(
                    matches!(args[4], ConsoleArg::Object { ref class_name, .. } if class_name == "Object"),
                    "object arg classified with className, got {:?}",
                    args[4]
                );
            }
            other => panic!("expected Console, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_exception_pushes_core_event() {
        let mut rt = JsRuntime::new();
        let (tx, rx) = std::sync::mpsc::channel::<CoreEvent>();
        rt.set_event_sink(tx);
        // Throwing surfaces as an exception result and a CoreEvent::Exception.
        let _ = rt.evaluate("throw new Error('boom')").await;
        let ev = rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("expected a CoreEvent::Exception");
        assert!(
            matches!(
                ev,
                CoreEvent::Exception { ref message, ref name, .. }
                    if message.contains("boom") && name == "Error"
            ),
            "expected Exception 'boom' with name 'Error', got {ev:?}"
        );
    }

    #[tokio::test]
    async fn test_console_log_multiple_args() {
        let mut rt = JsRuntime::new();
        let result = rt.evaluate("console.log('a', 1, true)").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.console_output, vec!["a 1 true"]);
    }

    #[tokio::test]
    async fn test_console_warn_error_info() {
        let mut rt = JsRuntime::new();
        let result = rt
            .evaluate("console.warn('w'); console.error('e'); console.info('i')")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.console_output.len(), 3);
    }

    #[tokio::test]
    async fn test_console_log_with_expressions() {
        let mut rt = JsRuntime::new();
        let result = rt
            .evaluate("let x = 10; console.log('x is', x)")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.console_output, vec!["x is 10"]);
    }

    #[tokio::test]
    async fn test_multiple_console_logs() {
        let mut rt = JsRuntime::new();
        let result = rt
            .evaluate("console.log('line1'); console.log('line2')")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.console_output.len(), 2);
    }

    // --- Errors ---

    #[tokio::test]
    async fn test_evaluate_error() {
        let mut rt = JsRuntime::new();
        let result = rt.evaluate("throw new Error('oops')").await.unwrap();
        assert!(!result.is_ok());
        let msg = result.exception.unwrap();
        assert!(msg.contains("Error"), "msg: {}", msg);
        assert!(msg.contains("oops"), "msg: {}", msg);
    }

    #[tokio::test]
    async fn test_syntax_error() {
        let mut rt = JsRuntime::new();
        let result = rt.evaluate("function {").await.unwrap();
        assert!(!result.is_ok());
        assert!(result.exception.unwrap().to_lowercase().contains("syntax"));
    }

    #[tokio::test]
    async fn test_type_error() {
        let mut rt = JsRuntime::new();
        let result = rt.evaluate("undefined.foo").await.unwrap();
        assert!(!result.is_ok());
        assert!(result.exception.unwrap().to_lowercase().contains("type"));
    }

    // --- Globals ---

    #[tokio::test]
    async fn test_global_math() {
        let mut rt = JsRuntime::new();
        let result = rt.evaluate("Math.PI").await.unwrap();
        assert!(result.is_ok());
        let pi = result.value.unwrap().as_f64().unwrap();
        assert!((pi - std::f64::consts::PI).abs() < 0.0001);
    }

    #[tokio::test]
    async fn test_set_global_string() {
        let mut rt = JsRuntime::new();
        rt.set_global("myVar", Value::String("hello".into()));
        let result = rt.evaluate("myVar").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::String("hello".into())));
    }

    #[tokio::test]
    async fn test_set_global_number() {
        let mut rt = JsRuntime::new();
        rt.set_global("count", Value::Number(42.into()));
        let result = rt.evaluate("count + 8").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value.unwrap().as_f64().unwrap(), 50.0);
    }

    #[tokio::test]
    async fn test_set_global_object() {
        let mut rt = JsRuntime::new();
        rt.set_global("cfg", serde_json::json!({ "name": "test" }));
        let result = rt.evaluate("cfg.name").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::String("test".into())));
    }

    // --- Objects & Arrays ---

    #[tokio::test]
    async fn test_object_literal() {
        let mut rt = JsRuntime::new();
        let result = rt.evaluate("({ a: 1, b: 'hi' })").await.unwrap();
        assert!(result.is_ok());
        let val = result.value.unwrap();
        assert!(val.is_object());
        let map = val.as_object().unwrap();
        assert_eq!(map.get("a"), Some(&Value::Number(1.into())));
    }

    #[tokio::test]
    async fn test_array_literal() {
        let mut rt = JsRuntime::new();
        let result = rt.evaluate("[1, 2, 3]").await.unwrap();
        assert!(result.is_ok());
        let arr = result.value.unwrap().as_array().unwrap().clone();
        assert_eq!(arr.len(), 3);
    }

    #[tokio::test]
    async fn test_array_map() {
        let mut rt = JsRuntime::new();
        let result = rt.evaluate("[1, 2, 3].map(x => x * 2)").await.unwrap();
        assert!(result.is_ok());
        let arr = result.value.unwrap().as_array().unwrap().clone();
        assert_eq!(arr[0], Value::Number(2.into()));
        assert_eq!(arr[2], Value::Number(6.into()));
    }

    #[tokio::test]
    async fn test_json_stringify() {
        let mut rt = JsRuntime::new();
        let result = rt.evaluate("JSON.stringify({x: 1})").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::String("{\"x\":1}".to_string())));
    }

    #[tokio::test]
    async fn test_json_parse() {
        let mut rt = JsRuntime::new();
        let result = rt.evaluate("JSON.parse('{\"a\": 1}').a").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::Number(1.into())));
    }

    // --- JS features ---

    #[tokio::test]
    async fn test_template_literal() {
        let mut rt = JsRuntime::new();
        let result = rt.evaluate("`hello ${1 + 2}`").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::String("hello 3".into())));
    }

    #[tokio::test]
    async fn test_arrow_function() {
        let mut rt = JsRuntime::new();
        let result = rt.evaluate("const sq = x => x * x; sq(5)").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::Number(25.into())));
    }

    #[tokio::test]
    async fn test_try_catch() {
        let mut rt = JsRuntime::new();
        let result = rt
            .evaluate("try { throw 'oops'; } catch(e) { 'caught: ' + e }")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::String("caught: oops".into())));
    }

    #[tokio::test]
    async fn test_class() {
        let mut rt = JsRuntime::new();
        let result = rt
            .evaluate("class Foo { constructor(x) { this.x = x; } getX() { return this.x; } } new Foo(42).getX()")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::Number(42.into())));
    }

    #[tokio::test]
    async fn test_destructuring() {
        let mut rt = JsRuntime::new();
        let result = rt
            .evaluate("const {a, b} = {a: 1, b: 2}; a + b")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::Number(3.into())));
    }

    #[tokio::test]
    async fn test_regex() {
        let mut rt = JsRuntime::new();
        let result = rt.evaluate("/hello/.test('hello world')").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::Bool(true)));
    }

    #[tokio::test]
    async fn test_for_loop() {
        let mut rt = JsRuntime::new();
        let result = rt
            .evaluate("let sum = 0; for (let i = 1; i <= 10; i++) sum += i; sum")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::Number(55.into())));
    }

    #[tokio::test]
    async fn test_array_reduce() {
        let mut rt = JsRuntime::new();
        let result = rt
            .evaluate("[1,2,3,4,5].reduce((a,x) => a+x, 0)")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::Number(15.into())));
    }

    // ========================================
    // State persistence tests
    // ========================================

    #[tokio::test]
    async fn test_state_persists_across_evals() {
        let mut rt = JsRuntime::new();
        rt.evaluate("let x = 42").await.unwrap();
        let result = rt.evaluate("x + 8").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value.unwrap().as_f64().unwrap(), 50.0);
    }

    #[tokio::test]
    async fn test_function_persists() {
        let mut rt = JsRuntime::new();
        rt.evaluate("function add(a, b) { return a + b; }")
            .await
            .unwrap();
        let result = rt.evaluate("add(3, 4)").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::Number(7.into())));
    }

    #[tokio::test]
    async fn test_var_persists_across_evals() {
        let mut rt = JsRuntime::new();
        rt.evaluate("var greeting = 'hello'").await.unwrap();
        let result = rt.evaluate("greeting + ' world'").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::String("hello world".into())));
    }

    #[tokio::test]
    async fn test_closure_state_persists() {
        let mut rt = JsRuntime::new();
        rt.evaluate("const counter = (function() { let n = 0; return () => ++n; })()")
            .await
            .unwrap();

        let r1 = rt.evaluate("counter()").await.unwrap();
        assert_eq!(r1.value, Some(Value::Number(1.into())));

        let r2 = rt.evaluate("counter()").await.unwrap();
        assert_eq!(r2.value, Some(Value::Number(2.into())));

        let r3 = rt.evaluate("counter()").await.unwrap();
        assert_eq!(r3.value, Some(Value::Number(3.into())));
    }

    #[tokio::test]
    async fn test_set_global_persists_in_js_state() {
        let mut rt = JsRuntime::new();
        rt.set_global("baseUrl", Value::String("https://example.com".into()));
        rt.evaluate("let path = '/api'").await.unwrap();
        let result = rt.evaluate("baseUrl + path").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(
            result.value,
            Some(Value::String("https://example.com/api".into()))
        );
    }

    // ========================================
    // DOM snapshot + document object tests
    // ========================================

    async fn make_frame(html: &str) -> Frame {
        let url = Url::parse("https://example.com").unwrap();
        Frame::from_html(url, html).await.unwrap()
    }
    #[tokio::test]
    async fn test_document_title_no_snapshot() {
        let mut rt = JsRuntime::new();
        let result = rt.evaluate("document.title").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::String(String::new())));
    }

    #[tokio::test]
    async fn test_document_title_with_snapshot() {
        let mut rt = JsRuntime::new();
        let html = "<html><head><title>My Page</title></head><body><p>Hello</p></body></html>";
        let frame = make_frame(html).await;
        let snapshot = DomSnapshot::from_frame(&frame);
        rt.set_dom_snapshot(Some(snapshot));

        let result = rt.evaluate("document.title").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::String("My Page".into())));
    }

    #[tokio::test]
    async fn test_document_url() {
        let mut rt = JsRuntime::new();
        let html = "<html><body></body></html>";
        let frame = make_frame(html).await;
        let snapshot = DomSnapshot::from_frame(&frame);
        rt.set_dom_snapshot(Some(snapshot));

        let result = rt.evaluate("document.URL").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(
            result.value,
            Some(Value::String("https://example.com/".into()))
        );
    }

    #[tokio::test]
    async fn test_document_query_selector() {
        let mut rt = JsRuntime::new();
        let html =
            r#"<html><body><p class="intro">Hello</p><a href="/link">click</a></body></html>"#;
        let frame = make_frame(html).await;
        let snapshot = DomSnapshot::from_frame(&frame);
        rt.set_dom_snapshot(Some(snapshot));

        let result = rt
            .evaluate("document.querySelector('a').tagName")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::String("A".into())));
    }

    #[tokio::test]
    async fn test_document_query_selector_not_found() {
        let mut rt = JsRuntime::new();
        let html = "<html><body><p>Hello</p></body></html>";
        let frame = make_frame(html).await;
        let snapshot = DomSnapshot::from_frame(&frame);
        rt.set_dom_snapshot(Some(snapshot));

        let result = rt
            .evaluate("document.querySelector('video')")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::Null));
    }

    #[tokio::test]
    async fn test_element_query_selector() {
        let mut rt = JsRuntime::new();
        let html = r#"<html><body><div><p class="intro">Hello</p><a href="/link">click</a></div><a href="/other">other</a></body></html>"#;
        let frame = make_frame(html).await;
        let snapshot = DomSnapshot::from_frame(&frame);
        rt.set_dom_snapshot(Some(snapshot));

        // element.querySelector should find child element
        let result = rt
            .evaluate("document.querySelector('div').querySelector('a').href")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::String("/link".into())));

        // element.querySelector should not find elements outside subtree
        let result = rt
            .evaluate("document.querySelector('div').querySelector('span')")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::Null));
    }

    #[tokio::test]
    async fn test_element_query_selector_all() {
        let mut rt = JsRuntime::new();
        let html = r#"<html><body><ul><li>a</li><li>b</li></ul><li>c</li></body></html>"#;
        let frame = make_frame(html).await;
        let snapshot = DomSnapshot::from_frame(&frame);
        rt.set_dom_snapshot(Some(snapshot));

        // element.querySelectorAll should find only descendants
        let result = rt
            .evaluate("document.querySelector('ul').querySelectorAll('li').length")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(serde_json::json!(2)));

        // document.querySelectorAll should find all
        let result = rt
            .evaluate("document.querySelectorAll('li').length")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(serde_json::json!(3)));
    }

    #[tokio::test]
    async fn test_element_query_selector_class() {
        let mut rt = JsRuntime::new();
        let html = r#"<html><body><div class="outer"><span class="inner">yes</span><span class="other">no</span></div></body></html>"#;
        let frame = make_frame(html).await;
        let snapshot = DomSnapshot::from_frame(&frame);
        rt.set_dom_snapshot(Some(snapshot));

        let result = rt
            .evaluate("document.querySelector('.outer').querySelector('.inner').textContent")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::String("yes".into())));
    }

    #[tokio::test]
    async fn test_document_query_selector_all() {
        let mut rt = JsRuntime::new();
        let html = "<html><body><ul><li>a</li><li>b</li><li>c</li></ul></body></html>";
        let frame = make_frame(html).await;
        let snapshot = DomSnapshot::from_frame(&frame);
        rt.set_dom_snapshot(Some(snapshot));

        let result = rt
            .evaluate("document.querySelectorAll('li').length")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value.unwrap().as_f64().unwrap(), 3.0);
    }

    #[tokio::test]
    async fn test_document_get_element_by_id() {
        let mut rt = JsRuntime::new();
        let html = r#"<html><body><div id="main">content</div></body></html>"#;
        let frame = make_frame(html).await;
        let snapshot = DomSnapshot::from_frame(&frame);
        rt.set_dom_snapshot(Some(snapshot));

        let result = rt
            .evaluate("document.getElementById('main').id")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::String("main".into())));
    }

    #[tokio::test]
    async fn test_element_from_point_layout_hit_test() {
        // Layout-based hit test: two stacked divs; elementFromPoint must return
        // the element whose laid-out box contains the point (not a heuristic).
        let mut rt = JsRuntime::new();
        let html = concat!(
            "<!DOCTYPE html><html><head><style>",
            "body { margin: 0; }",
            "#a { width: 100px; height: 50px; background: red; }",
            "#b { width: 100px; height: 50px; background: blue; }",
            "</style></head><body><div id=\"a\"></div><div id=\"b\"></div></body></html>"
        );
        rt.set_document(html, Some("https://example.com/"), (200, 200))
            .await
            .expect("set_document should build the render doc");

        // #a occupies y=0..50; #b occupies y=50..100.
        let r = rt
            .evaluate("var e=document.elementFromPoint(10,10); e?e.id:'null'")
            .await
            .unwrap();
        assert!(r.is_ok(), "eval failed");
        assert_eq!(r.value, Some(Value::String("a".into())), "point in #a");

        let r = rt
            .evaluate("var e=document.elementFromPoint(10,70); e?e.id:'null'")
            .await
            .unwrap();
        assert!(r.is_ok());
        assert_eq!(r.value, Some(Value::String("b".into())), "point in #b");
    }

    #[tokio::test]
    async fn test_geolocation_override() {
        let mut rt = JsRuntime::new();
        rt.set_document(
            "<html><body></body></html>",
            Some("https://example.com/"),
            (400, 300),
        )
        .await
        .unwrap();

        // With an override, getCurrentPosition reports the coords synchronously.
        set_geolocation_override(37.7749, -122.4194, 10.0);
        let r = rt
            .evaluate(
                "var lat, lon;\
                 navigator.geolocation.getCurrentPosition(function (p) {\
                   lat = p.coords.latitude; lon = p.coords.longitude;\
                 });\
                 lat + ',' + lon",
            )
            .await
            .unwrap();
        assert!(r.is_ok(), "eval failed");
        assert_eq!(
            r.value,
            Some(Value::String("37.7749,-122.4194".into())),
            "geolocation override should be returned"
        );

        // After clearing, getCurrentPosition reports POSITION_UNAVAILABLE.
        clear_geolocation_override();
        let r = rt
            .evaluate(
                "var code = -1;\
                 navigator.geolocation.getCurrentPosition(function(){}, function (e) {\
                   code = e.code;\
                 });\
                 code",
            )
            .await
            .unwrap();
        assert!(r.is_ok());
        assert_eq!(
            r.value,
            Some(Value::Number(2.into())),
            "POSITION_UNAVAILABLE (2)"
        );
    }
    #[tokio::test]
    async fn test_document_get_elements_by_tag_name() {
        let mut rt = JsRuntime::new();
        let html = "<html><body><p>a</p><p>b</p></body></html>";
        let frame = make_frame(html).await;
        let snapshot = DomSnapshot::from_frame(&frame);
        rt.set_dom_snapshot(Some(snapshot));

        let result = rt
            .evaluate("document.getElementsByTagName('p').length")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value.unwrap().as_f64().unwrap(), 2.0);
    }

    #[tokio::test]
    async fn test_document_get_elements_by_class_name() {
        let mut rt = JsRuntime::new();
        let html =
            r#"<html><body><div class="item">a</div><div class="item">b</div></body></html>"#;
        let frame = make_frame(html).await;
        let snapshot = DomSnapshot::from_frame(&frame);
        rt.set_dom_snapshot(Some(snapshot));

        let result = rt
            .evaluate("document.getElementsByClassName('item').length")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value.unwrap().as_f64().unwrap(), 2.0);
    }

    #[tokio::test]
    async fn test_element_href_attribute() {
        let mut rt = JsRuntime::new();
        let html =
            r#"<html><body><a href="https://example.com" class="link">click</a></body></html>"#;
        let frame = make_frame(html).await;
        let snapshot = DomSnapshot::from_frame(&frame);
        rt.set_dom_snapshot(Some(snapshot));

        let result = rt
            .evaluate("document.querySelector('a').href")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(
            result.value,
            Some(Value::String("https://example.com".into()))
        );
    }

    #[tokio::test]
    async fn test_element_get_attribute() {
        let mut rt = JsRuntime::new();
        let html = r#"<html><body><a href="/page" id="link">go</a></body></html>"#;
        let frame = make_frame(html).await;
        let snapshot = DomSnapshot::from_frame(&frame);
        rt.set_dom_snapshot(Some(snapshot));

        let result = rt
            .evaluate("document.querySelector('a').getAttribute('href')")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::String("/page".into())));
    }

    #[tokio::test]
    async fn test_element_has_attribute() {
        let mut rt = JsRuntime::new();
        let html = r#"<html><body><a href="/page">go</a></body></html>"#;
        let frame = make_frame(html).await;
        let snapshot = DomSnapshot::from_frame(&frame);
        rt.set_dom_snapshot(Some(snapshot));

        let result = rt
            .evaluate("document.querySelector('a').hasAttribute('href')")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::Bool(true)));
    }

    #[tokio::test]
    async fn test_element_class_name() {
        let mut rt = JsRuntime::new();
        let html = r#"<html><body><div class="foo bar">content</div></body></html>"#;
        let frame = make_frame(html).await;
        let snapshot = DomSnapshot::from_frame(&frame);
        rt.set_dom_snapshot(Some(snapshot));

        let result = rt
            .evaluate("document.querySelector('div').className")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::String("foo bar".into())));
    }

    #[tokio::test]
    async fn test_element_text_content() {
        let mut rt = JsRuntime::new();
        let html = "<html><body><p>Hello World</p></body></html>";
        let frame = make_frame(html).await;
        let snapshot = DomSnapshot::from_frame(&frame);
        rt.set_dom_snapshot(Some(snapshot));

        let result = rt
            .evaluate("document.querySelector('p').textContent")
            .await
            .unwrap();
        assert!(result.is_ok());
        let text = result.value.unwrap().as_str().unwrap().to_string();
        assert!(
            text.contains("Hello World"),
            "textContent should contain 'Hello World', got: {:?}",
            text
        );
    }

    #[tokio::test]
    async fn test_element_inner_text_matches_text_content() {
        let mut rt = JsRuntime::new();
        let html = "<html><body><p>Hello World</p></body></html>";
        let frame = make_frame(html).await;
        let snapshot = DomSnapshot::from_frame(&frame);
        rt.set_dom_snapshot(Some(snapshot));

        let result = rt
            .evaluate(
                "var p = document.querySelector('p'); p.innerText === p.textContent && p.innerText",
            )
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::String("Hello World".into())));
    }

    #[tokio::test]
    async fn test_performance_now_standalone_global_matches_window() {
        let mut rt = JsRuntime::new();
        let result = rt
            .evaluate("var t = performance.now(); typeof performance === 'object' && typeof performance.now === 'function' && window.performance === performance && typeof t === 'number' && t > 1000000000000")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::Bool(true)));
    }

    #[tokio::test]
    async fn test_element_children() {
        let mut rt = JsRuntime::new();
        let html = "<html><body><div><p>a</p><p>b</p></div></body></html>";
        let frame = make_frame(html).await;
        let snapshot = DomSnapshot::from_frame(&frame);
        rt.set_dom_snapshot(Some(snapshot));

        let result = rt
            .evaluate("document.querySelector('div').children.length")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value.unwrap().as_f64().unwrap(), 2.0);
    }

    #[tokio::test]
    async fn test_document_snapshot_update() {
        let mut rt = JsRuntime::new();

        // First snapshot
        let html1 = "<html><head><title>Page 1</title></head><body></body></html>";
        let frame1 = make_frame(html1).await;
        let snapshot1 = DomSnapshot::from_frame(&frame1);
        rt.set_dom_snapshot(Some(snapshot1));

        let r1 = rt.evaluate("document.title").await.unwrap();
        assert_eq!(r1.value, Some(Value::String("Page 1".into())));

        // Second snapshot replaces
        let html2 = "<html><head><title>Page 2</title></head><body></body></html>";
        let frame2 = make_frame(html2).await;
        let snapshot2 = DomSnapshot::from_frame(&frame2);
        rt.set_dom_snapshot(Some(snapshot2));

        let r2 = rt.evaluate("document.title").await.unwrap();
        assert_eq!(r2.value, Some(Value::String("Page 2".into())));
    }

    // ========================================
    // Runtime limits & timeout tests
    // ========================================

    #[tokio::test]
    async fn test_max_recursive_calls() {
        // Infinite recursion should be caught by recursion limit
        let mut rt = JsRuntime::new();
        let result = rt
            .evaluate("function f() { return f(); } f()")
            .await
            .unwrap();
        assert!(!result.is_ok(), "infinite recursion should fail");
        let msg = result.exception.unwrap();
        assert!(
            msg.to_lowercase().contains("exceeded")
                || msg.to_lowercase().contains("recursion")
                || msg.to_lowercase().contains("stack"),
            "error should mention stack/recursion limit, got: {msg}"
        );
    }

    #[tokio::test]
    async fn test_max_loop_iterations() {
        // Infinite loop should be caught by loop iteration limit
        let mut rt = JsRuntime::new();
        let result = rt.evaluate("while(true) {}").await.unwrap();
        assert!(!result.is_ok(), "infinite loop should fail");
        let msg = result.exception.unwrap();
        assert!(
            msg.contains("loop iteration limit"),
            "error should mention loop iteration limit, got: {msg}"
        );
    }

    #[tokio::test]
    async fn test_evaluate_after_infinite_loop() {
        // After an infinite loop error, the runtime should still work
        let mut rt = JsRuntime::new();

        // Trigger an infinite loop
        let result = rt.evaluate("while(true) {}").await.unwrap();
        assert!(!result.is_ok());

        // Runtime should still be functional for normal evals
        let result = rt.evaluate("1 + 1").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::Number(2.into())));
    }

    #[tokio::test]
    async fn test_evaluate_after_infinite_recursion() {
        // After infinite recursion error, the runtime should still work
        let mut rt = JsRuntime::new();

        // Trigger infinite recursion
        let result = rt
            .evaluate("function f() { return f(); } f()")
            .await
            .unwrap();
        assert!(!result.is_ok());

        // Runtime should still be functional
        let result = rt.evaluate("42").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::Number(42.into())));
    }

    #[tokio::test]
    async fn test_normal_loop_within_limits() {
        // Normal loops should work fine within limits
        let mut rt = JsRuntime::new();
        let result = rt
            .evaluate("let sum = 0; for (let i = 0; i < 1000; i++) { sum += i; } sum")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value.unwrap().as_f64().unwrap(), 499500.0);
    }

    #[tokio::test]
    async fn test_normal_recursion_within_limits() {
        // Normal recursion should work fine within limits
        let mut rt = JsRuntime::new();
        let result = rt
            .evaluate("function fib(n) { return n <= 1 ? n : fib(n-1) + fib(n-2); } fib(10)")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::Number(55.into())));
    }

    #[tokio::test]
    async fn test_js_runtime_config_custom() {
        // Verify custom config is applied
        let config = JsRuntimeConfig {
            timeout_ms: 1000,
            max_recursion: 10,
            max_loop_iterations: 50,
            max_stack_size: 256,
            nav_script_max_loop_iterations: 50,
            nav_script_max_recursion: 10,
            nav_script_max_stack_size: 256,
            nav_script_timeout_ms: 1000,
            viewport_width: 1280,
            viewport_height: 720,
            user_agent: "Test/1.0".to_string(),
        };
        let mut rt = JsRuntime::with_config(config);

        // A loop of 50 iterations should fail with limit of 50
        let result = rt
            .evaluate("let x = 0; for (let i = 0; i < 100; i++) { x++; } x")
            .await
            .unwrap();
        assert!(!result.is_ok(), "loop exceeding limit should fail");
    }

    // ========================================
    // Timer / async API tests (sync emulation)
    // ========================================

    #[tokio::test]
    async fn test_set_timeout() {
        let mut rt = JsRuntime::new();
        // setTimeout schedules the callback; it fires during timer drain after eval.
        rt.evaluate("let x = 0; setTimeout(() => { x = 42; }, 0)")
            .await
            .unwrap();
        // Verify on the next evaluate() that x was set by the timer callback.
        let result = rt.evaluate("x").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::Number(42.into())));
    }

    #[tokio::test]
    async fn test_set_timeout_with_args() {
        let mut rt = JsRuntime::new();
        rt.evaluate("let r; setTimeout((a, b) => { r = a + b; }, 0, 3, 4)")
            .await
            .unwrap();
        let result = rt.evaluate("r").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::Number(7.into())));
    }

    #[tokio::test]
    async fn test_set_interval_executes_once() {
        let mut rt = JsRuntime::new();
        // setInterval fires once during timer drain, then re-schedules
        rt.evaluate("let c = 0; setInterval(() => { c++; }, 0)")
            .await
            .unwrap();
        let result = rt.evaluate("c").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::Number(1.into())));
    }

    #[tokio::test]
    async fn test_clear_timeout_cancels_timer() {
        let mut rt = JsRuntime::new();
        rt.evaluate("let x = 0; let id = setTimeout(() => { x = 99; }, 0); clearTimeout(id)")
            .await
            .unwrap();
        let result = rt.evaluate("x").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::Number(0.into())));
    }

    #[tokio::test]
    async fn test_clear_interval_cancels_timer() {
        let mut rt = JsRuntime::new();
        rt.evaluate("let c = 0; let id = setInterval(() => { c++; }, 0); clearInterval(id)")
            .await
            .unwrap();
        let result = rt.evaluate("c").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::Number(0.into())));
    }

    #[tokio::test]
    async fn test_fetch_returns_promise() {
        let mut rt = JsRuntime::new();
        // fetch() returns a Promise (no channel set, so returns Promise.reject)
        let result = rt.evaluate("fetch('https://example.com')").await.unwrap();
        // Should return a Promise object
        assert!(result.is_ok());
        assert!(result.value.is_some());
    }

    #[tokio::test]
    async fn test_fetch_no_channel_returns_promise() {
        let mut rt = JsRuntime::new();
        // fetch() returns Promise.reject when no channel is set
        // Just verify fetch() doesn't panic and returns something
        let result = rt.evaluate("typeof fetch").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::String("function".into())));
    }

    #[tokio::test]
    async fn test_fetch_with_mock_channel() {
        let mut rt = JsRuntime::new();
        // Set up a mock fetch channel (Phase 3 id-routed API).
        let (tx, rx) = std::sync::mpsc::channel::<FetchRequestMsg>();
        let (_resp_tx, resp_rx) = std::sync::mpsc::channel::<FetchResponseMsg>();
        rt.set_fetch_channel(tx, resp_rx);

        // Drop the request receiver so dispatch fails gracefully.
        drop(rx);

        let result = rt
            .evaluate("fetch('https://example.com').catch(e => 'error: ' + e.message)")
            .await
            .unwrap();
        assert!(result.is_ok());
    }

    /// JS `fetch(url, {method, body})` must extract method + body into the
    /// byte-typed `FetchRequestMsg`. Combined with the wire-level
    /// `HttpClient::request` test this proves JS fetch POST reaches the wire.
    #[tokio::test]
    async fn test_fetch_post_extracts_method_and_body() {
        let mut rt = JsRuntime::new();
        let (req_tx, req_rx) = std::sync::mpsc::channel::<FetchRequestMsg>();
        let (resp_tx, resp_rx) = std::sync::mpsc::channel::<FetchResponseMsg>();
        rt.set_fetch_channel(req_tx, resp_rx);

        let captured = Arc::new(parking_lot::Mutex::new(None::<(String, Vec<u8>)>));
        let cap = captured.clone();
        let resp_tx2 = resp_tx.clone();
        std::thread::spawn(move || {
            while let Ok(req) = req_rx.recv() {
                *cap.lock() = Some((req.method.clone(), req.body.clone().unwrap_or_default()));
                let _ = resp_tx2.send(FetchResponseMsg {
                    id: req.id,
                    status: 200,
                    status_text: "OK".to_string(),
                    url: req.url,
                    headers: vec![],
                    body: String::new(),
                    error: None,
                });
            }
        });

        // evaluate() returns the Promise object, not its resolved value, so
        // observe settlement through a global side effect (existing pattern).
        let r = rt
            .evaluate(
                "fetch('http://x/', {method:'POST', body:'hello',\
                 headers:{'content-type':'text/plain'}})\
                 .then(r => { window.__status = r.status; })",
            )
            .await
            .unwrap();
        if r.value.is_none() && r.exception.is_some() {
            panic!("fetch POST eval failed: {:?}", r.exception);
        }

        // The fetch request reached the dispatch channel with method + body.
        for _ in 0..100 {
            if captured.lock().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let got = captured
            .lock()
            .clone()
            .expect("fetch POST never reached the dispatch channel");
        assert_eq!(got.0, "POST", "method not extracted");
        assert_eq!(got.1, b"hello".to_vec(), "body not extracted as bytes");

        // And the promise settled to the 200 response during the pump.
        let r2 = rt.evaluate("window.__status").await.unwrap();
        assert_eq!(
            r2.value.as_ref().and_then(|v| v.as_f64()),
            Some(200.0),
            "fetch POST promise did not settle to 200: {:?}",
            r2.value
        );
    }

    #[tokio::test]
    async fn test_blob_basic() {
        let mut rt = JsRuntime::new();
        let r = rt
            .evaluate(
                "var b = new Blob([1,2,3]);\
                 var b2 = new Blob([new Uint8Array([4,5])], {type:'image/png'});\
                 var b3 = new Blob(['hi']);\
                 JSON.stringify({s1:b.size, t1:b.type, s2:b2.size, t2:b2.type, s3:b3.size})",
            )
            .await
            .unwrap();
        let s = r
            .value
            .as_ref()
            .and_then(|v| v.as_str())
            .expect("blob eval produced no value");
        let v: serde_json::Value = serde_json::from_str(s).expect("valid json");
        assert_eq!(v["s1"], 3, "Blob([1,2,3]).size");
        assert_eq!(v["t1"], "", "default type empty");
        assert_eq!(v["s2"], 2, "Blob(Uint8Array).size");
        assert_eq!(v["t2"], "image/png");
        assert_eq!(v["s3"], 2, "Blob(['hi']).size");
    }

    #[tokio::test]
    async fn test_formdata_append_get_has() {
        let mut rt = JsRuntime::new();
        let r = rt
            .evaluate(
                "var fd = new FormData();\
                 fd.append('a','1'); fd.append('b','2'); fd.append('a','3');\
                 JSON.stringify({hasA: fd.has('a'), hasZ: fd.has('z'),\
                  getA: fd.get('a'), allA: fd.getAll('a').join(','),\
                  gone: (fd.delete('b'), fd.has('b'))})",
            )
            .await
            .unwrap();
        let s = r
            .value
            .as_ref()
            .and_then(|v| v.as_str())
            .expect("formdata eval produced no value");
        let v: serde_json::Value = serde_json::from_str(s).expect("valid json");
        assert_eq!(v["hasA"], true);
        assert_eq!(v["hasZ"], false);
        assert_eq!(v["getA"], "1", "get returns first appended");
        assert_eq!(v["allA"], "1,3", "getAll returns every match");
        assert_eq!(v["gone"], false, "delete removes the entry");
    }

    /// fetch(FormData) must serialize to multipart/form-data with a boundary,
    /// and the content-type header must reflect it.
    #[tokio::test]
    async fn test_fetch_formdata_serializes_to_multipart() {
        let mut rt = JsRuntime::new();
        let (req_tx, req_rx) = std::sync::mpsc::channel::<FetchRequestMsg>();
        let (resp_tx, resp_rx) = std::sync::mpsc::channel::<FetchResponseMsg>();
        rt.set_fetch_channel(req_tx, resp_rx);
        let captured = Arc::new(parking_lot::Mutex::new(
            None::<(Vec<(String, String)>, Vec<u8>)>,
        ));
        let cap = captured.clone();
        let resp_tx2 = resp_tx.clone();
        std::thread::spawn(move || {
            while let Ok(req) = req_rx.recv() {
                *cap.lock() = Some((req.headers.clone(), req.body.clone().unwrap_or_default()));
                let _ = resp_tx2.send(FetchResponseMsg {
                    id: req.id,
                    status: 200,
                    status_text: "OK".to_string(),
                    url: req.url,
                    headers: vec![],
                    body: String::new(),
                    error: None,
                });
            }
        });
        let r = rt
            .evaluate(
                "var fd = new FormData();\
                 fd.append('a','1');\
                 fd.append('file', new Blob([0,1,2,3], {type:'text/plain'}), 'x.bin');\
                 fetch('http://x/', {method:'POST', body: fd})\
                   .then(r => { window.__s = r.status; })",
            )
            .await
            .unwrap();
        if r.value.is_none() && r.exception.is_some() {
            panic!("fetch FormData eval failed: {:?}", r.exception);
        }
        for _ in 0..100 {
            if captured.lock().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let (headers, body) = captured
            .lock()
            .clone()
            .expect("fetch FormData never reached the dispatch channel");
        let ct = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        assert!(
            ct.starts_with("multipart/form-data; boundary="),
            "content-type was {ct:?}"
        );
        let body_text = String::from_utf8_lossy(&body);
        assert!(
            body_text.contains("name=\"a\""),
            "missing text field: {body_text}"
        );
        assert!(
            body_text.contains("filename=\"x.bin\""),
            "missing file field: {body_text}"
        );
        assert!(
            body_text.contains("\r\n--"),
            "no multipart boundary delimiters"
        );
    }
    /// createElement of a defined custom element upgrades it: the returned
    /// element is an instance of the constructor with its prototype methods.
    #[tokio::test]
    async fn test_custom_element_create_element_upgrade() {
        let mut rt = JsRuntime::new();
        let r = rt
            .evaluate(
                "class XFoo extends HTMLElement { greet() { return 'hi'; } }\
                 customElements.define('x-foo', XFoo);\
                 var el = document.createElement('x-foo');\
                 JSON.stringify({\
                   isInstance: (el instanceof XFoo),\
                   hasGreet: typeof el.greet === 'function',\
                   res: el.greet && el.greet()\
                 })",
            )
            .await
            .unwrap();
        let s = r
            .value
            .as_ref()
            .and_then(|v| v.as_str())
            .expect("custom-element eval produced no value");
        let v: serde_json::Value = serde_json::from_str(s).expect("valid json");
        assert_eq!(
            v["isInstance"], true,
            "createElement must upgrade to the ctor"
        );
        assert_eq!(v["hasGreet"], true, "prototype method must be present");
        assert_eq!(v["res"], "hi");
    }

    /// connectedCallback / disconnectedCallback / attributeChangedCallback fire
    /// on the render-doc appendChild / remove / setAttribute hooks.
    #[tokio::test]
    async fn test_custom_element_lifecycle_callbacks() {
        let mut rt = JsRuntime::new();
        rt.set_document(
            "<!DOCTYPE html><html><head></head><body></body></html>",
            None,
            (400, 300),
        )
        .await
        .unwrap();
        let r = rt
            .evaluate(
                "var log = [];\
                 class XLc extends HTMLElement {\
                   connectedCallback() {\
                    log.push('connected');\
                    this.setAttribute('data-cb', 'on');\
                    this.appendChild(document.createElement('span'));\
                   }\
                   disconnectedCallback() { log.push('disconnected'); }\
                   static get observedAttributes() { return ['data-x', 'data-cb']; }\
                   attributeChangedCallback(n, o, v) { log.push('attr:' + n + '=' + v); }\
                 }\
                 customElements.define('x-lc', XLc);\
                 var el = document.createElement('x-lc');\
                document.body.appendChild(el);\
                 el.setAttribute('data-x', '1');\
                 el.remove();\
                 JSON.stringify(log)",
            )
            .await
            .unwrap();
        if r.exception.is_some() {
            panic!("lifecycle eval threw: {:?}", r.exception);
        }
        let s = r
            .value
            .as_ref()
            .and_then(|v| v.as_str())
            .expect("lifecycle eval produced no value");
        assert!(
            s.contains("connected"),
            "connectedCallback should fire: {s}"
        );
        assert!(
            s.contains("attr:data-x=1"),
            "attributeChangedCallback should fire: {s}"
        );
        assert!(
            s.contains("disconnected"),
            "disconnectedCallback should fire: {s}"
        );
    }

    /// Shadow DOM slot composition: a custom element's shadow `<slot>`
    /// distributes the host's light-DOM children into the composed (flattened)
    /// snapshot tree, so DomSnapshot-backed reads see slotted content.
    #[tokio::test]
    async fn test_shadow_dom_slot_composition() {
        use std::collections::HashSet;
        let mut rt = JsRuntime::new();
        rt.set_document(
            "<!DOCTYPE html><html><head></head><body></body></html>",
            None,
            (400, 300),
        )
        .await
        .unwrap();
        let r = rt
            .evaluate(
                "class MyCard extends HTMLElement {\
                   connectedCallback() {\
                     var s = this.attachShadow({ mode: 'open' });\
                     var slot = document.createElement('slot');\
                     s.appendChild(slot);\
                   }\
                 }\
                 customElements.define('my-card', MyCard);\
                 var card = document.createElement('my-card');\
                 var light = document.createElement('p');\
                 light.id = 'light'; light.textContent = 'slotted';\
                 card.appendChild(light);\
                 document.body.appendChild(card);",
            )
            .await
            .unwrap();
        assert!(
            r.exception.is_none(),
            "shadow setup threw: {:?}",
            r.exception
        );

        let snap = rt.dom_snapshot("about:blank").await.unwrap();
        let body = snap.body_id.expect("body exists");
        // Walk the composed tree from <body>.
        let mut reachable = HashSet::new();
        let mut stack = vec![body];
        while let Some(id) = stack.pop() {
            if !reachable.insert(id) {
                continue;
            }
            if let Some(n) = snap.nodes.get(&id) {
                stack.extend(n.children.iter().copied());
            }
        }
        let light_id = snap.id_index.get("light").copied();
        assert!(light_id.is_some(), "light child should be in the snapshot");
        assert!(
            reachable.contains(&light_id.unwrap()),
            "light child must be composed into the rendered tree via the default <slot>"
        );
        // The <slot> itself should be replaced by its assigned light child.
        let slot_reachable = reachable
            .iter()
            .any(|id| snap.nodes.get(id).is_some_and(|n| n.tag == "slot"));
        assert!(
            !slot_reachable,
            "default <slot> should be replaced by its assigned light child"
        );
    }

    /// Named-slot composition: a child with `slot="header"` is distributed
    /// into `<slot name="header">`; a child with no matching slot is dropped
    /// from the rendered (flattened) tree.
    #[tokio::test]
    async fn test_shadow_dom_named_slot_composition() {
        use std::collections::HashSet;
        let mut rt = JsRuntime::new();
        rt.set_document(
            "<!DOCTYPE html><html><head></head><body></body></html>",
            None,
            (400, 300),
        )
        .await
        .unwrap();
        let r = rt
            .evaluate(
                "class MyCard extends HTMLElement {\
                   connectedCallback() {\
                     var s = this.attachShadow({ mode: 'open' });\
                     var hslot = document.createElement('slot');\
                     hslot.setAttribute('name', 'header');\
                     s.appendChild(hslot);\
                   }\
                 }\
                 customElements.define('my-card', MyCard);\
                 var card = document.createElement('my-card');\
                 var hdr = document.createElement('h1');\
                 hdr.setAttribute('slot', 'header'); hdr.id = 'hdr';\
                 var orphan = document.createElement('p');\
                 orphan.id = 'orphan';\
                 card.appendChild(hdr); card.appendChild(orphan);\
                 document.body.appendChild(card);",
            )
            .await
            .unwrap();
        assert!(
            r.exception.is_none(),
            "named-slot setup threw: {:?}",
            r.exception
        );

        let snap = rt.dom_snapshot("about:blank").await.unwrap();
        let body = snap.body_id.expect("body exists");
        let mut reachable = HashSet::new();
        let mut stack = vec![body];
        while let Some(id) = stack.pop() {
            if !reachable.insert(id) {
                continue;
            }
            if let Some(n) = snap.nodes.get(&id) {
                stack.extend(n.children.iter().copied());
            }
        }
        // The named child is distributed into <slot name="header">.
        let hdr_id = snap.id_index.get("hdr").copied();
        assert!(hdr_id.is_some(), "header child should be in the snapshot");
        assert!(
            reachable.contains(&hdr_id.unwrap()),
            "slot='header' child must be composed into the rendered tree"
        );
        // The orphan (no matching slot, no default slot) is NOT rendered.
        let orphan_reachable = snap
            .id_index
            .get("orphan")
            .map(|id| reachable.contains(id))
            .unwrap_or(false);
        assert!(
            !orphan_reachable,
            "child with no matching slot must be dropped from the flattened tree"
        );
    }

    /// `DomSnapshot::to_html` serializes the composed (shadow-flattened) tree:
    /// shadow content and slotted light children appear; `<slot>` elements are
    /// replaced by their assignments. This is exactly what the
    /// compose-then-feed screenshot path re-parses into Blitz.
    #[tokio::test]
    async fn test_to_html_serializes_composed_shadow_tree() {
        let mut rt = JsRuntime::new();
        rt.set_document(
            "<!DOCTYPE html><html><head></head><body></body></html>",
            None,
            (400, 300),
        )
        .await
        .unwrap();
        let r = rt
            .evaluate(
                "class MyHost extends HTMLElement {\
                   connectedCallback() {\
                     var s = this.attachShadow({ mode: 'open' });\
                     var wrap = document.createElement('div');\
                     wrap.id = 'shadow-wrap';\
                     wrap.textContent = 'SHADOW-MARKER';\
                     s.appendChild(wrap);\
                     var slot = document.createElement('slot');\
                     s.appendChild(slot);\
                   }\
                 }\
                 customElements.define('my-host', MyHost);\
                 var host = document.createElement('my-host');\
                 var light = document.createElement('span');\
                 light.id = 'light'; light.textContent = 'LIGHT-MARKER';\
                 host.appendChild(light);\
                 document.body.appendChild(host);",
            )
            .await
            .unwrap();
        assert!(r.exception.is_none(), "setup threw: {:?}", r.exception);

        let snap = rt.dom_snapshot("about:blank").await.unwrap();
        let html = snap.to_html();

        // Shadow content is composed into the flattened tree.
        assert!(
            html.contains("SHADOW-MARKER"),
            "shadow content must survive to_html; got: {html}"
        );
        // The default <slot> is replaced by its assigned light child.
        assert!(
            html.contains("LIGHT-MARKER"),
            "slotted light child must survive to_html; got: {html}"
        );
        // Slots themselves are gone (replaced, not emitted).
        assert!(
            !html.contains("<slot"),
            "<slot> must be replaced by its assignment in to_html; got: {html}"
        );
    }

    /// Screenshot rasterization reflects Shadow DOM content: a colored box
    /// appended to a shadow root is invisible to Blitz's flat tree, but the
    /// compose-then-feed path (flatten → serialize → reparse → rasterize)
    /// paints it.
    #[tokio::test]
    async fn test_screenshot_renders_shadow_content() {
        let mut rt = JsRuntime::new();
        rt.set_document(
            "<!DOCTYPE html><html><head></head><body></body></html>",
            None,
            (300, 300),
        )
        .await
        .unwrap();
        let r = rt
            .evaluate(
                "class RedShadow extends HTMLElement {\
                   connectedCallback() {\
                     var s = this.attachShadow({ mode: 'open' });\
                     var box = document.createElement('div');\
                     box.setAttribute('style', 'width:200px;height:200px;background-color:#ff0000');\
                     s.appendChild(box);\
                   }\
                 }\
                 customElements.define('red-shadow', RedShadow);\
                 var el = document.createElement('red-shadow');\
                 document.body.appendChild(el);",
            )
            .await
            .unwrap();
        assert!(
            r.exception.is_none(),
            "shadow setup threw: {:?}",
            r.exception
        );

        let png = rt
            .capture_png(CaptureOpts {
                full_page: true,
                ..Default::default()
            })
            .await
            .expect("capture_png should render the composed shadow tree");

        // Decode and count red pixels — the shadow box. Only present if the
        // compose-then-feed path flattened the shadow subtree before raster.
        let red = image::load_from_memory(&png)
            .expect("decode captured png")
            .to_rgba8()
            .pixels()
            .filter(|p| p[0] > 200 && p[1] < 80 && p[2] < 80)
            .count();
        assert!(
            red > 500,
            "expected the shadow red box in the screenshot, got {red} red px"
        );
    }

    /// `slot.assignedNodes()`/`assignedElements()` return the light-DOM
    /// children distributed into the (default) `<slot>`; `node.assignedSlot`
    /// resolves back to that slot for open shadow trees.
    #[tokio::test]
    async fn test_slot_assigned_nodes_and_assigned_slot() {
        let mut rt = JsRuntime::new();
        rt.set_document(
            "<!DOCTYPE html><html><head></head><body></body></html>",
            None,
            (400, 300),
        )
        .await
        .unwrap();
        let r = rt
            .evaluate(
                "class SlotHost extends HTMLElement {\
                   connectedCallback() {\
                     var s = this.attachShadow({ mode: 'open' });\
                     var slot = document.createElement('slot');\
                     s.appendChild(slot);\
                     globalThis.__slotRef = slot;\
                   }\
                 }\
                 customElements.define('slot-host', SlotHost);\
                 var h = document.createElement('slot-host');\
                 var kid = document.createElement('p');\
                 kid.id = 'kid'; kid.textContent = 'hi';\
                 h.appendChild(kid);\
                 document.body.appendChild(h);\
                 globalThis.__assignedLen = globalThis.__slotRef.assignedNodes().length;\
                 globalThis.__assignedElLen = globalThis.__slotRef.assignedElements().length;\
                 globalThis.__kidAssignedSlotIsNull = (kid.assignedSlot === null);\
                 globalThis.__kidAssignedSlotId = kid.assignedSlot && kid.assignedSlot.tagName;\
                 var alsoText = document.createElement('span'); alsoText.id = 'noSlot';\
                 globalThis.__unrelated = (alsoText.assignedSlot === null);",
            )
            .await
            .unwrap();
        assert!(r.exception.is_none(), "slot setup threw: {:?}", r.exception);

        let len = rt.evaluate("globalThis.__assignedLen").await.unwrap().value;
        assert_eq!(
            len,
            Some(Value::Number(1.into())),
            "default slot assigns 1 child"
        );

        let elen = rt
            .evaluate("globalThis.__assignedElLen")
            .await
            .unwrap()
            .value;
        assert_eq!(
            elen,
            Some(Value::Number(1.into())),
            "assignedElements returns the <p> element"
        );

        // The slotted child's assignedSlot points back at the <slot> ('SLOT').
        let is_null = rt
            .evaluate("globalThis.__kidAssignedSlotIsNull")
            .await
            .unwrap()
            .value;
        assert_eq!(
            is_null,
            Some(Value::Bool(false)),
            "assignedSlot must not be null"
        );
        let slot_tag = rt
            .evaluate("globalThis.__kidAssignedSlotId")
            .await
            .unwrap()
            .value;
        assert_eq!(
            slot_tag,
            Some(Value::String("SLOT".into())),
            "assignedSlot must be the slot element"
        );

        // A node never distributed into a slot has assignedSlot === null.
        let unrelated = rt.evaluate("globalThis.__unrelated").await.unwrap().value;
        assert_eq!(
            unrelated,
            Some(Value::Bool(true)),
            "unrelated node.assignedSlot must be null"
        );
    }

    /// `attachShadow({ mode: 'closed' })` hides the root from
    /// `element.shadowRoot` and from `node.assignedSlot`, but the shadow
    /// content still renders and internal `assignedNodes()` keeps working.
    #[tokio::test]
    async fn test_closed_shadow_hiding() {
        let mut rt = JsRuntime::new();
        rt.set_document(
            "<!DOCTYPE html><html><head></head><body></body></html>",
            None,
            (400, 300),
        )
        .await
        .unwrap();
        let r = rt
            .evaluate(
                "class ClosedHost extends HTMLElement {\
                   connectedCallback() {\
                     var s = this.attachShadow({ mode: 'closed' });\
                     var slot = document.createElement('slot');\
                     s.appendChild(slot);\
                     globalThis.__closedSlotRef = slot;\
                   }\
                 }\
                 customElements.define('closed-host', ClosedHost);\
                 var h = document.createElement('closed-host');\
                 var kid = document.createElement('p'); kid.id = 'ckid';\
                 h.appendChild(kid);\
                 document.body.appendChild(h);\
                 globalThis.__closedSRNull = (h.shadowRoot === null);\
                 globalThis.__closedAssignedSlotNull = (kid.assignedSlot === null);\
                 globalThis.__closedAssignedNodes = globalThis.__closedSlotRef.assignedNodes().length;",
            )
            .await
            .unwrap();
        assert!(
            r.exception.is_none(),
            "closed setup threw: {:?}",
            r.exception
        );

        let sr_null = rt
            .evaluate("globalThis.__closedSRNull")
            .await
            .unwrap()
            .value;
        assert_eq!(
            sr_null,
            Some(Value::Bool(true)),
            "closed shadow root must be hidden from element.shadowRoot"
        );
        let aslot_null = rt
            .evaluate("globalThis.__closedAssignedSlotNull")
            .await
            .unwrap()
            .value;
        assert_eq!(
            aslot_null,
            Some(Value::Bool(true)),
            "assignedSlot must be null for a slot in a closed tree"
        );
        let nodes = rt
            .evaluate("globalThis.__closedAssignedNodes")
            .await
            .unwrap()
            .value;
        assert_eq!(
            nodes,
            Some(Value::Number(1.into())),
            "internal assignedNodes() still works on closed roots"
        );
    }

    /// `shadowRoot.innerHTML = html` parses the fragment and appends the nodes
    /// as shadow children; the composed tree then contains them.
    #[tokio::test]
    async fn test_shadow_root_inner_html() {
        use std::collections::HashSet;
        let mut rt = JsRuntime::new();
        rt.set_document(
            "<!DOCTYPE html><html><head></head><body></body></html>",
            None,
            (400, 300),
        )
        .await
        .unwrap();
        let r = rt
            .evaluate(
                "class IhHost extends HTMLElement {\
                   connectedCallback() {\
                     var s = this.attachShadow({ mode: 'open' });\
                     s.innerHTML = '<div id=\"inner\">hello</div><span id=\"tail\">x</span>';\
                   }\
                 }\
                 customElements.define('ih-host', IhHost);\
                 var h = document.createElement('ih-host');\
                 document.body.appendChild(h);",
            )
            .await
            .unwrap();
        assert!(
            r.exception.is_none(),
            "innerHTML setup threw: {:?}",
            r.exception
        );

        let snap = rt.dom_snapshot("about:blank").await.unwrap();
        let body = snap.body_id.expect("body exists");
        let mut reachable = HashSet::new();
        let mut stack = vec![body];
        while let Some(id) = stack.pop() {
            if !reachable.insert(id) {
                continue;
            }
            if let Some(n) = snap.nodes.get(&id) {
                stack.extend(n.children.iter().copied());
            }
        }
        let inner = snap.id_index.get("inner").copied();
        assert!(inner.is_some(), "innerHTML <div id=inner> should exist");
        assert!(
            reachable.contains(&inner.unwrap()),
            "innerHTML shadow content must be composed into the flattened tree"
        );
        // The text inside the parsed <div> survives.
        let has_hello = snap
            .nodes
            .values()
            .any(|n| n.text_content.contains("hello"));
        assert!(has_hello, "innerHTML text content must be present");
    }

    /// `shadowRoot.append(a, b, …)` records multiple shadow children.
    #[tokio::test]
    async fn test_shadow_root_append_multiple() {
        use std::collections::HashSet;
        let mut rt = JsRuntime::new();
        rt.set_document(
            "<!DOCTYPE html><html><head></head><body></body></html>",
            None,
            (400, 300),
        )
        .await
        .unwrap();
        let r = rt
            .evaluate(
                "class AppendHost extends HTMLElement {\
                   connectedCallback() {\
                     var s = this.attachShadow({ mode: 'open' });\
                     var a = document.createElement('p'); a.id = 'ap1';\
                     var b = document.createElement('p'); b.id = 'ap2';\
                     s.append(a, b);\
                   }\
                 }\
                 customElements.define('append-host', AppendHost);\
                 var h = document.createElement('append-host');\
                 document.body.appendChild(h);",
            )
            .await
            .unwrap();
        assert!(
            r.exception.is_none(),
            "append setup threw: {:?}",
            r.exception
        );

        let snap = rt.dom_snapshot("about:blank").await.unwrap();
        let body = snap.body_id.expect("body exists");
        let mut reachable = HashSet::new();
        let mut stack = vec![body];
        while let Some(id) = stack.pop() {
            if !reachable.insert(id) {
                continue;
            }
            if let Some(n) = snap.nodes.get(&id) {
                stack.extend(n.children.iter().copied());
            }
        }
        for marker in ["ap1", "ap2"] {
            let id = snap.id_index.get(marker).copied();
            assert!(id.is_some(), "appended <{marker}> should exist");
            assert!(
                reachable.contains(&id.unwrap()),
                "shadowRoot.append child <{marker}> must be composed into the tree"
            );
        }
    }

    /// Declarative shadow DOM: `<template shadowrootmode="open">` parsed at
    /// navigate time attaches a shadow root to its host; the template's content
    /// becomes the shadow tree and the host's light children distribute into
    /// any `<slot>`.
    #[tokio::test]
    async fn test_declarative_shadow_dom() {
        use std::collections::HashSet;
        let mut rt = JsRuntime::new();
        let html = concat!(
            "<!DOCTYPE html><html><head></head><body>",
            "<host-elem>",
            "<p id=\"light\">slotted</p>",
            "<template shadowrootmode=\"open\">",
            "<div id=\"shadow\">shadow-content</div>",
            "<slot></slot>",
            "</template>",
            "</host-elem>",
            "</body></html>"
        );
        rt.set_document(html, None, (400, 300)).await.unwrap();

        let snap = rt.dom_snapshot("about:blank").await.unwrap();
        let body = snap.body_id.expect("body exists");
        let mut reachable = HashSet::new();
        let mut stack = vec![body];
        while let Some(id) = stack.pop() {
            if !reachable.insert(id) {
                continue;
            }
            if let Some(n) = snap.nodes.get(&id) {
                stack.extend(n.children.iter().copied());
            }
        }
        // The declarative shadow <div id=shadow> is composed into the tree.
        let shadow_id = snap.id_index.get("shadow").copied();
        assert!(
            shadow_id.is_some(),
            "declarative shadow <div id=shadow> should exist"
        );
        assert!(
            reachable.contains(&shadow_id.unwrap()),
            "declarative shadow content must be composed into the flattened tree"
        );
        // The host's light child is distributed into the default <slot>.
        let light_id = snap.id_index.get("light").copied();
        assert!(light_id.is_some(), "light child should exist");
        assert!(
            reachable.contains(&light_id.unwrap()),
            "light child must be composed (distributed into the declarative <slot>)"
        );
        // The declarative shadow text rendered.
        assert!(
            snap.nodes
                .values()
                .any(|n| n.text_content.contains("shadow-content")),
            "declarative shadow text content must be present"
        );
        // The <template> wrapper itself is detached (not in the flattened tree).
        let template_reachable = reachable
            .iter()
            .any(|id| snap.nodes.get(id).is_some_and(|n| n.tag == "template"));
        assert!(
            !template_reachable,
            "the declarative <template> wrapper must be detached after processing"
        );
    }

    /// canvas 2D shim: getContext('2d') must exist and not throw, measureText
    /// returns a TextMetrics, toDataURL returns a data: URL, webgl context truthy.
    #[tokio::test]
    async fn test_canvas_2d_shim() {
        let mut rt = JsRuntime::new();
        let r = rt
            .evaluate(
                "var c = document.createElement('canvas');\
                 var ctx = c.getContext('2d');\
                 ctx.fillRect(0,0,10,10);\
                 ctx.fillStyle = '#ffffff';\
                 var m = ctx.measureText('hello');\
                 JSON.stringify({\
                   hasCtx: !!ctx,\
                   measure: typeof ctx.measureText === 'function',\
                   mwPos: m.width > 0,\
                   dataUrl: c.toDataURL().slice(0, 5),\
                   gl: !!c.getContext('webgl'),\
                   img: !!ctx.getImageData\
                 })",
            )
            .await
            .unwrap();
        let s = r
            .value
            .as_ref()
            .and_then(|v| v.as_str())
            .expect("canvas eval produced no value");
        let v: serde_json::Value = serde_json::from_str(s).expect("valid json");
        assert_eq!(v["measure"], true);
        assert_eq!(v["mwPos"], true, "measureText width should be > 0");
        assert_eq!(v["dataUrl"], "data:", "toDataURL should return a data: URL");
        assert_eq!(v["gl"], true, "getContext('webgl') should be truthy");
        assert_eq!(v["img"], true, "getImageData should exist");
    }

    /// window.alert/confirm/prompt must exist and not throw; defaults are
    /// no-op / false / null (no event-driven dialog plumbing yet).
    #[tokio::test]
    async fn test_dialog_functions_no_throw() {
        let mut rt = JsRuntime::new();
        let r = rt
            .evaluate(
                "var c = confirm('q'); var p = prompt('q'); alert('a');\
                 JSON.stringify({a: typeof alert, c: typeof confirm, p: typeof prompt,\
                 cv: c, pv: p === null})",
            )
            .await
            .unwrap();
        let s = r
            .value
            .as_ref()
            .and_then(|v| v.as_str())
            .expect("dialog eval produced no value");
        let v: serde_json::Value = serde_json::from_str(s).expect("valid json");
        assert_eq!(v["a"], "function");
        assert_eq!(v["c"], "function");
        assert_eq!(v["p"], "function");
        assert_eq!(v["cv"], false, "confirm defaults to false");
        assert_eq!(v["pv"], true, "prompt defaults to null");
    }

    /// Background fetch handler for tests: spawn-per-request, each responding
    /// after `delay_ms` with `body`. Models concurrent in-flight I/O so the
    /// parallelism and non-blocking guarantees of Phase 3 are exercised.
    fn spawn_test_fetch_handler(
        request_rx: std::sync::mpsc::Receiver<FetchRequestMsg>,
        response_tx: std::sync::mpsc::Sender<FetchResponseMsg>,
        delay_ms: u64,
        body: String,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            while let Ok(req) = request_rx.recv() {
                let response_tx = response_tx.clone();
                let body = body.clone();
                // Independent thread per request = parallel in-flight.
                std::thread::spawn(move || {
                    if delay_ms > 0 {
                        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                    }
                    let _ = response_tx.send(FetchResponseMsg {
                        id: req.id,
                        status: 200,
                        status_text: "OK".to_string(),
                        url: req.url,
                        headers: vec![("content-type".to_string(), "application/json".to_string())],
                        body,
                        error: None,
                    });
                });
            }
        })
    }

    #[tokio::test]
    async fn test_async_fetch_non_blocking() {
        // fetch() must NOT block the JS thread: a 300 ms RTT must return to
        // the next statement in well under that. On `main` the recv() blocked
        // ≈ the full RTT.
        let mut rt = JsRuntime::new();
        let (req_tx, req_rx) = std::sync::mpsc::channel::<FetchRequestMsg>();
        let (resp_tx, resp_rx) = std::sync::mpsc::channel::<FetchResponseMsg>();
        rt.set_fetch_channel(req_tx, resp_rx);
        let _h = spawn_test_fetch_handler(req_rx, resp_tx, 300, "{\"value\":42}".to_string());

        let r = rt
            .evaluate(
                "window.__t0 = performance.now(); fetch('http://x/'); window.__t1 = performance.now(); (window.__t1 - window.__t0) < 60",
            )
            .await
            .unwrap();
        let ok = r.value.as_ref().and_then(|v| v.as_bool()).unwrap_or(false);
        assert!(
            ok,
            "fetch blocked the JS thread; timing value = {:?}",
            r.value
        );
    }

    #[tokio::test]
    async fn test_async_fetch_resolves_on_event_loop() {
        // fetch().then(cb) settles during the evaluate's post-script pump —
        // no second evaluate needed for the callback to run.
        let mut rt = JsRuntime::new();
        let (req_tx, req_rx) = std::sync::mpsc::channel::<FetchRequestMsg>();
        let (resp_tx, resp_rx) = std::sync::mpsc::channel::<FetchResponseMsg>();
        rt.set_fetch_channel(req_tx, resp_rx);
        let _h = spawn_test_fetch_handler(req_rx, resp_tx, 50, "{\"value\":42}".to_string());

        rt.evaluate(
            "fetch('http://x/').then(r => r.json()).then(o => { window.__done = o.value })",
        )
        .await
        .unwrap();

        let r = rt.evaluate("window.__done").await.unwrap();
        assert_eq!(r.value, Some(Value::from(42)));
    }

    #[tokio::test]
    async fn test_async_fetch_concurrent_parallel() {
        // Two slow fetches fired back-to-back must run in PARALLEL: total wall
        // time ≈ one RTT (300 ms), not two (600 ms). On `main` requests were
        // serialized, so this would take ≥ ~580 ms.
        let mut rt = JsRuntime::new();
        let (req_tx, req_rx) = std::sync::mpsc::channel::<FetchRequestMsg>();
        let (resp_tx, resp_rx) = std::sync::mpsc::channel::<FetchResponseMsg>();
        rt.set_fetch_channel(req_tx, resp_rx);
        let _h = spawn_test_fetch_handler(req_rx, resp_tx, 300, "{\"v\":1}".to_string());

        rt.evaluate(
            "window.__t0 = performance.now();\
             Promise.all([fetch('http://a/'), fetch('http://b/')])\
               .then(() => { window.__dur = performance.now() - window.__t0 })",
        )
        .await
        .unwrap();

        let r = rt.evaluate("(window.__dur || 9999) < 580").await.unwrap();
        let ok = r.value.as_ref().and_then(|v| v.as_bool()).unwrap_or(false);
        assert!(ok, "fetches were serial; dur value = {:?}", r.value);
    }
    #[tokio::test]
    async fn test_fetch_pre_aborted_signal_rejects_with_abort_error() {
        // fetch(url, {signal}) with an ALREADY-aborted signal must reject the
        // returned promise with an AbortError WITHOUT dispatching a request.
        let mut rt = JsRuntime::new();
        let (req_tx, req_rx) = std::sync::mpsc::channel::<FetchRequestMsg>();
        let (resp_tx, resp_rx) = std::sync::mpsc::channel::<FetchResponseMsg>();
        rt.set_fetch_channel(req_tx, resp_rx);
        let _h = spawn_test_fetch_handler(req_rx, resp_tx, 400, "{\"v\":1}".to_string());

        rt.evaluate(
            "var ac = new AbortController();\
             ac.abort();\
             globalThis.__name = 'none';\
             fetch('http://x/', {signal: ac.signal})\
               .catch(e => { globalThis.__name = e.name; })",
        )
        .await
        .unwrap();

        let r = rt.evaluate("globalThis.__name").await.unwrap();
        assert_eq!(r.value, Some(Value::from("AbortError")));
    }
    #[tokio::test]
    async fn test_fetch_inflight_abort_rejects_with_abort_error() {
        // abort() called AFTER fetch() starts must reject the in-flight promise
        // with an AbortError; the late response is then dropped (no entry).
        let mut rt = JsRuntime::new();
        let (req_tx, req_rx) = std::sync::mpsc::channel::<FetchRequestMsg>();
        let (resp_tx, resp_rx) = std::sync::mpsc::channel::<FetchResponseMsg>();
        rt.set_fetch_channel(req_tx, resp_rx);
        // Long RTT so the abort fires while the request is still in-flight.
        let _h = spawn_test_fetch_handler(req_rx, resp_tx, 400, "{\"v\":1}".to_string());

        rt.evaluate(
            "var ac = new AbortController();\
             globalThis.__name = 'none';\
             fetch('http://x/', {signal: ac.signal})\
               .catch(e => { globalThis.__name = e.name; });\
             ac.abort();",
        )
        .await
        .unwrap();

        let r = rt.evaluate("globalThis.__name").await.unwrap();
        assert_eq!(r.value, Some(Value::from("AbortError")));
    }

    #[tokio::test]
    async fn test_async_xhr_non_blocking() {
        // xhr.send(async) returns immediately; onload fires when the response
        // arrives during the pump. Both the sync sentinel and the async flag
        // must be set.
        let mut rt = JsRuntime::new();
        let (req_tx, req_rx) = std::sync::mpsc::channel::<FetchRequestMsg>();
        let (resp_tx, resp_rx) = std::sync::mpsc::channel::<FetchResponseMsg>();
        rt.set_fetch_channel(req_tx, resp_rx);
        let _h = spawn_test_fetch_handler(req_rx, resp_tx, 100, "hello".to_string());

        rt.evaluate(
            "var x = new XMLHttpRequest();\
             x.open('GET', 'http://x/', true);\
             x.onload = function () { window.__xhr = 1; };\
             x.send();\
             window.__sent = 1;",
        )
        .await
        .unwrap();

        let sent = rt.evaluate("window.__sent").await.unwrap();
        assert_eq!(sent.value, Some(Value::from(1)));
        let xhr = rt.evaluate("window.__xhr").await.unwrap();
        assert_eq!(xhr.value, Some(Value::from(1)));
    }

    #[tokio::test]
    async fn test_element_add_event_listener_noop() {
        let mut rt = JsRuntime::new();
        let html = r#"<html><body><div id="test">hi</div></body></html>"#;
        let frame = make_frame(html).await;
        let snapshot = DomSnapshot::from_frame(&frame);
        rt.set_dom_snapshot(Some(snapshot));

        let result = rt
            .evaluate("document.querySelector('div').addEventListener('click', () => {}); 'ok'")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::String("ok".into())));
    }

    #[tokio::test]
    async fn test_element_dispatch_event_noop() {
        let mut rt = JsRuntime::new();
        let html = r#"<html><body><button id="btn">click</button></body></html>"#;
        let frame = make_frame(html).await;
        let snapshot = DomSnapshot::from_frame(&frame);
        rt.set_dom_snapshot(Some(snapshot));

        let result = rt
            .evaluate("document.querySelector('button').dispatchEvent({})")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::Bool(true)));
    }

    #[tokio::test]
    async fn test_document_add_event_listener_noop() {
        let mut rt = JsRuntime::new();
        let result = rt
            .evaluate("document.addEventListener('DOMContentLoaded', () => {}); document.removeEventListener('DOMContentLoaded', () => {}); 'ok'")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::String("ok".into())));
    }

    // ========================================
    // Mutation tests
    // ========================================

    #[tokio::test]
    async fn test_mutation_set_attribute() {
        let mut rt = JsRuntime::new();
        let html = r#"<html><body><input id="q" value="old"></body></html>"#;
        let frame = make_frame(html).await;
        let snapshot = DomSnapshot::from_frame(&frame);
        rt.set_dom_snapshot(Some(snapshot));

        rt.evaluate("document.getElementById('q').setAttribute('value', 'new')")
            .await
            .unwrap();

        let mutations = rt.drain_mutations();
        assert_eq!(mutations.len(), 1);
        match &mutations[0] {
            DomMutation::SetAttribute { name, value, .. } => {
                assert_eq!(name, "value");
                assert_eq!(value, "new");
            }
            _ => panic!("Expected SetAttribute, got {:?}", mutations[0]),
        }
    }

    #[tokio::test]
    async fn test_mutation_click() {
        let mut rt = JsRuntime::new();
        let html = r#"<html><body><button id="btn">Click</button></body></html>"#;
        let frame = make_frame(html).await;
        let snapshot = DomSnapshot::from_frame(&frame);
        rt.set_dom_snapshot(Some(snapshot));

        rt.evaluate("document.getElementById('btn').click()")
            .await
            .unwrap();
        let mutations = rt.drain_mutations();
        assert!(!mutations.is_empty());
        match &mutations[0] {
            DomMutation::ClickElement { .. } => {}
            _ => panic!("Expected ClickElement, got {:?}", mutations[0]),
        }
    }
    #[tokio::test]
    async fn test_history_pushstate_updates_length_and_location() {
        let mut rt = JsRuntime::new();
        rt.set_page_url("https://example.com/");
        let before = rt
            .evaluate("history.length")
            .await
            .unwrap()
            .value
            .and_then(|v| v.as_i64())
            .unwrap_or(-1);
        rt.evaluate("history.pushState({ page: 2 }, '', '/p2')")
            .await
            .unwrap();
        let after = rt
            .evaluate("history.length")
            .await
            .unwrap()
            .value
            .and_then(|v| v.as_i64())
            .unwrap_or(-1);
        assert_eq!(after, before + 1, "pushState must grow history.length");
        let href = rt
            .evaluate("location.href")
            .await
            .unwrap()
            .value
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        assert!(
            href.contains("/p2"),
            "location.href must reflect pushState, got {href}"
        );
        // pushState is pure client-side routing — it must not trigger navigation.
        let muts = rt.drain_mutations();
        assert!(
            !muts
                .iter()
                .any(|m| matches!(m, DomMutation::Navigate { .. })),
            "pushState must not push a Navigate mutation, got {muts:?}"
        );
    }

    #[tokio::test]
    async fn test_location_assign_triggers_navigation() {
        let mut rt = JsRuntime::new();
        rt.set_page_url("https://example.com/");
        rt.evaluate("location.assign('https://example.com/next')")
            .await
            .unwrap();
        let muts = rt.drain_mutations();
        assert!(
            muts.iter().any(
                |m| matches!(m, DomMutation::Navigate { url } if url == "https://example.com/next")
            ),
            "location.assign must queue a Navigate mutation, got {muts:?}"
        );
    }

    #[tokio::test]
    async fn test_history_back_dispatches_popstate() {
        let mut rt = JsRuntime::new();
        rt.set_page_url("https://example.com/");
        rt.evaluate(
            "globalThis.__pcount = 0;\
             addEventListener('popstate', function () { globalThis.__pcount++; });\
             history.pushState({}, '', '/a');\
             history.pushState({}, '', '/b');\
             history.back();",
        )
        .await
        .unwrap();
        let count = rt
            .evaluate("globalThis.__pcount")
            .await
            .unwrap()
            .value
            .and_then(|v| v.as_i64())
            .unwrap_or(-1);
        assert_eq!(count, 1, "history.back() must dispatch one popstate event");
    }
    #[tokio::test]
    async fn test_intersection_and_resize_observers() {
        let mut rt = JsRuntime::new();
        rt.set_page_url("https://example.com/");
        rt.evaluate(
            "globalThis.__ioFired = false;\
             globalThis.__roFired = false;\
             new IntersectionObserver(function (entries) {\
               globalThis.__ioFired = entries[0] && entries[0].isIntersecting === true;\
             }).observe({});\
             new ResizeObserver(function (entries) {\
               globalThis.__roFired = entries.length === 1 && entries[0].target !== undefined;\
             }).observe({});",
        )
        .await
        .unwrap();
        let io = rt
            .evaluate("globalThis.__ioFired")
            .await
            .unwrap()
            .value
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        assert!(io, "IntersectionObserver callback must fire on observe");
        let ro = rt
            .evaluate("globalThis.__roFired")
            .await
            .unwrap()
            .value
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        assert!(ro, "ResizeObserver callback must fire on observe");
        // Feature detection — the #1 stealth-relevant check.
        let detect = rt
            .evaluate("'IntersectionObserver' in window && 'ResizeObserver' in window")
            .await
            .unwrap()
            .value
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        assert!(detect, "observers must be detectable via 'in window'");
    }
    #[tokio::test]
    async fn test_v8_parity_js_surface() {
        let mut rt = JsRuntime::new();
        rt.set_page_url("https://example.com/");

        // Intl present + timezone is a valid IANA zone (contains '/').
        let tz = rt
            .evaluate("Intl.DateTimeFormat().resolvedOptions().timeZone")
            .await
            .unwrap()
            .value
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();
        assert!(
            tz.contains('/') || tz == "UTC",
            "Intl timeZone must be IANA or UTC (got {:?})",
            tz
        );
        // An explicitly requested timeZone is honored (real browsers do); this
        // is the #1 Intl fingerprint cross-check, so it must not leak system TZ.
        let req_tz = rt
            .evaluate(
                "new Intl.DateTimeFormat('en', { timeZone: 'UTC' }).resolvedOptions().timeZone",
            )
            .await
            .unwrap()
            .value
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();
        assert_eq!(
            req_tz, "UTC",
            "Intl must honor an explicitly requested timeZone"
        );
        let loc = rt
            .evaluate("Intl.DateTimeFormat().resolvedOptions().locale")
            .await
            .unwrap()
            .value
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();
        assert!(!loc.is_empty(), "Intl locale must be non-empty");

        // Error.stack: V8-shaped, non-empty, starts with the error name.
        let stack = rt
            .evaluate("(function(){ try { throw new Error('boom') } catch(e){ return String(e.stack) } })()")
            .await
            .unwrap()
            .value
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();
        assert!(
            stack.starts_with("Error"),
            "Error.stack must start with error name (got {:?})",
            stack
        );
        assert!(stack.contains("at "), "Error.stack must contain a frame");

        // structuredClone deep-copies plain data.
        let cloned = rt
            .evaluate("(function(){ var o = {a:1,b:{c:2}}; var c = structuredClone(o); o.b.c = 99; return c.b.c })()")
            .await
            .unwrap()
            .value
            .and_then(|v| v.as_i64())
            .unwrap_or(-1);
        assert_eq!(cloned, 2, "structuredClone must deep-clone");

        // queueMicrotask callback fires before evaluate returns (it runs jobs).
        rt.evaluate("queueMicrotask(function(){ globalThis.__qm = 'fired' })")
            .await
            .unwrap();
        let qm = rt
            .evaluate("globalThis.__qm")
            .await
            .unwrap()
            .value
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();
        assert_eq!(qm, "fired", "queueMicrotask callback must execute");

        // FinalizationRegistry present + constructible.
        let fr = rt
            .evaluate("typeof FinalizationRegistry === 'function' && typeof new FinalizationRegistry(function(){}) === 'object'")
            .await
            .unwrap()
            .value
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        assert!(fr, "FinalizationRegistry must be constructible");

        // crossOriginIsolated is a boolean (false on a normal page).
        let coi = rt
            .evaluate("typeof crossOriginIsolated === 'boolean'")
            .await
            .unwrap()
            .value
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        assert!(coi, "crossOriginIsolated must be a boolean");

        // Feature-detection: Intl + FinalizationRegistry detectable via 'in window'.
        let detect = rt
            .evaluate("'Intl' in window && 'FinalizationRegistry' in window")
            .await
            .unwrap()
            .value
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        assert!(
            detect,
            "Intl and FinalizationRegistry must be detectable on window"
        );
    }
    #[tokio::test]
    async fn test_custom_elements_registry_and_shadow_dom() {
        let mut rt = JsRuntime::new();
        rt.set_page_url("https://example.com/");

        // customElements registry: define stores, get retrieves.
        rt.evaluate(
            "class FooBar extends HTMLElement {}\
             customElements.define('foo-bar', FooBar);\
             globalThis.__got = (customElements.get('foo-bar') === FooBar);",
        )
        .await
        .unwrap();
        let got = rt
            .evaluate("globalThis.__got")
            .await
            .unwrap()
            .value
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        assert!(got, "customElements.define + get must round-trip");

        // whenDefined resolves for an already-defined element (microtask fires
        // after the scheduling evaluate returns — read in a follow-up eval).
        rt.evaluate("customElements.whenDefined('foo-bar').then(function(c){ globalThis.__wd = (c === FooBar); })")
            .await
            .unwrap();
        let wd = rt
            .evaluate("globalThis.__wd")
            .await
            .unwrap()
            .value
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        assert!(
            wd,
            "customElements.whenDefined must resolve to the constructor"
        );

        // Invalid name throws.
        let bad = rt
            .evaluate("(function(){ try { customElements.define('NoHyphen', function(){}); return false } catch(e){ return true } })()")
            .await
            .unwrap()
            .value
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        assert!(
            bad,
            "customElements.define must reject names without a hyphen"
        );

        // attachShadow returns a root with .host + .mode; shadowRoot getter.
        rt.evaluate(
            "var host = Object.create(Element.prototype);\
             var sr = Element.prototype.attachShadow.call(host, { mode: 'open' });\
             globalThis.__srHost = (sr.host === host);\
             globalThis.__srMode = (sr.mode === 'open');\
             globalThis.__sRoot = (host.shadowRoot === sr);",
        )
        .await
        .unwrap();
        let sr_host = rt
            .evaluate("globalThis.__srHost")
            .await
            .unwrap()
            .value
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let sr_mode = rt
            .evaluate("globalThis.__srMode")
            .await
            .unwrap()
            .value
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let s_root = rt
            .evaluate("globalThis.__sRoot")
            .await
            .unwrap()
            .value
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        assert!(sr_host, "attachShadow root must reference host");
        assert!(sr_mode, "attachShadow root must carry mode");
        assert!(
            s_root,
            "element.shadowRoot must return the attached open root"
        );

        // Feature-detection surface.
        let detect = rt
            .evaluate("'customElements' in window && 'attachShadow' in Element.prototype && typeof HTMLElement === 'function' && typeof ShadowRoot === 'function'")
            .await
            .unwrap()
            .value
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        assert!(detect, "web component surface must be feature-detectable");
    }

    #[tokio::test]
    async fn test_mutation_input_value_setter() {
        let mut rt = JsRuntime::new();
        let html = r#"<html><body><input id="inp" value="old"></body></html>"#;
        let frame = make_frame(html).await;
        let snapshot = DomSnapshot::from_frame(&frame);
        rt.set_dom_snapshot(Some(snapshot));

        rt.evaluate("document.getElementById('inp').value = 'new'")
            .await
            .unwrap();
        let mutations = rt.drain_mutations();
        assert_eq!(mutations.len(), 1);
        match &mutations[0] {
            DomMutation::InputElement { value, .. } => {
                assert_eq!(value, "new");
            }
            _ => panic!("Expected InputElement, got {:?}", mutations[0]),
        }
    }

    #[tokio::test]
    async fn test_mutation_value_getter() {
        let mut rt = JsRuntime::new();
        let html = r#"<html><body><input id="inp" value="hello"></body></html>"#;
        let frame = make_frame(html).await;
        let snapshot = DomSnapshot::from_frame(&frame);
        rt.set_dom_snapshot(Some(snapshot));

        let result = rt
            .evaluate("document.getElementById('inp').value")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value, Some(Value::String("hello".into())));
    }

    #[tokio::test]
    async fn test_drain_mutations_clears_buffer() {
        let mut rt = JsRuntime::new();
        let html = r#"<html><body><button id="btn">Click</button></body></html>"#;
        let frame = make_frame(html).await;
        let snapshot = DomSnapshot::from_frame(&frame);
        rt.set_dom_snapshot(Some(snapshot));

        rt.evaluate("document.getElementById('btn').click()")
            .await
            .unwrap();
        let first = rt.drain_mutations();
        assert!(!first.is_empty());

        let second = rt.drain_mutations();
        assert!(second.is_empty());
    }

    #[tokio::test]
    async fn test_set_dom_snapshot_clears_mutations() {
        let mut rt = JsRuntime::new();
        let html = r#"<html><body><button id="btn">Click</button></body></html>"#;
        let frame = make_frame(html).await;
        let snapshot = DomSnapshot::from_frame(&frame);
        rt.set_dom_snapshot(Some(snapshot));

        rt.evaluate("document.getElementById('btn').click()")
            .await
            .unwrap();

        let snapshot2 = DomSnapshot::from_frame(&frame);
        rt.set_dom_snapshot(Some(snapshot2));
        let mutations = rt.drain_mutations();
        assert!(mutations.is_empty());
    }

    #[tokio::test]
    async fn test_mutation_set_attribute_via_query_selector() {
        let mut rt = JsRuntime::new();
        let html = r#"<html><body><a href="/page" id="link">go</a></body></html>"#;
        let frame = make_frame(html).await;
        let snapshot = DomSnapshot::from_frame(&frame);
        rt.set_dom_snapshot(Some(snapshot));

        rt.evaluate("document.querySelector('a').setAttribute('href', '/new-page')")
            .await
            .unwrap();
        let mutations = rt.drain_mutations();
        assert_eq!(mutations.len(), 1);
        match &mutations[0] {
            DomMutation::SetAttribute { name, value, .. } => {
                assert_eq!(name, "href");
                assert_eq!(value, "/new-page");
            }
            _ => panic!("Expected SetAttribute, got {:?}", mutations[0]),
        }
    }

    // ------------------------------------------------------------------------
    // atob / btoa / URL / URLSearchParams tests
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn test_btoa_basic() {
        let mut rt = JsRuntime::new();
        let result = rt.evaluate("btoa('Hello')").await.unwrap();
        assert!(result.is_ok());
        let val = result.value.unwrap();
        assert!(val.is_string());
        // Should be base64 encoded
    }

    #[tokio::test]
    async fn test_atob_basic() {
        let mut rt = JsRuntime::new();
        let result = rt.evaluate("atob('SGVsbG8=')").await.unwrap();
        assert!(result.is_ok());
        let val = result.value.unwrap();
        assert_eq!(val, Value::String("Hello".into()));
    }

    #[tokio::test]
    async fn test_url_class() {
        let mut rt = JsRuntime::new();
        let result = rt
            .evaluate("new URL('https://example.com:8080/path?foo=bar#hash').hostname")
            .await
            .unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_url_search_params() {
        let mut rt = JsRuntime::new();
        let result = rt
            .evaluate("new URLSearchParams('foo=bar&baz=1').get('foo')")
            .await
            .unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_array_from() {
        let mut rt = JsRuntime::new();

        // Array.from with array
        let result = rt.evaluate("Array.from([1, 2, 3]).length").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value.unwrap(), 3);

        // Array.from with array-like object
        let result = rt
            .evaluate("Array.from({length: 2, 0: 'a', 1: 'b'}).join(',')")
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value.unwrap(), "a,b");

        // Array.from with single string value → iterates chars (array-like with .length)
        let result = rt.evaluate("Array.from('hello').length").await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value.unwrap(), 5);
    }

    // ── Layout evaluation integration tests ──

    #[tokio::test]
    async fn test_get_computed_style_display_none() {
        let mut rt = JsRuntime::new();
        let html = r##"<html><body><div id="box" style="display:none">hidden</div></body></html>"##;
        let frame = make_frame(html).await;
        rt.set_dom_snapshot(Some(DomSnapshot::from_frame(&frame)));

        let result = rt
            .evaluate(r#"getComputedStyle(document.getElementById("box"))._visible"#)
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value.unwrap(), false);
    }

    #[tokio::test]
    async fn test_get_computed_style_visible_div() {
        let mut rt = JsRuntime::new();
        let html = r##"<html><body><div id="box">visible</div></body></html>"##;
        let frame = make_frame(html).await;
        rt.set_dom_snapshot(Some(DomSnapshot::from_frame(&frame)));

        let result = rt
            .evaluate(r#"getComputedStyle(document.getElementById("box"))._visible"#)
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value.unwrap(), true);
    }

    #[tokio::test]
    async fn test_get_computed_style_color() {
        let mut rt = JsRuntime::new();
        let html = r##"<html><body><p id="red" style="color:red">Red</p></body></html>"##;
        let frame = make_frame(html).await;
        rt.set_dom_snapshot(Some(DomSnapshot::from_frame(&frame)));

        let result = rt
            .evaluate(r#"getComputedStyle(document.getElementById("red")).color"#)
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value.unwrap(), "#ff0000");
    }

    #[tokio::test]
    async fn test_get_computed_style_interactive_button() {
        let mut rt = JsRuntime::new();
        let html = r##"<html><body><button id="btn">Click</button></body></html>"##;
        let frame = make_frame(html).await;
        rt.set_dom_snapshot(Some(DomSnapshot::from_frame(&frame)));

        let result = rt
            .evaluate(r#"getComputedStyle(document.getElementById("btn"))._interactive"#)
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value.unwrap(), true);
    }

    #[tokio::test]
    async fn test_get_computed_style_disabled_button() {
        let mut rt = JsRuntime::new();
        let html = r##"<html><body><button id="btn" disabled>Click</button></body></html>"##;
        let frame = make_frame(html).await;
        rt.set_dom_snapshot(Some(DomSnapshot::from_frame(&frame)));

        let result = rt
            .evaluate(r#"getComputedStyle(document.getElementById("btn"))._interactive"#)
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value.unwrap(), false);
    }

    #[tokio::test]
    async fn test_get_computed_style_get_property_value() {
        let mut rt = JsRuntime::new();
        let html =
            r##"<html><body><div id="box" style="position:absolute">Abs</div></body></html>"##;
        let frame = make_frame(html).await;
        rt.set_dom_snapshot(Some(DomSnapshot::from_frame(&frame)));

        let result = rt
            .evaluate(
                r#"getComputedStyle(document.getElementById("box")).getPropertyValue("position")"#,
            )
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value.unwrap(), "absolute");
    }

    #[tokio::test]
    async fn test_get_bounding_client_rect() {
        let mut rt = JsRuntime::new();
        let html = r##"<html><body><div id="box" style="width:200px;height:100px">Box</div></body></html>"##;
        let frame = make_frame(html).await;
        rt.set_dom_snapshot(Some(DomSnapshot::from_frame(&frame)));

        let result = rt
            .evaluate(r#"var r = document.getElementById("box").getBoundingClientRect(); r.width + "x" + r.height"#)
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value.unwrap(), "200x100");
    }

    #[tokio::test]
    async fn test_offset_width_height() {
        let mut rt = JsRuntime::new();
        let html = r##"<html><body><div id="box" style="width:300px;height:150px">Box</div></body></html>"##;
        let frame = make_frame(html).await;
        rt.set_dom_snapshot(Some(DomSnapshot::from_frame(&frame)));

        let result = rt
            .evaluate(r#"document.getElementById("box").offsetWidth + "x" + document.getElementById("box").offsetHeight"#)
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.value.unwrap(), "300x150");
    }

    // ---- WebSocket integration helpers (Phase 4) ----

    /// Spawn a local echo WebSocket server on an ephemeral port. Returns
    /// `(port, join_handle)`. Accepts exactly one connection and echoes frames.
    fn spawn_echo_ws_server() -> (u16, std::thread::JoinHandle<()>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                use futures::{SinkExt, StreamExt};
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                tx.send(listener.local_addr().unwrap().port()).unwrap();
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                while let Some(Ok(msg)) = ws.next().await {
                    if msg.is_close() {
                        break;
                    }
                    if msg.is_text() || msg.is_binary() {
                        ws.send(msg).await.unwrap();
                    }
                }
            });
        });
        let port = rx.recv().unwrap();
        (port, handle)
    }

    fn setup_ws_runtime() -> JsRuntime {
        let mut rt = JsRuntime::new();
        let (req_tx, req_rx) = std::sync::mpsc::channel::<WsReqMsg>();
        let (ev_tx, ev_rx) = std::sync::mpsc::channel::<WsEvent>();
        rt.set_ws_channel(req_tx, ev_rx);
        std::thread::spawn(move || crate::session::handle_ws_requests(req_rx, ev_tx));
        rt
    }

    /// ws.send(Uint8Array) must produce a binary frame, not coerce to text.
    /// Uses a controlled req channel (no real server) so we can inspect the
    /// emitted `WsReqMsg::Send` directly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_ws_send_binary_from_typed_array() {
        let mut rt = JsRuntime::new();
        let (req_tx, req_rx) = std::sync::mpsc::channel::<WsReqMsg>();
        let (_ev_tx, ev_rx) = std::sync::mpsc::channel::<WsEvent>();
        rt.set_ws_channel(req_tx, ev_rx);

        let r = rt
            .evaluate(
                "var ws = new WebSocket('ws://127.0.0.1:1/x');\
                 ws.send(new Uint8Array([1,2,3]));\
                 ws.close();\
                 'sent'",
            )
            .await
            .unwrap();
        assert_eq!(
            r.value
                .as_ref()
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            Some("sent".to_string()),
            "eval failed: {:?}",
            r.exception
        );

        // Drain: Connect first, then Send — capture the Send payload.
        let mut got: Option<WsData> = None;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            match req_rx.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(WsReqMsg::Send { data, .. }) => {
                    got = Some(data);
                    break;
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        let data = got.expect("no Send captured on the ws req channel");
        assert_eq!(data, WsData::Binary(vec![1, 2, 3]), "expected binary frame");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_ws_onopen_and_echo() {
        let (port, _srv) = spawn_echo_ws_server();
        let mut rt = setup_ws_runtime();
        let url = format!("ws://127.0.0.1:{port}");
        let r = rt
            .evaluate(&format!(
                "var ws = new WebSocket('{url}');\
                 ws.onopen = function() {{ ws.send('ping'); }};\
                 ws.onmessage = function(e) {{ globalThis.__got = e.data; }};"
            ))
            .await
            .unwrap();
        assert!(r.is_ok());
        let got = rt.evaluate("__got").await.unwrap();
        assert_eq!(got.value, Some(serde_json::Value::String("ping".into())));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_ws_readystate_and_close() {
        let (port, _srv) = spawn_echo_ws_server();
        let mut rt = setup_ws_runtime();
        let url = format!("ws://127.0.0.1:{port}");
        rt.evaluate(&format!(
            "var ws = new WebSocket('{url}');\
             globalThis.__rs0 = ws.readyState;\
             ws.onopen = function() {{ globalThis.__rs1 = ws.readyState; ws.close(1000, 'bye'); }};\
             ws.onclose = function(e) {{ globalThis.__rs3 = ws.readyState; globalThis.__code = e.code; }};"
        ))
        .await
        .unwrap();
        assert_eq!(
            rt.evaluate("__rs0").await.unwrap().value,
            Some(serde_json::Value::Number(0.into()))
        );
        assert_eq!(
            rt.evaluate("__rs1").await.unwrap().value,
            Some(serde_json::Value::Number(1.into()))
        );
        assert_eq!(
            rt.evaluate("__rs3").await.unwrap().value,
            Some(serde_json::Value::Number(3.into()))
        );
        assert_eq!(
            rt.evaluate("__code").await.unwrap().value,
            Some(serde_json::Value::Number(1000.into()))
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_ws_connect_failure() {
        let mut rt = setup_ws_runtime();
        // Port 1 is reserved and never accepts — connect fails fast.
        let r = rt
            .evaluate(
                "var ws = new WebSocket('ws://127.0.0.1:1');\
                 ws.onclose = function(e) { globalThis.__code = e.code; };",
            )
            .await
            .unwrap();
        assert!(r.is_ok());
        let code = rt.evaluate("__code").await.unwrap();
        assert_eq!(code.value, Some(serde_json::Value::Number(1006.into())));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_ws_concurrent_sockets() {
        let (port_a, _a) = spawn_echo_ws_server();
        let (port_b, _b) = spawn_echo_ws_server();
        let mut rt = setup_ws_runtime();
        let url_a = format!("ws://127.0.0.1:{port_a}");
        let url_b = format!("ws://127.0.0.1:{port_b}");
        rt.evaluate(&format!(
            "var a = new WebSocket('{url_a}');\
             var b = new WebSocket('{url_b}');\
             a.onopen = function() {{ a.send('A'); }};\
             b.onopen = function() {{ b.send('B'); }};\
             a.onmessage = function(e) {{ globalThis.__ga = e.data; }};\
             b.onmessage = function(e) {{ globalThis.__gb = e.data; }};"
        ))
        .await
        .unwrap();
        assert_eq!(
            rt.evaluate("__ga").await.unwrap().value,
            Some(serde_json::Value::String("A".into()))
        );
        assert_eq!(
            rt.evaluate("__gb").await.unwrap().value,
            Some(serde_json::Value::String("B".into()))
        );
    }

    #[tokio::test]
    async fn test_element_matches_and_closest() {
        let mut rt = JsRuntime::new();
        let html =
            "<html><body><div class=\"outer\"><span class=\"inner\">hi</span></div></body></html>";
        let frame = make_frame(html).await;
        rt.set_dom_snapshot(Some(DomSnapshot::from_frame(&frame)));

        let r = rt
            .evaluate(
                "var span = document.querySelector('span');\
                 globalThis.__m1 = span.matches('.inner');\
                 globalThis.__m2 = span.matches('div');\
                 globalThis.__c1 = span.closest('.outer') !== null;\
                 globalThis.__c2 = span.closest('span').className === 'inner';",
            )
            .await
            .unwrap();
        assert!(r.is_ok());
        assert_eq!(
            rt.evaluate("__m1").await.unwrap().value,
            Some(serde_json::Value::Bool(true))
        );
        assert_eq!(
            rt.evaluate("__m2").await.unwrap().value,
            Some(serde_json::Value::Bool(false))
        );
        assert_eq!(
            rt.evaluate("__c1").await.unwrap().value,
            Some(serde_json::Value::Bool(true))
        );
        assert_eq!(
            rt.evaluate("__c2").await.unwrap().value,
            Some(serde_json::Value::Bool(true))
        );
    }

    #[tokio::test]
    async fn test_url_create_object_url() {
        let mut rt = JsRuntime::new();
        let r = rt
            .evaluate(
                "typeof URL.createObjectURL === 'function' && URL.createObjectURL({}).startsWith('blob:')",
            )
            .await
            .unwrap();
        assert!(r.is_ok());
        assert_eq!(r.value, Some(serde_json::Value::Bool(true)));
    }

    #[tokio::test]
    async fn test_abort_controller() {
        let mut rt = JsRuntime::new();
        let r = rt
            .evaluate(
                "var ac = new AbortController(); var fired = false;\
                 ac.signal.addEventListener('abort', function(){ fired = true; });\
                 ac.abort('done');\
                 globalThis.__ab = ac.signal.aborted && fired && ac.signal.reason === 'done';",
            )
            .await
            .unwrap();
        assert!(r.is_ok());
        assert_eq!(r.value, Some(serde_json::Value::Bool(true)));
    }
}
