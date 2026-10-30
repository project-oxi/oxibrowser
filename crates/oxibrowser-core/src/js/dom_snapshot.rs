//! DOM snapshot for JS↔DOM bridge.
//!
//! Provides a `Send + Sync + Serialize` representation of the DOM tree
//! that can be passed between the main thread and the JS thread via channels.
//!
//! Architecture:
//! ```text
//! Frame's Document → DomSnapshot::from_frame() → JsCommand::SetDom → JS thread
//!                                                              ↓
//! JS: document.querySelector('a') → DomSnapshot::query_selector() → result
//! ```

use crate::css::ComputedStyle;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Mutex;

// ── Shadow-DOM composition registry ─────────────────────────────────────────
//
// `attachShadow` (native, in `runtime.rs`) records the render-doc node ids
// appended to each shadow root here. `compose_shadow_trees` (called from
// `from_render_document`) reads them and flattens the shadow trees into the
// snapshot — distributing light-DOM children into `<slot>` positions — so every
// DomSnapshot-backed read (CDP DOM.*, box models, extract, LayoutEngine) sees
// the composed tree. (Screenshot rasterization via `capture_png` is the one
// Blitz-gated path that does NOT reflect this.)
thread_local! {
    /// host node id → the render-doc node ids appended to its shadow root.
    pub(crate) static SHADOW_ROOTS: RefCell<HashMap<u32, ShadowHost>> =
        RefCell::new(HashMap::new());

    /// `<slot>` node id → the light-DOM child node ids distributed into it,
    /// in document order. Populated by [`distribute_slots`] during the compose
    /// pass. Read by the JS `slot.assignedNodes()`/`assignedElements()` APIs.
    /// Cleared at the start of every compose so it reflects the latest tree.
    pub(crate) static SLOT_ASSIGNMENTS: RefCell<HashMap<u32, Vec<u32>>> =
        RefCell::new(HashMap::new());

    /// light-DOM child node id → the `<slot>` node id it was distributed into.
    /// Only records assignments into **open** shadow trees (a slot in a closed
    /// root yields `null` for `node.assignedSlot`, per the HTML spec). Drives
    /// the JS `node.assignedSlot` getter.
    pub(crate) static ASSIGNED_SLOT: RefCell<HashMap<u32, u32>> =
        RefCell::new(HashMap::new());
}

/// `attachShadow({ mode })` — only `'open'` vs `'closed'` matter here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ShadowMode {
    #[default]
    Open,
    Closed,
}

/// Recorded shadow root for one host element.
#[derive(Debug, Default, Clone)]
pub(crate) struct ShadowHost {
    /// Render-doc node ids appended to the shadow root, in append order.
    pub child_ids: Vec<u32>,
    /// `open` (default) or `closed`. Closed roots still render (Chrome paints
    /// closed shadow content) but are hidden from `element.shadowRoot` and
    /// `node.assignedSlot`.
    pub mode: ShadowMode,
}

/// Register (or reset) a shadow root for `host_id` with the given `mode`.
pub(crate) fn register_shadow_host(host_id: u32, mode: ShadowMode) {
    SHADOW_ROOTS.with(|m| {
        m.borrow_mut().insert(
            host_id,
            ShadowHost {
                mode,
                ..Default::default()
            },
        );
    });
}

/// Record that `child_id` was appended to `host_id`'s shadow root.
pub(crate) fn push_shadow_child(host_id: u32, child_id: u32) {
    SHADOW_ROOTS.with(|m| {
        if let Some(host) = m.borrow_mut().get_mut(&host_id) {
            host.child_ids.push(child_id);
        }
    });
}

/// Drop all shadow roots (called on navigation / document rebuild).
pub fn clear_shadow_roots() {
    SHADOW_ROOTS.with(|m| m.borrow_mut().clear());
    SLOT_ASSIGNMENTS.with(|m| m.borrow_mut().clear());
    ASSIGNED_SLOT.with(|m| m.borrow_mut().clear());
}

/// Whether any shadow root is currently registered.
///
/// Used by the screenshot path to decide whether the compose-then-feed
/// round-trip (serialize the flattened tree → reparse → rasterize) is needed;
/// the no-shadow fast path rasterizes the live document directly.
pub fn has_shadow_roots() -> bool {
    SHADOW_ROOTS.with(|m| !m.borrow().is_empty())
}

/// Light-DOM child ids distributed into `slot_id` during the last compose.
/// Returns an empty vec when the slot has no assignment (callers should then
/// fall back to the slot's own shadow-DOM children, per the slot algorithm).
pub fn slot_assigned_nodes(slot_id: u32) -> Vec<u32> {
    SLOT_ASSIGNMENTS.with(|m| m.borrow().get(&slot_id).cloned().unwrap_or_default())
}

/// The `<slot>` node id `child_id` was distributed into, if any (and only when
/// that slot lives in an **open** shadow tree). `None` otherwise — the basis
/// for `node.assignedSlot` returning `null`.
pub fn assigned_slot_of(child_id: u32) -> Option<u32> {
    ASSIGNED_SLOT.with(|m| m.borrow().get(&child_id).copied())
}

/// DOM 변경 사항
///
/// Records mutations applied to the DOM so they can be replayed,
/// inspected, or transmitted over the CDP protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DomMutation {
    /// Set an attribute on a node.
    SetAttribute {
        node_id: u32,
        name: String,
        value: String,
    },
    /// Set the text content of a node.
    SetTextContent { node_id: u32, text: String },
    /// Simulate a click on an element.
    ClickElement { node_id: u32 },
    /// Input text into a form element.
    InputElement { node_id: u32, value: String },
    /// Create a new element node.
    CreateElement { node_id: u32, tag: String },
    /// Create a new text node.
    CreateTextNode { node_id: u32, text: String },
    /// Append a child node to a parent.
    AppendChild { parent_id: u32, child_id: u32 },
    /// Remove a child node from its parent.
    RemoveChild { parent_id: u32, child_id: u32 },
    /// Set innerHTML of an element (parse + replace children).
    SetInnerHtml { node_id: u32, html: String },
    /// Real navigation triggered from JS (`location.href`/`assign`/`replace`).
    /// Handled asynchronously by `Session::evaluate_js_with_await` (network I/O).
    Navigate { url: String },
    /// Page reload triggered from JS (`location.reload`).
    Reload,
    /// POST navigation triggered from JS (`form.submit()` / submit-control
    /// default action). Handled by `Session::post` like `Navigate`.
    NavigatePost {
        url: String,
        body: String,
        content_type: String,
    },
    /// Page-initiated new-tab (popup) request: `window.open(...)` or the
    /// default action of a `target="_blank"` link click. Handled by
    /// `Session::reconcile_native_mutations`, which emits
    /// `CoreEvent::TabSpawned`; the attached observer (upper layer) creates
    /// the actual session.
    OpenTab { url: String, target: String },
}

/// Serializable DOM node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomNode {
    pub id: u32,
    pub tag: String,
    pub attributes: HashMap<String, String>,
    pub text_content: String,
    pub children: Vec<u32>,
    pub parent: Option<u32>,
    /// 1 = Element, 3 = Text, 8 = Comment, 9 = Document.
    pub node_type: u8,
}

/// Kind of sub-resource referenced by the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    Script,
    Stylesheet,
    Image,
    Iframe,
}

/// A sub-resource URL extracted from the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceUrl {
    pub url: String,
    pub kind: ResourceKind,
}

/// A discovered `<iframe>` element and its content source (W3a: srcdoc/about:blank).
#[derive(Debug, Clone, Default)]
pub struct IframeElement {
    /// The `src` attribute, if present.
    pub src: Option<String>,
    /// The `srcdoc` attribute (inline iframe content), if present.
    pub srcdoc: Option<String>,
}

/// Whether a `<script>` is a classic or module script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptKind {
    Classic,
    Module,
}

/// When a `<script>` executes relative to document parsing.
///
/// Blocking classic scripts run before the ordered module/defer queue. The
/// DOM is already built in both cases. Async timing is recorded but still
/// approximated with document order among the blocking scripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecuteTiming {
    Blocking,
    Defer,
    Async,
}

/// An executable `<script>` extracted from the document, in document order.
///
/// `source` holds the inline text for inline scripts and is left empty for
/// external scripts — the navigation path fills it after fetching the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptSource {
    pub source: String,
    pub src_url: Option<String>,
    pub kind: ScriptKind,
    pub execute: ExecuteTiming,
}

/// Per-node `ComputedStyle` cache, keyed by `node_id` and invalidated by snapshot revision.
///
/// Stays private — only `DomSnapshot::compute_style_cached` reads or writes it.
#[derive(Debug, Clone, Default)]
struct StyleCache {
    /// Revision this cache was populated against. If `snapshot.revision` differs,
    /// the cache is stale and must be rebuilt from scratch.
    revision: u64,
    styles: HashMap<u32, ComputedStyle>,
}

/// DOM tree snapshot (Send + Serialize).
#[derive(Debug, Serialize, Deserialize)]
pub struct DomSnapshot {
    pub url: String,
    pub title: String,
    pub nodes: HashMap<u32, DomNode>,
    pub root_id: u32,
    pub body_id: Option<u32>,
    pub head_id: Option<u32>,
    /// Monotonic counter. Bumped on any mutation that affects computed styles
    /// OR structural shape (so the id/class/tag indices know to rebuild).
    /// `compute_style_cached` keys its cache against this; index-using methods
    /// lazily rebuild when `revision != index_revision`.
    #[serde(default)]
    pub revision: u64,
    /// Revision the id/class/tag indices were last rebuilt against.
    /// On any read-through, if `self.index_revision != self.revision`, indices
    /// are regenerated from `self.nodes` before use.
    #[serde(default)]
    pub index_revision: u64,
    /// `id` attribute → first node_id (HTML ids SHOULD be unique).
    #[serde(default)]
    pub id_index: HashMap<String, u32>,
    /// Class name → node_ids, in document (DFS pre-order) order.
    #[serde(default)]
    pub class_index: HashMap<String, Vec<u32>>,
    /// Tag name (already lowercased) → node_ids, in document order.
    #[serde(default)]
    pub tag_index: HashMap<String, Vec<u32>>,
    /// Lazily-populated per-node `ComputedStyle` cache. `Mutex` (not `RefCell`)
    /// so the snapshot stays `Send + Sync` — `Arc<RwLock<Option<DomSnapshot>>>`
    /// in runtime.rs requires `Sync`. Never serialized.
    #[serde(skip, default)]
    style_cache: Mutex<Option<StyleCache>>,
}

// Manual `Clone`: `std::sync::Mutex` doesn't derive Clone (cloning the inner
// while another thread holds the lock is a soundness hole). For our use case
// the cache is transient — every clone starts with a fresh empty cache.
impl Clone for DomSnapshot {
    fn clone(&self) -> Self {
        Self {
            url: self.url.clone(),
            title: self.title.clone(),
            nodes: self.nodes.clone(),
            root_id: self.root_id,
            body_id: self.body_id,
            head_id: self.head_id,
            revision: self.revision,
            index_revision: self.index_revision,
            id_index: self.id_index.clone(),
            class_index: self.class_index.clone(),
            tag_index: self.tag_index.clone(),
            style_cache: Mutex::new(None),
        }
    }
}

impl DomSnapshot {
    /// Create an empty snapshot (no document loaded).
    pub fn empty() -> Self {
        Self {
            url: String::new(),
            title: String::new(),
            nodes: HashMap::new(),
            root_id: 0,
            body_id: None,
            head_id: None,
            revision: 0,
            index_revision: 0,
            id_index: HashMap::new(),
            class_index: HashMap::new(),
            tag_index: HashMap::new(),
            style_cache: Mutex::new(None),
        }
    }

    /// Return the snapshot stored on the [`Frame`].
    ///
    /// The Frame owns its [`DomSnapshot`] (built from the parsed document), so
    /// this is a cheap clone. Historically this walked a separate webapi DOM
    /// tree; that DOM has been retired and the Frame's snapshot is the source.
    pub fn from_frame(frame: &crate::frame::Frame) -> Self {
        frame.document().clone()
    }

    /// Build a snapshot from the live [`RenderDocument`] (Blitz `BaseDocument`).
    ///
    /// This is the post-unification DOM source: the `RenderDocument` on the JS
    /// thread is the single source of truth that JS mutates directly, so a
    /// snapshot derived here reflects every JS-driven change (no stale
    /// navigate-time copy). Used by the CDP DOM domain, OXI extensions,
    /// `extract`, and the legacy snapshot-path JS bindings.
    pub fn from_render_document(
        rd: &oxibrowser_render::RenderDocument,
        url: &str,
        title: &str,
    ) -> Self {
        Self::build_from_render(rd, url, title, true)
    }

    /// Like [`Self::from_render_document`] but without password-value masking.
    ///
    /// INTERNAL ONLY: for form-activation serialization, where the submitted
    /// body must carry the live (real) field values. The resulting snapshot
    /// must never be exposed to JS or observation surfaces.
    pub fn from_render_document_unmasked(
        rd: &oxibrowser_render::RenderDocument,
        url: &str,
        title: &str,
    ) -> Self {
        Self::build_from_render(rd, url, title, false)
    }

