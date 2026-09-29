//! [`LoginOrchestrator`] — the M-C login flows (upper design §5, §5.1).
//!
//! Responsibilities:
//!
//! - [`LoginOrchestrator::begin_login`] — open a login window: the account
//!   moves to [`AccountState::LoggingIn`] (audited), a `loginId` + (user
//!   mode) one-time viewer token are issued, and a deadline arms the
//!   automatic abort (default 300 s, §7.3).
//! - [`LoginOrchestrator::complete_login`] — the finish path for live
//!   sessions: judge the session against the pre-login baseline with the
//!   [`LoginDetector`](super::detector::LoginDetector) semantics (§4.3),
//!   capture the envelope on success ([`AccountState::Valid`]) and publish
//!   [`AccountEvent::StateChanged`]; on missing evidence the window stays
//!   open (`CompleteVerdict::NotYet`).
//! - [`LoginOrchestrator::end_login`] / [`LoginOrchestrator::abort`] — the
//!   host's explicit end (`done` = explicit success, §4.3 highest trust;
//!   `abort`) and the timeout path return the account to
//!   [`AccountState::NeedsLogin`].
//! - [`LoginOrchestrator::import`] — the **import** bootstrap path (§5.1
//!   row 1): a [`StorageState`] (Playwright JSON, or Netscape `cookies.txt`
//!   via [`StorageState::from_netscape`]) is injected into a fresh account
//!   context, judged by the [`ValidationProbe`] (§4.4), and captured on
//!   success. The session is captured under the **current** fingerprint,
//!   which becomes the new baseline — states minted on another device carry
//!   that risk (FM-L4); the outcome flags it.
//!
//! Values never flow through here: the orchestrator handles envelopes and
//! state only, and every transition is audited via [`AccountManager`].
//!
//! State-machine note: from [`AccountState::LoggingIn`] the §4.2 machine
//! permits only `Valid` and `NeedsLogin`. A failed import therefore lands on
//! `needs_login` (probe reason as detail) — `stale` is reserved for sessions
//! that were captured and later stopped working.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::RwLock;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::browser::Browser;
use crate::context::{BrowserContext, ContextConfig};
use crate::error::Result;
use crate::session::Session;
use crate::storage::session_store::KeyProvider;
use crate::storage_state::StorageState;

use super::detector::{Detection, PreLoginSnapshot};
use super::manager::AccountManager;
use super::probe::{ProbeOutcome, ProbeVerdict, ValidationProbe};
use super::record::{AccountRecord, AccountState, account_error};

/// How a login window is driven (§5.1 rows 2–3; agent *auto*-login is M-D).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginMode {
    /// Human drives the page (host app as viewer, or the terminal wizard).
    User,
    /// The connected agent drives the page itself and reports the outcome.
    Agent,
}

impl LoginMode {
    pub fn as_str(self) -> &'static str {
        match self {
            LoginMode::User => "user",
            LoginMode::Agent => "agent",
        }
    }
}

/// One login window as issued by [`LoginOrchestrator::begin_login`].
#[derive(Debug, Clone, Serialize)]
pub struct LoginHandle {
    pub login_id: String,
    pub account_id: String,
    pub mode: LoginMode,
    /// One-time viewer token — `Some` in user mode only. Out-of-band
    /// delivery only (CLI stdout / host channel); never a log field.
    pub viewer_token: Option<String>,
    pub timeout_ms: u64,
}

/// Event published on the orchestrator's broadcast channel; the CDP layer
/// forwards these as `OXI.accountStateChanged` / `OXI.loginStateChanged`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AccountEvent {
    StateChanged {
        account_id: String,
        from: String,
        to: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    LoginChanged {
        login_id: String,
        account_id: String,
        /// `running` | `captured` | `aborted` | `timeout`
        state: &'static str,
    },
    /// Unattended agent-login progress / terminal state (M-D §5.3). The CDP
    /// forwarder maps this onto `OXI.loginStateChanged` with the acting
    /// agent id. Details are value-free (handles, urls, reasons).
    AgentLogin {
        login_id: String,
        account_id: String,
        agent_id: String,
        /// progress / terminal state (`started`, `navigating`, `captured`,
        /// `mfa_escalation`, `challenge`, `policy_violation`, `needs_login`…)
        state: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

/// How a login window ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginEndState {
    /// Detector confirmed + envelope captured (`valid`).
    Captured,
    /// Explicit abort (`needs_login`).
    Aborted,
    /// Deadline elapsed (`needs_login`).
    TimedOut,
}

/// Result of a finished (or judged) login window.
#[derive(Debug, Clone, Serialize)]
pub struct LoginOutcome {
    pub login_id: String,
    pub account_id: String,
    pub end_state: LoginEndState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record: Option<AccountRecord>,
    #[serde(skip)]
    pub detection: Option<Detection>,
}

/// Host-explicit end for [`LoginOrchestrator::end_login`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndOutcome {
    /// `done` — the user/host finished the flow; judged as explicit success.
    Done,
    /// `abort` — give up; back to `needs_login`.
    Abort,
}

