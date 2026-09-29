//! Account/login surface for the CDP server (upper design §5.2, §7.3).
//!
//! [`LoginSurface`] glues the core [`LoginOrchestrator`] to CDP concerns:
//!
//! - **takeover windows** (§5.2): while a login window is open for a
//!   context, agent connections may not drive input or capture in it —
//!   only the viewer (the human's mirror connection) may.
//! - **one-time viewer tokens**: issued by `OXI.beginLogin` in user mode,
//!   delivered out-of-band (CLI stdout / host channel), consumed by the
//!   first viewer WebSocket upgrade carrying `X-Oxi-Viewer-Token`.
//! - **login sessions**: each window gets a dedicated session in the
//!   account context, so `OXI.reportLoginSuccess` / `OXI.endLogin` capture
//!   the right jar regardless of which connection reports.
//!
//! Values never cross this module: it handles ids, states, and envelopes
//! via the orchestrator only.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock};
use std::time::{Duration, Instant};

use oxibrowser_core::Browser;
use oxibrowser_core::account::{
    EndOutcome, LoginEndState, LoginHandle, LoginMode, LoginOrchestrator, LoginOutcome,
};
use oxibrowser_core::context::{BrowserContext, ContextId};
use oxibrowser_core::session::Session;
use tokio::sync::RwLock;

use crate::credential::CredentialBroker;

/// One open takeover window (context-scoped).
struct Takeover {
    login_id: String,
    account_id: String,
    until: Instant,
}

/// Snapshot of an active takeover for gating decisions.
#[derive(Debug, Clone)]
pub struct TakeoverView {
    pub login_id: String,
    pub account_id: String,
}

/// CDP-side login state: orchestrator + takeover registry + viewer tokens.
pub struct LoginSurface {
    orchestrator: Arc<LoginOrchestrator>,
    browser: Arc<Browser>,
    /// account_id → bound context (`serve --account`, `oxiAccount`).
    bindings: StdMutex<HashMap<String, Arc<BrowserContext>>>,
    /// context_id → allowed to perform pattern-matched irreversible
    /// actions (roadmap item 16): CLI-bound contexts (local user direct)
    /// and `createBrowserContext` contexts whose agent holds an
    /// `irreversible` grant.
    irreversible_contexts: StdMutex<HashSet<String>>,
    /// context_id → active takeover (lazy expiry on read).
    takeovers: StdRwLock<HashMap<String, Takeover>>,
    /// One-time viewer tokens: token → context_id. Removed on first use.
    viewer_tokens: StdMutex<HashMap<String, String>>,
    /// login_id → the window's dedicated session (in the account context).
    login_sessions: StdMutex<HashMap<String, Arc<RwLock<Session>>>>,
    /// Accounts with a running unattended agent login (one at a time each).
    agent_runs: Arc<StdMutex<HashSet<String>>>,
}

impl LoginSurface {
    /// Surface over `orchestrator`; login contexts are minted on `browser`.
    pub fn new(orchestrator: Arc<LoginOrchestrator>, browser: Arc<Browser>) -> Arc<Self> {
        Arc::new(LoginSurface {
            orchestrator,
            browser,
            bindings: StdMutex::new(HashMap::new()),
            irreversible_contexts: StdMutex::new(HashSet::new()),
            takeovers: StdRwLock::new(HashMap::new()),
            viewer_tokens: StdMutex::new(HashMap::new()),
            login_sessions: StdMutex::new(HashMap::new()),
            agent_runs: Arc::new(StdMutex::new(HashSet::new())),
        })
    }

    /// The orchestrator (event subscription, registry reads).
    pub fn orchestrator(&self) -> &LoginOrchestrator {
        &self.orchestrator
    }

