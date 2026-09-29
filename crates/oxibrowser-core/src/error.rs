//! Error types for oxibrowser-core.

use thiserror::Error;

// Re-export intercept types from network module (single source of truth)
pub use crate::network::{InterceptedBody, InterceptedResponse};

/// Either a real HTTP response or a synthetic intercepted response.
#[derive(Debug)]
pub enum FetchResult {
    /// Real HTTP response from wreq.
    Real(wreq::Response),
    /// Synthetic response from Fetch.fulfillRequest.
    Intercepted(InterceptedResponse),
}

/// Core error type.
#[derive(Error, Debug)]
pub enum CoreError {
    #[error("navigation failed: {0}")]
    NavigationFailed(String),

    #[error("network error: {0}")]
    NetworkError(String),

    #[error("DNS resolution failed: {0}")]
    DnsError(String),

    #[error("connection timeout: {0}")]
    ConnectionTimeout(String),

    #[error("HTTP {status}: {message}")]
    HttpError { status: u16, message: String },

    #[error("JavaScript evaluation error: {0}")]
    JsError(String),

    #[error("JS execution timed out after {0}ms")]
    JsTimeout(u64),

    #[error("JS runtime limit exceeded: {0}")]
    JsRuntimeLimit(String),

    #[error("page not loaded")]
    PageNotLoaded,

    #[error("page error: {0}")]
    PageError(String),

    #[error("session error: {0}")]
    SessionError(String),

    #[error("session closed")]
    SessionClosed,

    #[error("browser closed")]
    BrowserClosed,

    #[error("invalid URL: {0}")]
    InvalidUrl(String),

    #[error("timeout: {0}")]
    Timeout(String),

    #[error("DOM error: {0}")]
    DomError(String),

    /// Intercepted response from Fetch.fulfillRequest (synthetic response).
    #[error("intercepted response")]
    InterceptedResponse(InterceptedResponse),

    #[error("screenshot error: {0}")]
    ScreenshotError(String),

    #[error("PDF render error: {0}")]
    PdfError(String),

    #[error("session store encryption error: {0}")]
    SessionStoreAead(String),

    #[error("session store file format error: {0}")]
    SessionStoreFormat(String),

    #[error("session fingerprint mismatch: {0}")]
    SessionFingerprintMismatch(String),

    #[error("session store io error: {0}")]
    SessionStoreIo(String),

    /// Another process holds this account's advisory lock (FM-L7 process
    /// boundary). Retry with `--lock-wait` / `--lock-timeout`, or after the
    /// holder exits — the kernel releases the lock on holder death.
    #[error("account is locked by another process: {detail}")]
    AccountLocked { detail: String },
}

/// Convenience Result alias.
pub type Result<T> = std::result::Result<T, CoreError>;

impl From<url::ParseError> for CoreError {
    fn from(e: url::ParseError) -> Self {
        CoreError::InvalidUrl(e.to_string())
    }
}

impl From<wreq::Error> for CoreError {
    fn from(e: wreq::Error) -> Self {
        if e.is_timeout() {
            return CoreError::ConnectionTimeout(e.to_string());
        }
        if e.is_connect() {
            let msg = e.to_string();
            if msg.contains("dns")
                || msg.contains("resolve")
                || msg.contains("getaddrinfo")
                || msg.contains("Name or service not known")
                || msg.contains("nodename nor servname")
            {
                return CoreError::DnsError(msg);
            }
            return CoreError::NetworkError(msg);
        }
        CoreError::NetworkError(e.to_string())
    }
}
