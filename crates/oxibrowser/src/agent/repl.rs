//! REPL host — the async side of the agent world (execution design M1).
//!
//! Owns the browser, the (possibly account-bound, credential-mode) context,
//! the session, and the enforcement pieces (`gates`). The world thread
//! (boa, `world.rs`) sends [`WorldCmd`]s over a tokio mpsc and blocks on a
//! std oneshot reply; the host never touches boa and the world never holds
//! a session lock — the deadlock boundary of the design.
//!
//! Actions resolve locators against the **native observation**
//! (`Session::native_observe`) and deliver via `Session::native_act` — the
//! eval-free path ghost built — after the gates run on snapshot-derived
//! descriptors.

use std::sync::Arc;
use std::sync::mpsc as std_mpsc;

use oxibrowser_cdp::account::LoginSurface;
use oxibrowser_cdp::refs::RefRegistry;
use oxibrowser_core::account::agent_login::CredentialSource;
use oxibrowser_core::context::BrowserContext;
use oxibrowser_core::session::Session;
use serde_json::{Value, json};
use tokio::sync::{RwLock, mpsc};

use super::gates::{self, GateError};
use super::locators::{self, LocatorQuery};
use super::obs::{self, ObsOpts};

/// Host-side reply channel (the world blocks on it with a timeout).
pub type Reply = std_mpsc::Sender<Result<Value, Value>>;
/// Host-side error: `{code, message}` JSON.
pub type HostError = Value;

/// How long the world waits for one host command before failing.
pub const BRIDGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// What the world asks the host to do.
pub enum WorldCmd {
    Goto {
        url: String,
        resp: Reply,
    },
    Content {
        resp: Reply,
    },
    Observe {
        budget_tokens: Option<u64>,
        resp: Reply,
    },
    Act {
        query: LocatorQuery,
        action: ActKind,
        resp: Reply,
    },
    FillCredential {
        query: LocatorQuery,
        account_id: String,
        resp: Reply,
    },
    Screenshot {
        resp: Reply,
    },
    Meta {
        resp: Reply,
    },
    WaitEvent {
        kind: String,
        timeout_ms: u64,
        resp: Reply,
    },
    ResolveDialog {
        accept: bool,
        prompt_text: Option<String>,
        resp: Reply,
    },
    Login {
        account_id: String,
        resp: Reply,
    },
    Replay {
        /// Substring match on the captured URL (empty = most recent).
        url: String,
        resp: Reply,
    },
    /// CUA fallback: coordinate click via layout hit-test.
    MouseClick {
        x: f64,
        y: f64,
        resp: Reply,
    },
}

/// Locator action kinds.
pub enum ActKind {
    Click,
    Fill(String),
    /// Resolve + mint a ref, act on nothing.
    RefOnly,
}

/// Everything the host owns for one REPL run.
pub struct ReplHost {
    pub browser: Arc<oxibrowser_core::Browser>,
    pub context: Arc<BrowserContext>,
    pub surface: Option<Arc<LoginSurface>>,
    pub session: Arc<RwLock<Session>>,
    pub broker: Option<Arc<oxibrowser_credentials::BrokerSource>>,
    /// Steering queue + popup sessions (M2).
    pub events: Arc<super::events::EventHub>,
    /// Pre-cloned dialog gate: resolution never needs the session lock
    /// (a blocking `alert()` holds the write lock).
    pub dialog_gate: oxibrowser_core::js::DialogGate,
}

/// scheme://host[:port] of a URL string, when parseable.
fn origin_of(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .map(|u| u.origin().ascii_serialization())
}

fn gate_err(e: GateError) -> Value {
    serde_json::to_value(e).unwrap_or_else(|_| json!({"code":"GATE_ERROR","message":"gate"}))
}

fn loc_err(e: locators::LocError) -> Value {
    serde_json::to_value(e).unwrap_or_else(|_| json!({"code":"LOCATOR_ERROR","message":"locator"}))
}

fn err(code: &str, message: impl Into<String>) -> Value {
    json!({"code": code, "message": message.into()})
}