    fn build_from_render(
        rd: &oxibrowser_render::RenderDocument,
        url: &str,
        title: &str,
        mask_passwords: bool,
    ) -> Self {
        use oxibrowser_render::BaseDocument;
        let doc: &BaseDocument = rd.document();
        let mut nodes: HashMap<u32, DomNode> = HashMap::new();
        let mut order: Vec<u32> = Vec::new();
        let mut body_id = None;
        let mut head_id = None;

        // The document root is <html>'s parent (the Document node); fall back
        // to <html> itself if Blitz did not synthesize a Document wrapper.
        let html = doc.root_element();
        let root = html.parent.unwrap_or(html.id);
        collect_from_render(
            root,
            doc,
            None,
            &mut nodes,
            &mut order,
            &mut body_id,
            &mut head_id,
        );
        // Flatten shadow trees into the snapshot: merge each host's shadow
        // subtree and distribute its light-DOM children into <slot> positions.
        compose_shadow_trees(&mut nodes, &mut order, doc);
        // Element text_content = concatenation of descendant text (mirrors
        // `collect_text_content` in the retired `from_frame` path).
        fill_element_text(&mut nodes, root as u32);
        // Security: password input values must not survive into any snapshot
        // reader (OXI.getInteractiveElements, HTML serialization, extract).
        // Fill uses selectors + explicit values and never reads the snapshot,
        // so masking here is observation-only.
        if mask_passwords {
            mask_password_inputs(&mut nodes);
        }

        let (id_index, class_index, tag_index) = build_indices(&nodes, &order);
        Self {
            url: url.to_string(),
            title: title.to_string(),
            nodes,
            root_id: root as u32,
            body_id,
            head_id,
            revision: 0,
            index_revision: 0,
            id_index,
            class_index,
            tag_index,
            style_cache: Mutex::new(None),
        }
    }

    /// Serialize the composed (shadow-flattened) tree to an HTML document.
    ///
    /// Round-trips structure, text, and the element attributes present in the
    /// snapshot (`class`, `id`, inline `style=`, etc.). Used to feed the
    /// flattened tree back into a fresh [`RenderDocument`] for screenshot
    /// rasterization: Blitz's flat `BaseDocument` has no shadow/host/slot
    /// concept, so the live document's shadow content is invisible to
    /// `capture_png`. Reparsing this serialized tree renders it.
    ///
    /// **Lossy by design** (accepted when option 2 was chosen over forking
    /// Blitz): CSSOM inline styles set via `element.style.x = …`, event
    /// listeners, and computed styles from stylesheets are not in the snapshot
    /// and are dropped. Structural + `style=`-attribute fidelity is preserved.
    pub fn to_html(&self) -> String {
        let mut out = String::new();
        out.push_str("<!DOCTYPE html>\n");
        // `root_id` is the Document node (type 9) when Blitz synthesized one,
        // else `<html>` itself. Serialize the document's element children.
        let is_doc = self
            .nodes
            .get(&self.root_id)
            .map(|n| n.node_type == 9)
            .unwrap_or(false);
        let top: Vec<u32> = if is_doc {
            self.nodes
                .get(&self.root_id)
                .map(|n| n.children.clone())
                .unwrap_or_default()
        } else {
            vec![self.root_id]
        };
        for id in top {
            serialize_node(&self.nodes, id, &mut out);
        }
        out
    }

    /// Bump the snapshot revision, invalidating the style cache AND marking
    /// the id/class/tag indices stale.
    ///
    /// Every in-place DOM mutation closure in `runtime.rs` MUST call this
    /// after mutating `snap.nodes`. Otherwise `compute_style_cached` returns
    /// cached styles for nodes whose attributes changed, and the indices
    /// return deleted/renamed ids/classes/tags.
    pub fn bump_revision(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        // Drop the style cache; the next `compute_style_cached` rebuilds it.
        // Single-threaded access under the snapshot's RwLock; the Mutex exists
        // only for `Sync` and Clone compatibility — poisoning is impossible.
        *self.style_cache.lock().expect("style cache mutex poisoned") = None;
        // Indices become stale but stay allocated; the next index-using read
        // detects `index_revision != revision` and rebuilds them.
    }

    /// Compute (and cache) the resolved style for a node.
    ///
    /// Cache lookup is keyed by `node_id` and validated against the snapshot's
    /// `revision`. A revision mismatch drops the entire cache and the next
    /// call rebuilds it (cheaper than per-entry versioning because mutations
    pub fn compute_style_cached(&self, node_id: u32) -> Option<ComputedStyle> {
        let mut cache_slot = self.style_cache.lock().expect("style cache mutex poisoned");
        let cache: &mut StyleCache = match cache_slot.as_mut() {
            Some(c) if c.revision == self.revision => c,
            _ => {
                *cache_slot = Some(StyleCache {
                    revision: self.revision,
                    styles: HashMap::new(),
                });
                cache_slot
                    .as_mut()
                    .expect("style cache was just initialized above")
            }
        };

        if let Some(hit) = cache.styles.get(&node_id) {
            return Some(hit.clone());
        }

        let computed = crate::css::LayoutEngine::compute_style(self, node_id)?;
        cache.styles.insert(node_id, computed.clone());
        Some(computed)
    }

    /// Fast path for `query_selector` on simple `#id`, `.class`, bare `tag`.
    ///
    /// Returns the first matching node_id in document order ONLY if the
    /// pre-built indices are still fresh (`index_revision == revision`).
    /// After any in-place mutation (which bumps `revision`), indices become
    /// stale and we deliberately return `None` so the caller falls back to
    /// the tree walk — safer than risking stale results, and the perf win
    /// between `from_frame` and the first mutation is the meaningful window.
    fn simple_selector_first(&self, selector: &str) -> Option<u32> {
        // Indices only trustworthy between `from_frame` and the first mutation.
        if self.index_revision != self.revision {
            return None;
        }
        if let Some(id) = selector.strip_prefix('#') {
            if id.is_empty() {
                return None;
            }
            return self.id_index.get(id).copied();
        }
        if let Some(class) = selector.strip_prefix('.') {
            if class.is_empty() {
                return None;
            }
            return self.class_index.get(class).and_then(|v| v.first().copied());
        }
        if !selector.is_empty()
            && !selector.bytes().any(|b| {
                matches!(
                    b,
                    b' ' | b'\t'
                        | b'\n'
                        | b'.'
                        | b'#'
                        | b'['
                        | b','
                        | b'>'
                        | b'+'
                        | b'~'
                        | b':'
                        | b'*'
                )
            })
        {
            return self
                .tag_index
                .get(&selector.to_lowercase())
                .and_then(|v| v.first().copied());
        }
        None
    }

    /// Fast path for `query_selector_all` on simple `#id`, `.class`, bare `tag`.
    ///
    /// Same freshness rule as `simple_selector_first`: returns `None` once any
    /// in-place mutation has invalidated the indices, deferring to the tree
    /// walk.
    fn simple_selector_all(&self, selector: &str) -> Option<Vec<u32>> {
        if self.index_revision != self.revision {
            return None;
        }
        if let Some(id) = selector.strip_prefix('#') {
            if id.is_empty() {
                return None;
            }
            return self.id_index.get(id).map(|&id| vec![id]);
        }
        if let Some(class) = selector.strip_prefix('.') {
            if class.is_empty() {
                return None;
            }
            return self.class_index.get(class).cloned();
        }
        if !selector.is_empty()
            && !selector.bytes().any(|b| {
                matches!(
                    b,
                    b' ' | b'\t'
                        | b'\n'
                        | b'.'
                        | b'#'
                        | b'['
                        | b','
                        | b'>'
                        | b'+'
                        | b'~'
                        | b':'
                        | b'*'
                )
            })
        {
            return self.tag_index.get(&selector.to_lowercase()).cloned();
        }
        None
    }

    /// Query the first matching node by CSS selector.
    ///
    /// Supports:
    /// - Tag name: `"a"`, `"div"`, `"p"`
    /// - Class: `".classname"`
    /// - ID: `"#id"`
    /// - Tag + class: `"div.class"`
    /// - Tag + ID: `"div#id"`
    /// - Attribute: `"[href]"`, `"a[href]"`
    pub fn query_selector(&self, selector: &str) -> Option<u32> {
        // Index fast-path for simple `#id`, `.class`, bare `tag` selectors
        // when the indices haven't been invalidated by a mutation.
        if let Some(first) = self.simple_selector_first(selector) {
            return Some(first);
        }

        // Walk nodes in tree order (DFS from root)
        let mut stack = vec![self.root_id];
        while let Some(id) = stack.pop() {
            if let Some(node) = self.nodes.get(&id) {
                if self.node_matches_selector(node, selector) {
                    return Some(id);
                }
                // Push children in reverse order so first child is processed first
                for &child in node.children.iter().rev() {
                    stack.push(child);
                }
            }
        }
        None
    }

    /// Query all matching nodes by CSS selector.
    ///
    /// Same fast-path strategy as `query_selector`: simple selectors consult
    /// the pre-built indices; everything else walks the tree.
    pub fn query_selector_all(&self, selector: &str) -> Vec<u32> {
        if let Some(matches) = self.simple_selector_all(selector) {
            return matches;
        }

        let mut results = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(self.root_id);
        while let Some(id) = queue.pop_front() {
            if let Some(node) = self.nodes.get(&id) {
                if self.node_matches_selector(node, selector) {
                    results.push(id);
                }
                for &child in &node.children {
                    queue.push_back(child);
                }
            }
        }
        results
    }

    /// Query selector scoped to a subtree rooted at `root_id`.
    /// Skips the root node itself — only searches descendants.
    pub fn query_selector_from(&self, root_id: u32, selector: &str) -> Option<u32> {
        let mut stack = vec![root_id];
        while let Some(id) = stack.pop() {
            if let Some(node) = self.nodes.get(&id) {
                if id != root_id && self.node_matches_selector(node, selector) {
                    return Some(id);
                }
                for &child in node.children.iter().rev() {
                    stack.push(child);
                }
            }
        }
        None
    }

    /// Query all matching nodes scoped to a subtree rooted at `root_id`.
    /// Skips the root node itself — only searches descendants.
    pub fn query_selector_all_from(&self, root_id: u32, selector: &str) -> Vec<u32> {
        let mut results = Vec::new();
        let mut stack = vec![root_id];
        while let Some(id) = stack.pop() {
            if let Some(node) = self.nodes.get(&id) {
                if id != root_id && self.node_matches_selector(node, selector) {
                    results.push(id);
                }
                for &child in node.children.iter().rev() {
                    stack.push(child);
                }
            }
        }
        results.reverse();
        results
    }
    /// Does `node_id` match `selector`? (Element.matches)
    pub fn element_matches(&self, node_id: u32, selector: &str) -> bool {
        self.nodes
            .get(&node_id)
            .map(|n| self.node_matches_selector(n, selector))
            .unwrap_or(false)
    }

    /// Nearest ancestor-or-self (incl. `node_id`) matching `selector`.
    /// (Element.closest) Returns the node id, or `None`.
    pub fn element_closest(&self, node_id: u32, selector: &str) -> Option<u32> {
        let mut current = Some(node_id);
        while let Some(id) = current {
            if let Some(node) = self.nodes.get(&id) {
                if node.node_type == 1 && self.node_matches_selector(node, selector) {
                    return Some(id);
                }
                current = node.parent;
            } else {
                break;
            }
        }
        None
    }

    /// Get an element by its ID attribute.
    ///
    /// Uses the `id_index` when fresh (between `from_frame` and the first
    /// in-place mutation); falls back to a linear scan otherwise. The
    /// linear scan also handles legacy snapshots that pre-date F-12 and
    /// were deserialized with an empty index.
    pub fn get_element_by_id(&self, id: &str) -> Option<u32> {
        if self.index_revision == self.revision
            && let Some(&node_id) = self.id_index.get(id)
        {
            return Some(node_id);
        }
        self.nodes
            .values()
            .find(|node| {
                node.node_type == 1 && node.attributes.get("id").map(|s| s.as_str()) == Some(id)
            })
            .map(|n| n.id)
    }

    /// Get all elements by tag name.
    ///
    /// Uses `tag_index` when fresh; falls back to a DFS walk otherwise. The
    /// tag index keys are already lowercased; we lowercase the query the
    /// same way.
    pub fn get_elements_by_tag_name(&self, tag: &str) -> Vec<u32> {
        let tag_lower = tag.to_lowercase();
        if self.index_revision == self.revision
            && let Some(ids) = self.tag_index.get(&tag_lower)
        {
            return ids.clone();
        }
        let mut results = Vec::new();
        let mut stack = vec![self.root_id];
        while let Some(id) = stack.pop() {
            if let Some(node) = self.nodes.get(&id) {
                if node.node_type == 1 && node.tag.to_lowercase() == tag_lower {
                    results.push(id);
                }
                for &child in node.children.iter().rev() {
                    stack.push(child);
                }
            }
        }
        results.reverse();
        results
    }

    /// Get all elements by class name.
    ///
    /// Uses `class_index` when fresh; falls back to a DFS walk otherwise.
    pub fn get_elements_by_class_name(&self, class: &str) -> Vec<u32> {
        if self.index_revision == self.revision
            && let Some(ids) = self.class_index.get(class)
        {
            return ids.clone();
        }
        let mut results = Vec::new();
        let mut stack = vec![self.root_id];
        while let Some(id) = stack.pop() {
            if let Some(node) = self.nodes.get(&id) {
                if node.node_type == 1
                    && let Some(cls) = node.attributes.get("class")
                    && cls.split_whitespace().any(|c| c == class)
                {
                    results.push(id);
                }
                for &child in node.children.iter().rev() {
                    stack.push(child);
                }
            }
        }
        results.reverse();
        results
    }

    // -----------------------------------------------------------------------
    // Structured data extraction (for OXI.getStructuredPage)
    // -----------------------------------------------------------------------

    /// Extract all headings from the document.
    ///
    /// Returns a list of `(level, text)` tuples where level is 1–6.
    /// Includes both `<h1>`–`<h6>` tags and elements with `role="heading"`.
    pub fn headings(&self) -> Vec<(u8, String)> {
        let heading_tags = ["h1", "h2", "h3", "h4", "h5", "h6"];
        let mut result = Vec::new();
        let mut stack = vec![self.root_id];
        while let Some(id) = stack.pop() {
            if let Some(node) = self.nodes.get(&id) {
                if node.node_type == 1 {
                    let tag_lower = node.tag.to_lowercase();
                    if let Some(idx) = heading_tags.iter().position(|t| *t == tag_lower) {
                        result.push((idx as u8 + 1, self.deep_text_content(node.id)));
                    } else if node.attributes.get("role").map(|s| s.as_str()) == Some("heading") {
                        let level: u8 = node
                            .attributes
                            .get("aria-level")
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(2);
                        result.push((level.clamp(1, 6), self.deep_text_content(node.id)));
                    }
                }
                for &child in node.children.iter().rev() {
                    stack.push(child);
                }
            }
        }
        // No reverse needed: children are pushed in reverse so first child
        // is popped first, producing correct document order.
        result
    }