    /// Launch an unattended agent login (M-D §5.3): mints a fresh account
    /// context and session, then the core [`AgentLoginEngine`] drives it on a
    /// detached task — jar restore, login-page discovery, broker-gated
    /// injection, TOTP, detection, capture. The call returns the run id
    /// immediately; progress and the terminal state flow through the
    /// orchestrator's event channel (`OXI.loginStateChanged`).
    ///
    /// One run per account at a time. Credential-plane authorization happens
    /// inside the engine's [`CredentialSource`]; `broker` supplies provider +
    /// policy engine.
    pub fn start_agent_login(
        &self,
        account_id: &str,
        agent_id: &str,
        broker: Arc<CredentialBroker>,
        timeout: Option<Duration>,
    ) -> anyhow::Result<String> {
        // Account existence up front (scoped error, not a browser error).
        self.orchestrator
            .manager()
            .registry()
            .get(account_id)
            .map_err(|e| anyhow::anyhow!("invalidAccount: {e}"))?;

        {
            let mut runs = self
                .agent_runs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !runs.insert(account_id.to_string()) {
                anyhow::bail!("an agent login is already running for {account_id}");
            }
        }

        let source = Arc::new(oxibrowser_credentials::BrokerSource::new(
            Arc::clone(&broker.provider),
            Arc::clone(&broker.engine),
        ));
        let mut engine = oxibrowser_core::account::AgentLoginEngine::new(
            self.orchestrator.shared_manager(),
            self.orchestrator.shared_keys(),
            source,
            agent_id,
        )
        .with_events(self.orchestrator.event_sender());
        if let Some(timeout) = timeout {
            engine = engine.with_timeout(timeout);
        }
        let login_id = engine.login_id.clone();

        let surface_accounts = account_id.to_string();
        let runs_guard = Arc::clone(&self.agent_runs);
        let orchestrator = Arc::clone(&self.orchestrator);
        tokio::spawn(async move {
            let opened = orchestrator.open_account_context(&surface_accounts).await;
            let (_ctx, session) = match opened {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::warn!(account = %surface_accounts, error = %e, "agent login context open failed");
                    runs_guard
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&surface_accounts);
                    return;
                }
            };
            let mut guard = session.write().await;
            let outcome = engine.login(&surface_accounts, &mut guard).await;
            drop(guard);
            if let Err(e) = outcome {
                tracing::warn!(account = %surface_accounts, error = %e, "agent login failed");
            }
            runs_guard
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&surface_accounts);
        });

        Ok(login_id)
    }

    /// Bind an account to an existing context (the `serve --account` /
    /// `Target.createBrowserContext {oxiAccount}` path). No irreversible
    /// capability — that is granted by [`LoginSurface::mark_irreversible`]
    /// (grant probe) or [`LoginSurface::bind_direct`] (local user).
    pub fn bind(&self, account_id: &str, ctx: Arc<BrowserContext>) {
        self.bindings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(account_id.to_string(), ctx);
    }

    /// Bind + irreversible capability for CLI-launched contexts
    /// (`serve/session --account`): the local user authorized the session
    /// directly, so pattern-matched irreversible actions are allowed.
    pub fn bind_direct(&self, account_id: &str, ctx: Arc<BrowserContext>) {
        self.mark_irreversible(ctx.id().as_str());
        self.bind(account_id, ctx);
    }

    /// Mark a context as irreversible-capable (the `createBrowserContext`
    /// grant-probe path — item 16).
    pub fn mark_irreversible(&self, context_id: &str) {
        self.irreversible_contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(context_id.to_string());
    }

    /// May interactions in this context execute pattern-matched irreversible
    /// actions?
    pub fn irreversible_allowed(&self, context_id: &str) -> bool {
        self.irreversible_contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(context_id)
    }

    /// Reverse lookup: which account is this context bound to?
    pub fn account_for_context(&self, context_id: &str) -> Option<String> {
        let bindings = self
            .bindings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        bindings
            .iter()
            .find(|(_, ctx)| ctx.id().as_str() == context_id)
            .map(|(account, _)| account.clone())
    }
    /// Register the takeover window for an open login (context, deadline,
    /// one-time viewer token).
    fn register_takeover(&self, handle: &LoginHandle, ctx: Arc<BrowserContext>) {
        let context_id = ctx.id().as_str().to_string();
        if let Some(token) = &handle.viewer_token {
            self.viewer_tokens
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(token.clone(), context_id.clone());
        }
        self.takeovers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                context_id,
                Takeover {
                    login_id: handle.login_id.clone(),
                    account_id: handle.account_id.clone(),
                    until: Instant::now() + Duration::from_millis(handle.timeout_ms),
                },
            );
    }

    /// Open a login window for `account_id` and register its takeover:
    /// reuses the bound context when there is one, mints (and binds) a fresh
    /// account context otherwise. Returns the handle plus the context the
    /// window runs in.
    pub async fn begin_login(
        &self,
        account_id: &str,
        mode: LoginMode,
        timeout: Option<Duration>,
    ) -> anyhow::Result<(oxibrowser_core::account::LoginHandle, Arc<BrowserContext>)> {
        // State check first — don't leak a context for a rejected window.
        let handle = self
            .orchestrator
            .begin_login(account_id, mode, timeout)
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        let ctx = {
            let bindings = self
                .bindings
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            bindings.get(account_id).cloned()
        };
        let ctx = match ctx {
            Some(ctx) => ctx,
            None => {
                let (ctx, _session) = self.orchestrator.open_account_context(account_id).await?;
                self.bind(account_id, Arc::clone(&ctx));
                ctx
            }
        };

        // Dedicated capture session inside the window's context.
        let session = match self.browser.new_session_in(&ctx).await {
            Ok(s) => s,
            Err(e) => {
                let _ = self.orchestrator.abort(&handle.login_id).await;
                return Err(anyhow::anyhow!("{}", e));
            }
        };
        self.login_sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(handle.login_id.clone(), session);

        self.register_takeover(&handle, Arc::clone(&ctx));
        Ok((handle, ctx))
    }

    /// Consume a one-time viewer token, returning the takeover context.
    /// `None` when the token is unknown, used, or its window expired.
    pub fn take_viewer(&self, token: &str) -> Option<Arc<BrowserContext>> {
        let context_id = {
            let mut tokens = self
                .viewer_tokens
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            tokens.remove(token)?
        };
        // The window must still be live — an expired takeover does not
        // grant viewer powers.
        self.active_takeover(&context_id)?;
        self.browser.context(&ContextId::from_string(context_id))
    }

    /// The active takeover for `context_id`, if any (expired entries are
    /// pruned lazily).
    pub fn active_takeover(&self, context_id: &str) -> Option<TakeoverView> {
        let mut takeovers = self
            .takeovers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match takeovers.get(context_id) {
            Some(t) if Instant::now() < t.until => Some(TakeoverView {
                login_id: t.login_id.clone(),
                account_id: t.account_id.clone(),
            }),
            Some(_) => {
                takeovers.remove(context_id);
                None
            }
            None => None,
        }
    }

    /// `OXI.reportLoginSuccess` — explicit success on the window's own
    /// session (not the reporting connection's). Closes the window on
    /// capture.
    pub async fn report_success(&self, login_id: &str) -> anyhow::Result<LoginOutcome> {
        let session = self.login_session(login_id)?;
        let mut guard = session.write().await;
        let outcome = match self
            .orchestrator
            .complete_login(login_id, &mut guard, true)
            .await
        {
            Ok(oxibrowser_core::account::CompleteVerdict::Captured { record, .. }) => {
                LoginOutcome {
                    login_id: login_id.to_string(),
                    account_id: record.account_id.clone(),
                    end_state: LoginEndState::Captured,
                    record: Some(*record),
                    detection: None,
                }
            }
            Ok(oxibrowser_core::account::CompleteVerdict::NotYet { detection }) => {
                return Err(anyhow::anyhow!(
                    "explicit success reported but the detector found no evidence: {} signals",
                    detection.signals.len()
                ));
            }
            Err(e) => return Err(anyhow::anyhow!("{}", e)),
        };
        drop(guard);
        self.close_login(login_id);
        Ok(outcome)
    }

    /// `OXI.endLogin {outcome}` — host-explicit end. `done` captures via the
    /// explicit path; `abort` reverts to `needs_login`.
    pub async fn end_login(
        &self,
        login_id: &str,
        outcome: EndOutcome,
    ) -> anyhow::Result<LoginOutcome> {
        let result = match outcome {
            EndOutcome::Done => {
                let session = self.login_session(login_id)?;
                let mut guard = session.write().await;
                self.orchestrator
                    .end_login(login_id, &mut guard, EndOutcome::Done)
                    .await
            }
            EndOutcome::Abort => self.orchestrator.abort(login_id).await,
        };
        let outcome = result.map_err(|e| anyhow::anyhow!("{}", e))?;
        self.close_login(login_id);
        Ok(outcome)
    }

    fn login_session(&self, login_id: &str) -> anyhow::Result<Arc<RwLock<Session>>> {
        self.login_sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(login_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown or closed loginId {login_id}"))
    }

    /// Close the takeover opened for `login_id` (capture / abort / timeout).
    pub fn close_login(&self, login_id: &str) {
        let closed_context = {
            let mut takeovers = self
                .takeovers
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let context = takeovers
                .iter()
                .find(|(_, t)| t.login_id == login_id)
                .map(|(ctx, _)| ctx.clone());
            if let Some(ctx) = &context {
                takeovers.remove(ctx);
            }
            context
        };
        if let Some(context_id) = closed_context {
            self.viewer_tokens
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|_, ctx| *ctx != context_id);
        }
        self.login_sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(login_id);
    }
}