impl ReplHost {
    /// Serve world commands until the channel closes.
    pub async fn run(self, mut rx: mpsc::Receiver<WorldCmd>) {
        while let Some(cmd) = rx.recv().await {
            let (result, resp) = match cmd {
                WorldCmd::Goto { url, resp } => (self.goto(&url).await, resp),
                WorldCmd::Content { resp } => (self.content().await, resp),
                WorldCmd::Observe {
                    budget_tokens,
                    resp,
                } => {
                    let mut guard = self.session.write().await;
                    let url_now = guard
                        .current_url()
                        .map(|u| u.to_string())
                        .unwrap_or_default();
                    let res = obs::observe_session(
                        &mut guard,
                        &ObsOpts {
                            budget_tokens,
                            max_actionable: None,
                        },
                    )
                    .await
                    .map_err(|e| err("OBSERVE_ERROR", e))
                    .and_then(|r| {
                        serde_json::to_value(r).map_err(|e| err("SERIALIZE_ERROR", e.to_string()))
                    })
                    .map(|mut v| {
                        if let Some(origin) = origin_of(&url_now) {
                            let hints = super::site::hints(&origin);
                            if !hints.is_empty() {
                                v["site_hints"] = serde_json::json!(hints);
                            }
                        }
                        v
                    });
                    drop(guard);
                    (res, resp)
                }
                WorldCmd::Act {
                    query,
                    action,
                    resp,
                } => (self.act(&query, &action).await, resp),
                WorldCmd::FillCredential {
                    query,
                    account_id,
                    resp,
                } => (self.fill_credential(&query, &account_id).await, resp),
                WorldCmd::Screenshot { resp } => (self.screenshot().await, resp),
                WorldCmd::Meta { resp } => (self.meta().await, resp),
                WorldCmd::WaitEvent {
                    kind,
                    timeout_ms,
                    resp,
                } => {
                    let res = self
                        .events
                        .wait_for(&kind, timeout_ms)
                        .await
                        .ok_or_else(|| {
                            err(
                                "WAIT_TIMEOUT",
                                format!("no '{kind}' event within {timeout_ms}ms"),
                            )
                        });
                    (res, resp)
                }
                WorldCmd::ResolveDialog {
                    accept,
                    prompt_text,
                    resp,
                } => {
                    super::events::resolve_dialog(&self.dialog_gate, accept, prompt_text);
                    self.events.clear_dialog_pending();
                    (Ok(json!({"resolved": true})), resp)
                }
                WorldCmd::Login { account_id, resp } => (self.login(&account_id).await, resp),
                WorldCmd::Replay { url, resp } => (self.replay(&url).await, resp),
                WorldCmd::MouseClick { x, y, resp } => (self.mouse_click(x, y).await, resp),
            };
            let _ = resp.send(result);
        }
    }

    async fn goto(&self, url: &str) -> Result<Value, HostError> {
        let mut guard = self.session.write().await;
        guard
            .navigate(url)
            .await
            .map_err(|e| err("NAVIGATION_ERROR", e.to_string()))?;
        Ok(json!({
            "url": guard.current_url().map(|u| u.to_string()).unwrap_or_default(),
            "title": guard.page().map(|p| p.title().unwrap_or_default()).unwrap_or_default(),
        }))
    }

    async fn content(&self) -> Result<Value, HostError> {
        let session = self.session.read().await;
        let page = session
            .page()
            .ok_or_else(|| err("NO_PAGE", "no page loaded — page.goto() first"))?;
        Ok(json!({
            "url": session.current_url().map(|u| u.to_string()).unwrap_or_default(),
            "title": page.title().unwrap_or_default(),
            "markdown": page.to_markdown(),
        }))
    }

    async fn meta(&self) -> Result<Value, HostError> {
        let guard = self.session.read().await;
        Ok(json!({
            "url": guard.current_url().map(|u| u.to_string()).unwrap_or_default(),
            "title": guard.page().map(|p| p.title().unwrap_or_default()).unwrap_or_default(),
        }))
    }

    async fn screenshot(&self) -> Result<Value, HostError> {
        let mut guard = self.session.write().await;
        let png = guard
            .capture_screenshot_png(1280)
            .await
            .map_err(|e| err("SCREENSHOT_ERROR", e.to_string()))?;
        let path = std::env::temp_dir().join(format!(
            "oxibrowser-repl-{}.png",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        ));
        std::fs::write(&path, &png).map_err(|e| err("IO_ERROR", e.to_string()))?;
        Ok(json!({"path": path.to_string_lossy(), "bytes": png.len()}))
    }

