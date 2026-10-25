//! Paint pipeline: `BaseDocument` → anyrender scene → RGBA buffer → PNG.
//!
//! Mirrors the pattern proven in Blitz's own `apps/browser/src/capture.rs`.

use anyrender::PaintScene;
use anyrender::render_to_buffer;
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::BaseDocument;
use blitz_dom::util::Color;
use blitz_paint::paint_scene;
use peniko::Fill;
use peniko::kurbo::Rect;

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

pub fn encode_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, RenderError> {
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

/// Encode a blank white PNG of the given size.
///
/// Used as a fallback when the render document cannot be captured — preserves
/// the "never hard-fail a screenshot" contract with a minimal valid PNG.
pub fn blank_png(width: u32, height: u32) -> Vec<u8> {
    let width = width.max(1);
    let height = height.max(1);
    let rgba = vec![0xFFu8; (width as usize) * (height as usize) * 4];
    encode_png(&rgba, width, height).unwrap_or_default()
}

/// Wrap a PNG in a single-page PDF whose page matches the image's aspect ratio.
///
/// The image fills the page; the page is sized to the image at 96 dpi
/// (1 CSS px ≈ 0.2646 mm). Returns `None` if the PNG cannot be decoded or
/// the PDF cannot be encoded.
pub fn png_to_pdf(png: &[u8]) -> Option<Vec<u8>> {
    use printpdf::*;
    let mut warnings = Vec::new();
    let raw = RawImage::decode_from_bytes(png, &mut warnings).ok()?;
    let (iw, ih) = (raw.width, raw.height);
    let mut doc = PdfDocument::new("OxiBrowser");
    let img_id = doc.add_image(&raw);
    // 96 dpi → 1 CSS px ≈ 0.2646 mm; the image fills the page.
    let mm_per_px = 0.264_583_33_f32;
    let page = PdfPage::new(
        Mm(iw as f32 * mm_per_px),
        Mm(ih as f32 * mm_per_px),
        vec![Op::UseXobject {
            id: img_id,
            transform: XObjectTransform {
                dpi: Some(96.0),
                ..Default::default()
            },
        }],
    );
    doc.pages.push(page);
    Some(doc.save(&PdfSaveOptions::default(), &mut warnings))
}

/// Paper size for paged PDF output ([`png_to_pdf_paged`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdfPageSize {
    /// ISO A4: 210 × 297 mm.
    A4,
    /// US Letter: 215.9 × 279.4 mm.
    Letter,
}

/// Page orientation for paged PDF output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdfOrientation {
    Portrait,
    Landscape,
}

/// Pagination options for [`png_to_pdf_paged`].
#[derive(Debug, Clone)]
pub struct PdfPageOptions {
    pub page_size: PdfPageSize,
    pub orientation: PdfOrientation,
    /// Page margin on all sides, in millimetres.
    pub margin_mm: f64,
}

impl Default for PdfPageOptions {
    fn default() -> Self {
        Self {
            page_size: PdfPageSize::A4,
            orientation: PdfOrientation::Portrait,
            margin_mm: 10.0,
        }
    }
}

/// Physical page dimensions `(width, height)` in mm for the given size and
/// orientation.
fn page_size_mm(size: PdfPageSize, orientation: PdfOrientation) -> (f64, f64) {
    let (w, h) = match size {
        PdfPageSize::A4 => (210.0, 297.0),
        PdfPageSize::Letter => (215.9, 279.4),
    };
    match orientation {
        PdfOrientation::Portrait => (w, h),
        PdfOrientation::Landscape => (h, w),
    }
}

