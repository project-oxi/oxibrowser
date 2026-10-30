//! Frame — a document frame within a page.
//!
//! Holds the document URL, original HTML, a [`DomSnapshot`] of the parsed
//! document, and child frames. The root Frame represents the main document.
//!
//! Note: the live (post-JS) DOM lives on the JS thread as a `RenderDocument`;
//! this snapshot is the static, Send view used by the CLI/extract path and as
//! the initial seed for the JS runtime. CDP/OXI reads use the live
//! `Session::dom_snapshot()`.

use crate::error::{CoreError, Result};
use crate::js::dom_snapshot::{DomSnapshot, ResourceUrl};
use std::sync::atomic::{AtomicU32, Ordering};
use url::Url;

/// Unique frame ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrameId(u32);

impl FrameId {
    fn next() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(1);
        Self(COUNTER.fetch_add(1, Ordering::Relaxed))
    }
    pub(crate) fn from_string(s: &str) -> Option<Self> {
        let rest = s.strip_prefix("frame-")?;
        rest.parse::<u32>().ok().map(Self)
    }
}

impl std::fmt::Display for FrameId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "frame-{}", self.0)
    }
}

/// A document frame with its parsed DOM snapshot.
pub struct Frame {
    /// Unique ID.
    id: FrameId,
    /// Frame URL.
    url: Url,
    /// Original HTML source.
    html: String,
    /// Parsed DOM snapshot (Send view; the live DOM is the RenderDocument).
    snapshot: DomSnapshot,
    /// Child frames (iframes).
    children: Vec<Frame>,
    /// Native owner in the live parent document, never a positional match.
    owner: Option<crate::js::runtime::frames::FrameOwner>,
    /// DOM version counter (for cache invalidation).
    dom_version: u64,
}

/// Build a DomSnapshot from HTML by parsing it through the (transient,
/// `!Send`) `RenderDocument` and converting. Kept as a sync helper so the
/// `!Send` value never crosses an await point.
fn snapshot_from_html(url: &Url, html: &str) -> Result<DomSnapshot> {
    let viewport = oxibrowser_render::Viewport::default();
    let rd = oxibrowser_render::RenderDocument::from_html(html, Some(url.as_str()), viewport)
        .map_err(|e| CoreError::PageError(e.to_string()))?;
    let title = rd
        .query_selector("title")
        .map(|id| rd.node_text(id))
        .unwrap_or_default();
    Ok(DomSnapshot::from_render_document(&rd, url.as_str(), &title))
}
impl Frame {
    /// Parse HTML into a Frame with its DOM snapshot.
    #[tracing::instrument(skip(html), err)]
    pub async fn from_html(url: Url, html: &str) -> Result<Self> {
        let id = FrameId::next();
        let snapshot = snapshot_from_html(&url, html)?;
        tracing::debug!(id = %id, url = %url, "frame created");
        Ok(Self {
            id,
            url,
            html: html.to_string(),
            snapshot,
            children: Vec::new(),
            owner: None,
            dom_version: 0,
        })
    }

    /// Get the frame ID.
    pub fn id(&self) -> FrameId {
        self.id
    }

    /// Get the frame URL.
    pub fn url(&self) -> &Url {
        &self.url
    }

    pub(crate) fn set_same_document_url(&mut self, url: Url) {
        self.snapshot.url = url.to_string();
        self.url = url;
    }

    /// Get the raw HTML.
    pub fn html(&self) -> &str {
        &self.html
    }

    /// Get the parsed DOM snapshot.
    pub fn document(&self) -> &DomSnapshot {
        &self.snapshot
    }

    pub fn owner(&self) -> Option<&crate::js::runtime::frames::FrameOwner> {
        self.owner.as_ref()
    }

    pub(crate) fn set_owner(&mut self, owner: crate::js::runtime::frames::FrameOwner) {
        self.owner = Some(owner);
    }

    /// Get child frames.
    pub fn children(&self) -> &[Frame] {
        &self.children
    }

    /// Add a child frame.
    pub fn add_child(&mut self, frame: Frame) {
        self.dom_version += 1;
        self.children.push(frame);
    }

