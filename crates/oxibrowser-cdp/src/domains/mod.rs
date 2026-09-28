//! CDP domains — implementations of CDP domain methods.
//!
//!
//! The `dispatch` function is async and receives a `DispatchContext` that
//! includes the browser `Session` for page interaction AND the `EventSender`
//! so domain handlers can emit CDP events.
pub mod browser;
pub mod dom;
pub mod emulation;
pub mod fetch;
pub mod input;
pub mod log;
pub mod network;
pub mod oxi;
pub mod page;
pub mod runtime;
pub mod target;
pub mod tracing;

use crate::credential::CredentialBroker;
use crate::event::EventSender;
use crate::protocol::CdpError;
use oxibrowser_core::context::BrowserContext;
use oxibrowser_core::network::SharedRegistry;
use oxibrowser_core::session::Session;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::RwLock;

/// A registered child target created via `Target.createTarget`.
#[derive(Clone)]
pub struct TargetEntry {
    /// CDP targetId assigned at creation.
    pub target_id: String,
    /// The child browser session backing this target.
    pub session: Arc<RwLock<Session>>,
    /// Handle to the child's CoreEvent drainer task, aborted on close/detach.
    pub drain_abort: Option<tokio::task::AbortHandle>,
    /// Real browser context id (`ctx-N`) the target was created in.
    pub browser_context_id: String,
}

/// Registry of attached child targets (multi-tab), keyed by CDP sessionId.
/// Populated by `Target.createTarget`; the dispatcher resolves the session for
/// an incoming command from its `sessionId`.
pub struct TargetRegistry {
    /// Child targets by sessionId.
    pub by_session: RwLock<HashMap<String, TargetEntry>>,
    /// SessionIds whose target has been closed. Commands for a closed target
    /// must fail (`-32001`) instead of falling back to the default session.
    closed: StdMutex<HashSet<String>>,
}

impl Default for TargetRegistry {
    fn default() -> Self {
        Self {
            by_session: RwLock::new(HashMap::new()),
            closed: StdMutex::new(HashSet::new()),
        }
    }
}

impl TargetRegistry {
    /// Whether the target for this sessionId has been closed.
    pub fn is_closed(&self, session_id: &str) -> bool {
        self.closed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(session_id)
    }

    /// Record a sessionId as closed (tombstone for routing).
    fn mark_closed(&self, session_id: &str) {
        self.closed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id.to_string());
    }
}

impl TargetRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Clone the entry for a sessionId, if registered.
    pub async fn get(&self, session_id: &str) -> Option<TargetEntry> {
        self.by_session.read().await.get(session_id).cloned()
    }

    /// Clone the child session for a sessionId, if registered.
    pub async fn session(&self, session_id: &str) -> Option<Arc<RwLock<Session>>> {
        self.by_session
            .read()
            .await
            .get(session_id)
            .map(|e| e.session.clone())
    }

    /// Register a child target under a sessionId.
    pub async fn insert(&self, session_id: String, entry: TargetEntry) {
        self.by_session.write().await.insert(session_id, entry);
    }

    /// Remove and return the entry whose targetId matches, as `(sessionId, entry)`.
    /// The sessionId is tombstoned so later commands for it fail loudly.
    pub async fn close_by_target(&self, target_id: &str) -> Option<(String, TargetEntry)> {
        let mut map = self.by_session.write().await;
        let sid = map
            .iter()
            .find(|(_, e)| e.target_id == target_id)
            .map(|(sid, _)| sid.clone())?;
        self.mark_closed(&sid);
        map.remove(&sid).map(|e| (sid, e))
    }

    /// Remove and return the entry for a sessionId (used by `detachFromTarget`).
    pub async fn detach(&self, session_id: &str) -> Option<TargetEntry> {
        self.remove_by_session(session_id).await
    }

    /// Remove and return the entry for a sessionId.
    pub async fn remove_by_session(&self, session_id: &str) -> Option<TargetEntry> {
        self.by_session.write().await.remove(session_id)
    }

    /// Snapshot of all registered entries as `(sessionId, entry)`.
    pub async fn entries(&self) -> Vec<(String, TargetEntry)> {
        self.by_session
            .read()
            .await
            .iter()
            .map(|(sid, e)| (sid.clone(), e.clone()))
            .collect()
    }
}

/// Shared handle to the child-target registry.
pub type ChildTargets = Arc<TargetRegistry>;

/// Context passed to all domain handlers.
///
/// Combines browser session access with the event sender, so handlers can
/// both read/write page data AND emit CDP events.
pub struct DispatchContext {
    /// Browser session (read/write for navigation, DOM access, JS eval).
    pub session: Arc<RwLock<Session>>,
    /// Event sender for emitting CDP events to the client.
    pub events: EventSender,
    /// Registry of paused requests for Fetch domain interception.
    pub fetch_registry: SharedRegistry,
    /// Shared dialog-resolution gate (for `Page.handleJavaScriptDialog`).
    /// Accessible without the session lock so dialogs resolve while a blocking
    /// `evaluate` holds the session write lock.
    pub dialog_gate: oxibrowser_core::js::DialogGate,
    /// Shared browser instance (for `Target.createTarget` to mint sessions).
    pub browser: Arc<oxibrowser_core::Browser>,
    /// Attached child targets (multi-tab), keyed by sessionId.
    pub child_targets: ChildTargets,
    /// The [`BrowserContext`] the routed session belongs to (main session →
    /// the browser's default context; child target → its creation context).
    /// Backs the credential-mode gate.
    pub browser_context: Arc<BrowserContext>,
    /// Credential broker (provider + policy engine + pending confirmations).
    /// `None` — the server default — makes every `OXI.credential*` call answer
    /// `credentialsUnavailable`; existing server behavior is unchanged.
    pub credentials: Option<Arc<CredentialBroker>>,
    /// Connection role (§5.2) — viewer allowlist, agent takeover denial.
    pub role: crate::session::RoleKind,
    /// Account/login surface. `None` — the server default — makes every
    /// `OXI.account*` call answer `accountsUnavailable`.
    pub logins: Option<Arc<crate::account::LoginSurface>>,
}