    /// Extract all links from the document.
    ///
    /// Returns `(text, href)` pairs.
    pub fn links(&self) -> Vec<(String, String)> {
        let mut result = Vec::new();
        let mut stack = vec![self.root_id];
        while let Some(id) = stack.pop() {
            if let Some(node) = self.nodes.get(&id) {
                if node.node_type == 1 && node.tag.to_lowercase() == "a" {
                    let href = node.attributes.get("href").cloned().unwrap_or_default();
                    let text = self.deep_text_content(node.id);
                    result.push((text, href));
                }
                for &child in node.children.iter().rev() {
                    stack.push(child);
                }
            }
        }
        // No reverse: children pushed in reverse → first child popped first → document order.
        result
    }

    /// Extract meta tags from the document.
    ///
    /// Returns name/property → content pairs (e.g. description, og:title).
    pub fn meta_tags(&self) -> HashMap<String, String> {
        let mut result = HashMap::new();
        for node in self.nodes.values() {
            if node.node_type == 1 && node.tag.to_lowercase() == "meta" {
                let name = node
                    .attributes
                    .get("name")
                    .or_else(|| node.attributes.get("property"));
                let content = node.attributes.get("content");
                if let (Some(n), Some(c)) = (name, content) {
                    result.insert(n.clone(), c.clone());
                }
            }
        }
        result
    }

    /// Collect all text content from a node and its descendants.
    ///
    /// Explicit stack, not recursion: framework markup nests thousands deep
    /// and a recursive walk overflows the native stack (figma.com class).
    fn deep_text_content(&self, node_id: u32) -> String {
        let mut text = String::new();
        let mut stack = vec![node_id];
        while let Some(id) = stack.pop() {
            let Some(node) = self.nodes.get(&id) else {
                continue;
            };
            match node.node_type {
                3 => {
                    // Text node: text_content holds the text
                    text.push_str(&node.text_content);
                    text.push(' ');
                }
                1 => {
                    // Element node: descend in document order
                    stack.extend(node.children.iter().rev());
                }
                _ => {}
            }
        }
        text.trim().to_string()
    }

    /// Check if a node matches a CSS selector.
    ///
    /// Supports:
    /// - Universal selector `*`
    /// - Multiple selectors `a, b` (comma-separated)
    /// - Descendant combinator `a b` (space-separated)
    /// - Attribute selectors `[attr]`, `[attr=val]`, `tag[attr]`
    /// - Tag name, `.class`, `#id`, `tag.class`, `tag#id`
    fn node_matches_selector(&self, node: &DomNode, selector: &str) -> bool {
        if node.node_type != 1 {
            return false;
        }

        // Handle comma-separated selectors: match any
        for single_sel in selector.split(',') {
            let single_sel = single_sel.trim();
            if self.node_matches_single(node, single_sel) {
                return true;
            }
        }
        false
    }

    /// Check a single selector (no commas) against a node.
    fn node_matches_single(&self, node: &DomNode, selector: &str) -> bool {
        // Universal selector
        if selector == "*" {
            return true;
        }

        // Descendant combinator: split on whitespace
        let parts: Vec<&str> = selector.split_whitespace().collect();
        if parts.len() > 1 {
            // Last part must match this node
            if !self.matches_simple(node, parts[parts.len() - 1]) {
                return false;
            }
            // Walk ancestors for preceding parts
            let ancestor_parts = &parts[..parts.len() - 1];
            let mut current = node.parent;
            let mut idx = ancestor_parts.len();
            while let Some(parent_id) = current {
                if idx == 0 {
                    return true;
                }
                let ancestor = match self.nodes.get(&parent_id) {
                    Some(a) => a,
                    None => break,
                };
                if self.matches_simple(ancestor, ancestor_parts[idx - 1]) {
                    idx -= 1;
                }
                current = ancestor.parent;
            }
            return idx == 0;
        }

        self.matches_simple(node, selector)
    }

    /// Match a simple selector (no commas, no descendant) against a node.
    fn matches_simple(&self, node: &DomNode, selector: &str) -> bool {
        if node.node_type != 1 {
            return false;
        }

        // Universal selector
        if selector == "*" {
            return true;
        }

        // Check for attribute selector: "a[href]" or "[href]"
        if let Some(bracket_start) = selector.find('[')
            && let Some(bracket_end) = selector.find(']')
            && bracket_start < bracket_end
        {
            let tag_part = &selector[..bracket_start];
            let attr_part = &selector[bracket_start + 1..bracket_end];

            // Check tag part matches (if any)
            if !tag_part.is_empty() && !node.tag.eq_ignore_ascii_case(tag_part) {
                return false;
            }

            // Check attribute: "href" or "href=value" or "href='value'"
            return if let Some(eq_pos) = attr_part.find('=') {
                let attr_name = &attr_part[..eq_pos];
                let val = attr_part[eq_pos + 1..].trim_matches('\'').trim_matches('"');
                let has_attr = node.attributes.contains_key(attr_name);
                has_attr && node.attributes.get(attr_name).map(|s| s.as_str()) == Some(val)
            } else {
                node.attributes.contains_key(attr_part)
            };
        }

        // ID selector: #foo
        if let Some(id) = selector.strip_prefix('#') {
            return node.attributes.get("id").map(|s| s.as_str()) == Some(id);
        }

        // Class selector: .foo
        if let Some(class) = selector.strip_prefix('.') {
            return node
                .attributes
                .get("class")
                .map(|cls| cls.split_whitespace().any(|c| c == class))
                .unwrap_or(false);
        }

        // Tag with class: tag.class
        if let Some(dot_pos) = selector.find('.') {
            let tag_part = &selector[..dot_pos];
            let class_part = &selector[dot_pos + 1..];
            return node.tag.eq_ignore_ascii_case(tag_part)
                && node
                    .attributes
                    .get("class")
                    .map(|cls| cls.split_whitespace().any(|c| c == class_part))
                    .unwrap_or(false);
        }

        // Tag with ID: tag#id
        if let Some(hash_pos) = selector.find('#') {
            let tag_part = &selector[..hash_pos];
            let id_part = &selector[hash_pos + 1..];
            return node.tag.eq_ignore_ascii_case(tag_part)
                && node.attributes.get("id").map(|s| s.as_str()) == Some(id_part);
        }

        // Simple tag name
        node.tag.eq_ignore_ascii_case(selector)
    }

    /// Get the first child node ID.
    pub fn first_child(&self, node_id: u32) -> Option<u32> {
        self.nodes
            .get(&node_id)
            .and_then(|n| n.children.first().copied())
    }

    /// Get the last child node ID.
    pub fn last_child(&self, node_id: u32) -> Option<u32> {
        self.nodes
            .get(&node_id)
            .and_then(|n| n.children.last().copied())
    }

    /// Get the next sibling node ID.
    pub fn next_sibling(&self, node_id: u32) -> Option<u32> {
        let parent_id = self.nodes.get(&node_id).and_then(|n| n.parent)?;
        let parent = self.nodes.get(&parent_id)?;
        let idx = parent.children.iter().position(|&id| id == node_id)?;
        parent.children.get(idx + 1).copied()
    }

    /// Get the previous sibling node ID.
    pub fn previous_sibling(&self, node_id: u32) -> Option<u32> {
        let parent_id = self.nodes.get(&node_id).and_then(|n| n.parent)?;
        let parent = self.nodes.get(&parent_id)?;
        let idx = parent.children.iter().position(|&id| id == node_id)?;
        if idx > 0 {
            parent.children.get(idx - 1).copied()
        } else {
            None
        }
    }

    /// Parse an HTML fragment and replace the children of `node_id` with the parsed nodes.
    ///
    /// Walks the parsed fragment's `html → body` skeleton, takes body's direct
    /// children, and inserts them under `node_id` (each new node receives a
    /// fresh id starting from `max(existing ids) + 1` so existing nodes are
    /// never overwritten). Old children of `node_id` are removed recursively
    /// from the snapshot. Revision is bumped on success.
    pub fn set_inner_html(&mut self, node_id: u32, html: &str) {
        // Parse the fragment HTML through the Blitz-backed RenderDocument
        // (the !Send render document lives only inside the sync helper below;
        // we ship the converted DomSnapshot out and immediately drop it).
        let snap = parse_html_fragment_to_snapshot(html);
        let Some(frag_root) = snap.body_id.or(Some(snap.root_id)) else {
            return;
        };
        if !self.nodes.contains_key(&node_id) {
            return;
        }

        // Drop the target's existing children.
        let to_remove: Vec<u32> = self
            .nodes
            .get(&node_id)
            .map(|n| n.children.clone())
            .unwrap_or_default();
        for child_id in to_remove {
            self.remove_subtree(child_id);
        }

        // Compute the next id offset so the grafted nodes never collide.
        let next_id = self.nodes.keys().max().copied().map(|m| m + 1).unwrap_or(0);
        self.graft_subtree_from_snapshot(&snap, frag_root, node_id, next_id);
        self.bump_revision();
    }

    /// Copy a subtree from `source` rooted at `src_root` into this snapshot
    /// under `dst_parent`, remapping ids to start at `id_offset`.
    fn graft_subtree_from_snapshot(
        &mut self,
        source: &DomSnapshot,
        src_root: u32,
        dst_parent: u32,
        id_offset: u32,
    ) {
        use std::collections::HashMap;
        let mut id_map: HashMap<u32, u32> = HashMap::new();
        let _next_free = Self::copy_node_ids(source, src_root, &mut id_map, id_offset);
        // After ids are remapped, transfer the entries into `self.nodes`.
        for (old, new) in &id_map {
            if let Some(node) = source.nodes.get(old) {
                self.nodes.insert(
                    *new,
                    DomNode {
                        id: *new,
                        tag: node.tag.clone(),
                        attributes: node.attributes.clone(),
                        text_content: node.text_content.clone(),
                        children: node
                            .children
                            .iter()
                            .filter_map(|c| id_map.get(c).copied())
                            .collect(),
                        parent: Some(dst_parent),
                        node_type: node.node_type,
                    },
                );
            }
        }
        if let Some(parent) = self.nodes.get_mut(&dst_parent)
            && let Some(&new_id) = id_map.get(&src_root)
        {
            parent.children.push(new_id);
        }
    }

    /// Allocate fresh ids for every node in `src_root`'s subtree.
    ///
    /// Explicit-stack DFS with ONE monotonic counter. The old recursive
    /// version advanced its counter per call frame, so a sibling's fresh id
    /// collided with ids already handed out inside the preceding sibling's
    /// subtree — any nested fragment corrupted its own remap.
    fn copy_node_ids(
        source: &DomSnapshot,
        src_root: u32,
        id_map: &mut std::collections::HashMap<u32, u32>,
        first_id: u32,
    ) -> u32 {
        let mut next_id = first_id;
        let mut stack = vec![src_root];
        while let Some(src_id) = stack.pop() {
            // Tree — the guard also makes a malformed (cyclic) snapshot safe.
            if id_map.contains_key(&src_id) {
                continue;
            }
            id_map.insert(src_id, next_id);
            next_id += 1;
            if let Some(n) = source.nodes.get(&src_id) {
                stack.extend(n.children.iter().rev());
            }
        }
        next_id
    }
    /// Recursively remove `node_id` and all of its descendants from the snapshot.
    ///
    /// Detaches `node_id` from its parent's `children` vector and removes every
    /// node in the subtree from `self.nodes`. Safe to call when the node has
    /// no parent (e.g. the root).
    fn remove_subtree(&mut self, node_id: u32) {
        // Pull a borrow first to learn the parent + descendant set without
        // holding the `self.nodes` borrow across mutations below.
        let parent_id = match self.nodes.get(&node_id) {
            Some(n) => n.parent,
            None => return,
        };
        let descendants = collect_subtree_ids(self, node_id);

        // Detach from parent's children vector.
        if let Some(parent_id) = parent_id
            && let Some(parent) = self.nodes.get_mut(&parent_id)
        {
            parent.children.retain(|&c| c != node_id);
        }

        // Drop in pre-order so parents are removed before re-using id vectors.
        let mut to_drop = vec![node_id];
        to_drop.extend(descendants);
        for id in to_drop {
            self.nodes.remove(&id);
        }
    }

    /// Text content of a single node (its own `text_content` field).
    pub fn text_content(&self, node_id: u32) -> Option<String> {
        self.nodes.get(&node_id).map(|n| n.text_content.clone())
    }

    /// Extract sub-resource URLs (`<script src>`, `<link href>` stylesheet,
    /// `<img src>`, `<iframe src>`) from the document tree.
    pub fn extract_resource_urls(&self) -> Vec<ResourceUrl> {
        let mut out = Vec::new();
        for node in self.nodes.values() {
            if node.node_type != 1 {
                continue;
            }
            match node.tag.to_lowercase().as_str() {
                "script" => {
                    if let Some(src) = node.attributes.get("src") {
                        out.push(ResourceUrl {
                            url: src.clone(),
                            kind: ResourceKind::Script,
                        });
                    }
                }
                "link" => {
                    let is_css = node
                        .attributes
                        .get("rel")
                        .map(|r| r.eq_ignore_ascii_case("stylesheet"))
                        .unwrap_or(false);
                    if is_css && let Some(href) = node.attributes.get("href") {
                        out.push(ResourceUrl {
                            url: href.clone(),
                            kind: ResourceKind::Stylesheet,
                        });
                    }
                }
                "img" => {
                    if let Some(src) = node.attributes.get("src") {
                        out.push(ResourceUrl {
                            url: src.clone(),
                            kind: ResourceKind::Image,
                        });
                    }
                }
                "iframe" => {
                    if let Some(src) = node.attributes.get("src") {
                        out.push(ResourceUrl {
                            url: src.clone(),
                            kind: ResourceKind::Iframe,
                        });
                    }
                }
                _ => {}
            }
        }
        out
    }
    /// DFS pre-order walk over the snapshot tree (root first, then each child),
    /// yielding every `<iframe>` element regardless of nesting depth. Used by
    /// `iframe_srcs` / `extract_iframes` so that `<iframe>` inside an iframe's
    /// body (the live `RenderDocument`'s nested structure) is reported, not
    /// missed at depth ≥ 2.
    fn collect_iframes(&self) -> Vec<u32> {
        // Iterative DFS to match `extract_scripts`'s shape — root first so the
        // document order is preserved across parent/child iframes.
        let mut out = Vec::new();
        let mut stack = vec![self.root_id];
        while let Some(id) = stack.pop() {
            let Some(node) = self.nodes.get(&id) else {
                continue;
            };
            for &child in node.children.iter().rev() {
                stack.push(child);
            }
            if node.node_type == 1 && node.tag.eq_ignore_ascii_case("iframe") {
                out.push(id);
            }
        }
        out
    }

