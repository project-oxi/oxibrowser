#![allow(unused_unsafe, unused_variables, unused_mut, unused_imports, clippy::clone_on_copy, clippy::arc_with_non_send_sync)]
//! JavaScript runtime using boa_engine with a persistent context.
//!
//! boa_engine is a pure Rust JavaScript engine (ES2024+), no C dependencies.
//! TEST EDIT
//! Provides real JS evaluation with console.log and Math, JSON, etc.
//! TEST EDIT
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

use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use boa_engine::object::builtins::JsArray;
use boa_engine::object::FunctionObjectBuilder;
use boa_engine::property::Attribute;
use boa_engine::{js_string, Context, JsString, JsValue, NativeFunction, Source};
use serde_json::Value;
use base64::Engine;

use crate::error::{CoreError, Result};
use crate::js::dom_snapshot::{DomMutation, DomNode, DomSnapshot};
use crate::js::job_queue::TokioJobQueue;
use std::rc::Rc;
use std::time::{Duration, Instant};

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

/// Commands sent from the async main thread to the JS thread.
enum JsCommand {
    /// Evaluate a JS expression.
    Eval {
        expression: String,
        /// Timeout in ms for this eval. None = use default.
        timeout_ms: Option<u64>,
        /// Max loop iterations. None = use default.
        max_loop_iterations: Option<u64>,
        /// Max recursion depth. None = use default.
        max_recursion: Option<usize>,
        /// Max operand stack size. None = use default.
        max_stack_size: Option<usize>,
    },
    /// Set a global variable in the persistent Context.
    SetGlobal { name: String, value: Value },
    /// Update the DOM snapshot available to `document` object.
    SetDom { snapshot: Option<DomSnapshot> },
    /// Update the page URL (for window.location).
    SetPageUrl { url: String },
    /// Set the fetch channel so JS can make real HTTP requests.
    SetFetchChannel { tx: std::sync::mpsc::Sender<FetchRequestMsg> },
    /// Set the localStorage sync channel so JS operations propagate to Session.
    SetLocalStorageChannel { tx: std::sync::mpsc::Sender<LocalStorageMsg> },
    /// Set the history command channel so JS history API calls propagate to Session.
    SetHistoryChannel { tx: std::sync::mpsc::Sender<HistoryCommandType> },
    /// Shut down the JS thread.
    Shutdown,
}

/// Commands sent from JS history API to Session for navigation.
#[derive(Debug, Clone)]
pub enum HistoryCommandType {
    /// history.pushState(state, title, url)
    PushState { url: String },
    /// history.replaceState(state, title, url)
    ReplaceState { url: String },
    /// history.back()
    Back,
    /// history.forward()
    Forward,
    /// history.go(delta)
    Go { delta: i32 },
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
}

// ---------------------------------------------------------------------------
// Fetch message types
// ---------------------------------------------------------------------------

/// A fetch request from JS.
pub struct FetchRequestMsg {
    /// URL to fetch.
    pub url: String,
    /// HTTP method.
    pub method: String,
    /// Request headers (name, value pairs).
    pub headers: Vec<(String, String)>,
    /// Request body (if any).
    pub body: Option<String>,
    /// Channel to send the response back.
    pub response_tx: std::sync::mpsc::Sender<FetchResponseMsg>,
}

/// HTTP response sent back to the JS thread.
pub struct FetchResponseMsg {
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
    /// Viewport width (pixels, 0 = headless).
    pub viewport_width: u32,
    /// Viewport height (pixels, 0 = headless).
    pub viewport_height: u32,
}

impl Default for JsRuntimeConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 5000,
            max_recursion: 100,
            max_loop_iterations: 100_000,
            max_stack_size: 1024,
            viewport_width: 1280,
            viewport_height: 720,
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
            viewport_width: config.viewport_width,
            viewport_height: config.viewport_height,
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
    /// Channel to receive responses from the JS thread.
    resp_rx: Mutex<Receiver<JsResponse>>,
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
}

impl JsRuntime {
    /// Create a new JS runtime with default configuration.
    ///
    /// Spawns a dedicated OS thread that owns the `boa_engine::Context`.
    pub fn new() -> Self {
        Self::with_config(JsRuntimeConfig::default())
    }

    /// Create a new JS runtime with the given configuration.
    pub fn with_config(config: JsRuntimeConfig) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<JsCommand>();
        let (resp_tx, resp_rx) = mpsc::channel::<JsResponse>();
        let console_output = Arc::new(RwLock::new(Vec::<String>::new()));
        let mutations = Arc::new(RwLock::new(Vec::<DomMutation>::new()));

        // Spawn JS thread
        let console_output_clone = console_output.clone();
        let mutations_clone = mutations.clone();
        let viewport = (config.viewport_width, config.viewport_height);
        let _local_storage = Arc::new(RwLock::new(HashMap::<String, String>::new()));
        std::thread::Builder::new()
            .name("oxibrowser-js".into())
            .spawn(move || {
                js_thread_loop(
                    cmd_rx,
                    resp_tx,
                    console_output_clone,
                    mutations_clone,
                    viewport,
                    None,
                );
            })
            .expect("failed to spawn JS thread");

