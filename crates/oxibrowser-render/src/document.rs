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

/// Build a `QualName` for an HTML-namespaced element from a runtime tag.
fn html_name(local: &str) -> QualName {
    QualName::new(None, ns!(html), LocalName::from(local))
}

/// A Blitz-backed renderable document.
///
/// Owns a [`BaseDocument`] that has already been parsed, style-resolved and
/// laid out. Not `Send` — must be used from a single thread (see crate docs).
pub struct RenderDocument {
    doc: BaseDocument,
    base_url: String,
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
            base_url: base_url.unwrap_or("about:blank").to_owned(),
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
            base_url: base_url.unwrap_or("about:blank").to_owned(),
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

    /// Flush pending style/layout changes before synchronous DOM geometry reads.
    pub fn flush_layout(&mut self) {
        self.doc.resolve(0.0);
    }

    /// Borrow the inner [`BaseDocument`] mutably. Callers are responsible for
    /// re-running [`BaseDocument::resolve`] after mutation before capturing.
    pub fn document_mut(&mut self) -> &mut BaseDocument {
        self.tree_revision += 1;
        self.incarnations.clear();
        self.attachments.clear();
        &mut self.doc
    }

    /// Injects a canvas element's RGBA8 drawing-buffer readback so blitz's
    /// normal image compositing (`draw_image` → object-fit) paints it. Same
    /// path as a decoded `<img>`: replace `SpecialElementData`, clear the
    /// node's style cache, and flag full damage.
    ///
    /// `document_mut` semantics apply (revision bump + incarnation reset).
    pub fn set_canvas_bitmap(
        &mut self,
        node: NodeId,
        width: u32,
        height: u32,
        rgba: std::sync::Arc<Vec<u8>>,
    ) -> bool {
        use blitz_dom::node::{ImageData, RasterImageData, SpecialElementData};
        use style::selector_parser::RestyleDamage;

        let doc = self.document_mut();
        let Some(node_ref) = doc.get_node_mut(node) else {
            return false;
        };
        let Some(el) = node_ref.element_data_mut() else {
            return false;
        };
        el.special_data = SpecialElementData::Image(Box::new(ImageData::Raster(
            RasterImageData::new(width, height, rgba),
        )));
        node_ref.cache.clear();
        // blitz-dom's private ALL_DAMAGE is the full damage bitset (0b1111111).
        node_ref.insert_damage(RestyleDamage::from_bits_retain(0b0111_1111));
        true
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

    /// Query descendants of a connected or detached element using Stylo.
    pub fn query_selector_all_from(
        &self,
        node: NodeId,
        selector: &str,
    ) -> Result<Vec<NodeId>, String> {
        use style::dom_apis::{MayUseInvalidation, QueryAll, query_selector};
        let selectors = self
            .doc
            .try_parse_selector_list(selector)
            .map_err(|_| "invalid selector".to_owned())?;
        let Some(root) = self.doc.get_node(node) else {
            return Ok(Vec::new());
        };
        let mut results = Default::default();
        query_selector::<&blitz_dom::Node, QueryAll>(
            root,
            &selectors,
            &mut results,
            MayUseInvalidation::No,
        );
        Ok(results.iter().map(|node| node.id).collect())
    }

    pub fn element_matches(&self, node: NodeId, selector: &str) -> Result<bool, String> {
        let selectors = self
            .doc
            .try_parse_selector_list(selector)
            .map_err(|_| "invalid selector".to_owned())?;
        Ok(self.doc.get_node(node).is_some_and(|node| {
            style::dom_apis::element_matches(
                &node,
                &selectors,
                style::context::QuirksMode::NoQuirks,
            )
        }))
    }

    pub fn element_closest(&self, node: NodeId, selector: &str) -> Result<Option<NodeId>, String> {
        let selectors = self
            .doc
            .try_parse_selector_list(selector)
            .map_err(|_| "invalid selector".to_owned())?;
        Ok(self
            .doc
            .get_node(node)
            .and_then(|node| {
                style::dom_apis::element_closest(
                    node,
                    &selectors,
                    style::context::QuirksMode::NoQuirks,
                )
            })
            .map(|node| node.id))
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

    /// Clone DOM structure and attributes, preserving element namespaces.
    /// Listeners and JS properties belong to the original wrapper only.
    pub fn clone_node(&mut self, node: NodeId, deep: bool) -> Option<NodeId> {
        let original = self.doc.get_node(node)?;
        let children = original.children.clone();
        let id = if let Some(element) = original.data.downcast_element() {
            let name = element.name.clone();
            let attrs = element.attrs().to_vec();
            self.tree_revision += 1;
            let id = self.doc.mutate().create_element(name, attrs);
            self.incarnations.remove(&id);
            self.node_incarnation(id);
            id
        } else {
            let text = original.text_data()?;
            let text = text.content.clone();
            self.create_text_node(&text)
        };
        if deep {
            for child in children {
                if let Some(child) = self.clone_node(child, true) {
                    self.append_child(id, child);
                }
            }
        }
        Some(id)
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

    /// Last child id of `node`, if any.
    pub fn last_child(&self, node: NodeId) -> Option<NodeId> {
        self.doc
            .get_node(node)
            .and_then(|n| n.children.last().copied())
    }

    /// All child ids of `node`, in document order.
    pub fn children_ids(&self, node: NodeId) -> Vec<NodeId> {
        self.doc
            .get_node(node)
            .map(|n| n.children.clone())
            .unwrap_or_default()
    }

    /// Next sibling id of `node`, if any.
    pub fn next_sibling(&self, node: NodeId) -> Option<NodeId> {
        let parent = self.doc.get_node(node).and_then(|n| n.parent)?;
        let siblings = self.doc.get_node(parent)?.children.clone();
        let idx = siblings.iter().position(|&c| c == node)?;
        siblings.get(idx + 1).copied()
    }

    /// Previous sibling id of `node`, if any.
    pub fn previous_sibling(&self, node: NodeId) -> Option<NodeId> {
        let parent = self.doc.get_node(node).and_then(|n| n.parent)?;
        let siblings = self.doc.get_node(parent)?.children.clone();
        let idx = siblings.iter().position(|&c| c == node)?;
        if idx == 0 {
            None
        } else {
            siblings.get(idx - 1).copied()
        }
    }

    /// Node kind: "element" | "text" | other.
    pub fn node_kind(&self, node: NodeId) -> Option<&'static str> {
        self.doc.get_node(node).map(|n| {
            if n.is_element() {
                "element"
            } else if n.is_text_node() {
                "text"
            } else {
                "other"
            }
        })
    }

    /// Text content of a text node.
    pub fn text_of(&self, node: NodeId) -> Option<String> {
        self.doc
            .get_node(node)
            .and_then(|n| n.text_data().map(|t| t.content.clone()))
    }

    /// Parent id of `node`, if any.
    pub fn parent_id(&self, node: NodeId) -> Option<NodeId> {
        self.doc.get_node(node).and_then(|n| n.parent)
    }

    /// Set (or replace) an attribute on `node`.
    pub fn set_attribute(&mut self, node: NodeId, name: &str, value: &str) {
        // HTML attributes have no namespace. Using the element's HTML
        // namespace creates a second attribute beside the parsed one, so
        // getAttribute keeps returning the old value and deletion misses it.
        self.doc.mutate().set_attribute(
            node,
            QualName::new(None, Default::default(), LocalName::from(name)),
            value,
        );
    }

    /// Remove an attribute from `node`.
    pub fn remove_attribute(&mut self, node: NodeId, name: &str) {
        self.doc.mutate().clear_attribute(
            node,
            QualName::new(None, Default::default(), LocalName::from(name)),
        );
    }

    /// Set an inline style property on `node` (e.g. `color`, `background-color`).
    pub fn set_inline_style(&mut self, node: NodeId, property: &str, value: &str) {
        self.doc.set_style_property(node, property, value);
    }

    /// Read the actual parsed inline declaration, including priority.
    pub fn inline_style(&self, node: NodeId, property: &str) -> (String, bool) {
        let Some(block) = self
            .doc
            .get_node(node)
            .and_then(|node| node.data.downcast_element())
            .and_then(|element| element.style_attribute.as_ref())
        else {
            return (String::new(), false);
        };
        let Ok(property) = style::properties::PropertyId::parse_unchecked(property, None) else {
            return (String::new(), false);
        };
        let guard = self.doc.guard().read();
        let block = block.read_with(&guard);
        let mut value = String::new();
        let _ = block.property_value_to_css(&property, &mut value);
        (value, block.property_priority(&property).important())
    }

    pub fn remove_inline_style(&mut self, node: NodeId, property: &str) -> String {
        let previous = self.inline_style(node, property).0;
        self.doc.remove_style_property(node, property);
        self.sync_style_attribute(node);
        previous
    }

    pub fn set_inline_style_with_priority(
        &mut self,
        node: NodeId,
        property: &str,
        value: &str,
        important: bool,
    ) {
        use style::properties::{Importance, PropertyId, SourcePropertyDeclaration};
        use style::stylesheets::{CssRuleType, Origin, UrlExtraData};
        let Ok(property_id) = PropertyId::parse_unchecked(property, None) else {
            return;
        };
        if value.is_empty() {
            self.remove_inline_style(node, property);
            return;
        }
        let Ok(url) = self.base_url.parse() else {
            return;
        };
        let url_data = UrlExtraData(style::servo_arc::Arc::new(url));
        let mut declarations = SourcePropertyDeclaration::default();
        if style::properties::declaration_block::parse_one_declaration_into(
            &mut declarations,
            property_id,
            value,
            Origin::Author,
            &url_data,
            None,
            style_traits::ParsingMode::DEFAULT,
            style::context::QuirksMode::NoQuirks,
            CssRuleType::Style,
        )
        .is_err()
        {
            return;
        }
        self.doc.remove_style_property(node, property);
        // Ensure a style block exists even on an element without style=.
        self.doc.set_style_property(node, property, value);
        if let Some(block) = self
            .doc
            .get_node(node)
            .and_then(|node| node.data.downcast_element())
            .and_then(|element| element.style_attribute.as_ref())
        {
            let mut guard = self.doc.guard().write();
            block.write_with(&mut guard).extend(
                declarations.drain(),
                if important {
                    Importance::Important
                } else {
                    Importance::Normal
                },
            );
        }
        self.sync_style_attribute(node);
    }

    fn sync_style_attribute(&mut self, node: NodeId) {
        let mut css = String::new();
        if let Some(block) = self
            .doc
            .get_node(node)
            .and_then(|node| node.data.downcast_element())
            .and_then(|element| element.style_attribute.as_ref())
        {
            let guard = self.doc.guard().read();
            let _ = block.read_with(&guard).to_css(&mut css);
        }
        self.set_attribute(node, "style", &css);
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