    /// Extract every `<iframe>` with its `src`/`srcdoc` attributes (document order).
    /// Walks the full tree so iframes nested inside other iframes are reported
    /// — the prior `nodes.values()` scan missed depth ≥ 2 and silently truncated
    /// multi-level frame hierarchies.
    ///
    /// Used by iframe population to handle `srcdoc`/`about:blank` alongside
    /// `http(s)` iframes.
    pub fn extract_iframes(&self) -> Vec<IframeElement> {
        self.collect_iframes()
            .into_iter()
            .filter_map(|id| self.nodes.get(&id))
            .map(|n| IframeElement {
                src: n.attributes.get("src").cloned(),
                srcdoc: n.attributes.get("srcdoc").cloned(),
            })
            .collect()
    }
    /// Extract `<iframe src>` URLs from the document.
    /// DFS over the snapshot tree so nested iframes are reported (matches
    /// `extract_iframes`'s behavior — kept consistent so both extraction
    /// entry points see the same set of frames at every depth).
    pub fn iframe_srcs(&self) -> Vec<String> {
        self.collect_iframes()
            .into_iter()
            .filter_map(|id| self.nodes.get(&id))
            .filter_map(|n| n.attributes.get("src").cloned())
            .collect()
    }

    pub fn extract_scripts(&self) -> Vec<ScriptSource> {
        let mut out = Vec::new();
        // Iterative DFS pre-order = document order. `tag_index` would also
        // work while fresh, but a tree walk stays correct after mutations.
        let mut stack = vec![self.root_id];
        while let Some(id) = stack.pop() {
            let Some(node) = self.nodes.get(&id) else {
                continue;
            };
            // Push children reversed so they are visited in forward order.
            for &child in node.children.iter().rev() {
                stack.push(child);
            }
            if node.node_type != 1 || !node.tag.eq_ignore_ascii_case("script") {
                continue;
            }
            let type_attr = node
                .attributes
                .get("type")
                .map(String::as_str)
                .unwrap_or("");
            let Some(kind) = classify_script_type(type_attr) else {
                continue; // non-JS type → data block, skip
            };
            // `nomodule` is honored only by non-module browsers; we support
            // modules, so skip these like real Chrome does.
            if node.attributes.contains_key("nomodule") {
                continue;
            }
            let src_url = node.attributes.get("src").cloned();
            let execute = if node.attributes.contains_key("async") {
                ExecuteTiming::Async
            } else if kind == ScriptKind::Module
                || (src_url.is_some() && node.attributes.contains_key("defer"))
            {
                ExecuteTiming::Defer
            } else {
                ExecuteTiming::Blocking
            };
            let source = match &src_url {
                Some(_) => String::new(),
                None => node.text_content.clone(),
            };
            out.push(ScriptSource {
                source,
                src_url,
                kind,
                execute,
            });
        }
        out
    }
}

/// Classify a `<script type>` attribute into classic / module, or `None` for a
/// non-JS type (data block) that must not execute.
fn classify_script_type(type_attr: &str) -> Option<ScriptKind> {
    let t = type_attr.trim().to_ascii_lowercase();
    if t.is_empty()
        || t == "text/javascript"
        || t == "application/javascript"
        || t == "text/ecmascript"
        || t == "application/ecmascript"
    {
        Some(ScriptKind::Classic)
    } else if t == "module" {
        Some(ScriptKind::Module)
    } else {
        None
    }
}

/// DFS pre-order collection of `node_id` and every descendant present in
/// `self.nodes`. Excludes `node_id` itself; callers typically prepend it.
fn collect_subtree_ids(snap: &DomSnapshot, node_id: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let mut stack: Vec<u32> = snap
        .nodes
        .get(&node_id)
        .map(|n| n.children.clone())
        .unwrap_or_default();
    while let Some(id) = stack.pop() {
        out.push(id);
        if let Some(n) = snap.nodes.get(&id) {
            stack.extend(n.children.iter().rev());
        }
    }
    out
}

/// DFS collection from a Blitz `BaseDocument` (the live RenderDocument tree).
/// Mirrors `collect_nodes` but reads Blitz node data instead of the webapi DOM.
fn collect_from_render(
    id: usize,
    doc: &oxibrowser_render::BaseDocument,
    parent: Option<u32>,
    nodes: &mut HashMap<u32, DomNode>,
    order: &mut Vec<u32>,
    body_id: &mut Option<u32>,
    head_id: &mut Option<u32>,
) {
    use oxibrowser_render::NodeData;
    let Some(node) = doc.get_node(id) else {
        return;
    };
    let id_u32 = id as u32;
    let (tag, attributes, node_type_u8, text_content) = match &node.data {
        NodeData::Document => (String::new(), HashMap::new(), 9u8, String::new()),
        NodeData::Element(e) => {
            let t = e.name.local.to_string();
            if t == "body" && body_id.is_none() {
                *body_id = Some(id_u32);
            } else if t == "head" && head_id.is_none() {
                *head_id = Some(id_u32);
            }
            let attrs: HashMap<String, String> = node
                .attrs()
                .map(|a| {
                    a.iter()
                        .map(|x| (x.name.local.to_string(), x.value.clone()))
                        .collect()
                })
                .unwrap_or_default();
            (t, attrs, 1u8, String::new())
        }
        NodeData::Text(tn) => (String::new(), HashMap::new(), 3u8, tn.content.clone()),
        NodeData::Comment => (String::new(), HashMap::new(), 8u8, String::new()),
        // Layout-only anonymous boxes are not part of the DOM.
        NodeData::AnonymousBlock(_) => return,
    };
    let children: Vec<u32> = node.children.iter().map(|c| *c as u32).collect();
    nodes.insert(
        id_u32,
        DomNode {
            id: id_u32,
            tag,
            attributes,
            text_content,
            children: children.clone(),
            parent,
            node_type: node_type_u8,
        },
    );
    order.push(id_u32);
    for &child in &node.children {
        collect_from_render(child, doc, Some(id_u32), nodes, order, body_id, head_id);
    }
}

/// Flatten shadow trees into the flat snapshot.
///
/// For each host that has a shadow root (recorded in [`SHADOW_ROOTS`]), this
/// merges the shadow subtree (read from `doc` by id — works for the detached
/// nodes `attachShadow`/`shadowRoot.appendChild` left in the render doc) into
/// `nodes`, splices the shadow root's children into the host's `children`, and
/// distributes the host's former light-DOM children into `<slot>` positions by
/// name. The result is the standard Shadow DOM **flattened tree**, visible to
/// every DomSnapshot-backed read (DOM queries, box models, `extract`, …).
fn compose_shadow_trees(
    nodes: &mut HashMap<u32, DomNode>,
    order: &mut Vec<u32>,
    doc: &oxibrowser_render::BaseDocument,
) {
    // Reset the slot-assignment view: it is rebuilt from scratch below so the
    // JS `assignedNodes()`/`assignedSlot` APIs reflect the current tree, not a
    // prior snapshot.
    SLOT_ASSIGNMENTS.with(|m| m.borrow_mut().clear());
    ASSIGNED_SLOT.with(|m| m.borrow_mut().clear());

    // Hosts present in this snapshot that also have a shadow root.
    let hosts: Vec<(u32, Vec<u32>, ShadowMode)> = SHADOW_ROOTS.with(|m| {
        let borrowed = m.borrow();
        borrowed
            .iter()
            .filter(|(h, _)| nodes.contains_key(*h))
            .map(|(h, info)| (*h, info.child_ids.clone(), info.mode))
            .collect()
    });

    for (host_id, shadow_child_ids, mode) in hosts {
        if shadow_child_ids.is_empty() {
            continue;
        }
        // Merge each shadow subtree (detached render-doc nodes) into `nodes`.
        let mut collected_top: Vec<u32> = Vec::new();
        for cid in &shadow_child_ids {
            let before = nodes.len();
            collect_from_render(
                *cid as usize,
                doc,
                Some(host_id),
                nodes,
                order,
                &mut None,
                &mut None,
            );
            if nodes.len() > before {
                collected_top.push(*cid);
            }
        }
        if collected_top.is_empty() {
            continue;
        }
        // The host's current children are its light-DOM children; they get
        // distributed into <slot> positions within the shadow content. The
        // composed children become the shadow root's top-level children.
        let light_children = nodes
            .get_mut(&host_id)
            .map(|h| std::mem::take(&mut h.children))
            .unwrap_or_default();
        // Install the shadow content as the host's children FIRST, so a
        // top-level <slot> (parent == host) can be spliced in distribute_slots.
        if let Some(h) = nodes.get_mut(&host_id) {
            h.children = collected_top.clone();
        }
        distribute_slots(nodes, &collected_top, &light_children, mode);
    }
}

/// Distribute `light_children` into `<slot>` positions within the shadow
/// subtree whose top-level nodes are `shadow_top`.
///
/// - A `<slot name="x">` receives light children whose `slot` attr == "x".
/// - A default `<slot>` (no name) receives light children with no `slot` attr
///   (only the first default slot is filled; later ones show fallback content).
/// - A slot with no assignment falls back to its own shadow-DOM children.
fn distribute_slots(
    nodes: &mut HashMap<u32, DomNode>,
    shadow_top: &[u32],
    light_children: &[u32],
    mode: ShadowMode,
) {
    // Collect every <slot> node id in the shadow subtree (DFS).
    let mut slots: Vec<u32> = Vec::new();
    collect_slot_ids(nodes, shadow_top, &mut slots);
    if slots.is_empty() {
        return;
    }

    // Partition light children by their `slot` attribute.
    let mut named: HashMap<String, Vec<u32>> = HashMap::new();
    let mut default_kids: Vec<u32> = Vec::new();
    for &lc in light_children {
        match nodes
            .get(&lc)
            .and_then(|n| n.attributes.get("slot").cloned())
        {
            Some(name) => named.entry(name).or_default().push(lc),
            None => default_kids.push(lc),
        }
    }

    // Gather replacement plans first (parent, replacement, slot) so we never
    // hold two mutable borrows of `nodes` at once.
    let mut plans: Vec<(u32, Vec<u32>, u32)> = Vec::new();
    for &slot_id in &slots {
        let (slot_name, fallback) = nodes
            .get(&slot_id)
            .map(|s| {
                (
                    s.attributes.get("name").cloned().unwrap_or_default(),
                    s.children.clone(),
                )
            })
            .unwrap_or_default();
        let assigned: Vec<u32> = if slot_name.is_empty() {
            std::mem::take(&mut default_kids)
        } else {
            named.remove(&slot_name).unwrap_or_default()
        };
        // Record the slot→assignment view for the JS `assignedNodes()`/
        // `assignedElements()` APIs (works regardless of mode — internal
        // component access). `node.assignedSlot` is only exposed for slots in
        // OPEN trees (closed roots hide it), per the HTML spec.
        if !assigned.is_empty() {
            SLOT_ASSIGNMENTS.with(|m| {
                m.borrow_mut().insert(slot_id, assigned.clone());
            });
            if mode == ShadowMode::Open {
                ASSIGNED_SLOT.with(|m| {
                    let mut bm = m.borrow_mut();
                    for &c in &assigned {
                        bm.insert(c, slot_id);
                    }
                });
            }
        }
        let replacement: Vec<u32> = if assigned.is_empty() {
            fallback
        } else {
            assigned
        };
        let parent_id = match nodes.get(&slot_id).and_then(|n| n.parent) {
            Some(p) => p,
            None => continue,
        };
        plans.push((parent_id, replacement, slot_id));
    }

    // Apply: reparent each replacement, then splice it into the parent
    // (re-finding the slot's index so multiple slots in one parent survive).
    for (pid, replacement, slot_id) in plans {
        for &r in &replacement {
            if let Some(n) = nodes.get_mut(&r) {
                n.parent = Some(pid);
            }
        }
        if let Some(p) = nodes.get_mut(&pid)
            && let Some(idx) = p.children.iter().position(|c| *c == slot_id)
        {
            p.children.splice(idx..=idx, replacement.iter().copied());
        }
    }

    // Drop the now-replaced slot nodes from the map (they're unreferenced).
    for &slot_id in &slots {
        nodes.remove(&slot_id);
    }
}

/// DFS over the shadow subtree rooted at `top`, collecting `<slot>` node ids.
fn collect_slot_ids(nodes: &HashMap<u32, DomNode>, top: &[u32], out: &mut Vec<u32>) {
    for &id in top {
        let Some(node) = nodes.get(&id) else { continue };
        if node.tag == "slot" {
            out.push(id);
        }
        // Recurse into children (skip into slots themselves — slot children are
        // fallback content, not slotted content).
        if node.tag != "slot" {
            collect_slot_ids(nodes, &node.children, out);
        }
    }
}