    async fn act(&self, query: &LocatorQuery, action: &ActKind) -> Result<Value, HostError> {
        let mut guard = self.session.write().await;
        let observation = guard
            .native_observe()
            .await
            .map_err(|e| err("OBSERVE_ERROR", e.to_string()))?;
        let el = locators::resolve(&observation.snapshot, query).map_err(loc_err)?;
        let generation = guard.page().map(|p| p.generation()).unwrap_or(0);
        let session_key = guard.id().to_string();

        // Mint the ref first (every path reports it; one observation → one
        // stable namespace, same recipe as the observe command).
        let r#ref = RefRegistry::allocate(
            &session_key,
            el.node_id,
            generation,
            observation.snapshot.fingerprint(el.node_id),
            el.selector.clone(),
        );

        match action {
            ActKind::RefOnly => {
                return Ok(json!({"ref": r#ref, "generation": generation}));
            }
            ActKind::Click => {
                let url = guard
                    .current_url()
                    .map(|u| u.to_string())
                    .unwrap_or_default();
                gates::check_click(
                    &self.context,
                    self.surface.as_deref(),
                    &url,
                    &observation.snapshot,
                    &el,
                )
                .map_err(gate_err)?;
                let target = observation
                    .target(el.node_id)
                    .ok_or_else(|| err("NOT_ACTIONABLE", "element has no native target"))?;
                let receipt = Self::native_act_guarded(
                    &mut guard,
                    target,
                    oxibrowser_core::js::runtime::NativeInput::Click,
                    &self.events,
                )
                .await?;
                if let Some(origin) = origin_of(&url) {
                    super::site::record_selector(&origin, &el.selector);
                }
                Ok(json!({
                    "acted": true, "ref": r#ref, "generation": generation,
                    "callbacks": receipt.callbacks,
                    "navigation_requested": receipt.navigation_requested,
                    "observe_required": true,
                }))
            }
            ActKind::Fill(value) => {
                gates::check_fill(&self.context, &el).map_err(gate_err)?;
                let target = observation
                    .target(el.node_id)
                    .ok_or_else(|| err("NOT_ACTIONABLE", "element has no native target"))?;
                let receipt = Self::native_act_guarded(
                    &mut guard,
                    target,
                    oxibrowser_core::js::runtime::NativeInput::Fill(value.clone()),
                    &self.events,
                )
                .await?;
                Ok(json!({
                    "acted": true, "ref": r#ref, "generation": generation,
                    "callbacks": receipt.callbacks,
                    "navigation_requested": receipt.navigation_requested,
                    "observe_required": true,
                }))
            }
        }
    }

    /// `native_act` with a 3s guard: a listener that parks the JS thread
    /// on a dialog (`alert`/`confirm`/`prompt`) would otherwise deadlock
    /// the bridge — the world thread waits on this reply and could never
    /// reach `waitForEvent('dialog')`. On timeout with a pending dialog we
    /// return early with `blocked_on_dialog`; the parked command finishes
    /// in the background once the gate is resolved (its receipt is lost —
    /// re-observe to see the effect).
    async fn native_act_guarded(
        guard: &mut tokio::sync::RwLockWriteGuard<'_, Session>,
        target: oxibrowser_core::js::runtime::NativeTarget,
        input: oxibrowser_core::js::runtime::NativeInput,
        events: &Arc<super::events::EventHub>,
    ) -> Result<oxibrowser_core::js::runtime::NativeReceipt, HostError> {
        match tokio::time::timeout(
            std::time::Duration::from_secs(3),
            guard.native_act(target, input),
        )
        .await
        {
            Ok(r) => r.map_err(|e| err("ACTION_FAILED", e.to_string())),
            Err(_) if events.dialog_pending() => {
                // The write guard drops with this early return, releasing
                // the session for the dialog-resolution path.
                Err(json!({
                    "code": "BLOCKED_ON_DIALOG",
                    "message": "action parked on a page dialog — waitForEvent('dialog'), \
                                then accept/dismiss and re-observe",
                    "blocked_on_dialog": true,
                }))
            }
            Err(_) => Err(err(
                "ACTION_STUCK",
                "action did not complete within 3s and no dialog is pending — \
                 the page may be busy; re-observe and retry",
            )),
        }
    }

    /// `page.mouse.click(x, y)` — CUA fallback: resolve the element at the
    /// viewport point via the engine's layout hit-test
    /// (`document.elementFromPoint`), then deliver a native click. The
    /// returned node id still goes through the native target validation
    /// (incarnation/attachment), so a patched `elementFromPoint` cannot
    /// forge the delivery target — worst case it points at the wrong node,
    /// the known CUA tradeoff when the DOM path has already failed.
    async fn mouse_click(&self, x: f64, y: f64) -> Result<Value, HostError> {
        let mut guard = self.session.write().await;
        let js = format!(
            r#"(function () {{
                var el = document.elementFromPoint({x}, {y});
                return el && el.__nodeId != null ? el.__nodeId : null;
            }})()"#
        );
        let node = guard
            .evaluate_js(&js)
            .await
            .map_err(|e| err("HIT_TEST_FAILED", e.to_string()))?;
        let node_id = node.value.and_then(|v| v.as_u64()).ok_or_else(|| {
            err(
                "NO_ELEMENT_AT_POINT",
                format!("no element at ({x}, {y}) — check page.screenshot() coordinates"),
            )
        })? as u32;
        let observation = guard
            .native_observe()
            .await
            .map_err(|e| err("OBSERVE_ERROR", e.to_string()))?;
        let target = observation
            .target(node_id)
            .ok_or_else(|| err("NOT_ACTIONABLE", "hit-test node has no native target"))?;
        let receipt = Self::native_act_guarded(
            &mut guard,
            target,
            oxibrowser_core::js::runtime::NativeInput::Click,
            &self.events,
        )
        .await?;
        Ok(json!({
            "clicked": true, "x": x, "y": y, "node_id": node_id,
            "callbacks": receipt.callbacks,
            "observe_required": true,
        }))
    }

    /// `page.replay({ url })` — replay the last captured network request
    /// matching `url` through the page's own `fetch` (same session, same
    /// cookies, same fingerprint — the site cannot tell it apart).
    async fn replay(&self, url_sub: &str) -> Result<Value, HostError> {
        let mut guard = self.session.write().await;
        let log = guard.network_log_snapshot();
        let entry = log
            .iter()
            .rev()
            .find(|r| {
                r.resource_type != "Document" && url_sub.is_empty()
                    || r.url.contains(url_sub) && !r.url.contains("data:")
            })
            .ok_or_else(|| {
                err(
                    "REPLAY_TARGET_NOT_FOUND",
                    format!("no captured request matches {url_sub:?} — navigate/interact first"),
                )
            })?;
        let (url, method, headers, body) = (
            entry.url.clone(),
            entry.method.clone(),
            entry.request_headers.clone(),
            entry.post_body.clone(),
        );
        let header_json = serde_json::to_string(
            &headers
                .iter()
                .map(|(k, v)| (k, v))
                .collect::<std::collections::BTreeMap<_, _>>(),
        )
        .unwrap_or_else(|_| "{}".into());
        let body_json = serde_json::to_string(
            &body
                .as_deref()
                .map(String::from_utf8_lossy)
                .unwrap_or_default(),
        )
        .unwrap_or_else(|_| "null".into());
        let js = format!(
            r#"(async () => {{
                const r = await fetch({url:?}, {{
                    method: {method:?},
                    headers: {header_json},
                    body: {method:?} === 'GET' || {method:?} === 'HEAD' ? undefined : {body_json},
                }});
                const text = await r.text();
                return JSON.stringify({{ status: r.status, body: text.slice(0, 4000) }});
            }})()"#,
            url = url,
            method = method,
        );
        let result = guard
            .evaluate_js_with_await(&js, true)
            .await
            .map_err(|e| err("REPLAY_FAILED", e.to_string()))?;
        let payload = result
            .value
            .and_then(|v| v.as_str().map(str::to_string))
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .unwrap_or(json!({"status": 0, "body": ""}));
        if let Some(origin) = origin_of(&url) {
            super::site::record_endpoint(&origin, &format!("{method} {url}"));
        }
        Ok(payload)
    }

    /// `page.login(account)` — agent-driven login inside the account's
    /// bound context. The login session shares the context jar, so fresh
    /// cookies are immediately visible to the live page (no session swap).
    async fn login(&self, account_id: &str) -> Result<Value, HostError> {
        let (surface, broker) = match (self.surface.as_ref(), self.broker.as_ref()) {
            (Some(s), Some(b)) => (s, b),
            _ => {
                return Err(err(
                    "BROKER_UNAVAILABLE",
                    "page.login requires --account (login surface + broker)",
                ));
            }
        };
        let ctx = surface.context_for_account(account_id).ok_or_else(|| {
            err(
                "ACCOUNT_NOT_BOUND",
                format!("account {account_id:?} is not bound"),
            )
        })?;
        let orch = surface.orchestrator();
        let engine = oxibrowser_core::account::AgentLoginEngine::new(
            orch.shared_manager(),
            orch.shared_keys(),
            broker.clone(),
            "repl",
        )
        .with_events(orch.event_sender());
        let session = self
            .browser
            .new_session_in(&ctx)
            .await
            .map_err(|e| err("RUNTIME_ERROR", e.to_string()))?;
        let outcome = {
            let mut guard = session.write().await;
            engine.login(account_id, &mut guard).await
        };
        // The login session's whole point was the shared jar; close it and
        // reclaim the slot (envelope capture already happened on success).
        if let Err(e) = session.write().await.close().await {
            tracing::warn!(error = %e, "login session close failed");
        }
        self.browser.cleanup_closed_sessions();
        match outcome {
            Ok(o) => Ok(json!({
                "account_id": account_id,
                "state": o.state(),
                "detail": o.detail(),
                "retryable": o.retryable(),
            })),
            Err(e) => Err(err("LOGIN_FAILED", e.to_string())),
        }
    }

    /// `oxi.fillCredential(locator, account)` — broker-resolved password,
    /// injected natively; the value never transits the model context.
    async fn fill_credential(
        &self,
        query: &LocatorQuery,
        account_id: &str,
    ) -> Result<Value, HostError> {
        let broker = self
            .broker
            .as_ref()
            .ok_or_else(|| err("BROKER_UNAVAILABLE", "fillCredential requires --account"))?;
        let (surface, record) = match self.surface.as_ref() {
            Some(s) => match s.orchestrator().shared_manager().registry().get(account_id) {
                Ok(r) => (s, r),
                Err(e) => return Err(err("ACCOUNT_NOT_FOUND", e.to_string())),
            },
            None => return Err(err("BROKER_UNAVAILABLE", "no login surface")),
        };

        let mut guard = self.session.write().await;
        let origin = guard
            .current_url()
            .map(|u| oxibrowser_core::network::origin_policy::Origin::parse(u.as_str()))
            .transpose()
            .map_err(|e| err("NO_PAGE", format!("page origin unusable: {e}")))?
            .ok_or_else(|| err("NO_PAGE", "fillCredential needs a page with an origin"))?;
        let resolved = broker
            .resolve_login(&record.credentials, &record.scope, &origin)
            .map_err(|e| err("CONSENT_REQUIRED", format!("{e:?}")))?;
        let observation = guard
            .native_observe()
            .await
            .map_err(|e| err("OBSERVE_ERROR", e.to_string()))?;
        let el = locators::resolve(&observation.snapshot, query).map_err(loc_err)?;
        if !el
            .input_type
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case("password"))
        {
            return Err(err(
                "NOT_A_PASSWORD_FIELD",
                "fillCredential targets password fields only",
            ));
        }
        let target = observation
            .target(el.node_id)
            .ok_or_else(|| err("NOT_ACTIONABLE", "element has no native target"))?;
        let receipt = guard
            .native_act(
                target,
                oxibrowser_core::js::runtime::NativeInput::FillCredential(
                    resolved.password.to_string(),
                ),
            )
            .await
            .map_err(|e| err("ACTION_FAILED", e.to_string()))?;
        let _ = surface; // audit flows through the broker
        Ok(json!({
            "acted": true,
            "handle": resolved.handle,
            "fingerprint": resolved.fingerprint,
            "username_hint": resolved.username,
            "callbacks": receipt.callbacks,
            "observe_required": true,
        }))
    }
}

