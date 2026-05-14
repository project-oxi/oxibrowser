//! CDP session — a single WebSocket connection for CDP communication.
//!
//! Manages a WebSocket connection, dispatches incoming CDP commands to
//! domain handlers, and sends responses and events back to the client.
//!
//! Each CDP session creates a corresponding Browser `Session` for page
//! interaction (navigation, DOM access, JS evaluation).

use std::collections::HashMap;

use parking_lot::RwLock as SyncRwLock;

use crate::domains;
use crate::event::{event_channel, EventReceiver, EventSender};
use crate::protocol::{CdpEvent, CdpRequest, CdpResponse};
use crate::server::MAX_CDP_MESSAGE_SIZE;
use futures::{SinkExt, StreamExt};
use oxibrowser_core::Browser;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite;
use tracing::{debug, error, info, warn};

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
    /// Event receiver (drained by background task).
    event_receiver: Option<EventReceiver>,
}

impl CdpSession {
    /// Create a new CDP session wrapping a WebSocket stream.
    ///
    /// This also creates a new Browser `Session` for page interaction
    /// and an event broadcaster for CDP event publishing.
    pub async fn new(
        ws_stream: tokio_tungstenite::WebSocketStream<
            hyper_util::rt::TokioIo<hyper::upgrade::Upgraded>,
        >,
        browser: Arc<Browser>,
    ) -> anyhow::Result<Self> {
        let (sink, ws) = ws_stream.split();
        let session_id = format!("session-{}", uuid::Uuid::new_v4());

        // Create a browser session for this CDP connection
        let session = browser.new_session().await?;

        // Create event broadcaster
        let (event_sender, event_receiver) = event_channel();

        info!(session_id = %session_id, "CDP session created");

        Ok(Self {
            ws,
            sink,
            session_id,
            target_id: None,
            browser,
            session,
            event_sender,
            event_receiver: Some(event_receiver),
        })
    }

    /// Run the message dispatch loop.
    ///
    /// Reads commands from WebSocket, dispatches to domain handlers,
    /// sends responses back, and forwards CDP events to the client.
    pub async fn run(mut self) -> anyhow::Result<()> {
        info!(session_id = %self.session_id, "CDP session started");

        // Take the event receiver out — the forwarding task owns it.
        let mut event_rx = self
            .event_receiver
            .take()
            .expect("event_receiver must be present at session start");

        loop {
            tokio::select! {
                // Incoming CDP commands
                msg = self.ws.next() => {
                    match msg {
                        Some(Ok(tungstenite::Message::Text(text))) => {
                            debug!(text = %text, "received CDP message");
                            self.handle_text_message(&text).await?;
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
                // Outgoing CDP events
                event = event_rx.recv() => {
                    match event {
                        Some(event) => {
                            if let Err(e) = self.send_event(event).await {
                                warn!(error = %e, "failed to send CDP event");
                                break;
                            }
                        }
                        None => {
                            // Channel closed
                            break;
                        }
                    }
                }
            }
        }

        info!(session_id = %self.session_id, "CDP session ended");
        Ok(())
    }

    /// Handle a single text message.
    async fn handle_text_message(&mut self, text: &str) -> anyhow::Result<()> {
        // Validate message size
        if text.len() > MAX_CDP_MESSAGE_SIZE {
            warn!(
                size = text.len(),
                max = MAX_CDP_MESSAGE_SIZE,
                "CDP message too large, dropping"
            );
            let response = CdpResponse {
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
            self.send_response(response).await?;
            return Ok(());
        }

        // Parse the CDP request
        let request: CdpRequest = match serde_json::from_str(text) {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "failed to parse CDP request");
                let response = CdpResponse {
                    id: 0,
                    result: None,
                    error: Some(crate::protocol::CdpError {
                        code: -32700,
                        message: format!("Parse error: {e}"),
                    }),
                    session_id: None,
                };
                self.send_response(response).await?;
                return Ok(());
            }
        };

        let request_id = request.id.unwrap_or(0);
        let session_id_for_response = request.session_id.clone();

        debug!(
            id = request_id,
            method = %request.method,
            "dispatching CDP command"
        );

        // Create dispatch context with session + event sender + paused requests
        let ctx = crate::domains::DispatchContext {
            session: self.session.clone(),
            events: self.event_sender.clone(),
            paused_requests: Arc::new(SyncRwLock::new(HashMap::new())),
            mock_responses: Arc::new(SyncRwLock::new(HashMap::new())),
        };

        // Dispatch to domain handler
        let response = match domains::dispatch(&request.method, request.params, &ctx).await {
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
        };

        self.send_response(response).await
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
