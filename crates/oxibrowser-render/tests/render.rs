//! Integration tests for the rendering pipeline.
//!
//! These exercise the full HTML → Stylo cascade → Taffy layout → vello_cpu
//! paint → PNG path end to end, asserting observable contracts (PNG validity,
//! dimensions, presence of non-background content) rather than exact pixels.

use oxibrowser_render::{CaptureOpts, RenderDocument, Viewport};

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

    let png = doc.capture_png(&CaptureOpts::default()).expect("capture_png");

    // Valid PNG with a real header.
    assert!(png.len() > 100, "png suspiciously small: {} bytes", png.len());
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
        .chunks_exact(4)
        .filter(|px| px != &[255, 255, 255, 255])
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
    let png = doc.capture_png(&CaptureOpts::default()).expect("capture_png");
    assert_eq!(&png[0..8], &PNG_SIG);
    assert!(png.len() > 50);
}
