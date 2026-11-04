//! Integration tests for the rendering pipeline.
//!
//! These exercise the full HTML → Stylo cascade → Taffy layout → vello_cpu
//! paint → PNG path end to end, asserting observable contracts (PNG validity,
//! dimensions, presence of non-background content) rather than exact pixels.

use oxibrowser_render::{CaptureOpts, RenderDocument, Viewport, blank_png, png_to_pdf};

const HTML: &str = r#"<html>
<head><style>
  body { margin: 0; background: #ffffff; }
  h1 { color: red; font-size: 32px; }
  .box { width: 60px; height: 60px; background: #0000ff; }
</style></head>
<body>
  <h1>Test Heading</h1>
  <div class="box"></div>
</body>
</html>"#;

/// PNG 8-byte signature.
const PNG_SIG: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

#[test]
fn parsed_attributes_are_replaced_and_removed_without_namespace_duplicates() {
    let mut doc = RenderDocument::from_html(
        "<p id='label' data-label='old' aria-busy='true'>Loading</p>",
        None,
        Viewport {
            width: 320,
            height: 240,
            scale: 1.0,
        },
    )
    .unwrap();
    let node = doc.query_selector("#label").unwrap();
    doc.set_attribute(node, "data-label", "new");
    doc.set_attribute(node, "aria-busy", "false");
    assert_eq!(doc.node_attr(node, "data-label").as_deref(), Some("new"));
    assert_eq!(doc.node_attr(node, "aria-busy").as_deref(), Some("false"));
    assert_eq!(
        doc.node_attributes(node)
            .iter()
            .filter(|(name, _)| name == "data-label")
            .count(),
        1
    );
    doc.remove_attribute(node, "data-label");
    assert_eq!(doc.node_attr(node, "data-label"), None);
}

#[test]
fn renders_basic_html_to_valid_png() {
    let mut doc = RenderDocument::from_html(
        HTML,
        None,
        Viewport {
            width: 400,
            height: 200,
            scale: 1.0,
        },
    )
    .expect("from_html");

    let png = doc
        .capture_png(&CaptureOpts::default())
        .expect("capture_png");

    // Valid PNG with a real header.
    assert!(
        png.len() > 100,
        "png suspiciously small: {} bytes",
        png.len()
    );
    assert_eq!(
        &png[0..8],
        &PNG_SIG,
        "missing PNG signature — not a valid PNG"
    );

    // Decode and verify dimensions match the requested viewport.
    let decoder = png::Decoder::new(std::io::Cursor::new(&png));
    let mut reader = decoder.read_info().expect("decode png");
    let info = reader.info();
    assert_eq!(info.width, 400, "wrong width");
    assert_eq!(info.height, 200, "wrong height");

    let mut buf = vec![0u8; reader.output_buffer_size().expect("buffer size")];
    reader.next_frame(&mut buf).expect("read frame");

    // There must be non-white content: a red heading and a blue box.
    let non_white = buf
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|px| *px != &[255, 255, 255, 255])
        .count();
    assert!(
        non_white > 100,
        "expected substantial rendered content, only {non_white} non-white pixels"
    );
}

#[test]
fn empty_document_produces_blank_png() {
    let mut doc = RenderDocument::from_html(
        "<html><body></body></html>",
        None,
        Viewport {
            width: 100,
            height: 100,
            scale: 1.0,
        },
    )
    .expect("from_html");
    let png = doc
        .capture_png(&CaptureOpts::default())
        .expect("capture_png");
    assert_eq!(&png[0..8], &PNG_SIG);
    assert!(png.len() > 50);
}

#[test]
fn dom_api_create_query_and_mutate() {
    let html = r#"<html><body><div id="host">hello</div></body></html>"#;
    let mut doc = RenderDocument::from_html(html, None, Viewport::default()).expect("from_html");

    let host = doc.query_selector("#host").expect("host exists");
    assert_eq!(doc.node_text(host), "hello");
    assert_eq!(doc.tag_name(host).as_deref(), Some("div"));

    // Create + configure + attach a new element.
    let span = doc.create_element("span");
    doc.set_attribute(span, "id", "new");
    doc.set_attribute(span, "class", "pill");
    doc.set_text(span, "world");
    doc.append_child(host, span);

    // The new node is queryable and reflects the written attrs/text.
    assert_eq!(doc.query_selector("#new"), Some(span));
    assert_eq!(doc.node_attr(span, "class").as_deref(), Some("pill"));
    assert_eq!(doc.node_text(span), "world");

    // set_attribute replaces; remove_attribute clears.
    doc.set_attribute(span, "class", "badge");
    assert_eq!(doc.node_attr(span, "class").as_deref(), Some("badge"));
    doc.remove_attribute(span, "class");
    assert!(doc.node_attr(span, "class").is_none());
}

#[test]
fn mutation_reflected_in_capture() {
    // Start from an empty host, inject a red box via the DOM API, and confirm
    // the next capture paints it — proving the mutate -> resolve -> paint loop.
    let html = r#"<html><body><div id="host"></div></body></html>"#;
    let mut doc = RenderDocument::from_html(
        html,
        None,
        Viewport {
            width: 400,
            height: 300,
            scale: 1.0,
        },
    )
    .expect("from_html");

    let host = doc.query_selector("#host").expect("host exists");
    let box_id = doc.create_element("div");
    doc.set_inline_style(box_id, "width", "120px");
    doc.set_inline_style(box_id, "height", "120px");
    doc.set_inline_style(box_id, "background-color", "#ff0000");
    doc.append_child(host, box_id);

    let png = doc
        .capture_png(&CaptureOpts {
            full_page: true,
            ..Default::default()
        })
        .expect("capture_png after mutation");

    // Decode and count red-ish pixels (the injected box).
    let decoder = png::Decoder::new(std::io::Cursor::new(&png));
    let mut reader = decoder.read_info().expect("decode png");
    let mut buf = vec![0u8; reader.output_buffer_size().expect("buf size")];
    reader.next_frame(&mut buf).expect("read frame");
    let red = buf
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|px| px[0] > 200 && px[1] < 80 && px[2] < 80)
        .count();
    assert!(
        red > 500,
        "expected an injected red box in the capture, got {red} red px"
    );
}

