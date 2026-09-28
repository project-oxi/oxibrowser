//! CDP session — a single WebSocket connection for CDP communication.
//!
//! Manages a WebSocket connection, dispatches incoming CDP commands to
//! domain handlers, and sends responses and events back to the client.
//!
//! Each CDP session creates a corresponding Browser `Session` for page
//! interaction (navigation, DOM access, JS evaluation).

use crate::domains;
use crate::domains::DispatchContext;
use crate::event::{EventReceiver, EventSender, event_channel};
use crate::protocol::{CdpError, CdpEvent, CdpRequest, CdpResponse};
use crate::server::MAX_CDP_MESSAGE_SIZE;
use futures::{SinkExt, StreamExt};
use oxibrowser_core::Browser;
use oxibrowser_core::context::BrowserContext;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite;
use tracing::{debug, error, info, warn};

/// Connection role (§5.2): agents automate; viewers mirror.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleKind {
    /// Full automation — input/capture denied only inside a takeover window.
    Agent,
    /// The human's mirror — screencast/input/navigate only (§5.2).
    Viewer,
}

/// What a WebSocket connection is allowed to be, resolved at upgrade time.
#[derive(Clone)]
pub struct ConnectionRole {
    pub kind: RoleKind,
    /// The context this connection's session is created in (`None` → the
    /// browser's default; viewers always carry the takeover context).
    pub context: Option<Arc<BrowserContext>>,
    /// Account/login surface (OXI account* methods, event forwarding).
    pub logins: Option<Arc<crate::account::LoginSurface>>,
}

impl ConnectionRole {
    /// An agent connection, optionally bound to the server's primary context.
    pub fn agent(
        context: Option<Arc<BrowserContext>>,
        logins: Option<Arc<crate::account::LoginSurface>>,
    ) -> Self {
        ConnectionRole {
            kind: RoleKind::Agent,
            context,
            logins,
        }
    }

    /// A viewer connection — always lands in the takeover context.
    pub fn viewer(
        context: Arc<BrowserContext>,
        logins: Option<Arc<crate::account::LoginSurface>>,
    ) -> Self {
        ConnectionRole {
            kind: RoleKind::Viewer,
            context: Some(context),
            logins,
        }
    }
}

