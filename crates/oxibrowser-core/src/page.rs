//! Page — container for a document and its frames.
//!

//! Page — container for a document and its frames.

use crate::error::{CoreError, Result};
use crate::frame::Frame;
use crate::network::resource::Resource;
use std::sync::atomic::{AtomicU32, Ordering};
use tracing::info;
use url::Url;

/// Unique page ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageId(u32);

impl PageId {
    fn next() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(1);
        Self(COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

impl std::fmt::Display for PageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "page-{}", self.0)
    }
}

/// A loaded web page with its DOM tree, resources, and metadata.
pub struct Page {
    /// Unique ID.
    id: PageId,
    /// Page URL.
    url: Url,
    /// Root frame (the main document).
    root_frame: Frame,
    /// HTTP status code.
    status: u16,
    /// Content-Type of the response.
    content_type: String,
    /// Loaded sub-resources.
    resources: Vec<Resource>,
    /// Page title (extracted from <title>).
    title: Option<String>,
}

impl Page {
    /// Create a page from HTML content.
    #[tracing::instrument(skip(html), err)]
    pub async fn from_html(
        url: Url,
        html: &str,
        status: u16,
        content_type: String,
    ) -> Result<Self> {
        let id = PageId::next();
        let root_frame = Frame::from_html(url.clone(), html).await?;

        // Extract title from the frame's DOM
        let title = root_frame.extract_title();

        info!(id = %id, url = %url, status, "page created");

        Ok(Self {
            id,
            url,
            root_frame,
            status,
            content_type,
            resources: Vec::new(),
            title,
        })
    }

    /// Get the page URL.
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// Get the page title.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Get the page's HTML content.
    pub fn content(&self) -> &str {
        self.root_frame.html()
    }

    /// Get the root frame.
    pub fn root_frame(&self) -> &Frame {
        &self.root_frame
    }

    /// Get the root frame mutably.
    pub fn root_frame_mut(&mut self) -> &mut Frame {
        &mut self.root_frame
    }

    /// Get the HTTP status code.
    pub fn status(&self) -> u16 {
        self.status
    }

    /// Get the Content-Type.
    pub fn content_type(&self) -> &str {
        &self.content_type
    }

    /// Get loaded sub-resources.
    pub fn resources(&self) -> &[Resource] {
        &self.resources
    }

    /// Add a loaded resource.
    pub fn add_resource(&mut self, resource: Resource) {
        self.resources.push(resource);
    }

    /// Render the page to a Markdown representation.
    pub fn to_markdown(&self) -> String {
        self.root_frame.to_markdown()
    }

    /// Render the page as text/ASCII art for terminal output.
    pub fn to_text_screenshot(&self) -> String {
        let snapshot = self.root_frame.to_dom_snapshot();
        crate::css::render_to_text(&snapshot)
    }

    /// Render the page as a PNG screenshot.
    ///
    /// Renders the page HTML through the Blitz pipeline (Stylo CSS cascade +
    /// Taffy layout + vello_cpu paint) as a full-page screenshot. This is a
    /// real CSS render — a successor to the legacy bitmap-font renderer.
    pub fn to_screenshot_png(&self, viewport_width: u32) -> Result<Vec<u8>> {
        let html = self.root_frame.html();
        let viewport = oxibrowser_render::Viewport {
            width: viewport_width.max(64),
            height: 800,
            scale: 1.0,
        };
        let mut doc = oxibrowser_render::RenderDocument::from_html(
            html,
            Some(self.url.as_str()),
            viewport,
        )
        .map_err(|e| CoreError::ScreenshotError(e.to_string()))?;
        doc.capture_png(&oxibrowser_render::CaptureOpts {
            full_page: true,
            ..Default::default()
        })
        .map_err(|e| CoreError::ScreenshotError(e.to_string()))
    }

    /// Render the page's **live** (post-JS) DOM as a full-page PNG screenshot.
    ///
    /// Serializes the current DOM snapshot — which reflects JS-driven mutations
    /// applied during page interaction — to HTML and renders it through the
    /// Blitz pipeline. The correct path for dynamic/SPA content. Falls back to
    /// the page source HTML if the serialized DOM is empty.
    pub fn to_screenshot_png_live(&self, viewport_width: u32) -> Result<Vec<u8>> {
        let snapshot = self.root_frame.to_dom_snapshot();
        let mut html_buf = String::new();
        if let Some(root) = snapshot.nodes.get(&snapshot.root_id) {
            crate::js::dom_serializer::serialize_node(root, &snapshot, &mut html_buf);
        }
        let html: &str = if html_buf.trim().is_empty() {
            self.root_frame.html()
        } else {
            html_buf.as_str()
        };

        let viewport = oxibrowser_render::Viewport {
            width: viewport_width.max(64),
            height: 800,
            scale: 1.0,
        };
        let mut doc = oxibrowser_render::RenderDocument::from_html(
            html,
            Some(self.url.as_str()),
            viewport,
        )
        .map_err(|e| CoreError::ScreenshotError(e.to_string()))?;
        doc.capture_png(&oxibrowser_render::CaptureOpts {
            full_page: true,
            ..Default::default()
        })
        .map_err(|e| CoreError::ScreenshotError(e.to_string()))
    }