#[test]
fn png_to_pdf_wraps_png_in_valid_pdf() {
    let png = blank_png(64, 64);
    let pdf = png_to_pdf(&png).expect("should produce a PDF");
    assert!(pdf.len() > 100, "PDF should be non-trivial");
    assert!(
        pdf.starts_with(b"%PDF-"),
        "PDF header missing, got: {:?}",
        String::from_utf8_lossy(&pdf[..8.min(pdf.len())])
    );
}

// ---------------------------------------------------------------------------
// Paged PDF output (png_to_pdf_paged)
// ---------------------------------------------------------------------------

use oxibrowser_render::{PdfOrientation, PdfPageOptions, PdfPageSize, png_to_pdf_paged};

/// A4 portrait with 10 mm margins: content box 190 × 277 mm. An image scaled
/// to 190 px wide has 1 px/mm, so one page's content height is exactly 277
/// scaled rows and `n_pages = ceil(height / 277)`.
#[test]
fn png_to_pdf_paged_a4_page_count_matches_px_mm_arithmetic() {
    let opts = PdfPageOptions {
        page_size: PdfPageSize::A4,
        orientation: PdfOrientation::Portrait,
        margin_mm: 10.0,
    };

    // Exact multiple: 2770 rows / 277 rows-per-page = 10 pages.
    let pdf = png_to_pdf_paged(&blank_png(190, 2770), &opts).expect("valid PNG must paginate");
    assert!(pdf.starts_with(b"%PDF"), "PDF header missing");
    assert_eq!(count_page_objects(&pdf), 10, "2770px @ A4/m10 == 10 pages");

    // Remainder: ceil(3000 / 277) = 11 pages (last one padded white).
    let pdf = png_to_pdf_paged(&blank_png(190, 3000), &opts).expect("valid PNG must paginate");
    assert!(pdf.starts_with(b"%PDF"), "PDF header missing");
    assert_eq!(count_page_objects(&pdf), 11, "3000px @ A4/m10 == 11 pages");

    // Shorter than one page: exactly 1 page.
    let pdf = png_to_pdf_paged(&blank_png(190, 100), &opts).expect("valid PNG must paginate");
    assert_eq!(count_page_objects(&pdf), 1, "100px @ A4/m10 == 1 page");
}