/// A single CDP session over a WebSocket connection.
///
/// Holds a reference to the shared `Browser`, owns a `Session` that
/// represents the browsing context, and runs an event broadcaster that
/// forwards CDP events to the WebSocket client.
pub struct CdpSession {
    /// The WebSocket sink (for sending responses).
    sink: futures::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<hyper_util::rt::TokioIo<hyper::upgrade::Upgraded>>,
        tungstenite::Message,
    >,
    /// The WebSocket stream (for receiving commands).
    ws: futures::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<hyper_util::rt::TokioIo<hyper::upgrade::Upgraded>>,
    >,
    /// Session ID for this CDP connection.
    session_id: String,
    /// Target ID this session is attached to.
    #[allow(dead_code)]
    target_id: Option<String>,
    /// Shared browser instance (for creating new sessions, etc.).
    #[allow(dead_code)]
    browser: Arc<Browser>,
    /// Browser session for page interaction.
    session: Arc<RwLock<oxibrowser_core::session::Session>>,
    /// Event sender (cloned into DispatchContext for domain handlers).
    event_sender: EventSender,
    /// Registry of paused Fetch requests (shared via DispatchContext).
    fetch_registry: oxibrowser_core::network::SharedRegistry,
    /// Shared dialog-resolution gate (for Page.handleJavaScriptDialog).
    dialog_gate: oxibrowser_core::js::DialogGate,
    /// Attached child targets (multi-tab): sessionId → child Browser Session.
    child_targets: crate::domains::ChildTargets,
    /// Credential broker (provider + policy engine), when the server was
    /// built with `with_credentials`. `None` keeps credential OXI methods
    /// answering `credentialsUnavailable`.
    credentials: Option<Arc<crate::credential::CredentialBroker>>,
    /// Connection role (§5.2) — gates dispatch per method.
    role: RoleKind,
    /// Account/login surface, when the server was built with `with_login`.
    logins: Option<Arc<crate::account::LoginSurface>>,
    /// The context this session's main target lives in (credential-mode
    /// gate + takeover gate).
    session_context: Arc<BrowserContext>,
    /// Event receiver (drained by background task).
    event_receiver: Option<EventReceiver>,
    /// Shutdown signal for the CoreEvent drainer task — sent when `run`
    /// ends so the drainer exits promptly (not just on channel disconnect).
    core_shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl CdpSession {
    /// Create a new CDP session wrapping a WebSocket stream.
    ///
    /// This also creates a Browser `Session` for page interaction (in the
    /// connection's context — default context for plain agent connections,
    /// the takeover context for viewers, the primary context for
    /// `--account`-bound servers) and an event broadcaster for CDP event
    /// publishing.
    #[tracing::instrument(skip(ws_stream, browser, credentials, conn), err)]
    pub async fn new(
        ws_stream: tokio_tungstenite::WebSocketStream<
            hyper_util::rt::TokioIo<hyper::upgrade::Upgraded>,
        >,
        browser: Arc<Browser>,
        credentials: Option<Arc<crate::credential::CredentialBroker>>,
        conn: ConnectionRole,
    ) -> anyhow::Result<Self> {
        let (sink, ws) = ws_stream.split();
        let session_id = format!("session-{}", uuid::Uuid::new_v4());

        // Create a browser session for this CDP connection, in its context.
        let session = match conn.context.as_ref() {
            Some(ctx) => browser.new_session_in(ctx).await?,
            None => browser.new_session().await?,
        };
        let session_context = match conn.context.as_ref() {
            Some(ctx) => Arc::clone(ctx),
            None => browser.default_context(),
        };

        let (event_sender, event_receiver) = event_channel();
        // CoreEvent sink: the JS thread (in core) pushes neutral CoreEvents
        // onto this channel; the drainer task translates them into CDP events.
        let (core_tx, core_rx) = std::sync::mpsc::channel::<oxibrowser_core::js::CoreEvent>();
        {
            let mut s = session.write().await;
            s.set_event_sink(core_tx);
        }
        // Spawn a drainer that pumps CoreEvents into CDP events. Exits when the
        // core sender drops (session/JS-thread teardown) OR when the shutdown
        // oneshot fires (run() ending) — whichever comes first — for a prompt,
        // deterministic exit instead of relying solely on disconnect.
        let (core_shutdown_tx, mut core_shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let drain_events = event_sender.clone();
        tokio::spawn(async move {
            loop {
                // Drain everything currently queued without blocking.
                loop {
                    match core_rx.try_recv() {
                        Ok(ev) => crate::core_event::emit_core_event(&drain_events, ev),
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                    }
                }
                // Wait for a new event tick or a shutdown signal.
                tokio::select! {
                    _ = &mut core_shutdown_rx => return,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
                }
            }
        });
        // Clone the shared dialog gate so Page.handleJavaScriptDialog can
        // resolve a pending dialog without acquiring the session lock.
        let dialog_gate = session.read().await.dialog_gate();
        let child_targets: crate::domains::ChildTargets =
            Arc::new(crate::domains::TargetRegistry::new());

        info!(session_id = %session_id, "CDP session created");

        Ok(Self {
            ws,
            sink,
            session_id,
            target_id: None,
            browser,
            session,
            dialog_gate,
            child_targets,
            credentials,
            role: conn.kind,
            logins: conn.logins,
            session_context,
            event_sender,
            fetch_registry: oxibrowser_core::network::intercept::shared_registry(),
            event_receiver: Some(event_receiver),
            core_shutdown: Some(core_shutdown_tx),
        })
    }
    /// Run the message dispatch loop.
    ///
    /// Reads commands from the WebSocket, dispatches each **concurrently**
    /// (so a long-running command — e.g. `Runtime.evaluate` blocked on a
    /// dialog — cannot stall event forwarding or other commands), and forwards
    /// CDP responses + events to the client.
    #[tracing::instrument(skip(self), fields(session_id = %self.session_id), err)]
    pub async fn run(mut self) -> anyhow::Result<()> {
        info!(session_id = %self.session_id, "CDP session started");

        // Take the event receiver out — drained by this loop's select!.
        let mut event_rx = self
            .event_receiver
            .take()
            .ok_or_else(|| anyhow::anyhow!("event_receiver must be present at session start"))?;

        // Take the CoreEvent drainer shutdown signal so we can fire it when the
        // dispatch loop ends (prompt, deterministic drainer exit).
        let core_shutdown = self.core_shutdown.take();

        // Command responses flow back from spawned dispatch tasks through this
        // channel, keeping the select! free to poll ws input, events, and
        // responses concurrently.
        let (response_tx, mut response_rx) = tokio::sync::mpsc::unbounded_channel::<CdpResponse>();

        // Account-event forwarder (§7.3): orchestrator broadcasts →
        // OXI.accountStateChanged / OXI.loginStateChanged on this connection.
        // Exits with the connection (sender closed).
        if let Some(surface) = self.logins.as_ref() {
            spawn_account_event_forwarder(surface.clone(), self.event_sender.clone());
        }

        loop {
            tokio::select! {
                // Incoming CDP commands
                msg = self.ws.next() => {
                    match msg {
                        Some(Ok(tungstenite::Message::Text(text))) => {
                            debug!(text = %text, "received CDP message");
                            let ctx = DispatchContext {
                                session: self.session.clone(),
                                events: self.event_sender.clone(),
                                fetch_registry: self.fetch_registry.clone(),
                                dialog_gate: self.dialog_gate.clone(),
                                browser: self.browser.clone(),
                                child_targets: self.child_targets.clone(),
                                browser_context: self.session_context.clone(),
                                credentials: self.credentials.clone(),
                                role: self.role,
                                logins: self.logins.clone(),
                            };
                            let response_tx = response_tx.clone();
                            // Spawn so dispatch never blocks the loop — a
                            // blocking evaluate (e.g. alert()) still lets
                            // Page.handleJavaScriptDialog be received & run.
                            tokio::spawn(async move {
                                let response = dispatch_command(text.to_string(), &ctx).await;
                                let _ = response_tx.send(response);
                            });
                        }
                        Some(Ok(tungstenite::Message::Close(_))) => {
                            info!(session_id = %self.session_id, "WebSocket closed by client");
                            break;
                        }
                        Some(Ok(tungstenite::Message::Ping(data))) => {
                            self.sink.send(tungstenite::Message::Pong(data)).await?;
                        }
                        Some(Ok(_)) => {
                            // Binary, Pong, Frame — ignore
                        }
                        Some(Err(e)) => {
                            error!(error = %e, "WebSocket read error");
                            break;
                        }
                        None => {
                            info!(session_id = %self.session_id, "WebSocket stream ended");
                            break;
                        }
                    }
                }
                // Command responses from spawned dispatch tasks
                response = response_rx.recv() => {
                    if let Some(response) = response
                        && let Err(e) = self.send_response(response).await
                    {
                        warn!(error = %e, "failed to send CDP response");
                        break;
                    }
                }
                // Outgoing CDP events (independent of dispatch — decoupled so
                // e.g. Page.javascriptDialogOpening reaches the client while a
                // blocking evaluate is still pending).
                event = event_rx.recv() => {
                    match event {
                        Some(event) => {
                            if let Err(e) = self.send_event(event).await {
                                warn!(error = %e, "failed to send CDP event");
                                break;
                            }
                        }
                        None => break,
                    }
                }
            }
        }

        // Drop our response sender; in-flight dispatch tasks finish and their
        // sends are dropped (channel receiver gone on return).
        drop(response_tx);
        // Signal the CoreEvent drainer to exit promptly (it also exits on
        // channel disconnect once the Session closes below).
        if let Some(tx) = core_shutdown {
            let _ = tx.send(());
        }

        // Close the underlying Session so is_closed() returns true.
        self.session.write().await.close().await.ok();

        // Free the session slot so new connections are not rejected
        // when max_sessions is reached.
        self.browser.cleanup_closed_sessions();

        info!(session_id = %self.session_id, "CDP session ended");
        Ok(())
    }

    /// Send a CDP response to the client.
    async fn send_response(&mut self, response: CdpResponse) -> anyhow::Result<()> {
        let text = serde_json::to_string(&response)?;
        debug!(text = %text, "sending CDP response");
        self.sink
            .send(tungstenite::Message::Text(text.into()))
            .await?;
        Ok(())
    }

    /// Send a CDP event to the client.
    async fn send_event(&mut self, event: CdpEvent) -> anyhow::Result<()> {
        let text = serde_json::to_string(&event)?;
        debug!(text = %text, "sending CDP event");
        self.sink
            .send(tungstenite::Message::Text(text.into()))
            .await?;
        Ok(())
    }

    /// Get the session ID.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Get the target ID (if attached).
    pub fn target_id(&self) -> Option<&str> {
        self.target_id.as_deref()
    }
}

/// Pump the orchestrator's account/login events onto this connection as
/// `OXI.accountStateChanged` / `OXI.loginStateChanged` (§7.3). Exits when
/// the connection's event channel closes.
fn spawn_account_event_forwarder(surface: Arc<crate::account::LoginSurface>, events: EventSender) {
    let mut rx = surface.orchestrator().subscribe();
    tokio::spawn(async move {
        loop {
            if events.is_closed() {
                return;
            }
            match tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await {
                Ok(Ok(oxibrowser_core::account::AccountEvent::StateChanged {
                    account_id,
                    to,
                    detail,
                    ..
                })) => {
                    events.send_event(
                        "OXI.accountStateChanged",
                        serde_json::json!({ "accountId": account_id, "state": to, "detail": detail }),
                    );
                }
                Ok(Ok(oxibrowser_core::account::AccountEvent::LoginChanged {
                    login_id,
                    account_id,
                    state,
                })) => {
                    events.send_event(
                        "OXI.loginStateChanged",
                        serde_json::json!({ "loginId": login_id, "accountId": account_id, "state": state }),
                    );
                }
                Ok(Ok(oxibrowser_core::account::AccountEvent::AgentLogin {
                    login_id,
                    account_id,
                    agent_id,
                    state,
                    detail,
                })) => {
                    events.send_event(
                        "OXI.loginStateChanged",
                        serde_json::json!({
                            "loginId": login_id,
                            "accountId": account_id,
                            "agentId": agent_id,
                            "state": state,
                            "detail": detail,
                        }),
                    );
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => return,
                Err(_timeout) => continue,
            }
        }
    });
}

/// Parse + dispatch a single CDP command text, returning the response.
///
/// Runs as a spawned task so dispatch never blocks the run loop's `select!`
/// (a long-running command like a dialog-blocked `Runtime.evaluate` must not
/// stall event forwarding or other commands).
async fn dispatch_command(text: String, ctx: &DispatchContext) -> CdpResponse {
    // Validate message size
    if text.len() > MAX_CDP_MESSAGE_SIZE {
        warn!(
            size = text.len(),
            max = MAX_CDP_MESSAGE_SIZE,
            "CDP message too large, dropping"
        );
        return CdpResponse {
            id: 0,
            result: None,
            error: Some(crate::protocol::CdpError {
                code: -32600,
                message: format!(
                    "Message too large: {} bytes (max {} bytes)",
                    text.len(),
                    MAX_CDP_MESSAGE_SIZE
                ),
            }),
            session_id: None,
        };
    }

    // Parse the CDP request
    let request: CdpRequest = match serde_json::from_str(&text) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "failed to parse CDP request");
            return CdpResponse {
                id: 0,
                result: None,
                error: Some(crate::protocol::CdpError {
                    code: -32700,
                    message: format!("Parse error: {e}"),
                }),
                session_id: None,
            };
        }
    };