        Self {
            cmd_tx,
            resp_rx: Mutex::new(resp_rx),
            console_output,
            mutations,
            globals: RwLock::new(HashMap::new()),
            config,
            fetch_tx: None,
        }
    }

    /// Set the channel for fetch requests. Must be called before JS can use fetch().
    pub fn set_fetch_channel(&mut self, tx: std::sync::mpsc::Sender<FetchRequestMsg>) {
        self.fetch_tx = Some(tx.clone());
        self.cmd_tx
            .send(JsCommand::SetFetchChannel { tx })
            .expect("JS thread has died");
        let resp = self
            .resp_rx
            .lock()
            .expect("resp_rx lock poisoned")
            .recv()
            .expect("JS thread has died");
        match resp {
            JsResponse::Done => {}
            _ => panic!("unexpected response"),
        }
    }

    /// Set the channel for localStorage sync.
    ///
    /// When JS calls localStorage.setItem/removeItem/clear, the operation
    /// is forwarded to Session via this channel.
    pub fn set_local_storage_channel(&mut self, tx: std::sync::mpsc::Sender<LocalStorageMsg>) {
        self.cmd_tx
            .send(JsCommand::SetLocalStorageChannel { tx })
            .expect("JS thread has died");
        let resp = self
            .resp_rx
            .lock()
            .expect("resp_rx lock poisoned")
            .recv()
            .expect("JS thread has died");
        match resp {
            JsResponse::Done => {}
            _ => panic!("unexpected response"),
        }
    }

    /// Set the channel for History API commands.
    ///
    /// When JS calls history.pushState/back/forward/go, the command
    /// is forwarded to Session via this channel.
    pub fn set_history_channel(&mut self, tx: std::sync::mpsc::Sender<HistoryCommandType>) {
        self.cmd_tx
            .send(JsCommand::SetHistoryChannel { tx })
            .expect("JS thread has died");
        let resp = self
            .resp_rx
            .lock()
            .expect("resp_rx lock poisoned")
            .recv()
            .expect("JS thread has died");
        match resp {
            JsResponse::Done => {}
            _ => panic!("unexpected response"),
        }
    }

    /// Evaluate a JavaScript expression and return the result.
    ///
    /// JS state persists across calls — variables, functions, closures
    /// defined in one `evaluate()` are available in the next.
    ///
    /// If the evaluation exceeds the configured timeout, the JS context
    /// is reset and previous state (variables, functions) is lost.
    pub async fn evaluate(&mut self, expression: &str) -> Result<JsEvalResult> {
        self.evaluate_with_timeout(expression, None).await
    }

    /// Evaluate a JavaScript expression with an explicit timeout override.
    ///
    /// If `timeout_ms` is `None`, uses the default from config.
    pub async fn evaluate_with_timeout(
        &mut self,
        expression: &str,
        timeout_ms: Option<u64>,
    ) -> Result<JsEvalResult> {
        // Clear shared console buffer
        self.console_output.write().clear();

        // Send eval command with limits
        self.cmd_tx
            .send(JsCommand::Eval {
                expression: expression.to_string(),
                timeout_ms: Some(timeout_ms.unwrap_or(self.config.timeout_ms)),
                max_loop_iterations: Some(self.config.max_loop_iterations),
                max_recursion: Some(self.config.max_recursion),
                max_stack_size: Some(self.config.max_stack_size),
            })
            .expect("JS thread has died");

        // Wait for response
        let resp = self
            .resp_rx
            .lock()
            .expect("resp_rx lock poisoned")
            .recv()
            .expect("JS thread has died");

        match resp {
            JsResponse::EvalResult {
                value,
                exception,
                console_output,
                timed_out,
            } => {
                if timed_out {
                    // Context was reset — clear our globals tracking too
                    // (they're stale in the new context)
                    // We do NOT clear self.globals because set_global can re-inject them.
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
            JsResponse::Done => Ok(JsEvalResult::void()),
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
    ///
    /// After calling this, the internal buffer is empty.
    pub fn drain_mutations(&self) -> Vec<DomMutation> {
        let mut guard = self.mutations.write();
        std::mem::take(&mut *guard)
    }

    /// Set a global variable — injected into the persistent JS Context.
    pub fn set_global(&mut self, name: impl Into<String>, value: Value) {
        let name = name.into();
        self.globals.write().insert(name.clone(), value.clone());

        // Also inject into the persistent JS Context
        self.cmd_tx
            .send(JsCommand::SetGlobal {
                name,
                value: value.clone(),
            })
            .expect("JS thread has died");

        // Wait for ack
        let resp = self
            .resp_rx
            .lock()
            .expect("resp_rx lock poisoned")
            .recv()
            .expect("JS thread has died");
        let _ = resp; // JsResponse::Done
    }

    /// Get a global variable (Rust-side tracking).
    pub fn get_global(&self, name: &str) -> Option<Value> {
        self.globals.read().get(name).cloned()
    }

    /// Set the DOM snapshot (called after navigate).
    ///
    /// Sends the snapshot to the JS thread so that `document.querySelector`
    /// and friends operate on real DOM data. Also clears the mutation buffer.
    pub fn set_dom_snapshot(&mut self, snapshot: Option<DomSnapshot>) {
        // Clear mutations when snapshot changes
        self.mutations.write().clear();

        self.cmd_tx
            .send(JsCommand::SetDom { snapshot })
            .expect("JS thread has died");
        // Wait for ack
        let resp = self
            .resp_rx
            .lock()
            .expect("resp_rx lock poisoned")
            .recv()
            .expect("JS thread has died");
        let _ = resp;
    }

    /// Update the page URL (used for window.location).
    pub fn set_page_url(&mut self, url: &str) {
        self.cmd_tx
            .send(JsCommand::SetPageUrl {
                url: url.to_string(),
            })
            .expect("JS thread has died");
        let resp = self
            .resp_rx
            .lock()
            .expect("resp_rx lock poisoned")
            .recv()
            .expect("JS thread has died");
        let _ = resp;
    }
}

impl Default for JsRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for JsRuntime {
    fn drop(&mut self) {
        // Signal the JS thread to shut down
        let _ = self.cmd_tx.send(JsCommand::Shutdown);
        // Best-effort: don't panic in drop if mutex is poisoned or thread is dead
        if let Ok(guard) = self.resp_rx.lock() {
            let _ = guard.recv();
        }
    }
}

// ---------------------------------------------------------------------------
// JS thread
// ---------------------------------------------------------------------------

/// Main loop for the JS thread.
///
/// Creates a single `Context`, registers globals, and processes commands
/// until a `Shutdown` is received.
fn js_thread_loop(
    cmd_rx: Receiver<JsCommand>,
    resp_tx: Sender<JsResponse>,
    console_output: Arc<RwLock<Vec<String>>>,
    mutations: Arc<RwLock<Vec<DomMutation>>>,
    viewport: (u32, u32),
    fetch_tx: Option<std::sync::mpsc::Sender<FetchRequestMsg>>,
) {
    let fetch_tx_arc: Arc<RwLock<Option<std::sync::mpsc::Sender<FetchRequestMsg>>>> =
        Arc::new(RwLock::new(None));
    let local_storage_tx_arc: Arc<RwLock<Option<std::sync::mpsc::Sender<LocalStorageMsg>>>> =
        Arc::new(RwLock::new(None));
    let history_tx_arc: Arc<RwLock<Option<std::sync::mpsc::Sender<HistoryCommandType>>>> =
        Arc::new(RwLock::new(None));
    let dom_snapshot: Arc<RwLock<Option<DomSnapshot>>> = Arc::new(RwLock::new(None));
    let (mut ctx, mut job_queue) = create_context(
        &console_output,
        &dom_snapshot,
        &mutations,
        viewport,
        "",
        "OxiBrowser/0.2",
        &fetch_tx_arc,
        &history_tx_arc,
    );

    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            JsCommand::Eval {
                expression,
                timeout_ms,
                max_loop_iterations,
                max_recursion,
                max_stack_size,
            } => {
                // Clear console buffer before eval
                console_output.write().clear();
                // Clear mutation buffer before eval
                mutations.write().clear();

                // Apply runtime limits to context
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

                let source = Source::from_bytes(&expression);
                let result = ctx.eval(source);

                // Drain Promise microtasks queued during eval
                ctx.run_jobs();

                // Drain due timers and fire their callbacks
                drain_timers(&job_queue, &mut ctx);

                let elapsed = start.elapsed();
                let console = console_output.read().clone();

                // Check if we timed out
                if elapsed.as_millis() > timeout as u128 {
                    // Context may be in a bad state — recreate it
                    let (new_ctx, new_queue) = create_context(
                        &console_output,
                        &dom_snapshot,
                        &mutations,
                        viewport,
                        "",
                        "OxiBrowser/0.2",
                        &fetch_tx_arc,
                        &history_tx_arc,
                    );
                    ctx = new_ctx;
                    job_queue = new_queue;
                    let _ = resp_tx.send(JsResponse::EvalResult {
                        value: None,
                        exception: Some(format!(
                            "JS execution timed out after {}ms — context was reset, previous state lost",
                            elapsed.as_millis()
                        )),
                        console_output: console,
                        timed_out: true,
                    });
                    continue;
                }

                match result {
                    Ok(value) => {
                        let json_value = js_value_to_json(&value, &mut ctx);
                        let _ = resp_tx.send(JsResponse::EvalResult {
                            value: Some(json_value),
                            exception: None,
                            console_output: console,
                            timed_out: false,
                        });
                    }
                    Err(err) => {
                        let msg = format_js_error(&err, &mut ctx);
                        // Check if it was a runtime limit error (loop/recursion/stack)
                        let is_runtime_limit = msg.contains("Maximum loop iteration limit")
                            || msg.contains("exceeded the maximum call stack size")
                            || msg.contains("recursion limit");

                        if is_runtime_limit {
                            // Runtime limit hit — context is still valid,
                            // but the partial execution may have left state.
                            // We don't reset the context here since boa throws
                            // a catchable error (the context is fine).
                        }

                        let _ = resp_tx.send(JsResponse::EvalResult {
                            value: None,
                            exception: Some(msg),
                            console_output: console,
                            timed_out: false,
                        });
                    }
                }
            }
            JsCommand::SetGlobal { name, value } => {
                let js_val = json_to_js_value(&value, &mut ctx);
                let _ = ctx.register_global_property(
                    JsString::from(name.as_str()),
                    js_val,
                    Attribute::all(),
                );
                let _ = resp_tx.send(JsResponse::Done);
            }
            JsCommand::SetDom { snapshot } => {
                *dom_snapshot.write() = snapshot;
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
                let _ = resp_tx.send(JsResponse::Done);
            }
            JsCommand::SetPageUrl { url } => {
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
                    "OxiBrowser/0.2",
                    &fetch_tx_arc,
                    &history_tx_arc,
                );
                // Also re-register localStorage on URL change (clears JS-side storage)
                let empty = std::collections::HashMap::new();
                register_local_storage(&mut ctx, empty, &dom_snapshot_ref, local_storage_tx_arc.clone());
                let _ = resp_tx.send(JsResponse::Done);
            }
            JsCommand::SetLocalStorageChannel { tx } => {
                *local_storage_tx_arc.write() = Some(tx);
                let _ = resp_tx.send(JsResponse::Done);
            }
            JsCommand::SetFetchChannel { tx } => {
                *fetch_tx_arc.write() = Some(tx);
                let _ = resp_tx.send(JsResponse::Done);
            }
            JsCommand::SetHistoryChannel { tx } => {
                *history_tx_arc.write() = Some(tx);
                let _ = resp_tx.send(JsResponse::Done);
            }
            JsCommand::Shutdown => {
                let _ = resp_tx.send(JsResponse::Done);
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
fn drain_timers(queue: &Rc<TokioJobQueue>, ctx: &mut Context) {
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

fn create_context(
    output: &Arc<RwLock<Vec<String>>>,
    dom_snapshot: &Arc<RwLock<Option<DomSnapshot>>>,
    mutations: &Arc<RwLock<Vec<DomMutation>>>,
    viewport: (u32, u32),
    page_url: &str,
    user_agent: &str,
    fetch_tx_arc: &Arc<RwLock<Option<std::sync::mpsc::Sender<FetchRequestMsg>>>>,
    history_tx_arc: &Arc<RwLock<Option<std::sync::mpsc::Sender<HistoryCommandType>>>>,
) -> (Context, Rc<TokioJobQueue>) {
    let job_queue = Rc::new(TokioJobQueue::new());
    let mut context = Context::builder()
        .job_queue(job_queue.clone())
        .build()
        .expect("failed to build boa Engine context");

    // --- Console functions ---

    macro_rules! console_fn {
        ($out:expr) => {
            unsafe {
                NativeFunction::from_closure(
                    move |_this: &JsValue, args: &[JsValue], ctx: &mut Context| {
                        let mut line = String::new();
                        for (i, arg) in args.iter().enumerate() {
                            if i > 0 {
                                line.push(' ');
                            }
                            let s = arg
                                .to_string(ctx)
                                .map(|s| s.to_std_string_escaped())
                                .unwrap_or_else(|_| "undefined".to_string());
                            line.push_str(&s);
                        }
                        {
                            let mut guard = $out.write();
                            guard.push(line);
                        }
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

    let log_fn = console_fn!(out_log);

    // Register standalone `log(...)` function
    let _ = context.register_global_callable(js_string!("log"), 1, log_fn.clone());

    // Build console object
    let console = boa_engine::object::ObjectInitializer::new(&mut context)
        .function(log_fn, js_string!("log"), 1)
        .function(console_fn!(out_warn), js_string!("warn"), 1)
        .function(console_fn!(out_error), js_string!("error"), 1)
        .function(console_fn!(out_info), js_string!("info"), 1)
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
            let delay_ms = args
                .get(1)
                .and_then(|v| v.as_number())
                .unwrap_or(0.0) as u64;
            let cb_args: Vec<JsValue> = args[2..].to_vec();

            if let Some(func) = callback.as_object().cloned() {
                if func.is_callable() {
                    let deadline = Instant::now() + Duration::from_millis(delay_ms);
                    let id = timer_queue_st.schedule_timer(
                        deadline,
                        func,
                        cb_args,
                        false,
                        None,
                    );
                    return Ok(JsValue::from(id as f64));
                }
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
            let delay_ms = args
                .get(1)
                .and_then(|v| v.as_number())
                .unwrap_or(0.0) as u64;
            let cb_args: Vec<JsValue> = args[2..].to_vec();

            if let Some(func) = callback.as_object().cloned() {
                if func.is_callable() {
                    let deadline = Instant::now() + Duration::from_millis(delay_ms);
                    let id = timer_queue_si.schedule_timer(
                        deadline,
                        func,
                        cb_args,
                        true,
                        Some(delay_ms),
                    );
                    return Ok(JsValue::from(id as f64));
                }
            }
            Ok(JsValue::undefined())
        })
    };

    let timer_queue_ct = job_queue.clone();
    let clear_timer_fn =
        unsafe {
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
            let mut body: Option<String> = None;
            let mut _timeout_ms: Option<u64> = None;

            if args.len() > 1 {
                if let Some(opts) = args[1].as_object() {
                    // method
                    if let Ok(m) = opts.get(js_string!("method"), ctx) {
                        if let Some(s) = m.as_string() {
                            method = s.to_std_string_escaped().to_uppercase();
                        }
                    }
                    // headers (simplified — just extract common ones)
                    // Full header iteration via enumerate() skipped for simplicity
                    // since boa 0.20's JsIterator API requires careful handling
                    // body
                    if let Ok(b) = opts.get(js_string!("body"), ctx) {
                        if !b.is_undefined() && !b.is_null() {
                            if let Some(s) = b.as_string() {
                                body = Some(s.to_std_string_escaped());
                            }
                        }
                    }
                    // timeout
                    if let Ok(t) = opts.get(js_string!("timeout"), ctx) {
                        if let Some(n) = t.as_number() {
                            _timeout_ms = Some(n as u64);
                        }
                    }
                }
            }

            // Send fetch request to main thread
            let tx = fetch_tx_inner.read();
            let tx = match tx.as_ref() {
                Some(t) => t.clone(),
                None => {
                    // No fetch channel — return rejected Promise
                    let reject_code = r#"
                        Promise.reject(new Error('fetch() is not available — channel not set'))
                    "#;
                    let result = ctx.eval(Source::from_bytes(reject_code.trim()));
                    return result;
                }
            };
            drop(tx);

            // Recreate tx after dropping the read guard (to avoid deadlock)
            let tx = match fetch_tx_inner.read().as_ref().cloned() {
                Some(t) => t,
                None => {
                    // No fetch channel — return rejected Promise
                    let reject_code = r#"
                        Promise.reject(new Error('fetch() is not available — channel not set'))
                    "#;
                    let result = ctx.eval(Source::from_bytes(reject_code.trim()));
                    return result;
                }
            };

            let (response_tx, response_rx) = std::sync::mpsc::channel::<FetchResponseMsg>();
            let request = FetchRequestMsg {
                url: url.clone(),
                method: method.clone(),
                headers,
                body,
                response_tx,
            };

            if let Err(e) = tx.send(request) {
                // Channel error — return rejected Promise
                let reject_code = format!(
                    "Promise.reject(new Error('fetch channel error: {}'))",
                    e
                );
                let result = ctx.eval(Source::from_bytes(reject_code.trim()));
                return result;
            }

            // Wait for response (blocks JS thread)
            let response = response_rx.recv();
            let resp_error: Option<String>;

            match response {
                Ok(resp) => {
                    if let Some(err) = resp.error {
                        resp_error = Some(err);
                    } else {
                        let text_fn_body = resp.body.clone();
                        let text_fn = unsafe {
                            NativeFunction::from_closure(move |_this, _args, ctx| {
                                let body_json = serde_json::to_string(&text_fn_body)
                                    .unwrap_or_else(|_| String::from("\"\""));
                                let code = format!("Promise.resolve({})", body_json);
                                ctx.eval(Source::from_bytes(code.trim()))
                            })
                        };

                        let json_fn_body = resp.body.clone();
                        let json_fn = unsafe {
                            NativeFunction::from_closure(move |_this, _args, ctx| {
                                let body_json = serde_json::to_string(&json_fn_body)
                                    .unwrap_or_else(|_| String::from("null"));
                                let code = format!("Promise.resolve(JSON.parse({}))", body_json);
                                ctx.eval(Source::from_bytes(code.trim()))
                            })
                        };

                        let headers_obj = boa_engine::object::ObjectInitializer::new(ctx).build();
                        for (k, v) in &resp.headers {
                            let _ = headers_obj.set(
                                JsString::from(k.as_str()),
                                JsValue::from(JsString::from(v.as_str())),
                                true, ctx
                            );
                        }

                        let response_obj = boa_engine::object::ObjectInitializer::new(ctx)
                            .property(js_string!("status"), JsValue::from(resp.status), Attribute::all())
                            .property(js_string!("statusText"), JsValue::from(JsString::from(resp.status_text.as_str())), Attribute::all())
                            .property(js_string!("ok"), JsValue::from(resp.status < 400), Attribute::all())
                            .property(js_string!("url"), JsValue::from(JsString::from(resp.url.as_str())), Attribute::all())
                            .property(js_string!("bodyUsed"), JsValue::from(false), Attribute::all())
                            .property(js_string!("type"), JsValue::from(JsString::from("basic")), Attribute::all())
                            .property(js_string!("headers"), JsValue::from(headers_obj), Attribute::all())
                            .function(text_fn, js_string!("text"), 0)
                            .function(json_fn, js_string!("json"), 0)
                            .build();

                        let _ = ctx.register_global_property(
                            js_string!("__fetch_response"),
                            JsValue::from(response_obj),
                            Attribute::all(),
                        );
                        let result = ctx.eval(Source::from_bytes(
                            "(() => { const r = __fetch_response; delete globalThis.__fetch_response; return Promise.resolve(r); })()"
                        ));
                        return result;
                    }
                }
                Err(_) => {
                    resp_error = Some("fetch channel closed".to_string());
                }
            }


            // Return rejected Promise on error
            let reject_code = format!(
                "Promise.reject(new Error('{}'))",
                resp_error.unwrap_or_else(|| "fetch failed".to_string())
            );
            let result = ctx.eval(Source::from_bytes(reject_code.trim()));
            result
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
            let onload_setter = unsafe {
                NativeFunction::from_closure(move |_this, args, _ctx| {
                    if let Some(v) = args.first() {
                        *onload_set.write() = Some(v.clone());
                    }
                    Ok(JsValue::undefined())
                })
            };
            let onload_setter_fn = FunctionObjectBuilder::new(ctx.realm(), onload_setter)
                .name(js_string!("set onload")).build();

            // onerror setter
            let onerror_set = onerror_cb.clone();
            let onerror_setter = unsafe {
                NativeFunction::from_closure(move |_this, args, _ctx| {
                    if let Some(v) = args.first() {
                        *onerror_set.write() = Some(v.clone());
                    }
                    Ok(JsValue::undefined())
                })
            };
            let onerror_setter_fn = FunctionObjectBuilder::new(ctx.realm(), onerror_setter)
                .name(js_string!("set onerror")).build();

            // onreadystatechange setter
            let onrsc_set = onreadystatechange_cb.clone();
            let onrsc_setter = unsafe {
                NativeFunction::from_closure(move |_this, args, _ctx| {
                    if let Some(v) = args.first() {
                        *onrsc_set.write() = Some(v.clone());
                    }
                    Ok(JsValue::undefined())
                })
            };
            let onrsc_setter_fn = FunctionObjectBuilder::new(ctx.realm(), onrsc_setter)
                .name(js_string!("set onreadystatechange")).build();

            // .open(method, url, async)
            let om = open_method.clone();
            let ou = open_url.clone();
            let oa = open_async.clone();
            let rs = ready_state.clone();
            let open_fn = unsafe {
                NativeFunction::from_closure(move |_this, args, ctx| {
                    let method = args.first()
                        .and_then(|v| v.to_string(ctx).ok())
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    let url = args.get(1)
                        .and_then(|v| v.to_string(ctx).ok())
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    let async_flag = args.get(2)
                        .and_then(|v| v.as_boolean())
                        .unwrap_or(true);
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
            let send_fn = unsafe {
                NativeFunction::from_closure(move |_this, args, ctx| {
                    let body = args.first()
                        .and_then(|v| v.as_string())
                        .map(|s| s.to_std_string_escaped());
                    let method = send_method.read().clone();
                    let url = send_url.read().clone();
                    let is_async = *send_async.read();

                    *send_rs.write() = 2.0; // HEADERS_RECEIVED

                    let tx_guard = send_tx.read();
                    if let Some(ref tx) = *tx_guard {
                        let (response_tx, response_rx) = std::sync::mpsc::channel::<FetchResponseMsg>();
                        let request = FetchRequestMsg {
                            url: url.clone(),
                            method: method.clone(),
                            headers: Vec::new(),
                            body,
                            response_tx,
                        };
                        if tx.send(request).is_ok() {
                            match response_rx.recv() {
                                Ok(resp) => {
                                    *send_rs.write() = 3.0; // LOADING
                                    *send_status.write() = resp.status as f64;
                                    *send_resp.write() = resp.body.clone();
                                    // Parse response headers
                                    let mut hdr_str = String::new();
                                    for (k, v) in &resp.headers {
                                        hdr_str.push_str(&format!("{}: {}\r\n", k, v));
                                    }
                                    *send_hdrs.write() = hdr_str;
                                    *send_rs.write() = 4.0; // DONE

                                    if resp.error.is_none() {
                                        // Fire onload
                                        if let Some(ref cb) = *send_onload.read() {
                                            if let Some(cb_obj) = cb.as_object() {
                                                if cb_obj.is_callable() {
                                                    let _ = cb_obj.call(&JsValue::undefined(), &[], ctx);
                                                }
                                            }
                                        }
                                    } else {
                                        if let Some(ref cb) = *send_onerror.read() {
                                            if let Some(cb_obj) = cb.as_object() {
                                                if cb_obj.is_callable() {
                                                    let _ = cb_obj.call(&JsValue::undefined(), &[], ctx);
                                                }
                                            }
                                        }
                                    }
                                    // Fire onreadystatechange
                                    if let Some(ref cb) = *send_onrsc.read() {
                                        if let Some(cb_obj) = cb.as_object() {
                                            if cb_obj.is_callable() {
                                                let _ = cb_obj.call(&JsValue::undefined(), &[], ctx);
                                            }
                                        }
                                    }
                                }
                                Err(_) => {
                                    *send_rs.write() = 4.0;
                                    *send_status.write() = 0.0;
                                }
                            }
                        }
                    }

                    Ok(JsValue::undefined())
                })
            };

            // .setRequestHeader(name, value) — noop for now
            let set_req_header_fn = unsafe {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    Ok(JsValue::undefined())
                })
            };

            // .getResponseHeader(name)
            let get_hdr_rs = response_headers.clone();
            let get_header_fn = unsafe {
                NativeFunction::from_closure(move |_this, args, ctx| {
                    let name = args.first()
                        .and_then(|v| v.to_string(ctx).ok())
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    let hdrs = get_hdr_rs.read();
                    for line in hdrs.lines() {
                        if let Some(eq) = line.find(':') {
                            let key = line[..eq].trim();
                            if key.eq_ignore_ascii_case(&name) {
                                return Ok(JsValue::from(JsString::from(line[eq+1..].trim())));
                            }
                        }
                    }
                    Ok(JsValue::null())
                })
            };

            // .abort() — reset state
            let abort_rs = ready_state.clone();
            let abort_fn = unsafe {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    *abort_rs.write() = 0.0;
                    Ok(JsValue::undefined())
                })
            };

            // Build object
            let rs_clone = ready_state.clone();
            let rs_getter = unsafe {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    Ok(JsValue::from(*rs_clone.read()))
                })
            };
            let rs_getter_fn = FunctionObjectBuilder::new(ctx.realm(), rs_getter)
                .name(js_string!("get readyState")).build();

            let st_clone = status_val.clone();
            let st_getter = unsafe {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    Ok(JsValue::from(*st_clone.read()))
                })
            };
            let st_getter_fn = FunctionObjectBuilder::new(ctx.realm(), st_getter)
                .name(js_string!("get status")).build();

            let rt_clone = response_text.clone();
            let rt_getter = unsafe {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    Ok(JsValue::from(JsString::from(rt_clone.read().as_str())))
                })
            };
            let rt_getter_fn = FunctionObjectBuilder::new(ctx.realm(), rt_getter)
                .name(js_string!("get responseText")).build();

            let ol_clone = onload_cb.clone();
            let ol_getter = unsafe {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    Ok(ol_clone.read().clone().unwrap_or(JsValue::null()))
                })
            };
            let ol_getter_fn = FunctionObjectBuilder::new(ctx.realm(), ol_getter)
                .name(js_string!("get onload")).build();

            let oe_clone = onerror_cb.clone();
            let oe_getter = unsafe {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    Ok(oe_clone.read().clone().unwrap_or(JsValue::null()))
                })
            };
            let oe_getter_fn = FunctionObjectBuilder::new(ctx.realm(), oe_getter)
                .name(js_string!("get onerror")).build();

            let obj = boa_engine::object::ObjectInitializer::new(ctx)
                .accessor(js_string!("readyState"), Some(rs_getter_fn), None, Attribute::all())
                .accessor(js_string!("status"), Some(st_getter_fn), None, Attribute::all())
                .accessor(js_string!("responseText"), Some(rt_getter_fn), None, Attribute::all())
                .accessor(js_string!("onload"), Some(ol_getter_fn), Some(onload_setter_fn), Attribute::all())
                .accessor(js_string!("onerror"), Some(oe_getter_fn), Some(onerror_setter_fn), Attribute::all())
                .accessor(js_string!("onreadystatechange"), None, Some(onrsc_setter_fn), Attribute::all())
                .property(js_string!("responseType"), JsValue::from(JsString::from("")), Attribute::all())
                .property(js_string!("timeout"), JsValue::from(0), Attribute::all())
                .property(js_string!("withCredentials"), JsValue::from(false), Attribute::all())
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

    // --- MutationObserver ---
    let mo_ctor = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let callback = args.first().cloned().unwrap_or(JsValue::undefined());

            // Observation state stored in JS object properties
            // __callback: the MutationCallback
            // __observing: boolean flag
            // __records: array of MutationRecord objects

            let disconnect_fn = unsafe {
                NativeFunction::from_closure(move |_this, _args, ctx| {
                    if let Some(obj) = _this.as_object() {
                        let _ = obj.set(js_string!("__observing"), JsValue::from(false), true, ctx);
                        let empty_arr = JsArray::new(ctx);
                        let _ = obj.set(js_string!("__records"), JsValue::from(empty_arr), true, ctx);
                    }
                    Ok(JsValue::undefined())
                })
            };

            let observe_fn = unsafe {
                NativeFunction::from_closure(move |_this, args, ctx| {
                    let _target = args.first();
                    let _options = args.get(1);

                    if let Some(obj) = _this.as_object() {
                        let _ = obj.set(js_string!("__observing"), JsValue::from(true), true, ctx);
                    }
                    Ok(JsValue::undefined())
                })
            };

            let take_records_fn = unsafe {
                NativeFunction::from_closure(move |_this, _args, ctx| {
                    if let Some(obj) = _this.as_object() {
                        let records = obj.get(js_string!("__records"), ctx).unwrap_or(JsValue::Null);
                        // Clear records
                        let empty_arr = JsArray::new(ctx);
                        let _ = obj.set(js_string!("__records"), JsValue::from(empty_arr), true, ctx);
                        return Ok(records);
                    }
                    let arr = JsArray::new(ctx);
                    Ok(JsValue::from(arr))
                })
            };

            let empty_arr = JsArray::new(ctx);
            let obj = boa_engine::object::ObjectInitializer::new(ctx)
                .property(js_string!("__callback"), callback, Attribute::all())
                .property(js_string!("__observing"), JsValue::from(false), Attribute::all())
                .property(js_string!("__records"), JsValue::from(empty_arr), Attribute::all())
                .function(observe_fn, js_string!("observe"), 2)
                .function(disconnect_fn, js_string!("disconnect"), 0)
                .function(take_records_fn, js_string!("takeRecords"), 0)
                .build();

            Ok(JsValue::from(obj))
        })
    };
    let _ = context.register_global_callable(js_string!("MutationObserver"), 1, mo_ctor);

    // --- Document object ---

    register_document_object(&mut context, dom_snapshot, mutations);

    // --- Window global ---

    register_window_globals(
        &mut context,
        dom_snapshot,
        mutations,
        viewport,
        page_url,
        user_agent,
        fetch_tx_arc,
        history_tx_arc,
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
            let mut storage = std::cell::RefCell::new(map);

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
            let get_fn = unsafe {
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
            let set_fn = unsafe {
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
            let app_fn = unsafe {
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
            let del_fn = unsafe {
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
            let has_fn = unsafe {
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
            let foreach_fn = unsafe {
                NativeFunction::from_closure(move |_this, _args, ctx| {
                    if let Some(callback) = _args.first() {
                        if let Some(cb_obj) = callback.as_object() {
                            if cb_obj.is_callable() {
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
                        }
                    }
                    Ok(JsValue::undefined())
                })
            };

            // --- toString ---
            let str_sp = storage_arc.clone();
            let str_fn = unsafe {
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
            let url_storage = std::sync::Arc::new(std::cell::RefCell::new(std::collections::HashMap::new()));

            // url_storage holds parsed values as strings
            {
                let mut s = url_storage.borrow_mut();
                match &parsed {
                    Ok(u) => {
                        s.insert("href".to_string(), u.to_string());
                        s.insert("origin".to_string(), u.origin().ascii_serialization());
                        s.insert("protocol".to_string(), format!("{}:", u.scheme()));
                        s.insert("host".to_string(), u.host().map(|h| h.to_string()).unwrap_or_default());
                        s.insert("hostname".to_string(), u.host().map(|h| h.to_string()).unwrap_or_default());
                        s.insert("pathname".to_string(), u.path().to_string());
                        s.insert("search".to_string(), u.query().map(|q| format!("?{}", q)).unwrap_or_default());
                        s.insert("hash".to_string(), u.fragment().map(|f| format!("#{}", f)).unwrap_or_default());
                        s.insert("port".to_string(), u.port().map(|p| p.to_string()).unwrap_or_default());
                        s.insert("username".to_string(), u.username().to_string());
                        s.insert("password".to_string(), u.password().map(|p| p.to_string()).unwrap_or_default());
                        s.insert("searchParams".to_string(), "URLSearchParams".to_string()); // marker
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
            let href_fn = unsafe {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    let href = href_storage.borrow().get("href").cloned().unwrap_or_default();
                    Ok(JsValue::from(JsString::from(href.as_str())))
                })
            };

            // origin getter
            let origin_fn = unsafe {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    let origin = us_storage.borrow().get("origin").cloned().unwrap_or_default();
                    Ok(JsValue::from(JsString::from(origin.as_str())))
                })
            };

            // protocol getter
            let proto_fn = unsafe {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    let proto = us_storage2.borrow().get("protocol").cloned().unwrap_or_default();
                    Ok(JsValue::from(JsString::from(proto.as_str())))
                })
            };

            // host getter
            let host_fn = unsafe {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    let host = us_storage3.borrow().get("host").cloned().unwrap_or_default();
                    Ok(JsValue::from(JsString::from(host.as_str())))
                })
            };

            // pathname getter
            let path_fn = unsafe {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    let path = us_storage4.borrow().get("pathname").cloned().unwrap_or_default();
                    Ok(JsValue::from(JsString::from(path.as_str())))
                })
            };

            // search getter
            let search_fn = unsafe {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    let search = us_storage5.borrow().get("search").cloned().unwrap_or_default();
                    Ok(JsValue::from(JsString::from(search.as_str())))
                })
            };

            // hash getter
            let hash_fn = unsafe {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    let hash = us_storage6.borrow().get("hash").cloned().unwrap_or_default();
                    Ok(JsValue::from(JsString::from(hash.as_str())))
                })
            };

            // searchParams getter (returns URLSearchParams instance)
            let sp_storage = us_storage7.clone();
            let sp_fn = unsafe {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    let search = sp_storage.borrow().get("search").cloned().unwrap_or_default();
                    let _query = search.trim_start_matches('?').to_string();
                    Ok(JsValue::undefined())
                })
            };

            // Build URL object — convert NativeFunction to JsFunction via FunctionObjectBuilder
            let href_getter = FunctionObjectBuilder::new(ctx.realm(), href_fn).name("get href").build();
            let origin_getter = FunctionObjectBuilder::new(ctx.realm(), origin_fn).name("get origin").build();
            let proto_getter = FunctionObjectBuilder::new(ctx.realm(), proto_fn).name("get protocol").build();
            let host_getter = FunctionObjectBuilder::new(ctx.realm(), host_fn).name("get host").build();
            let path_getter = FunctionObjectBuilder::new(ctx.realm(), path_fn).name("get pathname").build();
            let search_getter = FunctionObjectBuilder::new(ctx.realm(), search_fn).name("get search").build();
            let hash_getter = FunctionObjectBuilder::new(ctx.realm(), hash_fn).name("get hash").build();
            let sp_getter = FunctionObjectBuilder::new(ctx.realm(), sp_fn).name("get searchParams").build();

            let url_obj = boa_engine::object::ObjectInitializer::new(ctx)
                .accessor(js_string!("href"), Some(href_getter), None, Attribute::all())
                .accessor(js_string!("origin"), Some(origin_getter), None, Attribute::all())
                .accessor(js_string!("protocol"), Some(proto_getter), None, Attribute::all())
                .accessor(js_string!("host"), Some(host_getter), None, Attribute::all())
                .accessor(js_string!("pathname"), Some(path_getter), None, Attribute::all())
                .accessor(js_string!("search"), Some(search_getter), None, Attribute::all())
                .accessor(js_string!("hash"), Some(hash_getter), None, Attribute::all())
                .accessor(js_string!("searchParams"), Some(sp_getter), None, Attribute::all())
                .build();

            Ok(JsValue::from(url_obj))
        })
    };
    let _ = context.register_global_callable(js_string!("URL"), 1, url_ctor);

    // --- crypto.getRandomValues ---
    let get_random_values_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let arr = args.first().cloned().unwrap_or(JsValue::undefined());
            if let Some(arr_obj) = arr.as_object() {
                if let Ok(js_arr) = JsArray::from_object(arr_obj.clone()) {
                    if let Ok(len) = js_arr.length(ctx) {
                        for i in 0..len.min(65536) {
                            let val = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().subsec_nanos() as u8).wrapping_add(i as u8);
                            let _ = js_arr.set(i, JsValue::from(val), true, ctx);
                        }
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
                .property(js_string!("encoding"), JsValue::from(JsString::from("utf-8")), Attribute::all())
.build();
            // Return Uint8Array-like object
            let result = boa_engine::object::ObjectInitializer::new(ctx)
                .property(js_string!("buffer"), JsValue::from(arr.clone()), Attribute::all())
                .build();
            Ok(JsValue::from(result))
        })
    };
    // Avoid recursive closure — use a simpler approach
    let te_ctor = unsafe {
        NativeFunction::from_closure(move |_this, _args, ctx| {
            let encode_fn = unsafe {
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
                .property(js_string!("encoding"), JsValue::from(JsString::from("utf-8")), Attribute::all())
                .function(encode_fn, js_string!("encode"), 1)
                .build();
            Ok(JsValue::from(obj))
        })
    };
    let _ = context.register_global_callable(js_string!("TextEncoder"), 0, te_ctor);

    // --- TextDecoder ---
    let td_ctor = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let decode_fn = unsafe {
                NativeFunction::from_closure(move |_this2, args2, ctx2| {
                    // Decode buffer/array back to string
                    let input = args2.first().cloned().unwrap_or(JsValue::undefined());
                    if let Some(arr_obj) = input.as_object() {
                        if let Ok(arr) = JsArray::from_object(arr_obj.clone()) {
                            if let Ok(len) = arr.length(ctx2) {
                                let mut bytes = Vec::with_capacity(len as usize);
                                for i in 0..len {
                                    if let Ok(v) = arr.at(i as i64, ctx2) {
                                        if let Some(n) = v.as_number() {
                                            bytes.push(n as u8);
                                        }
                                    }
                                }
                                let s = String::from_utf8_lossy(&bytes).to_string();
                                return Ok(JsValue::from(JsString::from(s.as_str())));
                            }
                        }
                    }
                    Ok(JsValue::from(JsString::from("")))
                })
            };
            let encoding = args
                .first()
                .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                .unwrap_or_else(|| "utf-8".to_string());
            let obj = boa_engine::object::ObjectInitializer::new(ctx)
                .property(js_string!("encoding"), JsValue::from(JsString::from(encoding.as_str())), Attribute::all())
                .function(decode_fn, js_string!("decode"), 1)
                .build();
            Ok(JsValue::from(obj))
        })
    };
    let _ = context.register_global_callable(js_string!("TextDecoder"), 0, td_ctor);

    (context, job_queue)
}

