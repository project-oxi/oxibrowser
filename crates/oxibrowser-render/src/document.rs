//! [`RenderDocument`] — the public handle wrapping a Blitz [`BaseDocument`].

use blitz_dom::BaseDocument;
use blitz_dom::DocumentConfig;
use blitz_dom::LocalName;
use blitz_dom::NodeData;
use blitz_dom::QualName;
use blitz_dom::ns;
use blitz_html::HtmlDocument;
use blitz_traits::shell::ColorScheme;
use blitz_traits::shell::Viewport as BlitzViewport;

use std::sync::{LazyLock, RwLock};

use crate::paint;

/// Process-wide `prefers-color-scheme` override (`true` = dark, `false` =
/// light, `None` = no override, i.e. Blitz's built-in `ColorScheme::Light`).
///
/// Set via [`set_color_scheme_override`] before constructing a
/// [`RenderDocument`]; the value is read at construction time and baked into
/// the document's viewport, so Stylo media queries like
/// `@media (prefers-color-scheme: dark)` match accordingly.
static COLOR_SCHEME_OVERRIDE: LazyLock<RwLock<Option<bool>>> = LazyLock::new(|| RwLock::new(None));

/// Override the `prefers-color-scheme` media query result for documents
/// constructed afterwards. `Some(true)` forces dark, `Some(false)` forces
/// light, `None` clears the override (Blitz defaults to light).
pub fn set_color_scheme_override(dark: Option<bool>) {
    *COLOR_SCHEME_OVERRIDE
        .write()
        .expect("color scheme override lock poisoned") = dark;
}

/// Current `prefers-color-scheme` override (`true` = dark, `false` = light,
/// `None` = no override).
pub fn color_scheme_override_dark() -> Option<bool> {
    *COLOR_SCHEME_OVERRIDE
        .read()
        .expect("color scheme override lock poisoned")
}

/// Map the process-wide override to Blitz's shell `ColorScheme` for viewport
/// assembly. `None` (no override) falls back to `Light`, matching the previous
/// hardcoded value.
fn effective_color_scheme() -> ColorScheme {
    match color_scheme_override_dark() {
        Some(true) => ColorScheme::Dark,
        Some(false) | None => ColorScheme::Light,
    }
}

/// Opaque handle to a node within a [`RenderDocument`]. Maps to Blitz's `usize`
/// node id. Only valid for the [`RenderDocument`] that minted it.
pub type NodeId = usize;

/// Error returned by the rendering pipeline.
#[derive(Debug)]
pub enum RenderError {
    /// Blitz/Stylo/vello_cpu reported an error.
    Render(String),
    /// PNG decoding failed (invalid data or unsupported format).
    Decode(String),
    /// PNG encoding failed.
    Encode(String),
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderError::Render(m) => write!(f, "render error: {m}"),
            RenderError::Decode(m) => write!(f, "png decode error: {m}"),
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
    tree_revision: u64,
    next_incarnation: u64,
    incarnations: std::collections::HashMap<NodeId, u64>,
    attachments: std::collections::HashMap<NodeId, u64>,
}

impl RenderDocument {
    // ── Construction ───────────────────────────────────────────────────────

