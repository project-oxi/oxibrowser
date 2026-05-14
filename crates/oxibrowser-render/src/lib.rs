//! OxiBrowser rendering engine — HTML to PNG/PDF via Blitz.
//!
//! Provides a high-level API for rendering HTML content to pixel images (PNG)
//! or vector documents (PDF) without requiring a browser engine.
//!
//! Built on the [Blitz](https://github.com/DioxusLabs/blitz) rendering pipeline:
//! - **Stylo** — CSS cascade and style resolution (from Servo)
//! - **Taffy** — Flexbox/Grid/Block layout
//! - **Parley** — Text shaping and layout
//! - **Vello** (CPU) — 2D rendering to pixel buffer
//! - **Krilla** — PDF generation
//!
//! # Quick Start
//!
//! ```rust,ignore
//! use oxibrowser_render::{render, RenderConfig, OutputFormat};
//!
//! let html = "<h1>Hello, OxiBrowser!</h1>";
//!
//! // PNG
//! let png_bytes = render(html, RenderConfig::default())?;
//!
//! // PDF
//! let pdf_bytes = render(html, RenderConfig::default().format(OutputFormat::Pdf))?;
//! ```

mod config;
mod error;
mod render;

pub use config::{ColorScheme, OutputFormat, RenderConfig};
pub use error::{RenderError, RenderResult};

use blitz_dom::DocumentConfig;
use blitz_html::HtmlDocument;
use blitz_traits::shell::Viewport;

/// Render HTML content to the specified output format.
///
/// This is the main entry point. It parses the HTML, computes styles
/// and layout via the Blitz pipeline, and renders to PNG or PDF bytes.
pub fn render(html: &str, config: RenderConfig) -> RenderResult<Vec<u8>> {
    config.validate()?;

    // Parse HTML into Blitz DOM
    let mut document = create_document(html, &config)?;

    // Resolve styles and compute layout
    document.resolve(0.0);

    // Render to the specified format
    match config.format {
        OutputFormat::Png => render::png::render_to_png(&document, &config),
        OutputFormat::Pdf => render::pdf::render_to_pdf(&document, &config),
    }
}

/// Convenience: render HTML to PNG bytes.
#[cfg(feature = "png")]
pub fn render_to_png(html: &str, config: RenderConfig) -> RenderResult<Vec<u8>> {
    render(html, config.format(OutputFormat::Png))
}

/// Convenience: render HTML to PDF bytes.
#[cfg(feature = "pdf")]
pub fn render_to_pdf(html: &str, config: RenderConfig) -> RenderResult<Vec<u8>> {
    render(html, config.format(OutputFormat::Pdf))
}

/// Create and configure a Blitz document from HTML.
fn create_document(html: &str, config: &RenderConfig) -> RenderResult<HtmlDocument> {
    let viewport = Viewport::new(
        config.width,
        config.height,
        config.scale,
        config.color_scheme.into(),
    );

    let doc_config = DocumentConfig {
        viewport: Some(viewport),
        ..Default::default()
    };

    Ok(HtmlDocument::from_html(html, doc_config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = RenderConfig::default();
        assert_eq!(config.width, 1280);
        assert_eq!(config.height, 720);
        assert_eq!(config.scale, 1.0);
    }

    #[test]
    fn test_config_validation_rejects_zero_width() {
        let config = RenderConfig::new().width(0);
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_validation_rejects_zero_height() {
        let config = RenderConfig::new().height(0);
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_builder() {
        let config = RenderConfig::new()
            .width(1920)
            .height(1080)
            .scale(2.0)
            .format(OutputFormat::Pdf);

        assert_eq!(config.width, 1920);
        assert_eq!(config.height, 1080);
        assert_eq!(config.scale, 2.0);
        assert_eq!(config.format, OutputFormat::Pdf);
    }
}