    let request_id = request.id.unwrap_or(0);
    let session_id_for_response = request.session_id.clone();

    debug!(
        id = request_id,
        method = %request.method,
        "dispatching CDP command"
    );

    // Multi-tab: if the command carries a sessionId for an attached child
    // target, route to that session; otherwise use the default session.
    let effective_ctx = match &request.session_id {
        Some(sid) => {
            let child = ctx.child_targets.get(sid).await;
            match child {
                Some(entry) => {
                    // The child's credential-mode gate reads the context the
                    // target was created in (falling back to the default
                    // context if it has since been disposed).
                    let browser_context = ctx
                        .browser
                        .context(&oxibrowser_core::context::ContextId::from_string(
                            entry.browser_context_id.clone(),
                        ))
                        .unwrap_or_else(|| ctx.browser.default_context());
                    DispatchContext {
                        session: entry.session,
                        events: ctx.events.clone(),
                        fetch_registry: ctx.fetch_registry.clone(),
                        dialog_gate: ctx.dialog_gate.clone(),
                        browser: ctx.browser.clone(),
                        child_targets: ctx.child_targets.clone(),
                        browser_context,
                        credentials: ctx.credentials.clone(),
                        role: ctx.role,
                        logins: ctx.logins.clone(),
                    }
                }
                None => {
                    if ctx.child_targets.is_closed(sid) {
                        // Closed target: fail loudly rather than silently
                        // evaluating against the default session.
                        return CdpResponse {
                            id: request_id,
                            result: None,
                            error: Some(CdpError {
                                code: -32001,
                                message: "target closed".to_string(),
                            }),
                            session_id: session_id_for_response,
                        };
                    }
                    // Unknown sessionId — fall back to the default session.
                    DispatchContext {
                        session: ctx.session.clone(),
                        events: ctx.events.clone(),
                        fetch_registry: ctx.fetch_registry.clone(),
                        dialog_gate: ctx.dialog_gate.clone(),
                        browser: ctx.browser.clone(),
                        child_targets: ctx.child_targets.clone(),
                        browser_context: ctx.browser_context.clone(),
                        credentials: ctx.credentials.clone(),
                        role: ctx.role,
                        logins: ctx.logins.clone(),
                    }
                }
            }
        }
        None => DispatchContext {
            session: ctx.session.clone(),
            events: ctx.events.clone(),
            fetch_registry: ctx.fetch_registry.clone(),
            dialog_gate: ctx.dialog_gate.clone(),
            browser: ctx.browser.clone(),
            child_targets: ctx.child_targets.clone(),
            browser_context: ctx.browser_context.clone(),
            credentials: ctx.credentials.clone(),
            role: ctx.role,
            logins: ctx.logins.clone(),
        },
    };

    match domains::dispatch(&request.method, request.params, &effective_ctx).await {
        Ok(result) => CdpResponse {
            id: request_id,
            result: Some(result.unwrap_or(serde_json::json!({}))),
            error: None,
            session_id: session_id_for_response,
        },
        Err(cdp_error) => CdpResponse {
            id: request_id,
            result: None,
            error: Some(cdp_error),
            session_id: session_id_for_response,
        },
    }
}