// ---------------------------------------------------------------------------
// CLI entry — `oxibrowser repl`
// ---------------------------------------------------------------------------

/// Run the REPL: stdin lines are JS program blocks (top-level `await`
pub async fn run_repl(
    account: Option<&str>,
    allow_private_ips: bool,
    ref_tag: Option<&str>,
) -> i32 {
    let setup = match WorldSetup::new(account, allow_private_ips, ref_tag).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let world = &setup.world;

    // stdin on a dedicated thread → tokio channel (session REPL idiom).
    let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        use std::io::BufRead;
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(l) => {
                    if line_tx.send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    'world_loop: loop {
        let Some(line) = line_rx.recv().await else {
            break 'world_loop;
        };
        let src = line.trim();
        if src.is_empty() || src.starts_with('#') {
            continue;
        }
        if src == "exit" || src == "quit" {
            println!(
                "{}",
                serde_json::to_string(&json!({"ok": true, "data": {"exit": true}, "meta": {}}))
                    .unwrap()
            );
            break 'world_loop;
        }
        let started = std::time::Instant::now();
        let elapsed = || started.elapsed().as_millis() as u64;
        let resp = match world.eval(src) {
            Ok((value, done)) => json!({
                "ok": true, "data": value,
                "done": done,
                "meta": {"elapsed_ms": elapsed()},
            }),
            Err(e) => json!({"ok": false, "error": e, "meta": {"elapsed_ms": elapsed()}}),
        };
        println!("{}", serde_json::to_string(&resp).unwrap());
    }
    0
}

