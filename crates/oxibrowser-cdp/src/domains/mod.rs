//! CDP domains — implementations of CDP domain methods.
//!
//! Mirrors Lightpanda's `src/cdp/domains/` structure.
//!
//! The `dispatch` function is async and receives a `DispatchContext` that
//! includes the browser `Session` for page interaction AND the `EventSender`
//! so domain handlers can emit CDP events.

pub mod browser;
pub mod dom;
pub mod fetch;
pub mod network;
pub mod oxi;
pub mod page;
pub mod runtime;
pub mod target;

use crate::event::EventSender;
use crate::protocol::CdpError;
use oxibrowser_core::session::Session;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// A paused Fetch request awaiting client decision.
#[derive(Debug, Clone)]
pub struct PausedRequest {
    /// Unique request ID.
    pub request_id: String,
    /// Request URL.
    pub url: String,
    /// HTTP method.
    pub method: String,
    /// Resource type (Document, Script, Image, ...).
    pub resource_type: String,
    /// Frame ID.
    pub frame_id: String,
    /// Request headers as JSON map.
    pub headers: Map<String, Value>,
}

/// Context passed to all domain handlers.
///
/// Combines browser session access with the event sender, so handlers can
/// both read/write page data AND emit CDP events.
pub struct DispatchContext {
    /// Browser session (read/write for navigation, DOM access, JS eval).
    pub session: Arc<RwLock<Session>>,
    /// Event sender for emitting CDP events to the client.
    pub events: EventSender,
    /// Paused requests awaiting client decision.
    pub paused_requests: Arc<parking_lot::RwLock<HashMap<String, PausedRequest>>>,
    /// Mock responses for fulfilled requests.
    pub mock_responses: Arc<parking_lot::RwLock<HashMap<String, MockResponse>>>,
}

/// A mock response stored by Fetch.fulfillRequest.
#[derive(Debug, Clone)]
pub struct MockResponse {
    pub body: String,
    pub status: u16,
    pub headers: Map<String, Value>,
    pub base64_encoded: bool,
}

/// Result of handling a CDP domain method.
pub type DomainResult = std::result::Result<Option<Value>, CdpError>;

/// Dispatch a CDP method to the appropriate domain handler.
///
/// Returns `Ok(Some(result))` on success, `Ok(None)` for empty results,
/// or `Err(CdpError)` for unknown methods.
pub async fn dispatch(method: &str, params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
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
        "Fetch" => fetch::handle(method_name, params, ctx).await,
        "Network" => network::handle(method_name, params, ctx).await,
        "OXI" => oxi::handle(method_name, params, ctx),
        "Page" => page::handle(method_name, params, ctx).await,
        "Runtime" => runtime::handle(method_name, params, ctx).await,
        "Target" => target::handle(method_name, params),
        _ => Err(CdpError {
            code: -32601,
            message: format!("unknown domain: {domain}"),
        }),
    }
}