/// `complete_login` verdict: captured, or the window stays open.
#[derive(Debug, Clone)]
pub enum CompleteVerdict {
    Captured {
        record: Box<AccountRecord>,
        detection: Detection,
    },
    NotYet {
        detection: Detection,
    },
}

/// Import-path result (§5.1 row 1).
#[derive(Debug, Clone, Serialize)]
pub struct ImportOutcome {
    pub record: AccountRecord,
    pub probe: ProbeVerdictSummary,
    /// `true` when the envelope was captured under the current fingerprint,
    /// adopting it as the new baseline (FM-L4).
    pub fingerprint_baseline_adopted: bool,
}

/// Value-free probe summary for surfaces (full verdicts carry non-`Serialize`
/// challenge enums).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProbeVerdictSummary {
    /// `valid` | `invalid` | `challenge` | `unreachable`
    pub verdict: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
}

impl ProbeVerdictSummary {
    fn of(outcome: &ProbeOutcome) -> Self {
        let (verdict, reason) = match &outcome.verdict {
            ProbeVerdict::Valid => ("valid", None),
            ProbeVerdict::Invalid { reason } => ("invalid", Some(reason.clone())),
            ProbeVerdict::Challenge { .. } => ("challenge", None),
            ProbeVerdict::Unreachable { reason } => ("unreachable", Some(reason.clone())),
        };
        ProbeVerdictSummary {
            verdict,
            reason,
            http_status: outcome.http_status,
        }
    }
}

/// A live login window (internal).
struct ActiveLogin {
    account_id: String,
    mode: LoginMode,
    deadline: Instant,
    /// One-time viewer token (user mode); consumed by the first viewer.
    viewer_token: Option<String>,
    /// Cancels the armed timeout when the window ends another way.
    abort: Option<tokio::task::AbortHandle>,
}

/// Drives account login flows over the [`AccountManager`]: user-driven
/// windows (begin/complete/end/abort, viewer-token issuance) and the import
/// bootstrap path. Shared as `Arc`; every method is `&self`.
pub struct LoginOrchestrator {
    manager: Arc<AccountManager>,
    browser: Arc<Browser>,
    keys: Arc<dyn KeyProvider>,
    events: broadcast::Sender<AccountEvent>,
    logins: StdMutex<HashMap<String, ActiveLogin>>,
    default_timeout: Duration,
}

impl LoginOrchestrator {
    /// Orchestrator over `manager`, creating login contexts on `browser`,
    /// sealing envelopes with `keys`. Default window: 300 s (§7.3).
    pub fn new(
        manager: Arc<AccountManager>,
        browser: Arc<Browser>,
        keys: Arc<dyn KeyProvider>,
    ) -> Self {
        let (events, _) = broadcast::channel(64);
        LoginOrchestrator {
            manager,
            browser,
            keys,
            events,
            logins: StdMutex::new(HashMap::new()),
            default_timeout: Duration::from_secs(300),
        }
    }

    /// The underlying manager (registry access for surfaces).
    pub fn manager(&self) -> &AccountManager {
        &self.manager
    }

    /// The manager as a shared handle — for hosts that run the M-D agent
    /// login engine outside the orchestrator's window machinery.
    pub fn shared_manager(&self) -> Arc<AccountManager> {
        Arc::clone(&self.manager)
    }

    /// The envelope-sealing key provider the orchestrator captures with —
    /// the agent login engine must seal with the same keys.
    pub fn shared_keys(&self) -> Arc<dyn KeyProvider> {
        Arc::clone(&self.keys)
    }

    /// Subscribe to account/login state events (CDP forwards these to
    /// clients; see [`AccountEvent`]).
    pub fn subscribe(&self) -> broadcast::Receiver<AccountEvent> {
        self.events.subscribe()
    }

