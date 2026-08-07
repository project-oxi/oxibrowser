//! [`RenderDocument`] — the public handle wrapping a Blitz [`BaseDocument`].

use blitz_dom::ns;
use blitz_dom::BaseDocument;
use blitz_dom::DocumentConfig;
use blitz_dom::LocalName;
use blitz_dom::NodeData;
use blitz_dom::QualName;
use blitz_html::HtmlDocument;
use blitz_traits::shell::ColorScheme;
use blitz_traits::shell::Viewport as BlitzViewport;

use crate::paint;

/// Opaque handle to a node within a [`RenderDocument`]. Maps to Blitz's `usize`
/// node id. Only valid for the [`RenderDocument`] that minted it.
pub type NodeId = usize;

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

/// Build a `QualName` for an HTML-namespaced element/attribute from a runtime
/// tag/name string (no macro-literal required).
fn html_name(local: &str) -> QualName {
    QualName::new(None, ns!(html), LocalName::from(local))
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
    // ── Construction ───────────────────────────────────────────────────────

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

    // ── Queries ────────────────────────────────────────────────────────────

    /// The root element's node id (the `<html>` element).
    pub fn root_element_id(&self) -> NodeId {
        self.doc.root_element().id
    }

    /// First node matching a CSS selector, or `None`.
    pub fn query_selector(&self, selector: &str) -> Option<NodeId> {
        self.doc.query_selector(selector).ok().flatten()
    }

    /// All nodes matching a CSS selector.
    pub fn query_selector_all(&self, selector: &str) -> Vec<NodeId> {
        self.doc
            .query_selector_all(selector)
            .map(|s| s.to_vec())
            .unwrap_or_default()
    }

    /// The tag name of `node` (lowercased), or `None` if not an element.
    pub fn tag_name(&self, node: NodeId) -> Option<String> {
        self.doc
            .get_node(node)
            .and_then(|n| n.data.downcast_element())
            .map(|e| e.name.local.to_string())
    }

    /// An attribute value on `node`, or `None`.
    pub fn node_attr(&self, node: NodeId, name: &str) -> Option<String> {
        let target = LocalName::from(name);
        self.doc
            .get_node(node)
            .and_then(|n| n.attrs())
            .and_then(|attrs| attrs.iter().find(|a| a.name.local == target))
            .map(|a| a.value.clone())
    }

    /// Recursive text content of `node` (text node's content, or concatenation
    /// of descendant text for an element).
    pub fn node_text(&self, node: NodeId) -> String {
        let mut out = String::new();
        self.collect_text(node, &mut out);
        out
    }

    fn collect_text(&self, node_id: NodeId, out: &mut String) {
        if let Some(node) = self.doc.get_node(node_id) {
            match &node.data {
                NodeData::Text(t) => out.push_str(&t.content),
                _ => {
                    for &child in &node.children {
                        self.collect_text(child, out);
                    }
                }
            }
        }
    }

    // ── Mutation ───────────────────────────────────────────────────────────
    //
    // Each mutation goes through `BaseDocument::mutate()`, which returns a
    // short-lived `DocumentMutator` that flushes style/layout damage on drop.
    // After a batch of mutations, the next [`Self::capture_png`] re-runs
    // `resolve()` so the painted frame reflects the changes.

    /// Create a new element node (detached; use [`Self::append_child`] to
    /// attach it).
    pub fn create_element(&mut self, tag: &str) -> NodeId {
        self.doc.mutate().create_element(html_name(tag), Vec::new())
    }

    /// Create a new text node (detached).
    pub fn create_text_node(&mut self, text: &str) -> NodeId {
        self.doc.create_text_node(text)
    }

    /// Append `child` to `parent`'s children.
    pub fn append_child(&mut self, parent: NodeId, child: NodeId) {
        self.doc.mutate().append_children(parent, &[child]);
    }

    /// Set (or replace) an attribute on `node`.
    pub fn set_attribute(&mut self, node: NodeId, name: &str, value: &str) {
        self.doc.mutate().set_attribute(node, html_name(name), value);
    }

    /// Remove an attribute from `node`.
    pub fn remove_attribute(&mut self, node: NodeId, name: &str) {
        self.doc.mutate().clear_attribute(node, html_name(name));
    }

    /// Set an inline style property on `node` (e.g. `color`, `background-color`).
    pub fn set_inline_style(&mut self, node: NodeId, property: &str, value: &str) {
        self.doc.set_style_property(node, property, value);
    }

    /// Replace `node`'s children with a single text node containing `text`.
    pub fn set_text(&mut self, node: NodeId, text: &str) {
        let children: Vec<NodeId> = self
            .doc
            .get_node(node)
            .map(|n| n.children.clone())
            .unwrap_or_default();
        for child in children {
            self.doc.mutate().remove_node(child);
        }
        let text_id = self.doc.create_text_node(text);
        self.doc.mutate().append_children(node, &[text_id]);
    }

    /// Detach `node` from its parent (the node itself is dropped).
    pub fn remove_node(&mut self, node: NodeId) {
        self.doc.mutate().remove_node(node);
    }

    // ── Capture ────────────────────────────────────────────────────────────

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

    /// Render the current document state to a PNG. Re-runs Stylo/Taffy resolve
    /// first, so any mutations since construction are reflected.
    pub fn capture_png(&mut self, opts: &CaptureOpts) -> Result<Vec<u8>, RenderError> {
        let viewport = opts.viewport.unwrap_or(self.viewport);
        paint::capture_png(&mut self.doc, viewport, opts.full_page)
    }
}