/// Set each element node's `text_content` to the concatenation of its
/// descendant text nodes (mirrors the retired `collect_text_content`).
///
/// Explicit-stack post-order — framework DOM nests thousands deep and a
/// recursive walk overflows the native stack (figma.com class).
fn fill_element_text(nodes: &mut HashMap<u32, DomNode>, root: u32) {
    enum Task {
        Enter(u32),
        Assemble(u32, Vec<u32>),
    }
    let mut stack = vec![Task::Enter(root)];
    while let Some(task) = stack.pop() {
        match task {
            Task::Enter(id) => {
                let Some(node) = nodes.get(&id) else {
                    continue;
                };
                match node.node_type {
                    // Text leaf: the parent's Assemble reads text_content.
                    3 => {}
                    // Element: assemble after the subtree completes.
                    1 => {
                        let children = node.children.clone();
                        stack.push(Task::Assemble(id, children.clone()));
                        for &child in children.iter().rev() {
                            stack.push(Task::Enter(child));
                        }
                    }
                    // Document / comment / etc: transparent — descend so the
                    // subtree is still processed (the recursive original did
                    // exactly this; the snapshot root is a type-9 document).
                    _ => {
                        let children = node.children.clone();
                        for &child in children.iter().rev() {
                            stack.push(Task::Enter(child));
                        }
                    }
                }
            }
            Task::Assemble(id, children) => {
                let mut text = String::new();
                for child in children {
                    // Element children carry their assembled subtree text;
                    // text leaves carry their own. Comments et al. contribute
                    // nothing (matches the recursive original).
                    if let Some(n) = nodes.get(&child)
                        && (n.node_type == 1 || n.node_type == 3)
                    {
                        text.push_str(&n.text_content);
                    }
                }
                if let Some(n) = nodes.get_mut(&id) {
                    n.text_content = text;
                }
            }
        }
    }
}

// ── HTML serialization (for compose-then-feed screenshot rasterization) ────

/// True when `node` is an `<input type="password">` element.
pub(crate) fn is_password_input(node: &DomNode) -> bool {
    node.node_type == 1
        && node.tag.eq_ignore_ascii_case("input")
        && node
            .attributes
            .get("type")
            .map(|t| t.trim().eq_ignore_ascii_case("password"))
            .unwrap_or(false)
}

/// Replace `value` attributes of password inputs with the fixed redaction
/// marker. The snapshot is a read-only observation, so this never affects
/// the live document or form filling — only what snapshot readers see.
fn mask_password_inputs(nodes: &mut HashMap<u32, DomNode>) {
    for node in nodes.values_mut() {
        if is_password_input(node) && node.attributes.contains_key("value") {
            node.attributes.insert(
                "value".to_string(),
                crate::security::redact::REDACTED.to_string(),
            );
        }
    }
}

/// Serialize a single node (and its subtree) into `out` as HTML.
fn serialize_node(nodes: &HashMap<u32, DomNode>, id: u32, out: &mut String) {
    let Some(node) = nodes.get(&id) else {
        return;
    };
    match node.node_type {
        // Text node: emit its own content, entity-escaped.
        3 => escape_text(&node.text_content, out),
        // Comments are irrelevant to rasterization; skip.
        8 => {}
        // Document node (shouldn't normally be hit here): recurse children.
        9 => {
            for &child in &node.children {
                serialize_node(nodes, child, out);
            }
        }
        // Element (node_type == 1) and any other element-like node.
        _ => {
            let tag = node.tag.as_str();
            if tag.is_empty() {
                // Unknown element kind; still recurse so children survive.
                for &child in &node.children {
                    serialize_node(nodes, child, out);
                }
                return;
            }
            out.push('<');
            out.push_str(tag);
            for (name, value) in &node.attributes {
                out.push(' ');
                out.push_str(name);
                out.push_str("=\"");
                escape_attr(value, out);
                out.push('"');
            }
            out.push('>');
            if is_void_element(tag) {
                return;
            }
            if is_raw_text_element(tag) {
                // script/style: descendant text is raw — no escaping.
                collect_raw_text(nodes, id, out);
            } else {
                for &child in &node.children {
                    serialize_node(nodes, child, out);
                }
            }
            out.push_str("</");
            out.push_str(tag);
            out.push('>');
        }
    }
}

/// Concatenate all descendant text-node content of `id` (raw, unescaped) —
/// used for `<script>`/`<style>` bodies.
fn collect_raw_text(nodes: &HashMap<u32, DomNode>, id: u32, out: &mut String) {
    let Some(node) = nodes.get(&id) else {
        return;
    };
    if node.node_type == 3 {
        out.push_str(&node.text_content);
        return;
    }
    for &child in &node.children {
        collect_raw_text(nodes, child, out);
    }
}

/// HTML void elements — no closing tag, no children.
fn is_void_element(tag: &str) -> bool {
    matches!(
        tag,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

/// Raw-text elements whose content must not be entity-escaped on reparse.
fn is_raw_text_element(tag: &str) -> bool {
    matches!(tag, "script" | "style")
}

/// Escape text content for HTML: `&`, `<`, `>`.
fn escape_text(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
}

/// Escape an attribute value: `&`, `"`, plus newlines (kept literal would
/// survive but normalizing avoids parser quirks).
fn escape_attr(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("&#10;"),
            _ => out.push(c),
        }
    }
}

/// Build id/class/tag indices from `nodes` in the order given by `order`
/// (DFS pre-order), used exclusively by `DomSnapshot::from_frame` for the
/// initial snapshot. After any in-place mutation, indices are not refreshed
/// eagerly; the `&self` read methods on `DomSnapshot` detect staleness via
/// `index_revision != revision` and fall back to a tree walk.
impl DomSnapshot {
    /// Rebuild id/class/tag indices from the current `nodes` HashMap by
    /// performing a DFS pre-order walk starting at `root_id`. Called after
    /// in-place mutations (innerHTML, createElement, etc.) so that
    /// `query_selector` / `get_element_by_id` can find newly-inserted nodes
    /// via the fast-path index instead of falling back to a tree walk.
    pub fn rebuild_indices(&mut self) {
        let mut order: Vec<u32> = Vec::with_capacity(self.nodes.len());
        let mut stack = vec![self.root_id];
        while let Some(id) = stack.pop() {
            if let Some(node) = self.nodes.get(&id) {
                order.push(id);
                // Push children in reverse so DFS pre-order is preserved.
                for &child in node.children.iter().rev() {
                    stack.push(child);
                }
            }
        }
        let (id_index, class_index, tag_index) = build_indices(&self.nodes, &order);
        self.id_index = id_index;
        self.class_index = class_index;
        self.tag_index = tag_index;
        self.index_revision = self.revision;
    }
}
type DomIndices = (
    HashMap<String, u32>,
    HashMap<String, Vec<u32>>,
    HashMap<String, Vec<u32>>,
);
fn build_indices(nodes: &HashMap<u32, DomNode>, order: &[u32]) -> DomIndices {
    let mut id_index: HashMap<String, u32> = HashMap::new();
    let mut class_index: HashMap<String, Vec<u32>> = HashMap::new();
    let mut tag_index: HashMap<String, Vec<u32>> = HashMap::new();

    for &id in order {
        let node = match nodes.get(&id) {
            Some(n) if n.node_type == 1 => n,
            _ => continue,
        };
        if let Some(id_attr) = node.attributes.get("id")
            && !id_attr.is_empty()
            && !id_index.contains_key(id_attr)
        {
            id_index.insert(id_attr.clone(), id);
        }
        if let Some(cls) = node.attributes.get("class") {
            for token in cls.split_whitespace() {
                if !token.is_empty() {
                    class_index.entry(token.to_string()).or_default().push(id);
                }
            }
        }
        let tag_lower = node.tag.to_lowercase();
        if !tag_lower.is_empty() {
            tag_index.entry(tag_lower).or_default().push(id);
        }
    }

    (id_index, class_index, tag_index)
}

/// Parse an HTML fragment through the Blitz-backed RenderDocument and convert
/// it to a [`DomSnapshot`]. Used by `set_inner_html` (and available for tests).
pub(crate) fn parse_html_fragment_to_snapshot(html: &str) -> DomSnapshot {
    let rd = oxibrowser_render::RenderDocument::from_html(
        html,
        None,
        oxibrowser_render::Viewport::default(),
    )
    .expect("parse html fragment");
    DomSnapshot::from_render_document(&rd, "", "")
}

// ── Interactive element discovery (for OXI.getInteractiveElements) ──────────

/// Visible text is capped at this many chars per interactive element.
const MAX_INTERACTIVE_TEXT_CHARS: usize = 80;

/// One interactive element found by [`DomSnapshot::interactive_elements`].
///
/// Serialized shape matches the `OXI.getInteractiveElements` wire format
/// (camelCase). Geometry (`box`) is intentionally absent: `DomNode` carries no
/// layout rectangles and computing them requires a layout pass, so the field is
/// omitted from the wire format rather than reported as null.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InteractiveElement {
    /// Internal node id. Not part of the wire format.
    #[serde(skip)]
    pub node_id: u32,
    /// 0-based position across all interactive elements, in document order.
    pub index: usize,
    /// Lowercased tag name.
    pub tag: String,
    /// Computed role: explicit `role` attribute, else implicit per tag/type.
    pub role: String,
    /// Visible (descendant) text, whitespace-trimmed and capped at
    /// [`MAX_INTERACTIVE_TEXT_CHARS`].
    pub text: String,
    /// `href` attribute, when present and non-empty.
    pub href: Option<String>,
    /// `type` attribute (mainly meaningful for `<input>`).
    #[serde(rename = "inputType")]
    pub input_type: Option<String>,
    /// `name` attribute, when present and non-empty.
    #[serde(rename = "nameAttr")]
    pub name_attr: Option<String>,
    /// Computed accessible name (aria-labelledby → aria-label → img alt →
    /// placeholder → value → text), trimmed and capped at
    /// [`MAX_INTERACTIVE_TEXT_CHARS`]. `None` when no non-empty name exists.
    pub name: Option<String>,
    /// `aria-label` attribute, when present and non-empty.
    #[serde(rename = "ariaLabel")]
    pub aria_label: Option<String>,
    /// `placeholder` attribute, when present and non-empty.
    pub placeholder: Option<String>,
    /// Whether the `disabled` attribute is present.
    pub disabled: bool,
    /// Unique CSS selector path (`#id` short-circuits for valid CSS
    /// identifiers, otherwise `[id="…"]`; plus `tag` /
    /// `tag:nth-of-type(n)` steps joined by `" > "`).
    pub selector: String,
}

/// Trims a candidate accessible name and caps it at
/// [`MAX_INTERACTIVE_TEXT_CHARS`]; empty results become `None`.
fn cap_accessible_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(MAX_INTERACTIVE_TEXT_CHARS).collect())
}

/// Tags that are interactive regardless of attributes.
fn is_interactive_tag(tag: &str) -> bool {
    matches!(tag, "a" | "button" | "input" | "select" | "textarea")
}

/// ARIA roles that make an element interactive via the `role` attribute.
fn is_interactive_role(role: &str) -> bool {
    matches!(role, "button" | "link" | "tab" | "checkbox" | "radio")
}

/// Interactivity heuristic for a single node: interactive tag, `onclick`
/// attribute, interactive `role`, or `tabindex >= 0`.
pub fn is_interactive_element(node: &DomNode) -> bool {
    if node.node_type != 1 {
        return false;
    }
    if is_interactive_tag(&node.tag.to_lowercase()) {
        return true;
    }
    if node.attributes.contains_key("onclick") {
        return true;
    }
    if node
        .attributes
        .get("role")
        .is_some_and(|r| is_interactive_role(r.trim().to_lowercase().as_str()))
    {
        return true;
    }
    node.attributes
        .get("tabindex")
        .and_then(|t| t.trim().parse::<i32>().ok())
        .is_some_and(|t| t >= 0)
}

/// Computed role for an interactive element: the explicit `role` attribute
/// wins; otherwise the implicit role of the tag — consistent with the
/// accessibility tree's implicit roles (a → link, button → button,
/// select → listbox, textarea → textbox, input per type).
pub fn compute_interactive_role(node: &DomNode) -> String {
    if let Some(role) = node.attributes.get("role") {
        let role = role.trim();
        if !role.is_empty() {
            return role.to_string();
        }
    }
    let type_attr = || {
        node.attributes
            .get("type")
            .map(|s| s.trim().to_lowercase())
            .unwrap_or_default()
    };
    match node.tag.to_lowercase().as_str() {
        "a" => "link".into(),
        "button" => "button".into(),
        "select" => "listbox".into(),
        "textarea" => "textbox".into(),
        "input" => match type_attr().as_str() {
            "checkbox" => "checkbox".into(),
            "radio" => "radio".into(),
            "button" | "submit" | "reset" => "button".into(),
            _ => "textbox".into(),
        },
        _ => "generic".into(),
    }
}

impl DomSnapshot {
    /// Computed accessible name for `node_id`, following a simplified
    /// name-from-author precedence: `aria-labelledby` (space-separated id
    /// references resolved against the document) → `aria-label` → `alt` on
    /// `<img>` → `placeholder` on form controls → `value` → descendant text.
    /// Whitespace-trimmed and capped at [`MAX_INTERACTIVE_TEXT_CHARS`];
    /// `None` when no non-empty candidate exists.
    pub fn accessible_name(&self, node_id: u32) -> Option<String> {
        self.accessible_name_with_id_map(node_id, &self.element_id_map())
    }

    /// Maps non-empty `id` attribute values to node ids (single DFS over the
    /// body subtree) so `aria-labelledby` resolution is O(1) per reference.
    fn element_id_map(&self) -> HashMap<String, u32> {
        let mut map = HashMap::new();
        let mut stack = vec![self.root_id];
        while let Some(id) = stack.pop() {
            if let Some(node) = self.nodes.get(&id) {
                if node.node_type == 1
                    && let Some(id_attr) = node
                        .attributes
                        .get("id")
                        .map(|v| v.trim())
                        .filter(|v| !v.is_empty())
                {
                    map.entry(id_attr.to_string()).or_insert(node.id);
                }
                for &child in &node.children {
                    stack.push(child);
                }
            }
        }
        map
    }

