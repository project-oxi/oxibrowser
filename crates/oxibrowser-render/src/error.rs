//! Error types for the rendering engine.

use thiserror::Error;

/// Result type alias for render operations.
pub type RenderResult<T> = std::result::Result<T, RenderError>;

/// Errors that can occur during HTML rendering.
#[derive(Debug, Error)]
pub enum RenderError {
    /// The requested output format feature is not enabled.
    #[error("output format '{0}' is not enabled; enable the '{0}' feature flag")]
    FormatNotEnabled(&'static str),

    /// Invalid configuration.
    #[error("invalid config: {0}")]
    InvalidConfig(String),

    /// PNG rendering failed.
    #[error("PNG rendering failed: {0}")]
    PngRender(String),

    /// PNG encoding failed.
    #[error("PNG encoding failed: {0}")]
    PngEncode(String),

    /// PDF creation failed.
    #[error("PDF creation failed: {0}")]
    PdfCreate(String),

    /// PDF rendering failed.
    #[error("PDF rendering failed: {0}")]
    PdfRender(String),

    /// Font loading or rendering failed.
    #[error("font error: {0}")]
    Font(String),

    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}