/// Decode a PNG into RGBA8 pixels. Colour types are normalized via
/// [`png::Transformations::normalize_to_color8`], 16-bit samples are stripped
/// to 8-bit ([`png::Transformations::STRIP_16`]), and the result is converted
/// to RGBA8. Fails on undecodable input, unsupported colour types, or any
/// sample width that survives normalization other than 8-bit — matching on
/// 16-bit bytes as if they were 8-bit would garble every slice downstream.
fn decode_png_rgba(png: &[u8]) -> Result<(Vec<u8>, usize, usize), RenderError> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png));
    decoder.set_transformations(
        png::Transformations::normalize_to_color8() | png::Transformations::STRIP_16,
    );
    let mut reader = decoder
        .read_info()
        .map_err(|e| RenderError::Decode(format!("png header decode failed: {e}")))?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| RenderError::Decode("png output buffer size overflow".to_string()))?;
    let mut buf = vec![0u8; size];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| RenderError::Decode(format!("png frame decode failed: {e}")))?;
    if info.bit_depth != png::BitDepth::Eight {
        return Err(RenderError::Decode(format!(
            "unexpected {:?} after normalization; expected 8-bit samples",
            info.bit_depth
        )));
    }
    let (iw, ih) = (info.width as usize, info.height as usize);
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => {
            let mut out = Vec::with_capacity(iw * ih * 4);
            for px in buf.as_chunks::<3>().0 {
                out.extend_from_slice(&[px[0], px[1], px[2], 0xFF]);
            }
            out
        }
        png::ColorType::Grayscale => {
            let mut out = Vec::with_capacity(iw * ih * 4);
            for &g in &buf {
                out.extend_from_slice(&[g, g, g, 0xFF]);
            }
            out
        }
        png::ColorType::GrayscaleAlpha => {
            let mut out = Vec::with_capacity(iw * ih * 4);
            for px in buf.as_chunks::<2>().0 {
                out.extend_from_slice(&[px[0], px[0], px[0], px[1]]);
            }
            out
        }
        other => {
            return Err(RenderError::Decode(format!(
                "unsupported PNG color type: {other:?}"
            )));
        }
    };
    Ok((rgba, iw, ih))
}