    /// Parse HTML, resolve styles (Stylo), and lay out (Taffy) for `viewport`.
    ///
    /// `base_url`, if provided, is used to resolve linked resources
    /// (stylesheets, images, fonts).
    pub fn from_html(
        html: &str,
        base_url: Option<&str>,
        viewport: Viewport,
    ) -> Result<Self, RenderError> {
        // base_url MUST be in DocumentConfig: blitz-html resolves eager
        // sub-resource ops (<img src>) DURING parsing, before the post-parse
        // `set_base_url` runs — without this, a relative img URL panics
        // (resolved against blitz's data: default).
        let config = DocumentConfig {
            base_url: base_url.map(str::to_string),
            ..DocumentConfig::default()
        };
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
            effective_color_scheme(),
        ));

        // Drive Stylo restyle + Taffy relayout once so the tree is paint-ready.
        doc.resolve(0.0);

        Ok(Self {
            doc,
            viewport,
            tree_revision: 0,
            next_incarnation: 1,
            incarnations: Default::default(),
            attachments: Default::default(),
        })
    }

    /// Like [`from_html`](Self::from_html), but registers the supplied webfont
    /// bytes into a Parley `FontContext` used for layout — the `@font-face`
    /// path. Each entry is raw font-file bytes (TTF/OTF/WOFF/WOFF2; WOFF is
    /// decoded by Blitz). Families are auto-detected from the font's own name
    /// tables, so a CSS `font-family: 'X'` matches a font whose internal family
    /// is `X`. System fonts remain available for non-`@font-face` text. No Blitz
    /// fork is required: `DocumentConfig.font_ctx` is public.
    pub fn from_html_with_fonts(
        html: &str,
        base_url: Option<&str>,
        viewport: Viewport,
        fonts: &[Vec<u8>],
    ) -> Result<Self, RenderError> {
        let mut config = DocumentConfig {
            // See from_html: eager <img> ops resolve during parsing, so the
            // base URL has to be present in the config, not just post-parse.
            base_url: base_url.map(str::to_string),
            ..DocumentConfig::default()
        };
        if !fonts.is_empty() {
            config.font_ctx = Some(build_font_ctx(fonts));
        }
        let mut doc = HtmlDocument::from_html(html, config).into_inner();
        if let Some(url) = base_url {
            doc.set_base_url(url);
        }
        doc.set_viewport(BlitzViewport::new(
            viewport.width,
            viewport.height,
            viewport.scale as f32,
            effective_color_scheme(),
        ));
        doc.resolve(0.0);
        Ok(Self {
            doc,
            viewport,
            tree_revision: 0,
            next_incarnation: 1,
            incarnations: Default::default(),
            attachments: Default::default(),
        })
    }

    /// Borrow the inner [`BaseDocument`] (read-only).
    pub fn document(&self) -> &BaseDocument {
        &self.doc
    }
    /// The viewport this document was constructed (and laid out) against.
    pub fn viewport(&self) -> Viewport {
        self.viewport
    }

    /// Borrow the inner [`BaseDocument`] mutably. Callers are responsible for
    /// re-running [`BaseDocument::resolve`] after mutation before capturing.
    pub fn document_mut(&mut self) -> &mut BaseDocument {
        self.tree_revision += 1;
        self.incarnations.clear();
        self.attachments.clear();
        &mut self.doc
    }

    /// Invalidates observed targets on every structural edit, even a detach/reinsert.
    pub fn tree_revision(&self) -> u64 {
        self.tree_revision
    }

    /// Slab node IDs can be reused; an incarnation cannot.
    pub fn node_incarnation(&mut self, node: NodeId) -> Option<u64> {
        self.doc.get_node(node)?;
        if let Some(token) = self.incarnations.get(&node) {
            return Some(*token);
        }
        let token = self.next_incarnation;
        self.next_incarnation += 1;
        self.incarnations.insert(node, token);
        Some(token)
    }

    pub fn node_attachment_revision(&self, node: NodeId) -> u64 {
        self.attachments.get(&node).copied().unwrap_or(0)
    }

    fn mark_attachment_subtree(&mut self, node: NodeId) {
        let mut pending = vec![node];
        let mut seen = std::collections::HashSet::new();
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            if let Some(node) = self.doc.get_node(id) {
                pending.extend_from_slice(&node.children);
            }
            self.attachments.insert(id, self.tree_revision);
        }
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

    /// All attributes of `node` as `(name, value)` pairs, or empty.
    pub fn node_attributes(&self, node: NodeId) -> Vec<(String, String)> {
        self.doc
            .get_node(node)
            .and_then(|n| n.attrs())
            .map(|attrs| {
                attrs
                    .iter()
                    .map(|a| (a.name.local.to_string(), a.value.clone()))
                    .collect()
            })
            .unwrap_or_default()
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
        self.tree_revision += 1;
        let id = self.doc.mutate().create_element(html_name(tag), Vec::new());
        self.incarnations.remove(&id);
        self.node_incarnation(id);
        id
    }

    /// Create a new text node (detached).
    pub fn create_text_node(&mut self, text: &str) -> NodeId {
        self.tree_revision += 1;
        let id = self.doc.create_text_node(text);
        self.incarnations.remove(&id);
        self.node_incarnation(id);
        id
    }

    /// Append `child` to `parent`'s children.
    pub fn append_child(&mut self, parent: NodeId, child: NodeId) {
        self.tree_revision += 1;
        self.mark_attachment_subtree(child);
        self.doc.mutate().append_children(parent, &[child]);
    }

    /// Insert `new_node` before `anchor` under the same parent.
    pub fn insert_before(&mut self, anchor: NodeId, new_node: NodeId) {
        self.tree_revision += 1;
        self.mark_attachment_subtree(new_node);
        self.doc.mutate().insert_nodes_before(anchor, &[new_node]);
    }

    /// First child id of `node`, if any.
    pub fn first_child(&self, node: NodeId) -> Option<NodeId> {
        self.doc
            .get_node(node)
            .and_then(|n| n.children.first().copied())
    }

    /// Parent id of `node`, if any.
    pub fn parent_id(&self, node: NodeId) -> Option<NodeId> {
        self.doc.get_node(node).and_then(|n| n.parent)
    }

    /// Set (or replace) an attribute on `node`.
    pub fn set_attribute(&mut self, node: NodeId, name: &str, value: &str) {
        self.doc
            .mutate()
            .set_attribute(node, html_name(name), value);
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
            self.remove_node(child);
        }
        let text_id = self.create_text_node(text);
        self.append_child(node, text_id);
    }

    /// Detach `node` from its parent. Blitz preserves the node and listeners.
    pub fn remove_node(&mut self, node: NodeId) {
        self.tree_revision += 1;
        self.mark_attachment_subtree(node);
        self.doc.mutate().remove_node(node);
    }

    /// The laid-out box of `node` in CSS pixels: `(x, y, width, height)`.
    ///
    /// Valid only after a resolve (construction or `capture_png` both resolve).
    /// Returns zeros for unknown nodes or non-finite layouts.
    pub fn node_layout_rect(&self, node: NodeId) -> (f64, f64, f64, f64) {
        let Some(n) = self.doc.get_node(node) else {
            return (0.0, 0.0, 0.0, 0.0);
        };
        let loc = n.final_layout.location;
        let size = n.final_layout.size;
        let (x, y, w, h) = (
            loc.x as f64,
            loc.y as f64,
            size.width as f64,
            size.height as f64,
        );
        if [x, y, w, h].iter().all(|v| v.is_finite()) {
            (x, y, w, h)
        } else {
            (0.0, 0.0, 0.0, 0.0)
        }
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

/// Build a Parley `FontContext` that keeps system fonts available and registers
/// each supplied webfont (auto-detecting its family from the font's name tables).
/// Empty input returns a context equivalent to the document default.
fn build_font_ctx(fonts: &[Vec<u8>]) -> parley::FontContext {
    use parley::fontique::{Blob, Collection, CollectionOptions, SourceCache};
    use std::sync::Arc;
    let mut ctx = parley::FontContext {
        source_cache: SourceCache::new_shared(),
        collection: Collection::new(CollectionOptions {
            shared: false,
            // Native build: keep system fonts so ordinary text still renders,
            // and layer the @font-face fonts on top.
            system_fonts: !cfg!(target_arch = "wasm32"),
        }),
    };
    for bytes in fonts {
        let decoded = blitz_dom::decode_font_bytes(bytes).into_owned();
        ctx.collection
            .register_fonts(Blob::new(Arc::new(decoded) as _), None);
    }
    ctx
}
