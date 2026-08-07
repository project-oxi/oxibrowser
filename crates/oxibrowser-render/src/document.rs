//! [`RenderDocument`] — the public handle wrapping a Blitz [`BaseDocument`].

use blitz_dom::BaseDocument;
use blitz_dom::DocumentConfig;
use blitz_html::HtmlDocument;
use blitz_traits::shell::ColorScheme;
use blitz_traits::shell::Viewport as BlitzViewport;

use crate::paint;

/// Error returned by the rendering pipeline.
#[derive(Debug)]
pub enum RenderError {
    /// Blitz/Stylo/vello_cpu reported an error.
    Render(String),
    /// PNG encoding failed.
    Encode(String),
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderError::Render(m) => write!(f, "render error: {m}"),
            RenderError::Encode(m) => write!(f, "png encode error: {m}"),
        }
    }
}

impl std::error::Error for RenderError {}

/// Logical viewport for document layout.
#[derive(Debug, Clone, Copy)]
pub struct Viewport {
    /// CSS pixel width.
    pub width: u32,
    /// CSS pixel height.
    pub height: u32,
    /// Device pixel ratio (1.0 = no hi-dpi scaling).
    pub scale: f64,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            scale: 1.0,
        }
    }
}

/// Options for [`RenderDocument::capture_png`].
#[derive(Debug, Clone, Copy, Default)]
pub struct CaptureOpts {
    /// Override the document viewport size. If `None`, the document's current
    /// viewport is used.
    pub viewport: Option<Viewport>,
    /// Render the full document height (ignoring the viewport height), like a
    /// browser's full-page screenshot. The viewport width is still respected.
    pub full_page: bool,
}

/// A Blitz-backed renderable document.
///
/// Owns a [`BaseDocument`] that has already been parsed, style-resolved and
/// laid out. Not `Send` — must be used from a single thread (see crate docs).
pub struct RenderDocument {
    doc: BaseDocument,
    viewport: Viewport,
}

impl RenderDocument {
    /// Parse HTML, resolve styles (Stylo), and lay out (Taffy) for `viewport`.
    ///
    /// `base_url`, if provided, is used to resolve linked resources
    /// (stylesheets, images, fonts).
    pub fn from_html(html: &str, base_url: Option<&str>, viewport: Viewport) -> Result<Self, RenderError> {
        let config = DocumentConfig::default();
        let mut doc = HtmlDocument::from_html(html, config).into_inner();

        if let Some(url) = base_url {
            doc.set_base_url(url);
        }

        // Critical: set the viewport BEFORE resolve, otherwise Taffy lays out
        // against a (0,0) window and flexbox children collapse to zero size.
        doc.set_viewport(BlitzViewport::new(
            viewport.width,
            viewport.height,
            viewport.scale as f32,
            ColorScheme::Light,
        ));

        // Drive Stylo restyle + Taffy relayout once so the tree is paint-ready.
        doc.resolve(0.0);

        Ok(Self { doc, viewport })
    }

    /// Borrow the inner [`BaseDocument`] (read-only).
    pub fn document(&self) -> &BaseDocument {
        &self.doc
    }

    /// Borrow the inner [`BaseDocument`] mutably. Callers are responsible for
    /// re-running [`BaseDocument::resolve`] after mutation before capturing.
    pub fn document_mut(&mut self) -> &mut BaseDocument {
        &mut self.doc
    }

    /// The laid-out content size in CSS pixels (from the root element's
    /// `final_layout`). Valid only after [`Self::from_html`] (which resolves).
    pub fn content_size(&self) -> (u32, u32) {
        let size = self.doc.root_element().final_layout.size;
        let w = if size.width.is_finite() && size.width > 0.0 {
            size.width.ceil() as u32
        } else {
            self.viewport.width
        };
        let h = if size.height.is_finite() && size.height > 0.0 {
            size.height.ceil() as u32
        } else {
            self.viewport.height
        };
        (w, h)
    }

    /// Render the current document state to a PNG.
    pub fn capture_png(&mut self, opts: &CaptureOpts) -> Result<Vec<u8>, RenderError> {
        let viewport = opts.viewport.unwrap_or(self.viewport);
        paint::capture_png(&mut self.doc, viewport, opts.full_page)
    }
}