/// Shared assembly for `repl` and `agent run`: browser, (account-bound)
/// context, session, steering hub, host task, world thread.
pub struct WorldSetup {
    pub world: super::world::World,
    surface: Option<Arc<LoginSurface>>,
    context: Arc<BrowserContext>,
    session: Arc<RwLock<Session>>,
    browser: Arc<oxibrowser_core::Browser>,
    pub account_bound: bool,
}

impl WorldSetup {
    pub async fn new(
        account: Option<&str>,
        allow_private_ips: bool,
        ref_tag: Option<&str>,
    ) -> Result<Self, String> {
        let mut config = oxibrowser_core::BrowserConfig::headless();
        if allow_private_ips {
            config.enable_ssrf_filter = false;
            eprintln!("⚠ SSRF filter disabled: private/internal IP ranges accessible.");
        }
        let browser = Arc::new(
            oxibrowser_core::Browser::new(config)
                .await
                .map_err(|e| format!("browser init failed: {e}"))?,
        );

        let (context, surface, broker) = match account {
            Some(id) => setup_account(&browser, id, ref_tag).await?,
            None => (browser.default_context(), None, None),
        };
        let session = browser
            .new_session_in(&context)
            .await
            .map_err(|e| format!("session init failed: {e}"))?;
        // Clone the gate BEFORE any action: dialog resolution must never
        // need the session lock.
        let dialog_gate = session.read().await.dialog_gate();
        let events = {
            let mut guard = session.write().await;
            super::events::EventHub::attach(browser.clone(), context.clone(), &mut guard)
        };
        // Agent-plane logical viewport (execution design M1 §4.4): fixed
        // for CUA reproducibility; process-global override, so repl/agent
        // must stay dedicated processes (no serve in the same process).
        oxibrowser_core::session::set_viewport_override(1440, 900);
        // Downloads land in a per-run temp dir (process-global setting —
        // repl/agent are dedicated processes by design, execution doc §1).
        let download_dir = std::env::temp_dir().join(format!(
            "oxibrowser-agent-dl-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        ));
        oxibrowser_core::session::set_download_behavior(Some(download_dir));

        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        let host = ReplHost {
            browser: browser.clone(),
            context: context.clone(),
            surface: surface.clone(),
            session: session.clone(),
            broker,
            events,
            dialog_gate,
        };
        tokio::spawn(host.run(cmd_rx));
        let world = super::world::World::spawn(cmd_tx);
        Ok(Self {
            world,
            surface,
            context,
            session,
            browser,
            account_bound: account.is_some(),
        })
    }

    /// Login surface for consent flows (`agent run`).
    pub fn host_surface(&self) -> Option<Arc<LoginSurface>> {
        self.surface.clone()
    }

    pub fn context_id(&self) -> Option<String> {
        Some(self.context.id().to_string())
    }

    /// Best-effort cleanup: close the session (drops the world with it).
    pub async fn shutdown(mut self) -> Result<(), String> {
        let _ = &mut self.browser;
        self.session
            .write()
            .await
            .close()
            .await
            .map_err(|e| e.to_string())
    }
}

/// Account binding for the REPL: restored-envelope credential-mode context
/// + `bind_direct` (the local user launched this process, so
/// pattern-matched irreversible actions are allowed — same semantics as
/// `session/serve --account`) + the broker for `oxi.fillCredential`.
async fn setup_account(
    browser: &std::sync::Arc<oxibrowser_core::Browser>,
    id: &str,
    ref_tag: Option<&str>,
) -> Result<
    (
        std::sync::Arc<oxibrowser_core::context::BrowserContext>,
        Option<std::sync::Arc<LoginSurface>>,
        Option<std::sync::Arc<oxibrowser_credentials::BrokerSource>>,
    ),
    String,
> {
    let ctx = crate::account_cli::bind_account_context(browser, id, ref_tag).await?;
    let mut runtime = crate::session::executor::AccountRuntime::new(browser.clone());
    let orch = runtime.orch()?;
    let surface = LoginSurface::new(orch, browser.clone());
    surface.bind_direct(id, ctx.clone());
    let broker = runtime.agent_source()?;
    Ok((ctx, Some(surface), Some(broker)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::locators::{LocatorQuery, LocatorSpec};
    use oxibrowser_cdp::account::LoginSurface;
    use oxibrowser_core::account::{AccountManager, AccountRecord, LoginOrchestrator};
    use oxibrowser_core::security::audit::AuditLog;
    use oxibrowser_core::storage::session_store::StaticKeyProvider;
    use oxibrowser_credentials::{
        BrokerSource, ConsentRecord, ConsentStore, ConsentSubject, CredentialKind,
        CredentialProvider, InMemoryProvider, NewCredential, PolicyEngine, SecretBox,
    };
    use std::io::Write as _;

    /// Loopback fixture serving `html` for every request.
    fn serve(html: String) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buf = [0u8; 4096];
                let _ = std::io::Read::read(&mut stream, &mut buf);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    html.len(),
                    html
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        port
    }

    async fn test_browser() -> Arc<oxibrowser_core::Browser> {
        let mut config = oxibrowser_core::BrowserConfig::headless();
        config.enable_ssrf_filter = false;
        Arc::new(oxibrowser_core::Browser::new(config).await.unwrap())
    }

    /// Build a ReplHost with the M2 steering attachments (events hub +
    /// pre-cloned dialog gate).
    async fn host_with_events(
        browser: &Arc<oxibrowser_core::Browser>,
        context: Arc<oxibrowser_core::context::BrowserContext>,
        surface: Option<Arc<LoginSurface>>,
        broker: Option<Arc<BrokerSource>>,
        session: Arc<RwLock<Session>>,
    ) -> ReplHost {
        let dialog_gate = session.read().await.dialog_gate();
        let events = {
            let mut guard = session.write().await;
            crate::agent::events::EventHub::attach(browser.clone(), context.clone(), &mut guard)
        };
        ReplHost {
            browser: browser.clone(),
            context,
            surface,
            session,
            broker,
            events,
            dialog_gate,
        }
    }

    fn password_query() -> LocatorQuery {
        LocatorQuery {
            spec: LocatorSpec::Role {
                role: "textbox".into(),
                name: Some("Password".into()),
            },
            nth: None,
        }
    }

    const FORM_HTML: &str = r#"<!doctype html>
<html><head><title>Form</title></head><body>
  <input name="u" aria-label="Username">
  <input type="password" name="p" aria-label="Password">
  <button type="submit">Sign in</button>
</form></body></html>"#;

    #[tokio::test]
    async fn password_literal_fill_denied_in_credential_mode() {
        let browser = test_browser().await;
        let ctx = browser
            .new_context(oxibrowser_core::ContextConfig {
                label: None,
                proxy: None,
            })
            .unwrap();
        ctx.set_credential_mode(true);
        let session = browser.new_session_in(&ctx).await.unwrap();
        let host = host_with_events(&browser, ctx, None, None, session.clone()).await;
        let port = serve(FORM_HTML.into());
        session
            .write()
            .await
            .navigate(&format!("http://127.0.0.1:{port}/"))
            .await
            .unwrap();

        let denied = host
            .act(&password_query(), &ActKind::Fill("hunter2".into()))
            .await
            .unwrap_err();
        assert_eq!(denied["code"], "POLICY_DENIED");
        assert!(
            denied["message"]
                .as_str()
                .unwrap()
                .contains("passwordFillRequiresCredential"),
            "got {denied}"
        );

        // Non-password fields still fill in credential mode.
        let username = LocatorQuery {
            spec: LocatorSpec::Role {
                role: "textbox".into(),
                name: Some("Username".into()),
            },
            nth: None,
        };
        host.act(&username, &ActKind::Fill("alice".into()))
            .await
            .expect("username literal fill is allowed");

        // And without credential mode, password literal fills are ordinary
        // browsing (same condition as `OXI.fillRef`).
        let plain = browser.default_context();
        let session2 = browser.new_session_in(&plain).await.unwrap();
        let host2 = host_with_events(&browser, plain, None, None, session2.clone()).await;
        session2
            .write()
            .await
            .navigate(&format!("http://127.0.0.1:{port}/plain"))
            .await
            .unwrap();
        let denied2 = host2
            .act(&password_query(), &ActKind::Fill("hunter2".into()))
            .await
            .expect_err("literal password fill is denied at the native layer too");
        // Defense in depth: the REPL gate fires first in credential mode;
        // the native layer denies literal password fills everywhere, so
        // only broker-authorized FillCredential ever writes one.
        assert_eq!(denied2["code"], "ACTION_FAILED");
        assert!(
            denied2["message"]
                .as_str()
                .unwrap()
                .contains("NATIVE_CONTROL_DENIED")
        );
    }

    #[tokio::test]
    async fn fill_credential_resolves_broker_and_fills_natively() {
        let browser = test_browser().await;
        let dir = std::env::temp_dir().join(format!(
            "oxi-repl-fillcred-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let audit = Arc::new(AuditLog::open(dir.join("audit.jsonl")).unwrap());
        let registry =
            oxibrowser_core::account::AccountRegistry::open(dir.join("accounts")).unwrap();
        registry
            .add(AccountRecord::new("acc", "127.0.0.1").unwrap())
            .unwrap();
        let manager = Arc::new(AccountManager::with_audit(registry, audit));
        let orch = Arc::new(LoginOrchestrator::new(
            manager,
            browser.clone(),
            Arc::new(StaticKeyProvider::new([7u8; 32])),
        ));
        let surface = LoginSurface::new(orch, browser.clone());

        let port = serve(FORM_HTML.into());
        let origin = format!("http://127.0.0.1:{port}");
        let provider = InMemoryProvider::new();
        let cred_id = provider
            .put(NewCredential {
                agent_id: "main".into(),
                scope: "127.0.0.1".into(),
                kind: CredentialKind::Password,
                slug: "pw".into(),
                allowed_origins: vec![origin.clone()],
                login_hint: Some("alice".into()),
                password: Some(SecretBox::from_string("s3cret-value".into())),
                otpauth_uri: None,
            })
            .unwrap();
        let consents = ConsentStore::open(dir.join("consents.jsonl")).unwrap();
        consents
            .grant(ConsentRecord::new(
                ConsentSubject::Credential {
                    credential: cred_id,
                },
                &origin,
                &["login"],
                chrono::Duration::minutes(10),
                3,
            ))
            .unwrap();
        let engine = Arc::new(PolicyEngine::new(
            Vec::new(),
            consents,
            Arc::new(AuditLog::open(dir.join("policy.jsonl")).unwrap()),
        ));
        let broker = Arc::new(BrokerSource::new(Arc::new(provider), engine));

        let ctx = browser
            .new_context(oxibrowser_core::ContextConfig {
                label: Some("account:acc".into()),
                proxy: None,
            })
            .unwrap();
        ctx.set_credential_mode(true);
        surface.bind_direct("acc", ctx.clone());
        let session = browser.new_session_in(&ctx).await.unwrap();
        let host =
            host_with_events(&browser, ctx, Some(surface), Some(broker), session.clone()).await;
        session
            .write()
            .await
            .navigate(&format!("{origin}/"))
            .await
            .unwrap();

        let out = host
            .fill_credential(&password_query(), "acc")
            .await
            .expect("fillCredential resolves the broker");
        assert_eq!(out["acted"], true);
        assert_eq!(out["username_hint"], "alice");

        // The fill landed (acted, no error) AND the observation surface
        // redacts it — the secret must not leak back through snapshots.
        let value = {
            let mut guard = session.write().await;
            let obs = guard.native_observe().await.unwrap();
            obs.snapshot
                .interactive_elements()
                .into_iter()
                .find(|el| el.input_type.as_deref() == Some("password"))
                .and_then(|el| {
                    obs.snapshot
                        .nodes
                        .get(&el.node_id)
                        .and_then(|n| n.attributes.get("value").cloned())
                })
        };
        assert_eq!(value.as_deref(), Some("__REDACTED__"));
        assert!(
            !out.to_string().contains("s3cret-value"),
            "the secret must not appear in the returned payload"
        );
    }
}