/// Wrap a PNG in a multi-page PDF with real pagination.
///
/// The image is scaled to the content-box width (page size minus margins) and
/// cut into successive non-overlapping full-width horizontal slices, each
/// covering exactly one page's content height (in scaled pixels); the final
/// remainder is padded white to a full page. Each slice is stretched onto the
/// full content box (sub-pixel distortion only), so pages tile seamlessly.
///
/// Returns `Err` if the PNG cannot be decoded or the PDF cannot be encoded.
/// Non-fatal printpdf warnings are logged via `tracing::warn!`.
pub fn png_to_pdf_paged(png: &[u8], opts: &PdfPageOptions) -> Result<Vec<u8>, RenderError> {
    use printpdf::*;

    let (rgba, iw, ih) = decode_png_rgba(png)?;
    let ih = ih.max(1);

    let mm_per_px = 25.4 / 96.0_f64; // 96 dpi CSS px
    let (pw, ph) = page_size_mm(opts.page_size, opts.orientation);
    let m = opts.margin_mm.max(0.0);
    let cw = (pw - 2.0 * m).max(1.0);
    let ch = (ph - 2.0 * m).max(1.0);

    // The scaled image is exactly `cw` mm wide, so scaled px/mm is uniform:
    // one page's content height covers this many source pixel rows.
    let slice_rows = ((ch * iw as f64 / cw).round() as usize).max(1);
    let n_pages = ih.div_ceil(slice_rows);

    let mut warnings = Vec::new();
    let mut doc = PdfDocument::new("OxiBrowser");
    for k in 0..n_pages {
        let start = k * slice_rows;
        let end = ((k + 1) * slice_rows).min(ih);
        let mut slice = rgba[start * iw * 4..end * iw * 4].to_vec();
        // Pad the final remainder white to a full page's content height.
        slice.resize(slice_rows * iw * 4, 0xFF);
        let slice_png = encode_png(&slice, iw as u32, slice_rows as u32)?;
        let raw = RawImage::decode_from_bytes(&slice_png, &mut warnings)
            .map_err(|e| RenderError::Decode(format!("pdf image re-decode failed: {e}")))?;
        let img_id = doc.add_image(&raw);

        // PDF origin is bottom-left; every page carries one slice at the top
        // of its own content box (bottom edge = content-box bottom).
        // scale_x/scale_y stretch the slice onto the exact content box.
        let y_bottom_mm = ph - m - ch;
        let page = PdfPage::new(
            Mm(pw as f32),
            Mm(ph as f32),
            vec![Op::UseXobject {
                id: img_id,
                transform: XObjectTransform {
                    dpi: Some(96.0),
                    scale_x: Some((cw / (iw as f64 * mm_per_px)) as f32),
                    scale_y: Some((ch / (slice_rows as f64 * mm_per_px)) as f32),
                    translate_x: Some(Pt::from(Mm(m as f32))),
                    translate_y: Some(Pt::from(Mm(y_bottom_mm as f32))),
                    ..Default::default()
                },
            }],
        );
        doc.pages.push(page);
    }
    let pdf = doc.save(&PdfSaveOptions::default(), &mut warnings);
    for w in &warnings {
        tracing::warn!("png_to_pdf_paged: printpdf warning: {w:?}");
    }
    Ok(pdf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode raw sample bytes (big-endian, as the png crate expects them).
    fn encode_png_bytes(
        color: png::ColorType,
        depth: png::BitDepth,
        width: u32,
        height: u32,
        data: &[u8],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(color);
        encoder.set_depth(depth);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(data).unwrap();
        writer.finish().unwrap();
        out
    }

    #[test]
    fn decode_16bit_grayscale_yields_sane_rgba8() {
        // 3×2 16-bit grayscale; both STRIP_16 strategies (chop and scale) are
        // monotonic, so the decoded gray values must stay strictly increasing.
        let samples: [u16; 6] = [0x0000, 0x0100, 0x4000, 0x8000, 0xC000, 0xFFFF];
        let mut data = Vec::with_capacity(12);
        for s in samples {
            data.extend_from_slice(&s.to_be_bytes());
        }
        let png = encode_png_bytes(
            png::ColorType::Grayscale,
            png::BitDepth::Sixteen,
            3,
            2,
            &data,
        );

        let (rgba, iw, ih) = decode_png_rgba(&png).expect("16-bit grayscale must decode");
        assert_eq!((iw, ih), (3, 2));
        assert_eq!(rgba.len(), 3 * 2 * 4, "exactly one RGBA8 quad per pixel");
        let mut prev: u8 = 0;
        for px in rgba.as_chunks::<4>().0 {
            assert_eq!(px[0], px[1]);
            assert_eq!(px[1], px[2]);
            assert_eq!(px[3], 0xFF, "grayscale is opaque");
            assert!(
                px[0] >= prev,
                "gray values must be monotonic, got {} after {prev}",
                px[0]
            );
            prev = px[0];
        }
    }

    #[test]
    fn decode_16bit_rgba_yields_sane_rgba8() {
        // 2×1 16-bit RGBA; alpha 0xDEF0 must decode far above 0x0004 under any
        // monotonic 16→8 reduction (a per-byte misread would invert this).
        let samples: [u16; 8] = [
            0x1234, 0x5678, 0x9ABC, 0xDEF0, 0x0001, 0x0002, 0x0003, 0x0004,
        ];
        let mut data = Vec::with_capacity(16);
        for s in samples {
            data.extend_from_slice(&s.to_be_bytes());
        }
        let png = encode_png_bytes(png::ColorType::Rgba, png::BitDepth::Sixteen, 2, 1, &data);

        let (rgba, iw, ih) = decode_png_rgba(&png).expect("16-bit RGBA must decode");
        assert_eq!((iw, ih), (2, 1));
        assert_eq!(rgba.len(), 8);
        assert!(
            rgba[3] > rgba[7],
            "first pixel's alpha must exceed the near-transparent second pixel"
        );
    }

    #[test]
    fn decode_garbage_is_an_error() {
        assert!(decode_png_rgba(b"not a png").is_err());
    }
}
