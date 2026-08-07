//! Paint pipeline: `BaseDocument` → anyrender scene → RGBA buffer → PNG.
//!
//! Mirrors the pattern proven in Blitz's own `apps/browser/src/capture.rs`.

use anyrender::render_to_buffer;
use anyrender::PaintScene;
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::util::Color;
use blitz_dom::BaseDocument;
use blitz_paint::paint_scene;
use peniko::kurbo::Rect;
use peniko::Fill;

use crate::document::{RenderError, Viewport};

/// Render `doc` to a PNG byte buffer.
///
/// When `full_page` is true the height is taken from the root element's laid-out
/// content height (a full-page screenshot); otherwise `viewport.height` is used.
pub(crate) fn capture_png(
    doc: &mut BaseDocument,
    viewport: Viewport,
    full_page: bool,
) -> Result<Vec<u8>, RenderError> {
    // Ensure layout reflects the latest state before measuring/painting.
    doc.resolve(0.0);

    let width = viewport.width.max(1);
    let height = if full_page {
        let content_h = doc.root_element().final_layout.size.height;
        if content_h.is_finite() && content_h > 0.0 {
            content_h.ceil() as u32
        } else {
            viewport.height.max(1)
        }
    } else {
        viewport.height.max(1)
    };
    let scale = viewport.scale;

    // `render_to_buffer` hands the closure a `&mut VelloCpuScenePainter`
    // (the `R::ScenePainter` for `VelloCpuImageRenderer`). Its concrete type is
    // inferred — do not annotate it (matches Blitz's capture.rs).
    let buffer = render_to_buffer::<VelloCpuImageRenderer, _>(
        |scene| {
            // White background covering the whole output area.
            scene.fill(
                Fill::NonZero,
                Default::default(),
                Color::WHITE,
                Default::default(),
                &Rect::new(0.0, 0.0, width as f64, height as f64),
            );
            paint_scene(scene, doc, scale, width, height, 0, 0);
        },
        width,
        height,
    );

    encode_png(&buffer, width, height)
}

fn encode_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, RenderError> {
    let mut out = Vec::with_capacity(rgba.len() / 3);
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| RenderError::Encode(e.to_string()))?;
    writer
        .write_image_data(rgba)
        .map_err(|e| RenderError::Encode(e.to_string()))?;
    writer
        .finish()
        .map_err(|e| RenderError::Encode(e.to_string()))?;
    Ok(out)
}
