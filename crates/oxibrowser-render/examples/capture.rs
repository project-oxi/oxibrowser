//! Standalone example: render an HTML string to a PNG file.
//!
//! Run with: `cargo run -p oxibrowser-render --example capture`
//! Inspect: the PNG is written to `target/render-test.png`.

use oxibrowser_render::{CaptureOpts, RenderDocument, Viewport};

const HTML: &str = r#"<!DOCTYPE html>
<html>
<head><meta charset="utf-8"><style>
  body { margin: 0; font-family: sans-serif; background: #fafafa; }
  h1 { color: #1a73e8; font-size: 48px; margin: 24px; }
  p { color: #333; margin: 24px; font-size: 18px; }
  .row { display: flex; gap: 16px; margin: 24px; }
  .box { width: 120px; height: 120px; border-radius: 8px; }
  .r { background: #ea4335; }
  .g { background: #34a853; }
  .b { background: #4285f4; }
  .y { background: #fbbc05; }
</style></head>
<body>
  <h1>Hello OxiBrowser</h1>
  <p>Rendered with Stylo CSS + Taffy layout + vello_cpu paint.</p>
  <div class="row">
    <div class="box r"></div>
    <div class="box g"></div>
    <div class="box b"></div>
    <div class="box y"></div>
  </div>
</body>
</html>"#;

fn main() {
    let viewport = Viewport {
        width: 800,
        height: 400,
        scale: 1.0,
    };
    let mut doc = RenderDocument::from_html(HTML, None, viewport).expect("from_html");
    let png = doc
        .capture_png(&CaptureOpts::default())
        .expect("capture_png");

    let out = std::env::current_dir()
        .unwrap()
        .join("target/render-test.png");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    std::fs::write(&out, &png).expect("write png");

    println!("Wrote {} bytes to {}", png.len(), out.display());
    println!(
        "Dimensions expected: {}x{}",
        viewport.width, viewport.height
    );
}