    /// [`DomSnapshot::accessible_name`] with a prebuilt id map
    /// (see [`DomSnapshot::element_id_map`]).
    fn accessible_name_with_id_map(
        &self,
        node_id: u32,
        id_map: &HashMap<String, u32>,
    ) -> Option<String> {
        let node = self.nodes.get(&node_id)?;
        let attr = |name: &str| {
            node.attributes
                .get(name)
                .map(|v| v.trim())
                .filter(|v| !v.is_empty())
        };
        // 1. aria-labelledby: space-separated id references, texts joined.
        if let Some(refs) = attr("aria-labelledby") {
            let joined = refs
                .split_whitespace()
                .filter_map(|id| id_map.get(id).copied())
                .map(|target| self.deep_text_content(target))
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            if let Some(name) = cap_accessible_name(&joined) {
                return Some(name);
            }
        }
        // 2. aria-label.
        if let Some(label) = attr("aria-label")
            && let Some(name) = cap_accessible_name(label)
        {
            return Some(name);
        }
        // 3. img alt.
        if node.tag.eq_ignore_ascii_case("img")
            && let Some(alt) = attr("alt")
            && let Some(name) = cap_accessible_name(alt)
        {
            return Some(name);
        }
        // 4. placeholder on form controls.
        if matches!(
            node.tag.to_lowercase().as_str(),
            "input" | "select" | "textarea"
        ) && let Some(placeholder) = attr("placeholder")
            && let Some(name) = cap_accessible_name(placeholder)
        {
            return Some(name);
        }
        // 5. value. Skipped for password inputs: a typed/autofilled password
        // must never surface through the accessible name (security: see
        // docs/designs/2026-09-27-agent-auth-implementation.md §7 P0-2).
        if let Some(value) = attr("value")
            && !is_password_input(node)
            && let Some(name) = cap_accessible_name(value)
        {
            return Some(name);
        }
        // 6. descendant text.
        cap_accessible_name(&self.deep_text_content(node_id))
    }

    /// All interactive elements in document order (iterative DFS pre-order,
    /// same traversal as `headings`/`links`).
    pub fn interactive_elements(&self) -> Vec<InteractiveElement> {
        let id_map = self.element_id_map();
        let mut result: Vec<InteractiveElement> = Vec::new();
        let mut stack = vec![self.root_id];
        while let Some(id) = stack.pop() {
            if let Some(node) = self.nodes.get(&id) {
                if node.node_type == 1 && is_interactive_element(node) {
                    let element = self.build_interactive_element(node, result.len(), &id_map);
                    result.push(element);
                }
                for &child in node.children.iter().rev() {
                    stack.push(child);
                }
            }
        }
        result
    }

    fn build_interactive_element(
        &self,
        node: &DomNode,
        index: usize,
        id_map: &HashMap<String, u32>,
    ) -> InteractiveElement {
        let full_text = self.deep_text_content(node.id);
        let text: String = full_text.chars().take(MAX_INTERACTIVE_TEXT_CHARS).collect();
        let attr = |name: &str| {
            node.attributes
                .get(name)
                .filter(|v| !v.trim().is_empty())
                .cloned()
        };
        InteractiveElement {
            node_id: node.id,
            index,
            tag: node.tag.to_lowercase(),
            role: compute_interactive_role(node),
            text,
            href: attr("href"),
            input_type: attr("type"),
            name_attr: attr("name"),
            aria_label: attr("aria-label"),
            placeholder: attr("placeholder"),
            name: self.accessible_name_with_id_map(node.id, id_map),
            disabled: node.attributes.contains_key("disabled"),
            selector: self.css_selector_path(node.id),
        }
    }

    /// Unique CSS selector path for `node_id`.
    ///
    /// Climbs toward the document root emitting `tag` steps
    /// (`tag:nth-of-type(n)` when same-tag element siblings exist) and stops
    /// early at a non-empty `id` — ids are unique, so ancestors add nothing.
    /// The `#id` short form is only emitted when the id is a valid CSS
    /// identifier (`[A-Za-z][A-Za-z0-9_-]*` or `--*`); otherwise the
    /// attribute form `[id="<escaped>"]` is used. Per CSS `nth-of-type`
    /// rules, only element siblings with the same tag count; text/comment
    /// siblings are ignored.
    pub fn css_selector_path(&self, node_id: u32) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut current = Some(node_id);
        while let Some(id) = current {
            let Some(node) = self.nodes.get(&id) else {
                break;
            };
            if node.node_type == 9 {
                break; // the document node itself is not part of the path
            }
            if let Some(id_attr) = node.attributes.get("id").filter(|v| !v.trim().is_empty()) {
                let trimmed = id_attr.trim();
                if is_valid_css_identifier(trimmed) {
                    parts.push(format!("#{trimmed}"));
                } else {
                    // `#a.b` parses as id `a` + class `.b` and `#1x` is not
                    // valid CSS — fall back to the attribute form, which
                    // `query_selector` supports.
                    parts.push(format!("[id=\"{}\"]", escape_css_attr_value(id_attr)));
                }
                break;
            }
            let tag = node.tag.to_lowercase();
            let (mut position, mut total) = (0usize, 0usize);
            if let Some(parent) = node.parent.and_then(|p| self.nodes.get(&p)) {
                for &sibling in &parent.children {
                    if let Some(s) = self.nodes.get(&sibling)
                        && s.node_type == 1
                        && s.tag.eq_ignore_ascii_case(&node.tag)
                    {
                        total += 1;
                        if s.id == node.id {
                            position = total;
                        }
                    }
                }
            }
            parts.push(if total <= 1 {
                tag
            } else {
                format!("{tag}:nth-of-type({position})")
            });
            current = node.parent;
        }
        parts.reverse();
        parts.join(" > ")
    }
}

/// True when `id` can be referenced by a bare `#id` selector: a CSS
/// identifier (`[A-Za-z][A-Za-z0-9_-]*`) or a custom ident (`--*`).
/// Anything else (leading digits, `.`, `:`, whitespace, …) would either be
/// misparsed or is invalid CSS.
fn is_valid_css_identifier(id: &str) -> bool {
    if let Some(rest) = id.strip_prefix("--") {
        return rest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    }
    let mut chars = id.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Escape a raw attribute value for use inside `[id="<value>"]`: backslash
/// and double-quote are backslash-escaped, everything else is emitted as-is.
fn escape_css_attr_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out
}

// ── Stable refs + ARIA snapshot support (OXI domain) ────────────────────────

impl DomSnapshot {
    /// Content fingerprint for stable-ref (`OXI.*Ref`) validation.
    ///
    /// Combines the tag, sorted `key=value` attributes, and the first 80
    /// characters of deep text — enough to detect attribute or content drift
    /// between observation and use. The exact format is an implementation
    /// detail; callers only compare fingerprints for equality.
    pub fn fingerprint(&self, node_id: u32) -> String {
        let Some(node) = self.nodes.get(&node_id) else {
            return String::new();
        };
        let mut attrs: Vec<String> = node
            .attributes
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        attrs.sort();
        let text: String = self.deep_text_content(node_id).chars().take(80).collect();
        format!("{}|{}|{}", node.tag.to_lowercase(), attrs.join(","), text)
    }

    /// ARIA role for ARIA-snapshot rendering: explicit `role` attribute wins,
    /// then landmark tag mappings (`img`→img, `nav`→navigation, `header`→
    /// banner, `footer`→contentinfo, `main`→main, `aside`→complementary),
    /// then the implicit interactive roles of [`compute_interactive_role`]
    /// (`a`/`button`/`input`/`select`/`textarea`), falling back to `generic`
    /// (Playwright-style). Non-element nodes render as `text`.
    pub fn aria_role(&self, node_id: u32) -> String {
        let Some(node) = self.nodes.get(&node_id) else {
            return "generic".into();
        };
        if node.node_type != 1 {
            return "text".into();
        }
        if let Some(role) = node.attributes.get("role") {
            let role = role.trim();
            if !role.is_empty() {
                return role.to_string();
            }
        }
        match node.tag.to_lowercase().as_str() {
            "img" => "img".into(),
            "nav" => "navigation".into(),
            "header" => "banner".into(),
            "footer" => "contentinfo".into(),
            "main" => "main".into(),
            "aside" => "complementary".into(),
            "a" | "button" | "input" | "select" | "textarea" => compute_interactive_role(node),
            _ => "generic".into(),
        }
    }

    /// Whether the node is visible per the CSS `LayoutEngine`
    /// (`display: none`, `visibility: hidden`/`collapse`, and `opacity: 0`
    /// all hide it). Unknown nodes are invisible.
    pub fn is_visible(&self, node_id: u32) -> bool {
        crate::css::LayoutEngine::compute_style(self, node_id).is_some_and(|s| s.visible)
    }

    /// Render the visible tree as a Playwright-style YAML ARIA snapshot:
    ///
    /// ```text
    /// - button "Submit" [ref=e5]
    /// - navigation:
    ///   - link "Home" [ref=e3]
    /// ```
    ///
    /// `allocate_ref` decides which elements carry a `[ref=eN]` marker and
    /// owns the ref bookkeeping; returning `None` omits the marker. Invisible
    /// nodes and non-rendered tags (`script`/`style`/`head`) are skipped; the
    /// document/`html`/`body` wrappers are not rendered — their children start
    /// at depth 0. Leaf lines carry the accessible name; container lines end
    /// with `:`; checkboxes with a `checked` attribute gain `[checked]`.
    pub fn render_aria_yaml(&self, allocate_ref: &mut dyn FnMut(u32) -> Option<String>) -> String {
        let start = self.body_id.unwrap_or(self.root_id);
        let mut out = String::new();
        for child in self.aria_yaml_children(start) {
            self.render_aria_yaml_node(child, 0, allocate_ref, &mut out);
        }
        out
    }