/// Credential-mode gate (design `2026-09-27` §6.3): storage-exporting CDP
/// surface is denied while the session's context is in credential mode —
/// credentials flow only through the broker's `OXI.fillCredential`.
pub fn deny_in_credential_mode(ctx: &DispatchContext) -> Result<(), CdpError> {
    if ctx.browser_context.credential_mode() {
        return Err(CdpError {
            code: -32000,
            message: "deniedInCredentialMode".to_string(),
        });
    }
    Ok(())
}

/// Role gate (§5.2). Evaluated before every dispatch:
///
/// - **viewer** connections get the §5.2 allowlist only — screencast, input,
///   navigate, and the login-surface methods (`resolveConfirmation`,
///   `reportLoginSuccess`, `endLogin`). Cookies, storage,
///   `Runtime.evaluate`, `exportStorageState`, and everything else answer
///   `deniedForViewerRole`.
/// - **agent** connections can never resolve confirmation cards — approval
///   is the human's out-of-band act, so `OXI.resolveConfirmation` answers
///   `confirmationRequiresViewerRole` unconditionally (OXI-CONFIRM-SELF-APPROVE).
///   Inside a live takeover window for the routed session's context they are
///   additionally denied input and capture — including the DOM-event and
///   evaluate-based input/capture paths (`OXI.fillRef`, `OXI.clickRef`,
///   `OXI.getBoxModelScreenshot`, `Runtime.evaluate`)
///   (`input_denied_during_takeover` / `capture_denied_during_takeover`).
fn gate_role(method: &str, ctx: &DispatchContext) -> Result<(), CdpError> {
    match ctx.role {
        crate::session::RoleKind::Viewer => {
            let allowed = method.starts_with("Input.")
                || matches!(
                    method,
                    "Page.navigate"
                        | "Page.startScreencast"
                        | "Page.stopScreencast"
                        | "Page.screencastFrameAck"
                        | "OXI.resolveConfirmation"
                        | "OXI.reportLoginSuccess"
                        | "OXI.endLogin"
                );
            if allowed {
                Ok(())
            } else {
                Err(CdpError {
                    code: -32000,
                    message: "deniedForViewerRole".to_string(),
                })
            }
        }
        crate::session::RoleKind::Agent => {
            // Confirmation approval is the human's out-of-band act on a
            // viewer connection — never the requesting agent's own
            // (OXI-CONFIRM-SELF-APPROVE).
            if method == "OXI.resolveConfirmation" {
                return Err(CdpError {
                    code: -32000,
                    message: "confirmationRequiresViewerRole".to_string(),
                });
            }
            let Some(surface) = ctx.logins.as_ref() else {
                return Ok(());
            };
            let Some(_takeover) = surface.active_takeover(ctx.browser_context.id().as_str()) else {
                return Ok(());
            };
            if method.starts_with("Input.")
                || matches!(
                    method,
                    // DOM-event/evaluate-based input and capture paths —
                    // blocking `Input.*` alone leaves fillRef/clickRef and
                    // `el.click()`-via-evaluate open (TAKEOVER-INPUT-BYPASS).
                    "OXI.fillRef"
                        | "OXI.clickRef"
                        | "OXI.getBoxModelScreenshot"
                        | "Runtime.evaluate"
                )
            {
                Err(CdpError {
                    code: -32000,
                    message: "input_denied_during_takeover".to_string(),
                })
            } else if matches!(
                method,
                "Page.captureScreenshot" | "Page.startScreencast" | "Page.printToPDF"
            ) {
                Err(CdpError {
                    code: -32000,
                    message: "capture_denied_during_takeover".to_string(),
                })
            } else {
                Ok(())
            }
        }
    }
}

/// Result of handling a CDP domain method.
pub type DomainResult = std::result::Result<Option<Value>, CdpError>;

/// Dispatch a CDP method to the appropriate domain handler.
///
/// Returns `Ok(Some(result))` on success, `Ok(None)` for empty results,
/// or `Err(CdpError)` for unknown methods.
pub async fn dispatch(method: &str, params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    gate_role(method, ctx)?;
    let parts: Vec<&str> = method.splitn(2, '.').collect();
    if parts.len() != 2 {
        return Err(CdpError {
            code: -32601,
            message: format!("invalid method: {method}"),
        });
    }

    let (domain, method_name) = (parts[0], parts[1]);
    match domain {
        "Browser" => browser::handle(method_name, params),
        "DOM" => dom::handle(method_name, params, ctx).await,
        "Emulation" => emulation::handle(method_name, params, ctx).await,
        "Fetch" => fetch::handle(method_name, params, ctx).await,
        "Input" => input::handle(method_name, params, ctx).await,
        "Network" => network::handle(method_name, params, ctx).await,
        "OXI" => oxi::handle(method_name, params, ctx).await,
        "Log" => log::handle(method_name, params, ctx).await,
        "Page" => page::handle(method_name, params, ctx).await,
        "Runtime" => runtime::handle(method_name, params, ctx).await,
        "Target" => target::handle(method_name, params, ctx).await,
        "Tracing" => tracing::handle(method_name, params, ctx),
        _ => Err(CdpError {
            code: -32601,
            message: format!("unknown domain: {domain}"),
        }),
    }
}