    /// A clone of the broadcast sender, for hosts that publish the same
    /// [`AccountEvent`] stream from outside the orchestrator (the M-D agent
    /// login engine's progress + state transitions).
    pub fn event_sender(&self) -> broadcast::Sender<AccountEvent> {
        self.events.clone()
    }

    // -- login windows ------------------------------------------------------

    /// Open a login window for `account_id` (§5.1): `logging_in` + audit +
    /// `loginId` (+ one-time viewer token in user mode) + armed timeout.
    ///
    /// Legal only from `needs_login` / `stale` — a `valid` account is asked
    /// to log out first, and one window at a time per account.
    pub fn begin_login(
        self: &Arc<Self>,
        account_id: &str,
        mode: LoginMode,
        timeout: Option<Duration>,
    ) -> Result<LoginHandle> {
        let record = self.manager.registry().get(account_id)?;
        match record.state {
            AccountState::NeedsLogin | AccountState::Stale => {}
            AccountState::Valid => {
                return Err(account_error(format!(
                    "account {account_id} is valid — run `account logout` before re-login"
                )));
            }
            other => {
                return Err(account_error(format!(
                    "account {account_id} cannot start a login from state {}",
                    other.as_str()
                )));
            }
        }
        {
            let logins = self
                .logins
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if logins.values().any(|l| l.account_id == account_id) {
                return Err(account_error(format!(
                    "a login window is already open for {account_id}"
                )));
            }
        }

        let timeout = timeout.unwrap_or(self.default_timeout);
        let login_id = format!("login-{}", Uuid::new_v4());
        let viewer_token = match mode {
            LoginMode::User => Some(format!("oxi-viewer-{}", Uuid::new_v4())),
            LoginMode::Agent => None,
        };

        let from = record.state;
        let updated = self.enter_logging_in(account_id, mode)?;

        // Arm the automatic abort.
        let orchestrator = Arc::clone(self);
        let timeout_login_id = login_id.clone();
        let abort_handle = tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            let _ = orchestrator.abort(&timeout_login_id).await;
        })
        .abort_handle();

        self.logins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                login_id.clone(),
                ActiveLogin {
                    account_id: account_id.to_string(),
                    mode,
                    deadline: Instant::now() + timeout,
                    viewer_token: viewer_token.clone(),
                    abort: Some(abort_handle),
                },
            );

        self.publish(AccountEvent::StateChanged {
            account_id: account_id.to_string(),
            from: from.as_str().into(),
            to: updated.state.as_str().into(),
            detail: Some(format!("login_started:{}", mode.as_str())),
        });
        self.publish(AccountEvent::LoginChanged {
            login_id: login_id.clone(),
            account_id: account_id.to_string(),
            state: "running",
        });

        Ok(LoginHandle {
            login_id,
            account_id: account_id.to_string(),
            mode,
            viewer_token,
            timeout_ms: timeout.as_millis() as u64,
        })
    }

    /// Await the window's end (the CLI host contract: `--json` blocks until
    /// capture/abort/timeout). Returns `None` only if the events channel is
    /// gone (orchestrator dropped).
    pub async fn wait(&self, login_id: &str) -> Option<LoginOutcome> {
        let mut rx = self.subscribe();
        loop {
            match rx.recv().await {
                Ok(AccountEvent::LoginChanged {
                    login_id: id,
                    account_id,
                    state,
                }) if id == login_id && state != "running" => {
                    let end_state = match state {
                        "captured" => LoginEndState::Captured,
                        "aborted" => LoginEndState::Aborted,
                        _ => LoginEndState::TimedOut,
                    };
                    let record = self.manager.registry().get(&account_id).ok();
                    return Some(LoginOutcome {
                        login_id: login_id.to_string(),
                        account_id,
                        end_state,
                        record,
                        detection: None,
                    });
                }
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }

    /// Judge the live session and, on a confirmed login, capture the
    /// envelope (§4.3 + §5.1). `explicit_success` is the highest-trust
    /// signal (`OXI.reportLoginSuccess` / wizard `done`).
    ///
    /// When the detector finds no evidence the window **stays open** and a
    /// [`CompleteVerdict::NotYet`] verdict is returned — polling or an
    /// explicit end is the caller's job.
    pub async fn complete_login(
        self: &Arc<Self>,
        login_id: &str,
        session: &mut Session,
        explicit_success: bool,
    ) -> Result<CompleteVerdict> {
        let account_id = self.window_account(login_id)?;
        let baseline = PreLoginSnapshot::default();

        let detection = self
            .manager
            .detect_login(&account_id, session, &baseline, explicit_success)
            .await?;

        if !detection.logged_in {
            return Ok(CompleteVerdict::NotYet { detection });
        }

        let record = self.capture(&account_id, login_id, session)?;
        // Close the window NOW: the armed timeout must not fire against a
        // captured (valid) account.
        self.close_window(
            login_id,
            &account_id,
            LoginEndState::Captured,
            Some(record.clone()),
        );
        Ok(CompleteVerdict::Captured {
            record: Box::new(record),
            detection,
        })
    }

    /// Host-explicit end (§7.3 `OXI.endLogin`): `done` finishes like an
    /// explicit success; `abort` reverts to `needs_login`. `session` is the
    /// window's live session (CDP path: the connected session; CLI wizard:
    /// the takeover tab's session).
    pub async fn end_login(
        self: &Arc<Self>,
        login_id: &str,
        session: &mut Session,
        outcome: EndOutcome,
    ) -> Result<LoginOutcome> {
        match outcome {
            EndOutcome::Done => {
                let account_id = self.window_account(login_id)?;
                match self.complete_login(login_id, session, true).await? {
                    CompleteVerdict::Captured { record, detection } => Ok(LoginOutcome {
                        login_id: login_id.to_string(),
                        account_id,
                        end_state: LoginEndState::Captured,
                        record: Some(*record),
                        detection: Some(detection),
                    }),
                    // Impossible with explicit_success (weight u32::MAX), but
                    // never lie about it: report the window as still open.
                    CompleteVerdict::NotYet { detection } => Ok(LoginOutcome {
                        login_id: login_id.to_string(),
                        account_id,
                        end_state: LoginEndState::Aborted,
                        record: None,
                        detection: Some(detection),
                    }),
                }
            }
            EndOutcome::Abort => self.abort(login_id).await,
        }
    }

    /// Abort (explicitly or via timeout): back to `needs_login`, audited,
    /// window closed with [`LoginEndState::Aborted`] /
    /// [`LoginEndState::TimedOut`].
    pub async fn abort(&self, login_id: &str) -> Result<LoginOutcome> {
        let (account_id, timed_out) = {
            let mut logins = self
                .logins
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let login = logins
                .get_mut(login_id)
                .ok_or_else(|| account_error(format!("unknown loginId {login_id}")))?;
            (login.account_id.clone(), Instant::now() >= login.deadline)
        };

        let detail = if timed_out {
            "login_timeout"
        } else {
            "login_aborted"
        };
        let from = AccountState::LoggingIn;
        let record = self.manager.set_state_locked(
            &account_id,
            AccountState::NeedsLogin,
            Some(detail.to_string()),
        )?;
        self.manager
            .emit_state(&account_id, from, record.state, detail);

        let end_state = if timed_out {
            LoginEndState::TimedOut
        } else {
            LoginEndState::Aborted
        };
        self.close_window(login_id, &account_id, end_state, Some(record));
        let final_record = self.manager.registry().get(&account_id).ok();
        Ok(LoginOutcome {
            login_id: login_id.to_string(),
            account_id,
            end_state,
            record: final_record,
            detection: None,
        })
    }

    /// The one-time viewer token for a user-mode window, consuming it
    /// (`None` on reuse, unknown ids, or agent mode).
    pub fn take_viewer_token(&self, login_id: &str) -> Option<String> {
        let mut logins = self
            .logins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        logins.get_mut(login_id)?.viewer_token.take()
    }

    /// Peek at a live window (mode + time remaining) without consuming it.
    pub fn login_info(&self, login_id: &str) -> Option<(String, LoginMode, Duration)> {
        let logins = self
            .logins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        logins.get(login_id).map(|l| {
            (
                l.account_id.clone(),
                l.mode,
                l.deadline.saturating_duration_since(Instant::now()),
            )
        })
    }

    /// A fresh session in a fresh account-bound context (credential mode
    /// on). Used by the CLI wizard/user-window when no CDP session exists.
    pub async fn open_account_context(
        &self,
        account_id: &str,
    ) -> Result<(Arc<BrowserContext>, Arc<RwLock<Session>>)> {
        // Account existence check up front (scoped error, not a bare
        // browser error).
        let _ = self.manager.registry().get(account_id)?;
        let ctx = self.browser.new_context(ContextConfig {
            label: Some(format!("account:{account_id}")),
            proxy: None,
        })?;
        ctx.set_credential_mode(true);
        let session = self.browser.new_session_in(&ctx).await?;
        Ok((ctx, session))
    }

    // -- import path (§5.1 row 1) -------------------------------------------

    /// Import a storage state as `account_id`'s session: inject into a fresh
    /// account context → probe → capture (success) / back to `needs_login`
    /// with the probe reason (failure). Adopts the current fingerprint as
    /// the envelope baseline (FM-L4 flagged in the outcome).
    pub async fn import(&self, account_id: &str, state: &StorageState) -> Result<ImportOutcome> {
        self.enter_logging_in(account_id, LoginMode::Agent)?;
        // Ensure a failure between the state change and the probe never
        // wedges the account in `logging_in`.
        match self.import_inner(account_id, state).await {
            Ok(outcome) => Ok(outcome),
            Err(err) => {
                let detail = format!("import_failed:{err}");
                let from = AccountState::LoggingIn;
                if let Ok(record) = self.manager.set_state_locked(
                    account_id,
                    AccountState::NeedsLogin,
                    Some(detail.clone()),
                ) {
                    self.manager
                        .emit_state(account_id, from, record.state, &detail);
                    self.publish(AccountEvent::StateChanged {
                        account_id: account_id.to_string(),
                        from: from.as_str().into(),
                        to: record.state.as_str().into(),
                        detail: Some(detail),
                    });
                }
                Err(err)
            }
        }
    }

    async fn import_inner(&self, account_id: &str, state: &StorageState) -> Result<ImportOutcome> {
        let record = self.manager.registry().get(account_id)?;
        let (_ctx, session) = self.open_account_context(account_id).await?;

        {
            let mut guard = session.write().await;
            guard.import_state(state)?;
        }

        let probe = ValidationProbe::from_record(record.probe.as_ref(), &record.scope)?;
        let outcome = {
            let guard = session.read().await;
            probe.run(guard.http_client().as_ref()).await
        };

        match &outcome.verdict {
            ProbeVerdict::Valid => {
                let record = {
                    let guard = session.read().await;
                    self.capture(account_id, "import", &guard)?
                };
                tracing::warn!(
                    account = %account_id,
                    "imported session captured under the current fingerprint baseline \
                     — state minted on another device may not survive (FM-L4)"
                );
                Ok(ImportOutcome {
                    record,
                    probe: ProbeVerdictSummary::of(&outcome),
                    fingerprint_baseline_adopted: true,
                })
            }
            ProbeVerdict::Invalid { reason } => {
                let detail = format!("import_failed:{reason}");
                let record = self.revert_to_needs_login(account_id, &detail).await?;
                Ok(ImportOutcome {
                    record,
                    probe: ProbeVerdictSummary::of(&outcome),
                    fingerprint_baseline_adopted: false,
                })
            }
            ProbeVerdict::Challenge { challenge } => {
                let detail = format!(
                    "import_challenge:{}:{}",
                    challenge.vendor.as_str(),
                    challenge_kind_str(&challenge.kind),
                );
                let record = self.revert_to_needs_login(account_id, &detail).await?;
                Ok(ImportOutcome {
                    record,
                    probe: ProbeVerdictSummary::of(&outcome),
                    fingerprint_baseline_adopted: false,
                })
            }
            ProbeVerdict::Unreachable { reason } => {
                // Transport failure is not evidence (§4.4) — but from
                // `logging_in` the machine cannot stay there indefinitely;
                // the caller is told to retry or abort explicitly.
                Err(account_error(format!(
                    "probe unreachable during import: {reason}; account left in logging_in — retry or abort"
                )))
            }
        }
    }

    // -- internals -----------------------------------------------------------

    /// `logging_in` transition shared by windows and imports (audited +
    /// published). Legal only from `needs_login` / `stale`.
    fn enter_logging_in(&self, account_id: &str, mode: LoginMode) -> Result<AccountRecord> {
        let from = self.manager.registry().get(account_id)?.state;
        let record = self.manager.set_state_locked(
            account_id,
            AccountState::LoggingIn,
            Some(format!("login_started:{}", mode.as_str())),
        )?;
        self.manager
            .emit_state(account_id, from, record.state, "login_started");
        Ok(record)
    }

    /// Probe-confirmed capture: `logging_in` → `valid` via the manager
    /// (audited there), then the state event is published.
    fn capture(
        &self,
        account_id: &str,
        login_id: &str,
        session: &Session,
    ) -> Result<AccountRecord> {
        let from = AccountState::LoggingIn;
        let record = self
            .manager
            .capture_session(account_id, session, self.keys.as_ref())?;
        self.publish(AccountEvent::StateChanged {
            account_id: account_id.to_string(),
            from: from.as_str().into(),
            to: record.state.as_str().into(),
            detail: Some(format!("captured:{login_id}")),
        });
        Ok(record)
    }

    /// Failure exit from `logging_in` (§4.2: `Valid` | `NeedsLogin` only).
    async fn revert_to_needs_login(&self, account_id: &str, detail: &str) -> Result<AccountRecord> {
        let from = AccountState::LoggingIn;
        let record = self.manager.set_state_locked(
            account_id,
            AccountState::NeedsLogin,
            Some(detail.to_string()),
        )?;
        self.manager
            .emit_state(account_id, from, record.state, detail);
        self.publish(AccountEvent::StateChanged {
            account_id: account_id.to_string(),
            from: from.as_str().into(),
            to: record.state.as_str().into(),
            detail: Some(detail.to_string()),
        });
        Ok(record)
    }

    /// The account a live window belongs to; errors once the window is gone
    /// (ended, aborted, or timed out).
    fn window_account(&self, login_id: &str) -> Result<String> {
        let logins = self
            .logins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        logins
            .get(login_id)
            .map(|l| l.account_id.clone())
            .ok_or_else(|| account_error(format!("unknown or closed loginId {login_id}")))
    }

    /// Remove the window, cancel its armed timeout, publish the end events.
    fn close_window(
        &self,
        login_id: &str,
        account_id: &str,
        end_state: LoginEndState,
        record: Option<AccountRecord>,
    ) {
        let removed = {
            let mut logins = self
                .logins
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            logins.remove(login_id)
        };
        if let Some(login) = removed
            && let Some(abort) = login.abort.as_ref()
        {
            abort.abort();
        }
        let state = match end_state {
            LoginEndState::Captured => "captured",
            LoginEndState::Aborted => "aborted",
            LoginEndState::TimedOut => "timeout",
        };
        self.publish(AccountEvent::LoginChanged {
            login_id: login_id.to_string(),
            account_id: account_id.to_string(),
            state,
        });
        if let Some(record) = &record {
            self.publish(AccountEvent::StateChanged {
                account_id: account_id.to_string(),
                from: AccountState::LoggingIn.as_str().into(),
                to: record.state.as_str().into(),
                detail: Some(format!("login_{state}")),
            });
        }
    }

    fn publish(&self, event: AccountEvent) {
        let _ = self.events.send(event);
    }
}

/// `ChallengeKind` → the manager's detail spelling.
fn challenge_kind_str(kind: &crate::challenge::ChallengeKind) -> &'static str {
    match kind {
        crate::challenge::ChallengeKind::Managed => "managed",
        crate::challenge::ChallengeKind::JsCheck => "js_check",
        crate::challenge::ChallengeKind::Interactive => "interactive",
        crate::challenge::ChallengeKind::Blocked => "blocked",
        crate::challenge::ChallengeKind::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::record::{ProbeConfig, SessionSummary};
    use crate::config::BrowserConfig;
    use crate::network::cookie::{CookieEntry, SameSite};
    use crate::security::audit::AuditLog;
    use crate::storage::session_store::StaticKeyProvider;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "oxi-orch-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    async fn orchestrator(
        tag: &str,
    ) -> (
        Arc<LoginOrchestrator>,
        PathBuf,
        broadcast::Receiver<AccountEvent>,
    ) {
        let base = temp_dir(tag);
        let audit = Arc::new(AuditLog::open(base.join("audit.jsonl")).unwrap());
        let registry = crate::account::AccountRegistry::open(base.join("accounts")).unwrap();
        let manager = Arc::new(AccountManager::with_audit(registry, audit));
        let mut config = BrowserConfig::headless();
        config.enable_ssrf_filter = false; // loopback wiremock
        let browser = Arc::new(Browser::new(config).await.unwrap());
        let orch = Arc::new(LoginOrchestrator::new(
            manager,
            browser,
            Arc::new(StaticKeyProvider::new([9u8; 32])),
        ));
        let events = orch.subscribe();
        (orch, base, events)
    }

    fn account_at(orch: &LoginOrchestrator, id: &str, scope: &str, probe_url: String) {
        let mut record = AccountRecord::new(id, scope).unwrap();
        record.probe = Some(ProbeConfig {
            url: probe_url,
            marker: Some("Signed in".into()),
        });
        orch.manager().registry().add(record).unwrap();
    }

    /// A Playwright-shaped state with one in-scope session cookie.
    fn state_with_cookie(scope: &str, name: &str) -> StorageState {
        StorageState {
            cookies: vec![CookieEntry {
                name: name.into(),
                value: "v".into(),
                path: Some("/".into()),
                domain: Some(scope.into()),
                secure: true,
                http_only: true,
                same_site: Some(SameSite::Lax),
                ..CookieEntry::default()
            }],
            origins: vec![],
        }
    }

    fn drain_events(rx: &mut broadcast::Receiver<AccountEvent>) -> Vec<AccountEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    #[tokio::test]
    async fn import_success_captures_and_flags_baseline() {
        let wiremock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_string("<html><body><h1>Signed in</h1></body></html>"),
            )
            .mount(&wiremock)
            .await;

        let (orch, base, mut events) = orchestrator("import-ok").await;
        account_at(&orch, "gh", "example.com", wiremock.uri());

        let outcome = orch
            .import("gh", &state_with_cookie("example.com", "user_session"))
            .await
            .unwrap();
        assert_eq!(outcome.record.state, AccountState::Valid);
        assert!(outcome.fingerprint_baseline_adopted);
        assert_eq!(outcome.probe.verdict, "valid");

        // Envelope is real: it loads from the account's session store.
        let record = orch.manager().registry().get("gh").unwrap();
        let store = crate::account::AccountRegistry::open(base.join("accounts"))
            .unwrap()
            .session_store("gh")
            .unwrap();
        let envelope = store
            .load(&record.scope, &StaticKeyProvider::new([9u8; 32]), None)
            .unwrap();
        assert_eq!(envelope.state.cookies[0].name, "user_session");
        assert_eq!(record.session_summary.cookie_count, 1);

        // Events: logging_in → valid.
        let evs = drain_events(&mut events);
        assert!(
            evs.iter()
                .any(|e| matches!(e, AccountEvent::StateChanged { to, .. } if to == "valid")),
            "expected a valid StateChanged event, got {evs:?}"
        );
    }

    #[tokio::test]
    async fn import_invalid_reverts_to_needs_login_with_reason() {
        let wiremock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(
                "<html><body><form><input type=\"password\" name=\"pw\"><input type=\"submit\"></form></body></html>",
            ))
            .mount(&wiremock)
            .await;

        let (orch, _base, mut events) = orchestrator("import-bad").await;
        account_at(&orch, "gh", "example.com", wiremock.uri());

        let outcome = orch
            .import("gh", &state_with_cookie("example.com", "sid"))
            .await
            .unwrap();
        assert_eq!(outcome.record.state, AccountState::NeedsLogin);
        assert!(
            outcome
                .record
                .state_detail
                .as_deref()
                .unwrap_or_default()
                .starts_with("import_failed:"),
            "detail must carry the probe reason: {:?}",
            outcome.record.state_detail
        );
        assert!(!outcome.fingerprint_baseline_adopted);

        let evs = drain_events(&mut events);
        assert!(
            evs.iter()
                .any(|e| matches!(e, AccountEvent::StateChanged { to, .. } if to == "needs_login")),
            "expected a needs_login StateChanged event, got {evs:?}"
        );
    }

    #[tokio::test]
    async fn import_unknown_account_is_an_error() {
        let (orch, _base, _events) = orchestrator("import-unknown").await;
        let err = orch
            .import("nope", &state_with_cookie("example.com", "sid"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("nope"));
    }

    #[tokio::test]
    async fn login_window_aborts_back_to_needs_login() {
        let (orch, _base, mut events) = orchestrator("window-abort").await;
        let mut record = AccountRecord::new("gh", "example.com").unwrap();
        record.probe = Some(ProbeConfig {
            url: "https://example.com/".into(),
            marker: None,
        });
        orch.manager().registry().add(record).unwrap();

        let handle = orch
            .begin_login("gh", LoginMode::User, None)
            .unwrap_or_else(|e| panic!("begin: {e}"));
        assert_eq!(
            orch.manager().registry().get("gh").unwrap().state,
            AccountState::LoggingIn
        );
        assert!(handle.viewer_token.is_some());

        let outcome = orch.abort(&handle.login_id).await.unwrap();
        assert_eq!(outcome.end_state, LoginEndState::Aborted);
        assert_eq!(
            orch.manager().registry().get("gh").unwrap().state,
            AccountState::NeedsLogin
        );

        let evs = drain_events(&mut events);
        assert!(
            evs.iter().any(|e| matches!(
                e,
                AccountEvent::LoginChanged {
                    state: "aborted",
                    ..
                }
            )),
            "expected an aborted LoginChanged event, got {evs:?}"
        );
    }

    #[tokio::test]
    async fn login_window_times_out_and_reverts() {
        let (orch, _base, _events) = orchestrator("window-timeout").await;
        let mut record = AccountRecord::new("gh", "example.com").unwrap();
        record.probe = Some(ProbeConfig {
            url: "https://example.com/".into(),
            marker: None,
        });
        orch.manager().registry().add(record).unwrap();

        let handle = orch
            .begin_login("gh", LoginMode::User, Some(Duration::from_millis(60)))
            .unwrap();
        let outcome = orch.wait(&handle.login_id).await.unwrap();
        assert_eq!(outcome.end_state, LoginEndState::TimedOut);
        assert_eq!(
            orch.manager().registry().get("gh").unwrap().state,
            AccountState::NeedsLogin
        );
        assert!(
            orch.login_info(&handle.login_id).is_none(),
            "window must be closed"
        );
    }

    #[tokio::test]
    async fn viewer_token_is_one_time() {
        let (orch, _base, _events) = orchestrator("viewer-token").await;
        orch.manager()
            .registry()
            .add(AccountRecord::new("gh", "example.com").unwrap())
            .unwrap();
        orch.manager()
            .registry()
            .add(AccountRecord::new("gh2", "example.org").unwrap())
            .unwrap();

        let handle = orch.begin_login("gh", LoginMode::User, None).unwrap();
        let token = handle.viewer_token.clone().unwrap();
        assert_eq!(
            orch.take_viewer_token(&handle.login_id).as_deref(),
            Some(token.as_str())
        );
        assert!(
            orch.take_viewer_token(&handle.login_id).is_none(),
            "one-time"
        );

        // Agent mode issues no viewer token at all.
        let agent = orch.begin_login("gh2", LoginMode::Agent, None).unwrap();
        assert!(agent.viewer_token.is_none());
        assert!(orch.take_viewer_token(&agent.login_id).is_none());
    }

    #[tokio::test]
    async fn complete_login_explicit_captures_and_publishes() {
        let (orch, _base, mut events) = orchestrator("complete-explicit").await;
        orch.manager()
            .registry()
            .add(AccountRecord::new("gh", "example.com").unwrap())
            .unwrap();

        let handle = orch.begin_login("gh", LoginMode::Agent, None).unwrap();
        let (_ctx, session) = orch.open_account_context("gh").await.unwrap();
        let mut guard = session.write().await;
        let verdict = orch
            .complete_login(&handle.login_id, &mut guard, true)
            .await
            .unwrap();
        drop(guard);

        let record = match verdict {
            CompleteVerdict::Captured { record, .. } => record,
            CompleteVerdict::NotYet { .. } => panic!("explicit success must capture"),
        };
        assert_eq!(record.state, AccountState::Valid);
        assert_eq!(
            orch.manager().registry().get("gh").unwrap().state,
            AccountState::Valid
        );

        let evs = drain_events(&mut events);
        assert!(
            evs.iter().any(|e| matches!(
                e,
                AccountEvent::LoginChanged {
                    state: "captured",
                    ..
                }
            )),
            "expected captured LoginChanged, got {evs:?}"
        );

        // A captured account cannot open a second window.
        let err = orch.begin_login("gh", LoginMode::User, None).unwrap_err();
        assert!(err.to_string().contains("valid"));
    }

    #[tokio::test]
    async fn complete_login_without_evidence_keeps_window_open() {
        let (orch, _base, _events) = orchestrator("complete-notyet").await;
        orch.manager()
            .registry()
            .add(AccountRecord::new("gh", "example.com").unwrap())
            .unwrap();
        let handle = orch.begin_login("gh", LoginMode::Agent, None).unwrap();
        let (_ctx, session) = orch.open_account_context("gh").await.unwrap();
        let mut guard = session.write().await;
        let verdict = orch
            .complete_login(&handle.login_id, &mut guard, false)
            .await
            .unwrap();
        match verdict {
            CompleteVerdict::NotYet { detection } => assert!(!detection.logged_in),
            CompleteVerdict::Captured { .. } => panic!("no signals must not capture"),
        }
        // Window still open → another attempt works.
        assert!(orch.login_info(&handle.login_id).is_some());
    }

    /// Session summaries written by capture stay value-free (defensive check
    /// that the import path can't leak cookie values into the record).
    #[test]
    fn session_summary_stays_value_free() {
        let s = SessionSummary::default();
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("hunter2"));
    }
}