    /// Element children of `id` that the ARIA snapshot would render.
    fn aria_yaml_children(&self, id: u32) -> Vec<u32> {
        match self.nodes.get(&id) {
            Some(node) => node
                .children
                .iter()
                .copied()
                .filter(|&c| self.aria_yaml_renders(c))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Whether `id` appears in the ARIA snapshot: element nodes only,
    /// skipping `script`/`style`/`head`, and visible per
    /// [`DomSnapshot::is_visible`].
    fn aria_yaml_renders(&self, id: u32) -> bool {
        let Some(node) = self.nodes.get(&id) else {
            return false;
        };
        if node.node_type != 1 {
            return false;
        }
        if matches!(
            node.tag.to_lowercase().as_str(),
            "script" | "style" | "head"
        ) {
            return false;
        }
        self.is_visible(id)
    }

    fn render_aria_yaml_node(
        &self,
        id: u32,
        depth: usize,
        allocate_ref: &mut dyn FnMut(u32) -> Option<String>,
        out: &mut String,
    ) {
        let Some(node) = self.nodes.get(&id) else {
            return;
        };
        let role = self.aria_role(id);
        let r#ref = allocate_ref(id);
        let children = self.aria_yaml_children(id);

        let mut line = format!("{}- {}", "  ".repeat(depth), role);
        if children.is_empty() {
            // Leaf: `- role "accessible name" …` (containers stay unnamed).
            if let Some(name) = self.accessible_name(id) {
                line.push_str(&format!(" \"{name}\""));
            }
        }
        if node.tag.eq_ignore_ascii_case("input")
            && node.attributes.get("type").map(|t| t.trim()) == Some("checkbox")
            && node.attributes.contains_key("checked")
        {
            line.push_str(" [checked]");
        }
        if let Some(r) = r#ref {
            line.push_str(&format!(" [ref={r}]"));
        }
        if !children.is_empty() {
            line.push(':');
        }
        out.push_str(&line);
        out.push('\n');
        for child in children {
            self.render_aria_yaml_node(child, depth + 1, allocate_ref, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Frame;
    use url::Url;

    fn make_frame(html: &str) -> Frame {
        let url = Url::parse("https://example.com").unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(Frame::from_html(url, html)).unwrap()
    }

    #[test]
    fn test_dom_snapshot_from_frame() {
        let html = r#"<html><head><title>Test Page</title></head>
            <body><p class="intro">Hello</p><a href="/link">click</a></body></html>"#;
        let frame = make_frame(html);
        let snapshot = DomSnapshot::from_frame(&frame);

        assert_eq!(snapshot.url, "https://example.com/");
        assert_eq!(snapshot.title, "Test Page");
        assert!(snapshot.body_id.is_some(), "should find body element");
        assert!(snapshot.head_id.is_some(), "should find head element");
        assert!(snapshot.nodes.len() > 5, "should have multiple nodes");
    }

    #[test]
    fn test_dom_snapshot_from_render_document() {
        // The converter builds a DomSnapshot from the live RenderDocument (Blitz
        // BaseDocument) — the post-unification DOM source. It must reproduce the
        // same queryable structure as the retired from_frame path, and reflect
        // JS mutations applied to the RenderDocument.
        let html = r#"<html><head><title>Live Page</title></head>
            <body><p class="intro">Hello</p><a href="/link">click</a></body></html>"#;
        let rd = oxibrowser_render::RenderDocument::from_html(
            html,
            Some("https://example.com/"),
            oxibrowser_render::Viewport::default(),
        )
        .expect("parse");
        let snapshot = DomSnapshot::from_render_document(&rd, "https://example.com/", "Live Page");

        assert!(snapshot.body_id.is_some(), "body found");
        assert!(snapshot.head_id.is_some(), "head found");
        assert!(
            snapshot.nodes.len() > 5,
            "multiple nodes: {}",
            snapshot.nodes.len()
        );
        let p = snapshot.query_selector("p").expect("<p> found");
        assert_eq!(snapshot.nodes.get(&p).unwrap().tag, "p");
        let intro = snapshot.query_selector(".intro").expect(".intro found");
        assert_eq!(snapshot.nodes.get(&intro).unwrap().text_content, "Hello");

        // A mutation on the RenderDocument is reflected in a fresh snapshot.
        let mut rd = rd;
        let link = rd.query_selector("a").expect("<a> found");
        rd.set_attribute(link, "href", "/changed");
        let snap2 = DomSnapshot::from_render_document(&rd, "https://example.com/", "Live Page");
        let link2 = snap2.query_selector("a").unwrap();
        assert_eq!(
            snap2
                .nodes
                .get(&link2)
                .unwrap()
                .attributes
                .get("href")
                .map(|s| s.as_str()),
            Some("/changed"),
            "snapshot must reflect the RenderDocument mutation"
        );
    }

    #[test]
    fn test_extract_scripts_order_and_flags() {
        // Eight <script> elements in document order; the JSON-typed data block
        // and the `nomodule` script must be skipped (this engine supports
        // modules), leaving six executable scripts.
        let html = r#"<html><head>
            <script>window.__first = 1;</script>
            <script type="application/json" id="data">{"x":1}</script>
            <script src="/ext-a.js"></script>
            <script type="module">import './m.js';</script>
            <script src="/ext-b.js" defer></script>
            <script src="/ext-c.js" async></script>
            <script type="module" src="/mod.js"></script>
            <script nomodule>window.__nomod = 1;</script>
            </head><body></body></html>"#;
        let frame = make_frame(html);
        let snapshot = DomSnapshot::from_frame(&frame);
        let scripts = snapshot.extract_scripts();

        assert_eq!(scripts.len(), 6, "6 executable scripts, got: {scripts:?}");

        // [0] inline classic
        assert_eq!(scripts[0].source.trim(), "window.__first = 1;");
        assert!(scripts[0].src_url.is_none());
        assert_eq!(scripts[0].kind, ScriptKind::Classic);
        assert_eq!(scripts[0].execute, ExecuteTiming::Blocking);

        // [1] external classic
        assert_eq!(scripts[1].src_url.as_deref(), Some("/ext-a.js"));
        assert_eq!(scripts[1].source, "", "external source filled by caller");
        assert_eq!(scripts[1].kind, ScriptKind::Classic);
        assert_eq!(scripts[1].execute, ExecuteTiming::Blocking);

        // [2] inline module
        assert_eq!(scripts[2].kind, ScriptKind::Module);
        assert!(scripts[2].src_url.is_none());

        // [3] external defer
        assert_eq!(scripts[3].src_url.as_deref(), Some("/ext-b.js"));
        assert_eq!(scripts[3].execute, ExecuteTiming::Defer);

        // [4] external async
        assert_eq!(scripts[4].src_url.as_deref(), Some("/ext-c.js"));
        assert_eq!(scripts[4].execute, ExecuteTiming::Async);

        // [5] external module
        assert_eq!(scripts[5].src_url.as_deref(), Some("/mod.js"));
        assert_eq!(scripts[5].kind, ScriptKind::Module);
    }

    #[test]
    fn test_query_selector_tag() {
        let html = r#"<html><body><p>first</p><p>second</p></body></html>"#;
        let frame = make_frame(html);
        let snapshot = DomSnapshot::from_frame(&frame);

        let found = snapshot.query_selector("p");
        assert!(found.is_some(), "should find a <p> element");
        let node = snapshot.nodes.get(&found.unwrap()).unwrap();
        assert_eq!(node.tag, "p");
    }

    #[test]
    fn test_query_selector_class() {
        let html = r#"<html><body><div class="foo">content</div></body></html>"#;
        let frame = make_frame(html);
        let snapshot = DomSnapshot::from_frame(&frame);

        let found = snapshot.query_selector(".foo");
        assert!(found.is_some(), "should find element with class .foo");
    }

    #[test]
    fn test_query_selector_id() {
        let html = r#"<html><body><span id="bar">text</span></body></html>"#;
        let frame = make_frame(html);
        let snapshot = DomSnapshot::from_frame(&frame);

        let found = snapshot.query_selector("#bar");
        assert!(found.is_some(), "should find element with id #bar");
    }

    #[test]
    fn test_query_selector_tag_class() {
        let html =
            r#"<html><body><div class="main">main</div><p class="main">para</p></body></html>"#;
        let frame = make_frame(html);
        let snapshot = DomSnapshot::from_frame(&frame);

        let found = snapshot.query_selector("div.main");
        assert!(found.is_some(), "should find div.main");
        let node = snapshot.nodes.get(&found.unwrap()).unwrap();
        assert_eq!(node.tag, "div");
    }

    #[test]
    fn test_query_selector_tag_id() {
        let html = r#"<html><body><div id="content">c</div></body></html>"#;
        let frame = make_frame(html);
        let snapshot = DomSnapshot::from_frame(&frame);

        let found = snapshot.query_selector("div#content");
        assert!(found.is_some(), "should find div#content");
    }

    #[test]
    fn test_query_selector_attribute() {
        let html = r#"<html><body><a href="/link">click</a><p>no link</p></body></html>"#;
        let frame = make_frame(html);
        let snapshot = DomSnapshot::from_frame(&frame);

        let found = snapshot.query_selector("[href]");
        assert!(found.is_some(), "should find element with href attribute");
        let node = snapshot.nodes.get(&found.unwrap()).unwrap();
        assert_eq!(node.tag, "a");

        let found2 = snapshot.query_selector("a[href]");
        assert!(found2.is_some(), "should find a[href]");
    }

    #[test]
    fn test_query_selector_all() {
        let html = "<html><body><ul><li>a</li><li>b</li><li>c</li></ul></body></html>";
        let frame = make_frame(html);
        let snapshot = DomSnapshot::from_frame(&frame);

        let items = snapshot.query_selector_all("li");
        assert_eq!(items.len(), 3, "should find 3 <li> elements");
    }

    #[test]
    fn test_get_element_by_id() {
        let html = r#"<html><body><div id="main">content</div></body></html>"#;
        let frame = make_frame(html);
        let snapshot = DomSnapshot::from_frame(&frame);

        let found = snapshot.get_element_by_id("main");
        assert!(found.is_some(), "should find element by id");

        let not_found = snapshot.get_element_by_id("nonexistent");
        assert!(not_found.is_none(), "should not find nonexistent id");
    }

    #[test]
    fn test_get_elements_by_tag_name() {
        let html = "<html><body><p>a</p><p>b</p><p>c</p></body></html>";
        let frame = make_frame(html);
        let snapshot = DomSnapshot::from_frame(&frame);

        let items = snapshot.get_elements_by_tag_name("p");
        assert_eq!(items.len(), 3, "should find 3 <p> elements");
    }

    #[test]
    fn test_get_elements_by_class_name() {
        let html =
            r#"<html><body><div class="item">a</div><div class="item">b</div></body></html>"#;
        let frame = make_frame(html);
        let snapshot = DomSnapshot::from_frame(&frame);

        let items = snapshot.get_elements_by_class_name("item");
        assert_eq!(items.len(), 2, "should find 2 .item elements");
    }

    #[test]
    fn test_dom_snapshot_empty() {
        let snapshot = DomSnapshot::empty();
        assert!(snapshot.url.is_empty());
        assert!(snapshot.nodes.is_empty());
        assert!(snapshot.body_id.is_none());
    }

    #[test]
    fn test_node_text_content() {
        let html = r#"<html><body><p>Hello World</p></body></html>"#;
        let frame = make_frame(html);
        let snapshot = DomSnapshot::from_frame(&frame);

        let p_id = snapshot.query_selector("p").unwrap();
        let p_node = snapshot.nodes.get(&p_id).unwrap();
        assert!(
            p_node.text_content.contains("Hello World"),
            "text content should include 'Hello World'"
        );
    }

    #[test]
    fn test_node_parent_child_relationship() {
        let html = "<html><body><div><p>text</p></div></body></html>";
        let frame = make_frame(html);
        let snapshot = DomSnapshot::from_frame(&frame);

        let div_id = snapshot.query_selector("div").unwrap();
        let p_id = snapshot.query_selector("p").unwrap();

        let div_node = snapshot.nodes.get(&div_id).unwrap();
        let p_node = snapshot.nodes.get(&p_id).unwrap();

        assert!(
            div_node.children.contains(&p_id),
            "div should have p as child"
        );
        assert_eq!(p_node.parent, Some(div_id), "p's parent should be div");
    }

    // -----------------------------------------------------------------------
    // Structured data extraction tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_headings_extraction() {
        let html = r#"<html><body>
            <h1>Main Title</h1>
            <h2>Subtitle</h2>
            <h3>Section</h3>
            <p>Not a heading</p>
        </body></html>"#;
        let frame = make_frame(html);
        let snapshot = DomSnapshot::from_frame(&frame);

        let headings = snapshot.headings();
        assert_eq!(headings.len(), 3, "should find 3 headings");
        assert_eq!(headings[0].0, 1, "first heading should be h1");
        assert!(headings[0].1.contains("Main Title"));
        assert_eq!(headings[1].0, 2, "second heading should be h2");
        assert_eq!(headings[2].0, 3, "third heading should be h3");
    }

    #[test]
    fn test_headings_with_aria_role() {
        let html = r#"<html><body>
            <span role="heading" aria-level="2">ARIA Heading</span>
            <div role="heading">Default Level</div>
        </body></html>"#;
        let frame = make_frame(html);
        let snapshot = DomSnapshot::from_frame(&frame);

        let headings = snapshot.headings();
        assert_eq!(headings.len(), 2, "should find 2 ARIA headings");
        assert_eq!(headings[0].0, 2, "first should be level 2");
        assert!(headings[0].1.contains("ARIA Heading"));
        assert_eq!(headings[1].0, 2, "default level should be 2");
    }

    #[test]
    fn test_links_extraction() {
        let html = r#"<html><body>
            <a href="https://example.com">Example</a>
            <a href="/about">About Us</a>
            <a>No href</a>
        </body></html>"#;
        let frame = make_frame(html);
        let snapshot = DomSnapshot::from_frame(&frame);

        let links = snapshot.links();
        assert_eq!(links.len(), 3, "should find 3 links");
        assert_eq!(links[0].1, "https://example.com");
        assert!(links[0].0.contains("Example"));
        assert_eq!(links[1].1, "/about");
        assert_eq!(links[2].1, "", "link without href should have empty string");
    }

    #[test]
    fn test_meta_tags_extraction() {
        let html = r#"<html><head>
            <meta name="description" content="A test page">
            <meta property="og:title" content="OG Title">
            <meta name="viewport" content="width=device-width">
        </head><body></body></html>"#;
        let frame = make_frame(html);
        let snapshot = DomSnapshot::from_frame(&frame);

        let meta = snapshot.meta_tags();
        assert_eq!(meta.get("description").unwrap(), "A test page");
        assert_eq!(meta.get("og:title").unwrap(), "OG Title");
        assert_eq!(meta.get("viewport").unwrap(), "width=device-width");
    }

    #[test]
    fn test_structured_data_empty_page() {
        let snapshot = DomSnapshot::empty();
        assert!(snapshot.headings().is_empty());
        assert!(snapshot.links().is_empty());
        assert!(snapshot.meta_tags().is_empty());
    }

    #[test]
    fn test_remove_subtree_detaches_and_purges() {
        let html = r#"<html><body>
            <div id="root"><p id="child">a<span id="leaf">b</span></p></div>
        </body></html>"#;
        let frame = make_frame(html);
        let mut snapshot = DomSnapshot::from_frame(&frame);
        let root = snapshot.get_element_by_id("root").expect("root div");

        let pre_count = snapshot.nodes.len();
        snapshot.remove_subtree(root);
        let post_count = snapshot.nodes.len();

        // 4 nodes gone (root + p + span + leaf text node… actually <span>'s
        // text child counts too). Just assert strict shrinkage + no orphans.
        assert!(post_count < pre_count, "snapshot shrinks after remove");
        assert!(!snapshot.nodes.contains_key(&root));
        // `id_index` is not eagerly invalidated here — that's the
        // `bump_revision` contract — so assert directly against `nodes`.
        let still_present = snapshot
            .nodes
            .values()
            .any(|n| n.attributes.get("id").map(|s| s.as_str()) == Some("child"));
        assert!(!still_present, "<p id=child> purged from nodes");
        let still_present = snapshot
            .nodes
            .values()
            .any(|n| n.attributes.get("id").map(|s| s.as_str()) == Some("leaf"));
        assert!(!still_present, "<span id=leaf> purged from nodes");
    }

    // ── OXI.getInteractiveElements ──────────────────────────────────────────

    #[test]
    fn test_password_input_values_masked_in_snapshot() {
        let snapshot = parse_html_fragment_to_snapshot(
            r#"<html><body>
                <input type="password" value="hunter2" name="pw">
                <input type="text" value="typed" name="q">
            </body></html>"#,
        );
        let pw = snapshot.query_selector("input[type=password]").unwrap();
        let text = snapshot.query_selector("input[type=text]").unwrap();
        assert_eq!(
            snapshot.nodes[&pw]
                .attributes
                .get("value")
                .map(String::as_str),
            Some(crate::security::redact::REDACTED),
            "password value must not survive into the snapshot"
        );
        assert_eq!(
            snapshot.nodes[&text]
                .attributes
                .get("value")
                .map(String::as_str),
            Some("typed"),
            "non-password values are preserved"
        );
        // Accessible-name value fallback must not surface password values.
        assert_eq!(snapshot.accessible_name(pw), None);
        assert_eq!(snapshot.accessible_name(text), Some("typed".into()));
        // HTML serialization (screenshot raster feed) carries the marker too.
        let html = snapshot.to_html();
        assert!(!html.contains("hunter2"), "{html}");
        assert!(html.contains(crate::security::redact::REDACTED), "{html}");
    }

    #[test]
    fn test_password_input_without_value_has_no_marker_noise() {
        let snapshot = parse_html_fragment_to_snapshot(
            r#"<html><body><input type="password" name="pw"></body></html>"#,
        );
        let pw = snapshot.query_selector("input").unwrap();
        assert!(
            !snapshot.nodes[&pw].attributes.contains_key("value"),
            "no value attribute in, no marker injected"
        );
    }

    #[test]
    fn test_interactive_elements_by_tag() {
        let html = r#"<html><body>
            <a href="/l">link text</a>
            <button disabled>go</button>
            <input type="text" name="q" placeholder="search" aria-label="query">
            <select name="s"><option value="1">one</option></select>
            <textarea name="t"></textarea>
        </body></html>"#;
        let snapshot = DomSnapshot::from_frame(&make_frame(html));

        let els = snapshot.interactive_elements();
        assert_eq!(els.len(), 5, "one entry per interactive tag");
        assert!(els.iter().enumerate().all(|(i, e)| e.index == i));

        assert_eq!(els[0].tag, "a");
        assert_eq!(els[0].role, "link");
        assert_eq!(els[0].href.as_deref(), Some("/l"));
        assert_eq!(els[0].text, "link text");

        assert_eq!(els[1].tag, "button");
        assert_eq!(els[1].role, "button");
        assert!(els[1].disabled, "disabled attribute flagged");

        assert_eq!(els[2].tag, "input");
        assert_eq!(els[2].input_type.as_deref(), Some("text"));
        assert_eq!(els[2].name_attr.as_deref(), Some("q"));
        assert_eq!(els[2].placeholder.as_deref(), Some("search"));
        assert_eq!(els[2].aria_label.as_deref(), Some("query"));
        assert_eq!(els[2].name.as_deref(), Some("query"), "aria-label wins");
        assert!(!els[2].disabled);

        assert_eq!(els[3].tag, "select");
        assert_eq!(els[3].role, "listbox");
        assert_eq!(els[3].name_attr.as_deref(), Some("s"));

        assert_eq!(els[4].tag, "textarea");
        assert_eq!(els[4].role, "textbox");
        assert!(els[4].text.is_empty(), "empty textarea has no text");
    }

    #[test]
    fn test_accessible_name_priority() {
        // 1. aria-labelledby beats aria-label.
        let snapshot = DomSnapshot::from_frame(&make_frame(
            r#"<html><body>
                <span id="lbl">full label</span>
                <button aria-labelledby="lbl" aria-label="short">text</button>
            </body></html>"#,
        ));
        assert_eq!(
            snapshot.accessible_name(snapshot.query_selector("button").unwrap()),
            Some("full label".into())
        );

        // 1b. multiple labelledby ids join with spaces; unknown ids skipped.
        let snapshot = DomSnapshot::from_frame(&make_frame(
            r#"<html><body>
                <span id="a">first</span>
                <span id="b">second</span>
                <button aria-labelledby="a missing b">text</button>
            </body></html>"#,
        ));
        assert_eq!(
            snapshot.accessible_name(snapshot.query_selector("button").unwrap()),
            Some("first second".into())
        );

        // 2. aria-label beats img alt / placeholder / text.
        let snapshot = DomSnapshot::from_frame(&make_frame(
            r#"<html><body>
                <img alt="photo" role="button" aria-label="close">
            </body></html>"#,
        ));
        assert_eq!(
            snapshot.accessible_name(snapshot.query_selector("img").unwrap()),
            Some("close".into())
        );

        // 3. img alt beats text (none here) and placeholder.
        let snapshot = DomSnapshot::from_frame(&make_frame(
            r#"<html><body><img alt="photo" role="button"></body></html>"#,
        ));
        assert_eq!(
            snapshot.accessible_name(snapshot.query_selector("img").unwrap()),
            Some("photo".into())
        );

        // 4. input placeholder beats value and name attribute.
        let snapshot = DomSnapshot::from_frame(&make_frame(
            r#"<html><body>
                <input value="typed" name="q" placeholder="search here">
            </body></html>"#,
        ));
        assert_eq!(
            snapshot.accessible_name(snapshot.query_selector("input").unwrap()),
            Some("search here".into())
        );

        // 5. value beats text when no placeholder.
        let snapshot = DomSnapshot::from_frame(&make_frame(
            r#"<html><body><input value="typed"></body></html>"#,
        ));
        assert_eq!(
            snapshot.accessible_name(snapshot.query_selector("input").unwrap()),
            Some("typed".into())
        );

        // 6. descendant text is the final fallback.
        let snapshot = DomSnapshot::from_frame(&make_frame(
            r#"<html><body><button><span>click </span>me</button></body></html>"#,
        ));
        assert_eq!(
            snapshot.accessible_name(snapshot.query_selector("button").unwrap()),
            Some("click  me".into()), // deep text joins text nodes with spaces
        );

        // Empty/whitespace candidates collapse to None.
        let snapshot = DomSnapshot::from_frame(&make_frame(
            r#"<html><body><button>   </button></body></html>"#,
        ));
        assert_eq!(
            snapshot.accessible_name(snapshot.query_selector("button").unwrap()),
            None
        );

        // 80-char cap.
        let long = "x".repeat(120);
        let snapshot = DomSnapshot::from_frame(&make_frame(&format!(
            r#"<html><body><button>{long}</button></body></html>"#
        )));
        assert_eq!(
            snapshot.accessible_name(snapshot.query_selector("button").unwrap()),
            Some("x".repeat(80))
        );
    }

    #[test]
    fn test_interactive_element_name_on_wire() {
        let snapshot = DomSnapshot::from_frame(&make_frame(
            r#"<html><body><button name="b" aria-label="go">text</button></body></html>"#,
        ));
        let el = &snapshot.interactive_elements()[0];
        assert_eq!(el.name.as_deref(), Some("go"));
        assert_eq!(el.name_attr.as_deref(), Some("b"));
        let json = serde_json::to_value(el).unwrap();
        assert_eq!(json["name"], "go");
        assert_eq!(json["nameAttr"], "b");
    }

    #[test]
    fn test_interactive_role_onclick_tabindex_detection() {
        let html = r#"<html><body>
            <span role="button" id="rb">press</span>
            <span role="link">home</span>
            <div role="tab">tab one</div>
            <div role="checkbox" aria-label="check"></div>
            <span role="radio"></span>
            <div onclick="go()">clickable</div>
            <div tabindex="0">focusable</div>
            <div tabindex="-1">not in tab order</div>
        </body></html>"#;
        let snapshot = DomSnapshot::from_frame(&make_frame(html));

        let els = snapshot.interactive_elements();
        // tabindex="-1" must be excluded.
        assert_eq!(els.len(), 7, "got: {:?}", els.iter().map(|e| &e.selector));

        let roles: Vec<&str> = els.iter().map(|e| e.role.as_str()).collect();
        assert_eq!(
            roles,
            vec![
                "button", "link", "tab", "checkbox", "radio", "generic", "generic"
            ]
        );
        // Non-interactive tags get a generic role unless an explicit one exists.
        assert_eq!(els[5].tag, "div");
        assert_eq!(els[6].tag, "div");
    }

    #[test]
    fn test_interactive_excludes_non_interactive() {
        let html = r#"<html><body>
            <p>plain paragraph</p>
            <div>plain div</div>
            <span>plain span</span>
            <span role="heading">not interactive</span>
            <div role="article">also not</div>
            <h1>heading</h1>
        </body></html>"#;
        let snapshot = DomSnapshot::from_frame(&make_frame(html));
        assert!(
            snapshot.interactive_elements().is_empty(),
            "no false positives on static content"
        );
    }

    #[test]
    fn test_interactive_selector_uniqueness() {
        use std::collections::HashSet;

        let html = r#"<html><body>
            <button>a</button>
            <button>b</button>
            <div>
                <button>c</button>
                <button>d</button>
            </div>
            <span id="uniq" onclick="">x</span>
        </body></html>"#;
        let snapshot = DomSnapshot::from_frame(&make_frame(html));

        let els = snapshot.interactive_elements();
        assert_eq!(els.len(), 5);

        let selectors: Vec<&str> = els.iter().map(|e| e.selector.as_str()).collect();
        assert_eq!(selectors[0], "html > body > button:nth-of-type(1)");
        assert_eq!(selectors[1], "html > body > button:nth-of-type(2)");
        assert_eq!(selectors[2], "html > body > div > button:nth-of-type(1)");
        assert_eq!(selectors[3], "html > body > div > button:nth-of-type(2)");
        assert_eq!(selectors[4], "#uniq", "id short-circuits the path");

        assert_eq!(
            selectors.iter().collect::<HashSet<_>>().len(),
            selectors.len(),
            "every selector must be unique"
        );
    }

    #[test]
    fn test_css_selector_path_escapes_unsafe_ids() {
        let html = r#"<html><body>
            <button id="a.b">one</button>
            <button id="1x">two</button>
            <button id="he said &quot;hi&quot;">three</button>
            <button id="normal-id">four</button>
        </body></html>"#;
        let snapshot = DomSnapshot::from_frame(&make_frame(html));

        let els = snapshot.interactive_elements();
        assert_eq!(els.len(), 4);
        let selectors: Vec<&str> = els.iter().map(|e| e.selector.as_str()).collect();

        // `.` would parse as a class separator in `#a.b`.
        assert_eq!(
            selectors[0], r#"[id="a.b"]"#,
            "dot in id uses attribute form"
        );
        // A leading digit is invalid CSS in `#1x`.
        assert_eq!(
            selectors[1], r#"[id="1x"]"#,
            "leading digit uses attribute form"
        );
        // Double quotes in the id are backslash-escaped inside the attribute form.
        assert_eq!(selectors[2], r#"[id="he said \"hi\""]"#);
        // A valid identifier keeps the `#` short form.
        assert_eq!(
            selectors[3], "#normal-id",
            "valid identifier keeps # short form"
        );

        // The attribute form must actually resolve through query_selector.
        assert!(
            snapshot.query_selector(r#"[id="a.b"]"#).is_some(),
            "[id=\"a.b\"] must match the element"
        );
        assert!(
            snapshot.query_selector(r#"[id="1x"]"#).is_some(),
            "[id=\"1x\"] must match the element"
        );
        assert!(
            snapshot.query_selector("#normal-id").is_some(),
            "#normal-id must match the element"
        );
    }

    #[test]
    fn test_is_valid_css_identifier() {
        assert!(is_valid_css_identifier("a"));
        assert!(is_valid_css_identifier("uniq-1_x"));
        assert!(is_valid_css_identifier("--custom"));
        assert!(is_valid_css_identifier("--"));
        assert!(!is_valid_css_identifier("a.b"));
        assert!(!is_valid_css_identifier("1x"));
        assert!(!is_valid_css_identifier("-x"));
        assert!(!is_valid_css_identifier("a b"));
        assert!(!is_valid_css_identifier("a:b"));
        assert!(!is_valid_css_identifier(""));
    }

    #[test]
    fn test_interactive_text_trimmed_to_80_chars() {
        let long = "x".repeat(200);
        let html = format!(r#"<html><body><button>{long}</button></body></html>"#);
        let snapshot = DomSnapshot::from_frame(&make_frame(&html));

        let els = snapshot.interactive_elements();
        assert_eq!(els.len(), 1);
        assert_eq!(els[0].text.chars().count(), 80);
        assert!(
            els[0].text.starts_with("xxxx"),
            "text is the visible prefix"
        );
    }

    #[test]
    fn test_fingerprint_consistency() {
        let html = r#"<html><body><button id="b">Hit</button></body></html>"#;
        let snapshot = DomSnapshot::from_frame(&make_frame(html));
        let button = snapshot.query_selector("button").unwrap();
        let orig = snapshot.fingerprint(button);

        // Deterministic within a snapshot.
        assert_eq!(orig, snapshot.fingerprint(button));

        // Deterministic across independent rebuilds of identical content.
        let again = DomSnapshot::from_frame(&make_frame(html));
        assert_eq!(
            orig,
            again.fingerprint(again.query_selector("button").unwrap())
        );

        // Attribute drift changes the fingerprint.
        let attr_changed = DomSnapshot::from_frame(&make_frame(
            r#"<html><body><button id="b" data-x="1">Hit</button></body></html>"#,
        ));
        assert_ne!(
            orig,
            attr_changed.fingerprint(attr_changed.query_selector("button").unwrap())
        );

        // Text drift changes the fingerprint.
        let text_changed = DomSnapshot::from_frame(&make_frame(
            r#"<html><body><button id="b">Hit Me</button></body></html>"#,
        ));
        assert_ne!(
            orig,
            text_changed.fingerprint(text_changed.query_selector("button").unwrap())
        );

        // Missing nodes fingerprint to the empty string.
        assert_eq!(snapshot.fingerprint(999_999), "");
    }

    #[test]
    fn test_aria_role_priority() {
        let snapshot = DomSnapshot::from_frame(&make_frame(
            r#"<html><body>
                <div role="button" id="explicit">X</div>
                <nav id="site"></nav>
                <header id="top"></header>
                <footer id="bottom"></footer>
                <main id="content"></main>
                <aside id="side"></aside>
                <img src="x.png" alt="Logo">
                <a href="/">Home</a>
                <input type="checkbox">
                <select></select>
                <textarea></textarea>
                <div id="plain"></div>
            </body></html>"#,
        ));
        let role = |sel: &str| snapshot.aria_role(snapshot.query_selector(sel).unwrap());
        // Explicit `role` attribute wins over everything.
        assert_eq!(role("#explicit"), "button");
        // Landmark tag mappings.
        assert_eq!(role("#site"), "navigation");
        assert_eq!(role("#top"), "banner");
        assert_eq!(role("#bottom"), "contentinfo");
        assert_eq!(role("#content"), "main");
        assert_eq!(role("#side"), "complementary");
        // Interactive implicit roles (compute_interactive_role reuse).
        assert_eq!(role("img"), "img");
        assert_eq!(role("a"), "link");
        assert_eq!(role("input"), "checkbox");
        assert_eq!(role("select"), "listbox");
        assert_eq!(role("textarea"), "textbox");
        // Fallback for plain containers.
        assert_eq!(role("#plain"), "generic");
    }

    #[test]
    fn test_render_aria_yaml_golden() {
        let html = r#"<html><head><title>Golden</title></head>
            <body>
                <nav><a href="/">Home</a></nav>
                <button>Submit</button>
                <input type="checkbox" checked aria-label="Accept">
            </body></html>"#;
        let snapshot = DomSnapshot::from_frame(&make_frame(html));

        let mut next = 1u64;
        let yaml = snapshot.render_aria_yaml(&mut |node_id| {
            let node = snapshot.nodes.get(&node_id)?;
            if !is_interactive_element(node) {
                return None;
            }
            let r = format!("e{next}");
            next += 1;
            Some(r)
        });

        assert_eq!(
            yaml,
            "- navigation:\n\
             \x20 - link \"Home\" [ref=e1]\n\
             - button \"Submit\" [ref=e2]\n\
             - checkbox \"Accept\" [checked] [ref=e3]\n"
        );
    }
}