impl LoginSurface {
    /// Explicit envelope capture of a **bound** account's live context
    /// (`OXI.captureSession`, roadmap item 3): mint a session inside the
    /// bound context (it sees the context's live jar + storage), seal the
    /// envelope via the shared manager, close the session again.
    ///
    /// The use case is "stop the work" — the operator (knock) captures the
    /// freshest cookies right before tearing the child down, instead of
    /// waiting for a detector-gated auto-save.
    pub async fn capture_bound(
        &self,
        account_id: &str,
    ) -> anyhow::Result<oxibrowser_core::account::AccountRecord> {
        let ctx = {
            let bindings = self
                .bindings
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            bindings.get(account_id).cloned()
        }
        .ok_or_else(|| anyhow::anyhow!("noBoundContext: account {account_id} is not bound"))?;

        let session = self
            .browser
            .new_session_in(&ctx)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let manager = Arc::clone(&self.orchestrator().shared_manager());
        let keys = self.orchestrator().shared_keys();
        let account = account_id.to_string();
        // Capture reads the keychain + writes the envelope synchronously —
        // keep it off the tokio worker (same pattern as restore).
        let capture_session = Arc::clone(&session);
        let record = tokio::task::spawn_blocking(move || {
            let guard = capture_session.blocking_read();
            manager.capture_session(&account, &guard, keys.as_ref())
        })
        .await
        .map_err(|e| anyhow::anyhow!("capture task failed: {e}"))
        .and_then(|r| r.map_err(|e| anyhow::anyhow!("{e}")));
        // The capture session was a camera, not a worker — close it even on
        // failure (a locked account or keychain error must not leak the
        // session slot + runtime threads inside the bound context).
        if let Err(e) = session.write().await.close().await {
            tracing::warn!(error = %e, "capture session close failed");
        }
        self.browser.cleanup_closed_sessions();
        let record = record?;
        Ok(record)
    }
}