// ---------------------------------------------------------------------------
// Document object registration
// ---------------------------------------------------------------------------

/// Register the `document` global object with DOM query methods.
fn register_document_object(
    ctx: &mut Context,
    dom_snapshot: &Arc<RwLock<Option<DomSnapshot>>>,
    mutations: &Arc<RwLock<Vec<DomMutation>>>,
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

    let dom_capture_cookie = dom_snapshot.clone();
    let cookie_getter: NativeFunction = unsafe {
        NativeFunction::from_closure(move |_this, _args, _ctx| {
            let _unused = dom_capture_cookie.read();
            // Simplified: no cookie jar integration yet
            Ok(JsValue::from(JsString::from("")))
        })
    };
    let cookie_getter_fn = FunctionObjectBuilder::new(ctx.realm(), cookie_getter)
        .name(js_string!("get cookie"))
        .build();

    // querySelector(selector)
    let dom_capture_qs = dom_snapshot.clone();
    let mutations_capture_qs = mutations.clone();
    let query_selector_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let selector = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();

            let dom = dom_capture_qs.read();
            if let Some(ref snapshot) = *dom {
                if let Some(node_id) = snapshot.query_selector(&selector) {
                    if let Some(node) = snapshot.nodes.get(&node_id) {
                        return Ok(create_element_object(
                            snapshot,
                            node,
                            ctx,
                            &mutations_capture_qs,
                            &dom_capture_qs,
                        ));
                    }
                }
            }
            Ok(JsValue::null())
        })
    };

    // querySelectorAll(selector)
    let dom_capture_qsa = dom_snapshot.clone();
    let mutations_capture_qsa = mutations.clone();
    let query_selector_all_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let selector = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();

            let dom = dom_capture_qsa.read();
            if let Some(ref snapshot) = *dom {
                let ids = snapshot.query_selector_all(&selector);
                let js_values: Vec<JsValue> = ids
                    .iter()
                    .filter_map(|&id| {
                        snapshot.nodes.get(&id).map(|node| {
                            create_element_object(snapshot, node, ctx, &mutations_capture_qsa, &dom_capture_qsa)
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
    let get_element_by_id_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let id = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();

            let dom = dom_capture_gbi.read();
            if let Some(ref snapshot) = *dom {
                if let Some(node_id) = snapshot.get_element_by_id(&id) {
                    if let Some(node) = snapshot.nodes.get(&node_id) {
                        return Ok(create_element_object(
                            snapshot,
                            node,
                            ctx,
                            &mutations_capture_gbi,
                            &dom_capture_gbi,
                        ));
                    }
                }
            }
            Ok(JsValue::null())
        })
    };

    // getElementsByTagName(tag)
    let dom_capture_gtn = dom_snapshot.clone();
    let mutations_capture_gtn = mutations.clone();
    let get_elements_by_tag_name_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let tag = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();

            let dom = dom_capture_gtn.read();
            if let Some(ref snapshot) = *dom {
                let ids = snapshot.get_elements_by_tag_name(&tag);
                let js_values: Vec<JsValue> = ids
                    .iter()
                    .filter_map(|&id| {
                        snapshot.nodes.get(&id).map(|node| {
                            create_element_object(snapshot, node, ctx, &mutations_capture_gtn, &dom_capture_gtn)
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
    let get_elements_by_class_name_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let class = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();

            let dom = dom_capture_gcn.read();
            if let Some(ref snapshot) = *dom {
                let ids = snapshot.get_elements_by_class_name(&class);
                let js_values: Vec<JsValue> = ids
                    .iter()
                    .filter_map(|&id| {
                        snapshot.nodes.get(&id).map(|node| {
                            create_element_object(snapshot, node, ctx, &mutations_capture_gcn, &dom_capture_gcn)
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
            let listeners_val = this_obj.get(js_string!("__listeners"), ctx).unwrap_or(JsValue::Null);
            // Create __listeners if missing
            if listeners_val.as_object().is_none() {
                let obj = boa_engine::object::ObjectInitializer::new(ctx).build();
                let _ = this_obj.set(js_string!("__listeners"), JsValue::from(obj), true, ctx);
            }
            let lv2 = this_obj.get(js_string!("__listeners"), ctx).unwrap_or(JsValue::Null);
            let listeners_obj = match lv2.as_object() {
                Some(o) => o,
                None => return Ok(JsValue::undefined()),
            };

            // Ensure array for this event type
            let arr_key = JsString::from(event_type.as_str());
            let ev = listeners_obj.get(arr_key.clone(), ctx).unwrap_or(JsValue::Null);
            if ev.as_object().is_none() {
                let a: JsValue = JsValue::from(JsArray::new(ctx));
                let _ = listeners_obj.set(arr_key.clone(), a, true, ctx);
            }
            let arr_val = listeners_obj.get(arr_key, ctx).unwrap_or(JsValue::Null);
            if let Some(arr_obj) = arr_val.as_object() {
                if let Ok(arr) = JsArray::from_object(arr_obj.clone()) {
                    let _ = arr.push(callback, ctx);
                }
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

            if let Some(this_obj) = _this.as_object() {
                if let Ok(l_val) = this_obj.get(js_string!("__listeners"), ctx) {
                    if let Some(l_obj) = l_val.as_object() {
                        let _ = l_obj.set(JsString::from(event_type.as_str()), JsValue::Null, true, ctx);
                    }
                }
            }
            Ok(JsValue::undefined())
        })
    };

    let doc_dispatch_event_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let event = args.first().cloned().unwrap_or(JsValue::undefined());

            let event_type = if let Some(evt_obj) = event.as_object() {
                evt_obj.get(js_string!("type"), ctx).ok()
                    .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                    .unwrap_or_default()
            } else if let Some(s) = event.as_string() {
                s.to_std_string_escaped()
            } else {
                return Ok(JsValue::from(true));
            };

            if let Some(this_obj) = _this.as_object() {
                if let Ok(l_val) = this_obj.get(js_string!("__listeners"), ctx) {
                    if let Some(l_obj) = l_val.as_object() {
                        let arr_val = l_obj.get(JsString::from(event_type.as_str()), ctx).unwrap_or(JsValue::Null);
                        if let Some(arr_obj) = arr_val.as_object() {
                            if let Ok(arr) = JsArray::from_object(arr_obj.clone()) {
                                if let Ok(len) = arr.length(ctx) {
                                    for i in 0..len {
                                        if let Ok(cb) = arr.at(i as i64, ctx) {
                                            if let Some(cb_obj) = cb.as_object() {
                                                if cb_obj.is_callable() {
                                                    let _ = cb_obj.call(_this, std::slice::from_ref(&event), ctx);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            Ok(JsValue::from(true))
        })
    };

    // document.body / document.head / document.documentElement getters
    let dom_snap_body = dom_snapshot.clone();
    let dom_snap_body_clone = dom_snapshot.clone();
    let body_getter_fn = {
        let mutations_clone = mutations.clone();
        let getter: NativeFunction = unsafe {
            NativeFunction::from_closure(move |_this, _args, ctx| {
                let snap = dom_snap_body.read();
                if let Some(ref s) = *snap {
                    if let Some(bid) = s.body_id {
                        if let Some(node) = s.nodes.get(&bid) {
                            return Ok(create_element_object(s, node, ctx, &mutations_clone, &dom_snap_body_clone));
                        }
                    }
                }
                Ok(JsValue::null())
            })
        };
        FunctionObjectBuilder::new(ctx.realm(), getter)
            .name(js_string!("get body"))
            .build()
    };

    let dom_snap_head = dom_snapshot.clone();
    let dom_snap_head_clone = dom_snapshot.clone();
    let head_getter_fn = {
        let mutations_clone = mutations.clone();
        let getter: NativeFunction = unsafe {
            NativeFunction::from_closure(move |_this, _args, ctx| {
                let snap = dom_snap_head.read();
                if let Some(ref s) = *snap {
                    if let Some(hid) = s.head_id {
                        if let Some(node) = s.nodes.get(&hid) {
                            return Ok(create_element_object(s, node, ctx, &mutations_clone, &dom_snap_head_clone));
                        }
                    }
                }
                Ok(JsValue::null())
            })
        };
        FunctionObjectBuilder::new(ctx.realm(), getter)
            .name(js_string!("get head"))
            .build()
    };

    let dom_snap_de = dom_snapshot.clone();
    let document_element_getter_fn = {
        let mutations_clone = mutations.clone();
        let getter: NativeFunction = unsafe {
            NativeFunction::from_closure(move |_this, _args, ctx| {
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
                        return Ok(create_element_object(s, node, ctx, &mutations_clone, &dom_snap_de));
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

            // Generate a new node ID (use a large number to avoid collision with parsed nodes)
            let new_id = {
                let dom = dom_snap_ce.read();
                let max_id = dom.as_ref()
                    .map(|s| s.nodes.keys().max().copied().unwrap_or(0))
                    .unwrap_or(0);
                max_id + 1 + std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_nanos() % 10000
            };

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
            let mut attrs_map: HashMap<String, String> = HashMap::new();
            let dom_snap_el = dom_snap_ce.clone();
            let mutations_el = mutations_ce.clone();

            // setAttribute for this element
            let mut_set_attr = mutations_el.clone();
            let mut_set_id = id_for_obj;
            let set_attr_fn = unsafe {
                NativeFunction::from_closure(move |_this, args, _ctx| {
                    let name = args.first().and_then(|v| v.as_string()).map(|s| s.to_std_string_escaped()).unwrap_or_default();
                    let value = args.get(1).and_then(|v| v.as_string()).map(|s| s.to_std_string_escaped()).unwrap_or_default();
                    mut_set_attr.write().push(DomMutation::SetAttribute { node_id: mut_set_id, name, value });
                    Ok(JsValue::undefined())
                })
            };

            // getAttribute for this element
            let attrs_for_get = attrs_map.clone();
            let get_attr_fn = unsafe {
                NativeFunction::from_closure(move |_this, args, _ctx| {
                    let name = args.first().and_then(|v| v.as_string()).map(|s| s.to_std_string_escaped()).unwrap_or_default();
                    match attrs_for_get.get(&name) {
                        Some(v) => Ok(JsValue::from(JsString::from(v.as_str()))),
                        None => Ok(JsValue::null()),
                    }
                })
            };

            // click for this element
            let mut_click = mutations_el.clone();
            let click_id = id_for_obj;
            let click_fn = unsafe {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    mut_click.write().push(DomMutation::ClickElement { node_id: click_id });
                    Ok(JsValue::undefined())
                })
            };

            // appendChild for this element
            let dom_snap_ac = dom_snap_el.clone();
            let parent_id_ac = id_for_obj;
            let append_child_fn = unsafe {
                NativeFunction::from_closure(move |_this, args, ctx| {
                    let child = args.first().cloned().unwrap_or(JsValue::undefined());
                    let child_id = child.as_object()
                        .and_then(|o| o.get(js_string!("__nodeId"), ctx).ok())
                        .and_then(|v| v.as_number().map(|n| n as u32));

                    if let Some(cid) = child_id {
                        // Update snapshot
                        let mut dom = dom_snap_ac.write();
                        if let Some(ref mut snap) = *dom {
                            // Add child to parent's children list
                            if let Some(parent) = snap.nodes.get_mut(&parent_id_ac) {
                                if !parent.children.contains(&cid) {
                                    parent.children.push(cid);
                                }
                            }
                            // Set child's parent
                            if let Some(child_node) = snap.nodes.get_mut(&cid) {
                                child_node.parent = Some(parent_id_ac);
                            }
                        }
                    }

                    Ok(child)
                })
            };

            let obj = boa_engine::object::ObjectInitializer::new(ctx)
                .property(js_string!("tagName"), JsValue::from(JsString::from(tag_for_obj.as_str())), Attribute::all())
                .property(js_string!("nodeName"), JsValue::from(JsString::from(tag_for_obj.as_str())), Attribute::all())
                .property(js_string!("textContent"), JsValue::from(JsString::from("")), Attribute::all())
                .property(js_string!("id"), JsValue::from(JsString::from("")), Attribute::all())
                .property(js_string!("className"), JsValue::from(JsString::from("")), Attribute::all())
                .property(js_string!("__nodeId"), JsValue::from(id_for_obj), Attribute::all())
                .function(get_attr_fn, js_string!("getAttribute"), 1)
                .function(set_attr_fn, js_string!("setAttribute"), 2)
                .function(click_fn, js_string!("click"), 0)
                .function(append_child_fn, js_string!("appendChild"), 1)
                .build();

            Ok(JsValue::from(obj))
        })
    };

    // === DOM Mutation: createTextNode ===
    let dom_snap_ct = dom_snapshot.clone();
    let mutations_ct = mutations.clone();
    let create_text_node_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let text = args
                .first()
                .and_then(|v| v.to_string(ctx).ok())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();

            let new_id = {
                let dom = dom_snap_ct.read();
                let max_id = dom.as_ref()
                    .map(|s| s.nodes.keys().max().copied().unwrap_or(0))
                .unwrap_or(0);
                max_id + 1 + std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_nanos() % 10000
            };

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
                .property(js_string!("textContent"), JsValue::from(JsString::from(text.as_str())), Attribute::all())
                .property(js_string!("nodeType"), JsValue::from(3), Attribute::all())
                .property(js_string!("__nodeId"), JsValue::from(new_id), Attribute::all())
                .build();

            Ok(JsValue::from(obj))
        })
    };

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
            None,
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
            Some(body_getter_fn),
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
    let id_val = node.attributes.get("id").map(|s| s.as_str()).unwrap_or("");
    let class_val = node
        .attributes
        .get("class")
        .map(|s| s.as_str())
        .unwrap_or("");
    let href_val = node
        .attributes
        .get("href")
        .map(|s| s.as_str())
        .unwrap_or("");
    let src_val = node.attributes.get("src").map(|s| s.as_str()).unwrap_or("");

    // getAttribute(name)
    let attrs_clone: HashMap<String, String> = node.attributes.clone();
    let get_attribute_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            let name = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            match attrs_clone.get(&name) {
                Some(val) => Ok(JsValue::from(JsString::from(val.as_str()))),
                None => Ok(JsValue::null()),
            }
        })
    };

    // hasAttribute(name)
    let attrs_clone2: HashMap<String, String> = node.attributes.clone();
    let has_attribute_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            let name = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            Ok(JsValue::from(attrs_clone2.contains_key(&name)))
        })
    };

    // addEventListener — stores callback by event type on the JS object itself.
    // We use a hidden `__listeners` property: { "click": [fn1, fn2], "DOMContentLoaded": [fn3] }
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
            let lv = this_obj.get(js_string!("__listeners"), ctx).unwrap_or(JsValue::Null);
            if lv.as_object().is_none() {
                let obj = boa_engine::object::ObjectInitializer::new(ctx).build();
                let _ = this_obj.set(js_string!("__listeners"), JsValue::from(obj), true, ctx);
            }
            let listeners_val2 = this_obj.get(js_string!("__listeners"), ctx).unwrap_or(JsValue::Null);
            let listeners_obj = match listeners_val2.as_object() {
                Some(o) => o,
                None => return Ok(JsValue::undefined()),
            };
            // Ensure array for this event type
            let arr_key = JsString::from(event_type.as_str());
            let ev = listeners_obj.get(arr_key.clone(), ctx).unwrap_or(JsValue::Null);
            if ev.as_object().is_none() {
                let a: JsValue = JsValue::from(JsArray::new(ctx));
                let _ = listeners_obj.set(arr_key.clone(), a, true, ctx);
            }
            let arr_val = listeners_obj.get(arr_key, ctx).unwrap_or(JsValue::Null);
            if let Some(arr_obj) = arr_val.as_object() {
                if let Ok(arr) = JsArray::from_object(arr_obj.clone()) {
                    let _ = arr.push(callback, ctx);
                }
            }

            Ok(JsValue::undefined())
        })
    };

    // removeEventListener — removes callback from __listeners
    let remove_event_listener_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let event_type = args
                .first()
                .and_then(|v| v.to_string(ctx).ok())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            let _callback = args.get(1);

            let this_obj = match _this.as_object() {
                Some(o) => o,
                None => return Ok(JsValue::undefined()),
            };
            let listeners = this_obj.get(js_string!("__listeners"), ctx);
            if let Ok(l_val) = listeners {
                if let Some(l_obj) = l_val.as_object() {
                    let _ = l_obj.set(JsString::from(event_type.as_str()), JsValue::Null, true, ctx);
                }
            }

            Ok(JsValue::undefined())
        })
    };

    // dispatchEvent — calls all registered callbacks for the event type
    let dispatch_event_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let event = args.first().cloned().unwrap_or(JsValue::undefined());

            // Get event type from the event object or use empty string
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

            let this_obj = match _this.as_object() {
                Some(o) => o,
                None => return Ok(JsValue::from(true)),
            };
            let listeners = this_obj.get(js_string!("__listeners"), ctx);
            if let Ok(l_val) = listeners {
                if let Some(l_obj) = l_val.as_object() {
                    let arr_val = l_obj.get(JsString::from(event_type.as_str()), ctx).unwrap_or(JsValue::Null);
                    if let Some(arr_obj) = arr_val.as_object() {
                        if let Ok(arr) = JsArray::from_object(arr_obj.clone()) {
                            if let Ok(len) = arr.length(ctx) {
                                for i in 0..len {
                                    if let Ok(cb) = arr.at(i as i64, ctx) {
                                        if let Some(cb_obj) = cb.as_object() {
                                            if cb_obj.is_callable() {
                                                let evt_arg = event.clone();
                                                let _ = cb_obj.call(
                                                    _this,
                                                    &[evt_arg],
                                                    ctx,
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            Ok(JsValue::from(true))
        })
    };

    // click() → records DomMutation::ClickElement
    let node_id_click = node.id;
    let mutations_click = mutations.clone();
    let click_fn = unsafe {
        NativeFunction::from_closure(move |_this, _args, _ctx| {
            mutations_click.write().push(DomMutation::ClickElement {
                node_id: node_id_click,
            });
            Ok(JsValue::undefined())
        })
    };

    // setAttribute(name, value) → records DomMutation::SetAttribute
    let node_id_sa = node.id;
    let mutations_sa = mutations.clone();
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
            let child_id = child.as_object()
                .and_then(|o| o.get(js_string!("__nodeId"), ctx).ok())
                .and_then(|v| v.as_number().map(|n| n as u32));

            if let Some(cid) = child_id {
                let mut dom = dom_snap_ac.write();
                if let Some(ref mut snap) = *dom {
                    if let Some(parent) = snap.nodes.get_mut(&node_id_ac) {
                        if !parent.children.contains(&cid) {
                            parent.children.push(cid);
                        }
                    }
                    if let Some(child_node) = snap.nodes.get_mut(&cid) {
                        child_node.parent = Some(node_id_ac);
                    }
                }
                mutations_ac.write().push(DomMutation::AppendChild {
                    parent_id: node_id_ac,
                    child_id: cid,
                });
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
            let child_id = child.as_object()
                .and_then(|o| o.get(js_string!("__nodeId"), ctx).ok())
                .and_then(|v| v.as_number().map(|n| n as u32));

            if let Some(cid) = child_id {
                let mut dom = dom_snap_rc.write();
                if let Some(ref mut snap) = *dom {
                    if let Some(parent) = snap.nodes.get_mut(&node_id_rc) {
                        parent.children.retain(|&id| id != cid);
                    }
                    if let Some(child_node) = snap.nodes.get_mut(&cid) {
                        child_node.parent = None;
                    }
                }
                mutations_rc.write().push(DomMutation::RemoveChild {
                    parent_id: node_id_rc,
                    child_id: cid,
                });
            }
            Ok(child)
        })
    };

    // value getter
    let value_val = node
        .attributes
        .get("value")
        .map(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let value_getter = unsafe {
        NativeFunction::from_closure(move |_this, _args, _ctx| {
            Ok(JsValue::from(JsString::from(value_val.as_str())))
        })
    };
    let value_getter_fn = FunctionObjectBuilder::new(ctx.realm(), value_getter)
        .name(js_string!("get value"))
        .build();

    // value setter → records DomMutation::InputElement
    let node_id_vs = node.id;
    let mutations_vs = mutations.clone();
    let value_setter = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            let val = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
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

    let obj = boa_engine::object::ObjectInitializer::new(ctx)
        .property(
            js_string!("tagName"),
            JsValue::from(JsString::from(tag_upper.as_str())),
            Attribute::all(),
        )
        .property(
            js_string!("textContent"),
            JsValue::from(JsString::from(node.text_content.as_str())),
            Attribute::all(),
        )
        .property(
            js_string!("innerHTML"),
            JsValue::from(JsString::from(node.text_content.as_str())),
            Attribute::all(),
        )
        .property(
            js_string!("id"),
            JsValue::from(JsString::from(id_val)),
            Attribute::all(),
        )
        .property(
            js_string!("className"),
            JsValue::from(JsString::from(class_val)),
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
        .property(js_string!("__nodeId"), JsValue::from(node.id), Attribute::all())
        .accessor(
            js_string!("value"),
            Some(value_getter_fn),
            Some(value_setter_fn),
            Attribute::all(),
        )
        .build();

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
            if obj.is_array() {
                if let Ok(arr) = JsArray::from_object(obj.clone()) {
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

    if let Some(json_obj) = json_global.as_object() {
        if let Ok(stringify_fn) = json_obj.get(js_string!("stringify"), context) {
            if stringify_fn.is_callable() {
                if let Some(obj_inner) = stringify_fn.as_object() {
                    if let Ok(result) =
                        obj_inner.call(&JsValue::undefined(), &[obj.clone().into()], context)
                    {
                        if let Some(s) = result.as_string() {
                            let json_str = s.to_std_string_escaped();
                            if let Ok(parsed) = serde_json::from_str::<Value>(&json_str) {
                                return parsed;
                            }
                            return Value::String(json_str);
                        }
                    }
                }
            }
        }
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
        NativeFunction::from_closure(move |_this: &JsValue, args: &[JsValue], ctx: &mut Context| {
            let key = args.first()
                .and_then(|v| v.to_string(ctx).ok())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            let val = get_storage.borrow().get(&key).cloned();
            match val {
                Some(v) => Ok(JsValue::from(JsString::from(v.as_str()))),
                None => Ok(JsValue::null()),
            }
        })
    };

    // --- setItem ---
    let set_storage = storage_arc.clone();
    let set_ls_tx = local_storage_tx.clone();
    let set_item_fn = unsafe {
        NativeFunction::from_closure(move |_this: &JsValue, args: &[JsValue], ctx: &mut Context| {
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
        })
    };

    // --- removeItem ---
    let rem_storage = storage_arc.clone();
    let rem_ls_tx = local_storage_tx.clone();
    let remove_item_fn = unsafe {
        NativeFunction::from_closure(move |_this: &JsValue, args: &[JsValue], ctx: &mut Context| {
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
        })
    };

    // --- clear ---
    let clear_storage = storage_arc.clone();
    let clear_ls_tx = local_storage_tx.clone();
    let clear_fn = unsafe {
        NativeFunction::from_closure(move |_this: &JsValue, _args: &[JsValue], _ctx: &mut Context| {
            clear_storage.borrow_mut().clear();
            // Sync to Session
            let tx_opt = { clear_ls_tx.read().as_ref().cloned() };
            if let Some(tx) = tx_opt {
                let _ = tx.send(LocalStorageMsg::Clear);
            }
            Ok(JsValue::undefined())
        })
    };

    // --- key ---
    let key_storage = storage_arc.clone();
    let key_fn = unsafe {
        NativeFunction::from_closure(move |_this: &JsValue, args: &[JsValue], ctx: &mut Context| {
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
        })
    };

    // --- get length (snapshot) ---
    let len_storage = storage_arc.clone();
    let _len_fn = unsafe {
        NativeFunction::from_closure(move |_this: &JsValue, _args: &[JsValue], _ctx: &mut Context| {
            Ok(JsValue::from(len_storage.borrow().len() as i32))
        })
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
        NativeFunction::from_closure(move |_this: &JsValue, args: &[JsValue], ctx: &mut Context| {
            let key = args.first()
                .and_then(|v| v.to_string(ctx).ok())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            match get_s.borrow().get(&key).cloned() {
                Some(v) => Ok(JsValue::from(JsString::from(v.as_str()))),
                None => Ok(JsValue::null()),
            }
        })
    };

    // setItem
    let set_s = storage_arc.clone();
    let set_item_fn = unsafe {
        NativeFunction::from_closure(move |_this: &JsValue, args: &[JsValue], ctx: &mut Context| {
            if args.len() >= 2 {
                let key = args[0].to_string(ctx).map(|s| s.to_std_string_escaped()).unwrap_or_default();
                let val = args[1].to_string(ctx).map(|s| s.to_std_string_escaped()).unwrap_or_default();
                set_s.borrow_mut().insert(key, val);
            }
            Ok(JsValue::undefined())
        })
    };

    // removeItem
    let rem_s = storage_arc.clone();
    let remove_item_fn = unsafe {
        NativeFunction::from_closure(move |_this: &JsValue, args: &[JsValue], ctx: &mut Context| {
            if let Some(k) = args.first() {
                let key = k.to_string(ctx).map(|s| s.to_std_string_escaped()).unwrap_or_default();
                rem_s.borrow_mut().remove(&key);
            }
            Ok(JsValue::undefined())
        })
    };

    // clear
    let clr_s = storage_arc.clone();
    let clear_fn = unsafe {
        NativeFunction::from_closure(move |_this: &JsValue, _args: &[JsValue], _ctx: &mut Context| {
            clr_s.borrow_mut().clear();
            Ok(JsValue::undefined())
        })
    };

    // key
    let key_s = storage_arc.clone();
    let key_fn = unsafe {
        NativeFunction::from_closure(move |_this: &JsValue, args: &[JsValue], ctx: &mut Context| {
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
        })
    };

    // length (computed via getter property)
    let _len_s = storage_arc.clone();
    // Use a data property instead of a getter for length (avoids move conflict)
    let storage_len = storage_arc.borrow().len() as i32;
    let storage_obj = boa_engine::object::ObjectInitializer::new(ctx)
        .function(get_item_fn, js_string!("getItem"), 1)
        .function(set_item_fn, js_string!("setItem"), 2)
        .function(remove_item_fn, js_string!("removeItem"), 1)
        .function(clear_fn, js_string!("clear"), 0)
        .function(key_fn, js_string!("key"), 1)
        .property(js_string!("length"), JsValue::from(storage_len), Attribute::all())
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
        if let Some(obj) = opaque.as_object() {
            if let Ok(msg_val) = obj.get(js_string!("message"), context) {
                if let Some(msg) = msg_val.as_string() {
                    let msg_str = msg.to_std_string_escaped();
                    if !msg_str.is_empty() {
                        if let Ok(name_val) = obj.get(js_string!("name"), context) {
                            if let Some(name) = name_val.as_string() {
                                return format!("{}: {}", name.to_std_string_escaped(), msg_str);
                            }
                        }
                        return msg_str;
                    }
                }
            }
        }
        return format!("Error: {:?}", opaque);
    }

    "Unknown JavaScript error".to_string()
}

// ---------------------------------------------------------------------------
// `window` global object
// ---------------------------------------------------------------------------

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
    history_tx_arc: &Arc<RwLock<Option<std::sync::mpsc::Sender<HistoryCommandType>>>>,
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
            if let Some(ref s) = *snap {
                if let Some(bid) = s.body_id {
                    if let Some(node) = s.nodes.get(&bid) {
                        return Ok(create_element_object(s, node, ctx, &mutations_body, &snap_body));
                    }
                }
            }
            Ok(JsValue::null())
        })
    };

    let snap_head = dom_snapshot.clone();
    let mutations_head = mutations.clone();
    let head_getter = unsafe {
        NativeFunction::from_closure(move |_this, _args, ctx| {
            let snap = snap_head.read();
            if let Some(ref s) = *snap {
                if let Some(hid) = s.head_id {
                    if let Some(node) = s.nodes.get(&hid) {
                        return Ok(create_element_object(s, node, ctx, &mutations_head, &snap_head));
                    }
                }
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
            JsValue::from(js_string!("MacIntel")),
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
        .build();

    // --- window.history ---
    // Clone Arc into each closure to avoid borrowing issues with 'static lifetime
    let history_tx_arc_clone = history_tx_arc.clone();
    let history_push_state_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            let url = args
                .get(2)
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            if let Some(tx) = history_tx_arc_clone.read().as_ref() {
                let _ = tx.send(HistoryCommandType::PushState { url });
            }
            Ok(JsValue::undefined())
        })
    };
    let history_tx_arc_clone2 = history_tx_arc.clone();
    let history_replace_state_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            let url = args
                .get(2)
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            if let Some(tx) = history_tx_arc_clone2.read().as_ref() {
                let _ = tx.send(HistoryCommandType::ReplaceState { url });
            }
            Ok(JsValue::undefined())
        })
    };
    let history_tx_arc_clone3 = history_tx_arc.clone();
    let history_back_fn = unsafe {
        NativeFunction::from_closure(move |_this, _args, _ctx| {
            if let Some(tx) = history_tx_arc_clone3.read().as_ref() {
                let _ = tx.send(HistoryCommandType::Back);
            }
            Ok(JsValue::undefined())
        })
    };
    let history_tx_arc_clone4 = history_tx_arc.clone();
    let history_forward_fn = unsafe {
        NativeFunction::from_closure(move |_this, _args, _ctx| {
            if let Some(tx) = history_tx_arc_clone4.read().as_ref() {
                let _ = tx.send(HistoryCommandType::Forward);
            }
            Ok(JsValue::undefined())
        })
    };
    let history_tx_arc_clone5 = history_tx_arc.clone();
    let history_go_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            let delta = args
                .get(0)
                .and_then(|v| v.as_number())
                .unwrap_or(0.0) as i32;
            if let Some(tx) = history_tx_arc_clone5.read().as_ref() {
                let _ = tx.send(HistoryCommandType::Go { delta });
            }
            Ok(JsValue::undefined())
        })
    };

    let history_obj = boa_engine::object::ObjectInitializer::new(ctx)
        .property(js_string!("length"), JsValue::from(1.0), Attribute::all())
        .function(history_push_state_fn, js_string!("pushState"), 3)
        .function(history_replace_state_fn, js_string!("replaceState"), 3)
        .function(history_back_fn, js_string!("back"), 0)
        .function(history_forward_fn, js_string!("forward"), 0)
        .function(history_go_fn, js_string!("go"), 1)
        .build();

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
    let global_doc = ctx
        .global_object()
        .get(js_string!("document"), ctx)
        .unwrap_or(JsValue::undefined());
    let global_console = ctx
        .global_object()
        .get(js_string!("console"), ctx)
        .unwrap_or(JsValue::undefined());

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
            JsValue::from(nav_obj),
            Attribute::all(),
        )
        .property(
            js_string!("location"),
            JsValue::from(location_obj),
            Attribute::all(),
        )
        .property(
            js_string!("performance"),
            JsValue::from(perf_obj),
            Attribute::all(),
        )
        .property(
            js_string!("history"),
            JsValue::from(history_obj),
            Attribute::all(),
        )
        // DOM shortcuts (as functions since boa 0.20 doesn't support
        // adding accessors to pre-existing objects)
        .function(body_getter, js_string!("getBody"), 0)
        .function(head_getter, js_string!("getHead"), 0)
        .function(document_element_getter, js_string!("getDocumentElement"), 0)
        .build();

    let _ = ctx.register_global_property(
        js_string!("window"),
        JsValue::from(window_final.clone()),
        Attribute::all(),
    );
    let _ = ctx.register_global_property(
        js_string!("self"),
        JsValue::from(window_final),
        Attribute::all(),
    );
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Frame;
    use url::Url;

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

    fn make_frame(html: &str) -> Frame {
        let url = Url::parse("https://example.com").unwrap();
        let doc = oxibrowser_webapi::dom::Document::parse(html);
        // Recreate what Frame::from_html does, but synchronously
        Frame::from_doc(url, doc, html)
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
        let frame = make_frame(html);
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
        let frame = make_frame(html);
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
        let frame = make_frame(html);
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
        let frame = make_frame(html);
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
    async fn test_document_query_selector_all() {
        let mut rt = JsRuntime::new();
        let html = "<html><body><ul><li>a</li><li>b</li><li>c</li></ul></body></html>";
        let frame = make_frame(html);
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
        let frame = make_frame(html);
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
    async fn test_document_get_elements_by_tag_name() {
        let mut rt = JsRuntime::new();
        let html = "<html><body><p>a</p><p>b</p></body></html>";
        let frame = make_frame(html);
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
        let frame = make_frame(html);
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
        let frame = make_frame(html);
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
        let frame = make_frame(html);
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
        let frame = make_frame(html);
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
        let frame = make_frame(html);
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
        let frame = make_frame(html);
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
    async fn test_element_children() {
        let mut rt = JsRuntime::new();
        let html = "<html><body><div><p>a</p><p>b</p></div></body></html>";
        let frame = make_frame(html);
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
        let frame1 = make_frame(html1);
        let snapshot1 = DomSnapshot::from_frame(&frame1);
        rt.set_dom_snapshot(Some(snapshot1));

        let r1 = rt.evaluate("document.title").await.unwrap();
        assert_eq!(r1.value, Some(Value::String("Page 1".into())));

        // Second snapshot replaces
        let html2 = "<html><head><title>Page 2</title></head><body></body></html>";
        let frame2 = make_frame(html2);
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
            viewport_width: 1280,
            viewport_height: 720,
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
        // Set up a mock fetch channel
        let (tx, rx) = std::sync::mpsc::channel();
        rt.set_fetch_channel(tx);

        // Drop the receiver so the fetch fails gracefully
        drop(rx);

        let result = rt
            .evaluate("fetch('https://example.com').catch(e => 'error: ' + e.message)")
            .await
            .unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_element_add_event_listener_noop() {
        let mut rt = JsRuntime::new();
        let html = r#"<html><body><div id="test">hi</div></body></html>"#;
        let frame = make_frame(html);
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
        let frame = make_frame(html);
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
        let frame = make_frame(html);
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
        let frame = make_frame(html);
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
    async fn test_mutation_input_value_setter() {
        let mut rt = JsRuntime::new();
        let html = r#"<html><body><input id="inp" value="old"></body></html>"#;
        let frame = make_frame(html);
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
        let frame = make_frame(html);
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
        let frame = make_frame(html);
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
        let frame = make_frame(html);
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
        let frame = make_frame(html);
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
        let result = rt.evaluate("new URL('https://example.com:8080/path?foo=bar#hash').hostname").await.unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_url_search_params() {
        let mut rt = JsRuntime::new();
        let result = rt.evaluate("new URLSearchParams('foo=bar&baz=1').get('foo')").await.unwrap();
        assert!(result.is_ok());
    }
}