    /// Find a frame by id, returning a mutable borrow of the matching node
    /// (this frame or any descendant). Used by iframe population to attach
    /// nested child frames to the correct parent in the tree, not just to
    /// the root.
    pub fn find_mut_by_id(&mut self, target: FrameId) -> Option<&mut Frame> {
        if self.id == target {
            return Some(self);
        }
        for child in self.children.iter_mut() {
            if let Some(found) = child.find_mut_by_id(target) {
                return Some(found);
            }
        }
        None
    }

    /// Find a frame by id, immutable variant (used during frame-tree walks
    /// that must not hold a mutable borrow across an `await`).
    pub fn find_by_id(&self, target: FrameId) -> Option<&Frame> {
        if self.id == target {
            return Some(self);
        }
        for child in self.children.iter() {
            if let Some(found) = child.find_by_id(target) {
                return Some(found);
            }
        }
        None
    }

    /// Same as `find_by_id` but accepts the printed form (`frame-N`) so the
    /// session's `frame_contexts` map (keyed by the string id) can be looked
    /// up during child-frame script prefetch without exposing FrameId
    /// internals to that module.
    pub fn find_by_frame_id_str(&self, id_str: &str) -> Option<&Frame> {
        let target = FrameId::from_string(id_str)?;
        self.find_by_id(target)
    }

    /// Get the DOM version (for cache invalidation).
    pub fn dom_version(&self) -> u64 {
        self.dom_version
    }

    /// Extract the page title.
    pub fn extract_title(&self) -> Option<String> {
        if !self.snapshot.title.is_empty() {
            return Some(self.snapshot.title.clone());
        }
        // Fallback: extract from raw HTML.
        let html = &self.html;
        let start = html.find("<title>").map(|i| i + 7)?;
        let end = html.find("</title>")?;
        if start < end {
            Some(html[start..end].trim().to_string())
        } else {
            None
        }
    }

    /// Convert the frame's content to a Markdown string.
    pub fn to_markdown(&self) -> String {
        crate::css::render_to_markdown(&self.snapshot)
    }

    /// Query the DOM using a CSS selector.
    pub fn query_selector(&self, selector: &str) -> Option<u32> {
        self.snapshot.query_selector(selector)
    }

    /// Extract sub-resource URLs from the DOM.
    pub fn extract_resource_urls(&self) -> Vec<ResourceUrl> {
        self.snapshot.extract_resource_urls()
    }

    /// Extract iframe `src` URLs from this frame's document.
    pub fn iframe_srcs(&self) -> Vec<String> {
        self.snapshot.iframe_srcs()
    }

    /// Extract executable `<script>` elements in document order (Phase 1).
    /// External scripts return an empty `source`; the caller fetches the body.
    pub fn extract_scripts(&self) -> Vec<crate::js::dom_snapshot::ScriptSource> {
        self.snapshot.extract_scripts()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_frame(html: &str) -> Frame {
        let url = Url::parse("https://example.com").unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(Frame::from_html(url, html)).unwrap()
    }

    #[test]
    fn test_iframe_srcs() {
        let html = r#"<html><body>
            <iframe src="/embed.html"></iframe>
            <iframe src="https://other.com/widget"></iframe>
            <iframe></iframe>
        </body></html>"#;
        let frame = make_frame(html);
        let srcs = frame.iframe_srcs();

        assert_eq!(srcs.len(), 2, "should find 2 iframes with src");
        assert!(srcs.contains(&"/embed.html".to_string()));
        assert!(srcs.contains(&"https://other.com/widget".to_string()));
    }

    #[test]
    fn test_iframe_srcs_empty() {
        let html = "<html><body><p>No iframes</p></body></html>";
        let frame = make_frame(html);
        assert!(frame.iframe_srcs().is_empty(), "should find no iframes");
    }

    #[test]
    fn test_add_child_frame() {
        let html = "<html><body><p>Parent</p></body></html>";
        let mut frame = make_frame(html);
        assert_eq!(frame.children().len(), 0);

        let child = make_frame("<html><body><p>Child</p></body></html>");
        frame.add_child(child);
        assert_eq!(frame.children().len(), 1);
    }
}
