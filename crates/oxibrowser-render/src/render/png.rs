//! PNG rendering via Blitz + Vello CPU.

use crate::config::RenderConfig;
use crate::error::{RenderError, RenderResult};

#[cfg(feature = "png")]
use anyrender::render_to_buffer;
#[cfg(feature = "png")]
use anyrender_vello_cpu::VelloCpuImageRenderer;
#[cfg(feature = "png")]
use blitz_html::HtmlDocument;
#[cfg(feature = "png")]
use blitz_paint::paint_scene;

/// Render a Blitz document to PNG bytes.
#[cfg(feature = "png")]
pub fn render_to_png(document: &HtmlDocument, config: &RenderConfig) -> RenderResult<Vec<u8>> {
    let scale = config.scale as f64;
    let width = config.width;
    let height = if config.auto_height {
        get_content_height(document).unwrap_or(config.height)
    } else {
        config.height
    };

    let render_width = (width as f64 * scale) as u32;
    let render_height = (height as f64 * scale) as u32;

    // Render to RGBA pixel buffer via Vello CPU
    let buffer = render_to_buffer::<VelloCpuImageRenderer, _>(
        |scene| {
            paint_scene(scene, document.as_ref(), scale, render_width, render_height);
        },
        render_width,
        render_height,
    );

    // Encode RGBA buffer to PNG
    encode_png(&buffer, render_width, render_height)
}

/// Encode RGBA pixel buffer to PNG bytes.
#[cfg(feature = "png")]
fn encode_png(buffer: &[u8], width: u32, height: u32) -> RenderResult<Vec<u8>> {
    let mut output = Vec::new();

    {
        let mut encoder = png::Encoder::new(&mut output, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);

        let mut writer = encoder
            .write_header()
            .map_err(|e| RenderError::PngEncode(e.to_string()))?;

        writer
            .write_image_data(buffer)
            .map_err(|e| RenderError::PngEncode(e.to_string()))?;
    }

    Ok(output)
}

/// Get actual content height from document layout.
#[cfg(feature = "png")]
fn get_content_height(document: &HtmlDocument) -> Option<u32> {
    let doc = document.as_ref();
    let root = doc.root_element();
    Some(root.final_layout.size.height as u32)
}

#[cfg(not(feature = "png"))]
pub fn render_to_png(
    _document: &blitz_html::HtmlDocument,
    _config: &RenderConfig,
) -> RenderResult<Vec<u8>> {
    Err(RenderError::FormatNotEnabled("png"))
}