/// Letter portrait with 10 mm margins: content 195.9 × 259.4 mm; a 190 px
/// wide image covers round(259.4 · 190/195.9) = 252 rows per page.
#[test]
fn png_to_pdf_paged_letter_page_count() {
    let opts = PdfPageOptions {
        page_size: PdfPageSize::Letter,
        orientation: PdfOrientation::Portrait,
        margin_mm: 10.0,
    };
    let pdf = png_to_pdf_paged(&blank_png(190, 2520), &opts).expect("valid PNG must paginate");
    assert_eq!(
        count_page_objects(&pdf),
        10,
        "2520px @ Letter/m10 == 10 pages"
    );
}

/// A4 landscape with 10 mm margins: content 277 × 190 mm; a 190 px wide image
/// covers round(190 · 190/277) = 130 rows per page.
#[test]
fn png_to_pdf_paged_landscape_page_count() {
    let opts = PdfPageOptions {
        page_size: PdfPageSize::A4,
        orientation: PdfOrientation::Landscape,
        margin_mm: 10.0,
    };
    let pdf = png_to_pdf_paged(&blank_png(190, 1300), &opts).expect("valid PNG must paginate");
    assert_eq!(
        count_page_objects(&pdf),
        10,
        "1300px @ A4-landscape/m10 == 10 pages"
    );
}

/// Undecodable input must surface as `Err`, not collapse into an empty PDF.
#[test]
fn png_to_pdf_paged_rejects_undecodable_png() {
    let opts = PdfPageOptions::default();
    let err = png_to_pdf_paged(b"not a png", &opts)
        .expect_err("garbage input must fail, not emit an empty document");
    assert!(err.to_string().contains("png"), "unexpected error: {err}");
}

/// Heuristic page count from raw PDF bytes: every page object carries
/// `/Type/Page`; the page-tree node `/Type/Pages` also contains that
/// substring, so subtract its occurrences.
fn count_page_objects(pdf: &[u8]) -> usize {
    let page = pdf
        .windows(b"/Type/Page".len())
        .filter(|w| *w == b"/Type/Page")
        .count();
    let pages = pdf
        .windows(b"/Type/Pages".len())
        .filter(|w| *w == b"/Type/Pages")
        .count();
    page - pages
}

/// `prefers-color-scheme` override: with `@media (prefers-color-scheme: dark)`
/// styling, forcing dark (override `Some(true)`) must produce a different
/// frame than no override (light default).
#[test]
fn color_scheme_override_changes_media_query_result() {
    use oxibrowser_render::{color_scheme_override_dark, set_color_scheme_override};

    const HTML: &str = r#"<html><head><style>
      body { margin: 0; background: #ffffff; }
      @media (prefers-color-scheme: dark) {
        body { background: #101010; }
      }
    </style></head><body><p>x</p></body></html>"#;

    const VIEWPORT: Viewport = Viewport {
        width: 64,
        height: 64,
        scale: 1.0,
    };

    // Guard: test assumes no override leaked in from another test.
    assert_eq!(color_scheme_override_dark(), None);

    let mut light_doc = RenderDocument::from_html(HTML, None, VIEWPORT).expect("light from_html");
    let light_png = light_doc
        .capture_png(&CaptureOpts::default())
        .expect("light capture_png");

    set_color_scheme_override(Some(true));
    assert_eq!(color_scheme_override_dark(), Some(true));

    let mut dark_doc = RenderDocument::from_html(HTML, None, VIEWPORT).expect("dark from_html");
    let dark_png = dark_doc
        .capture_png(&CaptureOpts::default())
        .expect("dark capture_png");

    set_color_scheme_override(None);
    assert_eq!(color_scheme_override_dark(), None);

    assert_ne!(light_png, dark_png, "dark override must change the frame");
}

/// Regression: a relative `<img src>` with a base_url must not panic.
/// blitz-html resolves eager sub-resource ops (img) during parsing, so the
/// base URL has to come from `DocumentConfig`, not the post-parse
/// `set_base_url` call (which used to run too late).
#[test]
fn relative_img_with_base_url_does_not_panic() {
    const VIEWPORT: Viewport = Viewport {
        width: 64,
        height: 64,
        scale: 1.0,
    };
    let html = r#"<html><body><img src="pic.png"><p>hi</p></body></html>"#;
    let mut doc = RenderDocument::from_html(html, Some("https://example.com/page.html"), VIEWPORT)
        .expect("from_html with relative img must not panic");
    let png = doc
        .capture_png(&CaptureOpts::default())
        .expect("capture_png");
    assert!(!png.is_empty());
}
