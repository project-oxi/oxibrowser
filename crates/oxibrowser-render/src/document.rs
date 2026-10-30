//! [`RenderDocument`] — the public handle wrapping a Blitz [`BaseDocument`].

use blitz_dom::BaseDocument;
use blitz_dom::DocumentConfig;
use blitz_dom::LocalName;
use blitz_dom::NodeData;
use blitz_dom::QualName;
use blitz_dom::ns;
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
    stylesheets: crate::PreloadedStylesheets,
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
        Self::from_html_with_resources(html, base_url, viewport, &[], &Default::default())
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
        Self::from_html_with_resources(html, base_url, viewport, fonts, &Default::default())
    }

    /// Parse with document-scoped stylesheet bytes fetched through core's
    /// network policy. Link elements retain their original attributes; the
    /// renderer performs no networking and invents no resource/load events.
    pub fn from_html_with_resources(
        html: &str,
        base_url: Option<&str>,
        viewport: Viewport,
        fonts: &[Vec<u8>],
        stylesheets: &crate::PreloadedStylesheets,
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
        let mut doc = crate::resources::parse(html, config, stylesheets);
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
            stylesheets: stylesheets.clone(),
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

    /// Reuse only for a composed view of this same document, never navigation.
    pub fn preloaded_stylesheets(&self) -> &crate::PreloadedStylesheets {
        &self.stylesheets
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Attach authorized stylesheet response text to an existing link/style node.
    ///
    /// The response URL supplies CSS's base URL. This replaces the node's prior
    /// sheet, preserves its DOM attributes, and performs no network fetching for
    /// imports. Call [`Self::flush_layout`] before observing the resulting style.
    /// RenderDocument's mutation methods defer eager link fetching to the caller.
    pub fn attach_stylesheet(
        &mut self,
        node: NodeId,
        response_url: &str,
        css: &str,
    ) -> Result<(), RenderError> {
        use blitz_traits::net::Url;
        use style::stylesheets::{
            AllowImportRules, DocumentStyleSheet, Origin, Stylesheet, UrlExtraData,
        };

        if !matches!(self.tag_name(node).as_deref(), Some("link" | "style")) {
            return Err(RenderError::Render(
                "stylesheet target must be a link or style element".into(),
            ));
        }
        let final_url = Url::parse(response_url).map_err(|error| {
            RenderError::Render(format!("invalid stylesheet response URL: {error}"))
        })?;
        let guard = self.doc.guard().clone();
        let stylesheet = Stylesheet::from_str(
            css,
            UrlExtraData(style::servo_arc::Arc::new(final_url.clone())),
            Origin::Author,
            style::servo_arc::Arc::new(guard.wrap(style::media_queries::MediaList::empty())),
            guard,
            None,
            None,
            style::context::QuirksMode::NoQuirks,
            AllowImportRules::Yes,
        );
        self.doc.add_stylesheet_for_node(
            DocumentStyleSheet(style::servo_arc::Arc::new(stylesheet)),
            node,
        );
        if let Some(href) = self.node_attr(node, "href") {
            let request_url = Url::parse(&self.base_url)
                .and_then(|base| base.join(&href))
                .or_else(|_| Url::parse(&href));
            if let Ok(request_url) = request_url {
                self.stylesheets.insert(crate::PreloadedStylesheet {
                    request_url: request_url.to_string(),
                    response_url: final_url.to_string(),
                    body: css.as_bytes().to_vec(),
                });
            }
        }
        Ok(())
    }

    /// Flush pending style/layout changes before synchronous DOM geometry reads.
    pub fn flush_layout(&mut self) {
        self.doc.resolve(0.0);
    }

    /// Scroll without changing native DOM identities or attachment tokens.
    pub fn scroll_to_node(&mut self, node: NodeId) {
        self.doc.scroll_to_node(node);
    }

    /// Reconcile loaded sheets with DOM order, connection, rel and disabled.
    /// Inactive nodes retain their parsed response for reinsertion; changing
    /// their href explicitly forgets that response until new bytes arrive.
    pub fn sync_stylesheets(&mut self) {
        self.doc.sync_stylesheets();
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
    /// Pixel updates preserve DOM identity and native input target tokens.
    pub fn set_canvas_bitmap(
        &mut self,
        node: NodeId,
        width: u32,
        height: u32,
        rgba: std::sync::Arc<Vec<u8>>,
    ) -> bool {
        use blitz_dom::node::{ImageData, RasterImageData, SpecialElementData};
        use style::selector_parser::RestyleDamage;

        let doc = &mut self.doc;
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

    /// Remove a previously injected bitmap without changing DOM identity.
    /// Used when a composed frame document is revoked or replaced.
    pub fn clear_canvas_bitmap(&mut self, node: NodeId) -> bool {
        use blitz_dom::node::SpecialElementData;
        use style::selector_parser::RestyleDamage;
        let Some(node) = self.doc.get_node_mut(node) else {
            return false;
        };
        let Some(element) = node.element_data_mut() else {
            return false;
        };
        if !matches!(element.special_data, SpecialElementData::Image(_)) {
            return false;
        }
        element.special_data = SpecialElementData::None;
        node.cache.clear();
        node.insert_damage(RestyleDamage::from_bits_retain(0b0111_1111));
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

    /// Create a detached comment anchor. Blitz preserves its position, while
    /// comment data remains owned by the caller's DOM wrapper.
    pub fn create_comment_node(&mut self, _text: &str) -> NodeId {
        self.tree_revision += 1;
        let id = self.doc.mutate().create_comment_node();
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
        // Blitz removes the old parent's occurrences after insertion, which
        // drops a same-parent move. Detach first, preserving the node itself.
        if self.parent_id(child).is_some() {
            self.remove_node(child);
        }
        self.tree_revision += 1;
        self.mark_attachment_subtree(child);
        self.with_deferred_link_loading(child, |doc| {
            doc.mutate().append_children(parent, &[child]);
        });
    }

    /// Insert `new_node` before `anchor` under the same parent.
    pub fn insert_before(&mut self, anchor: NodeId, new_node: NodeId) {
        if anchor == new_node {
            return;
        }
        if self.parent_id(new_node).is_some() {
            self.remove_node(new_node);
        }
        self.tree_revision += 1;
        self.mark_attachment_subtree(new_node);
        self.with_deferred_link_loading(new_node, |doc| {
            doc.mutate().insert_nodes_before(anchor, &[new_node]);
        });
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
            } else if matches!(n.data, NodeData::Comment) {
                "comment"
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
        if name.eq_ignore_ascii_case("href")
            && self.tag_name(node).as_deref() == Some("link")
            && self.node_attr(node, name).as_deref() != Some(value)
        {
            self.doc.remove_stylesheet_for_node(node);
        }
        let operation = |doc: &mut BaseDocument| {
            doc.mutate().set_attribute(
                node,
                QualName::new(None, Default::default(), LocalName::from(name)),
                value,
            );
        };
        if name.eq_ignore_ascii_case("href") && self.tag_name(node).as_deref() == Some("link") {
            self.with_deferred_link_loading(node, operation);
        } else {
            operation(&mut self.doc);
        }
        self.sync_stylesheets();
    }

    // The core resource owner supplies real CSS bytes or a load error. Blitz's
    // eager loader has no failure callback and would leave unknown head links
    // permanently critical. Mask only rel during the synchronous mutation, then
    // restore it directly before any style/layout observation can occur.
    fn with_deferred_link_loading(
        &mut self,
        root: NodeId,
        operation: impl FnOnce(&mut BaseDocument),
    ) {
        let name = QualName::new(None, Default::default(), LocalName::from("rel"));
        let mut stack = vec![root];
        let mut restored = Vec::new();
        while let Some(node) = stack.pop() {
            stack.extend(self.children_ids(node));
            if self.tag_name(node).as_deref() == Some("link")
                && let Some(element) = self
                    .doc
                    .get_node_mut(node)
                    .and_then(|n| n.element_data_mut())
                && let Some(attribute) = element.attrs.remove(&name)
            {
                restored.push((node, attribute.value));
            }
        }
        operation(&mut self.doc);
        for (node, value) in restored {
            if let Some(element) = self
                .doc
                .get_node_mut(node)
                .and_then(|n| n.element_data_mut())
            {
                element.attrs.set(name.clone(), &value);
            }
        }
        self.sync_stylesheets();
    }

    /// Remove an attribute from `node`.
    pub fn remove_attribute(&mut self, node: NodeId, name: &str) {
        // Blitz's href removal assumes a stylesheet was attached. A link
        // awaiting core's asynchronous fetch has no stylesheet yet.
        if name.eq_ignore_ascii_case("href")
            && self.tag_name(node).as_deref() == Some("link")
            && self
                .doc
                .get_node(node)
                .and_then(|n| n.element_data())
                .is_some_and(|e| {
                    !matches!(
                        e.special_data,
                        blitz_dom::node::SpecialElementData::Stylesheet(_)
                    )
                })
        {
            self.set_attribute(node, name, "");
            if let Some(element) = self
                .doc
                .get_node_mut(node)
                .and_then(|n| n.element_data_mut())
            {
                element.attrs.remove(&QualName::new(
                    None,
                    Default::default(),
                    LocalName::from(name),
                ));
            }
            return;
        }
        self.doc.mutate().clear_attribute(
            node,
            QualName::new(None, Default::default(), LocalName::from(name)),
        );
        self.sync_stylesheets();
    }

    /// Set an inline style property on `node` (e.g. `color`, `background-color`).
    pub fn set_inline_style(&mut self, node: NodeId, property: &str, value: &str) {
        self.set_inline_style_with_priority(node, property, value, false);
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

    /// Read Stylo's cascaded, inherited and resolved value for a CSS property.
    ///
    /// Property names use CSS syntax (`font-size`, including custom properties).
    /// Call [`Self::flush_layout`] after mutations before reading. For elements
    /// with a layout box, width/height expose the used size in CSS pixels,
    /// respecting box-sizing; other values use Stylo's CSS serialization.
    /// Unknown properties and nodes without computed styles return an empty string.
    pub fn resolved_style(&self, node: NodeId, property: &str) -> String {
        use style::properties::{LonghandId, PropertyDeclarationId, PropertyId};
        use style::values::resolved;

        let Some(node) = self.doc.get_node(node) else {
            return String::new();
        };
        let Some(styles) = node.primary_styles() else {
            return String::new();
        };
        let Ok(property) = PropertyId::parse_enabled_for_all_content(property) else {
            return String::new();
        };
        let display = styles.clone_display();
        if matches!(
            property.longhand_id(),
            Some(LonghandId::Width | LonghandId::Height)
        ) && !display.is_none()
            && !display.is_contents()
            && !display.is_inline_flow()
            && node.layout_parent.get().is_some()
            && self.node_has_layout_box(node.id)
        {
            let layout = &node.final_layout;
            let border_box = styles
                .computed_value_to_string(PropertyDeclarationId::Longhand(LonghandId::BoxSizing))
                == "border-box";
            let size = match (property.longhand_id(), border_box) {
                (Some(LonghandId::Width), true) => layout.size.width,
                (Some(LonghandId::Height), true) => layout.size.height,
                (Some(LonghandId::Width), false) => layout.content_box_width(),
                _ => layout.content_box_height(),
            };
            if size.is_finite() {
                return format!("{}px", size.max(0.0));
            }
        }
        match property.as_shorthand() {
            Err(id) => styles.computed_value_to_string(id),
            Ok(shorthand) => {
                let mut context = resolved::Context {
                    style: &styles,
                    for_property: property.clone(),
                    current_longhand: None,
                };
                let declarations: Vec<_> = shorthand
                    .longhands()
                    .map(|id| {
                        context.current_longhand = Some(id);
                        styles.computed_or_resolved_declaration(id, Some(&mut context))
                    })
                    .collect();
                let declarations: Vec<_> = declarations.iter().collect();
                let mut value = String::new();
                let _ = shorthand.longhands_to_css(&declarations, &mut value);
                value
            }
        }
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

    /// Update a text node's data, or replace an element's children with text.
    pub fn set_text(&mut self, node: NodeId, text: &str) {
        if self
            .doc
            .get_node(node)
            .is_some_and(|node| node.text_data().is_some())
        {
            self.doc.mutate().set_node_text(node, text);
            return;
        }
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
        self.sync_stylesheets();
    }

    /// The viewport-relative border box in CSS pixels: `(x, y, width, height)`.
    ///
    /// Includes layout ancestors, their scroll offsets, viewport scrolling and
    /// the same 2D transforms used by capture. The returned box encloses a
    /// transformed rectangle. Valid only after a resolve. Nodes without a
    /// rendered box and non-finite layouts return zeros.
    pub fn node_layout_rect(&self, node: NodeId) -> (f64, f64, f64, f64) {
        use peniko::kurbo::Rect;

        let Some(transform) = self.node_viewport_transform(node) else {
            return (0.0, 0.0, 0.0, 0.0);
        };
        let size = self.doc.get_node(node).unwrap().final_layout.size;
        let rect = transform.transform_rect_bbox(Rect::new(
            0.0,
            0.0,
            size.width as f64,
            size.height as f64,
        ));
        let result = (rect.x0, rect.y0, rect.width(), rect.height());
        if [result.0, result.1, result.2, result.3]
            .iter()
            .all(|v| v.is_finite())
        {
            result
        } else {
            (0.0, 0.0, 0.0, 0.0)
        }
    }

    /// Actual content, padding, border and margin quads, followed by the local
    /// border-box width/height. Quads contain TL, TR, BR, BL viewport CSS points.
    /// Two-dimensional rotations preserve their corner coordinates rather than
    /// becoming axis-aligned boxes. Call flush_layout after mutations.
    /// Returns None for absent boxes, non-finite geometry or unsupported 3D
    /// transforms/perspective on the element or its layout ancestors.
    #[allow(clippy::type_complexity)]
    pub fn node_box_quads(
        &self,
        node: NodeId,
    ) -> Option<([f64; 8], [f64; 8], [f64; 8], [f64; 8], f64, f64)> {
        use peniko::kurbo::Point;

        let transform = self.node_viewport_transform(node)?;
        let layout = &self.doc.get_node(node)?.final_layout;
        let width = layout.size.width as f64;
        let height = layout.size.height as f64;
        let quad = |left: f64, top: f64, right: f64, bottom: f64| {
            let tl = transform * Point::new(left, top);
            let tr = transform * Point::new(right, top);
            let br = transform * Point::new(right, bottom);
            let bl = transform * Point::new(left, bottom);
            [tl.x, tl.y, tr.x, tr.y, br.x, br.y, bl.x, bl.y]
        };
        let border = quad(0.0, 0.0, width, height);
        let padding_left = layout.border.left as f64;
        let padding_top = layout.border.top as f64;
        let padding_right = width - layout.border.right as f64;
        let padding_bottom = height - layout.border.bottom as f64;
        let padding = quad(padding_left, padding_top, padding_right, padding_bottom);
        let content_left = padding_left + layout.padding.left as f64;
        let content_top = padding_top + layout.padding.top as f64;
        let content = quad(
            content_left,
            content_top,
            (padding_right - layout.padding.right as f64).max(content_left),
            (padding_bottom - layout.padding.bottom as f64).max(content_top),
        );
        let margin = quad(
            -(layout.margin.left as f64),
            -(layout.margin.top as f64),
            width + layout.margin.right as f64,
            height + layout.margin.bottom as f64,
        );
        if [content, padding, border, margin]
            .iter()
            .flatten()
            .all(|v| v.is_finite())
            && width.is_finite()
            && height.is_finite()
        {
            Some((content, padding, border, margin, width, height))
        } else {
            None
        }
    }

    /// Map local CSS pixels to viewport CSS pixels using capture's transform.
    fn node_viewport_transform(&self, node: NodeId) -> Option<peniko::kurbo::Affine> {
        use peniko::kurbo::Affine;
        use style::values::generics::{
            box_::Perspective,
            transform::{Rotate, Scale, ToMatrix, Translate},
        };

        if !self.node_has_layout_box(node) {
            return None;
        }
        let scale = self.doc.viewport().scale_f64();
        if !scale.is_finite() || scale <= 0.0 {
            return None;
        }
        let mut ancestors = Vec::new();
        let mut current = Some(node);
        while let Some(id) = current {
            let ancestor = self.doc.get_node(id)?;
            if let Some(styles) = ancestor.primary_styles() {
                let box_style = styles.get_box();
                // Blitz's painter currently discards 3D components. Do not
                // expose the remaining flat coordinates as a real box model.
                if box_style.transform.0.iter().any(ToMatrix::is_3d)
                    || matches!(box_style.rotate, Rotate::Rotate3D(..))
                    || matches!(&box_style.translate, Translate::Translate(_, _, z) if z.px() != 0.0)
                    || matches!(box_style.scale, Scale::Scale(_, _, z) if z != 1.0)
                    || !matches!(box_style.perspective, Perspective::None)
                {
                    return None;
                }
            }
            ancestors.push(id);
            if id == self.doc.root_element().id {
                break;
            }
            current = ancestor.layout_parent.get();
            current?;
        }
        let scroll = self.doc.viewport_scroll();
        let mut transform = Affine::translate((-scroll.x * scale, -scroll.y * scale));
        for id in ancestors.into_iter().rev() {
            let ancestor = self.doc.get_node(id)?;
            let location = ancestor.final_layout.location;
            transform = transform
                * Affine::translate((location.x as f64 * scale, location.y as f64 * scale))
                * ancestor.transform.unwrap_or_default();
            if id != node {
                transform = transform.then_translate(
                    (
                        -ancestor.scroll_offset.x * scale,
                        -ancestor.scroll_offset.y * scale,
                    )
                        .into(),
                );
            }
        }
        Some(Affine::scale(1.0 / scale) * transform * Affine::scale(scale))
    }

    fn node_has_layout_box(&self, node: NodeId) -> bool {
        let mut current = Some(node);
        while let Some(id) = current {
            let Some(ancestor) = self.doc.get_node(id) else {
                return false;
            };
            if id == self.doc.root_node().id {
                return true;
            }
            let Some(styles) = ancestor.primary_styles() else {
                return false;
            };
            let display = styles.clone_display();
            if display.is_none() || (id == node && display.is_contents()) {
                return false;
            }
            current = ancestor.parent;
        }
        false
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