    /// Get the page ID.
    pub fn id(&self) -> PageId {
        self.id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::resource::ResourceType;
    use bytes::Bytes;

    fn make_test_html(title: &str) -> String {
        format!(
            "<!DOCTYPE html><html><head><title>{title}</title></head><body><p>Hello</p></body></html>"
        )
    }

    #[tokio::test]
    async fn test_page_from_html_extracts_title() {
        let url = Url::parse("https://example.com/").unwrap();
        let html = make_test_html("Test Page Title");
        let page = Page::from_html(url, &html, 200, "text/html".to_string())
            .await
            .unwrap();

        assert_eq!(page.title(), Some("Test Page Title"));
    }

    #[tokio::test]
    async fn test_page_content_returns_html() {
        let url = Url::parse("https://example.com/").unwrap();
        let html = make_test_html("Content Test");
        let page = Page::from_html(url, &html, 200, "text/html".to_string())
            .await
            .unwrap();

        let content = page.content();
        assert!(
            content.contains("Hello"),
            "content should contain body text"
        );
        assert!(
            content.contains("<html"),
            "content should contain HTML tags"
        );
    }

    #[tokio::test]
    async fn test_page_to_text_screenshot_non_empty() {
        let url = Url::parse("https://example.com/").unwrap();
        let html = make_test_html("Screenshot Test");
        let page = Page::from_html(url, &html, 200, "text/html".to_string())
            .await
            .unwrap();

        let text = page.to_text_screenshot();
        assert!(!text.is_empty(), "text screenshot should not be empty");
    }

    #[tokio::test]
    async fn test_page_to_screenshot_png_valid_header() {
        let url = Url::parse("https://example.com/").unwrap();
        let html =
            "<!DOCTYPE html><html><head><title>PNG</title></head><body><p>X</p></body></html>";
        let page = Page::from_html(url, html, 200, "text/html".to_string())
            .await
            .unwrap();

        let png = page
            .to_screenshot_png(800)
            .expect("PNG generation should succeed");
        // PNG magic header: 137 80 78 71 13 10 26 10
        assert!(png.len() > 8, "PNG data should be more than 8 bytes");
        assert_eq!(
            &png[0..4],
            &[0x89, 0x50, 0x4E, 0x47],
            "should start with PNG magic"
        );
    }

    #[tokio::test]
    async fn test_page_to_screenshot_png_renders_styled_content() {
        // Exercises the full Blitz pipeline (Stylo + Taffy + vello_cpu) via the
        // page's HTML, asserting real CSS-rendered content (not a blank image).
        let url = Url::parse("https://example.com/").unwrap();
        let html = r#"<!DOCTYPE html><html><head><style>
            body { margin: 0; background: #ffffff; }
            h1 { color: #ff0000; font-size: 32px; }
            .box { width: 100px; height: 100px; background: #0000ff; }
        </style></head><body>
            <h1>Red Heading</h1>
            <div class="box"></div>
        </body></html>"#;
        let page = Page::from_html(url, html, 200, "text/html".to_string())
            .await
            .unwrap();
        let png = page
            .to_screenshot_png(400)
            .expect("Blitz PNG generation should succeed");

        // Decode and verify there is substantial non-white content (the red
        // heading and the blue box), proving the CSS render actually engaged.
        let img = image::load_from_memory(&png)
            .expect("decode screenshot png")
            .to_rgba8();
        let non_white = img
            .pixels()
            .filter(|p| p.0 != [255, 255, 255, 255])
            .count();
        assert!(
            non_white > 200,
            "expected substantial CSS-rendered content, got {non_white} non-white px"
        );
    }

    #[tokio::test]
    async fn test_page_to_screenshot_png_live_renders_dom() {
        // The live path serializes the DOM snapshot and re-renders it. Asserts
        // the serialize → re-parse → Blitz render round-trip produces styled
        // content (proving post-JS DOM can be captured).
        let url = Url::parse("https://example.com/").unwrap();
        let html = r#"<!DOCTYPE html><html><head><style>
            .box { width: 80px; height: 80px; background: #00aa00; }
            h1 { color: #cc0000; }
        </style></head><body><h1>Live DOM</h1><div class="box"></div></body></html>"#;
        let page = Page::from_html(url, html, 200, "text/html".to_string())
            .await
            .unwrap();
        let png = page
            .to_screenshot_png_live(400)
            .expect("live DOM render should succeed");
        let img = image::load_from_memory(&png)
            .expect("decode live screenshot png")
            .to_rgba8();
        let non_white = img
            .pixels()
            .filter(|p| p.0 != [255, 255, 255, 255])
            .count();
        assert!(
            non_white > 100,
            "live DOM render should contain styled content, got {non_white} px"
        );
    }

    #[tokio::test]
    async fn test_page_add_resource_tracks_resources() {
        let url = Url::parse("https://example.com/").unwrap();
        let html = make_test_html("Resource Test");
        let mut page = Page::from_html(url, &html, 200, "text/html".to_string())
            .await
            .unwrap();

        assert!(page.resources().is_empty(), "initially no resources");

        let resource = Resource {
            url: "https://example.com/style.css".to_string(),
            resource_type: ResourceType::Stylesheet,
            status: 200,
            mime_type: "text/css".to_string(),
            body: Bytes::from_static(b"body { color: red; }"),
            loaded_at: std::time::Instant::now(),
        };

        page.add_resource(resource);
        assert_eq!(page.resources().len(), 1);
        assert_eq!(page.resources()[0].url, "https://example.com/style.css");
        assert_eq!(page.resources()[0].resource_type, ResourceType::Stylesheet);
    }
}
