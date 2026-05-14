//! PDF rendering via Blitz + Krilla.
//!
//! Converts the Blitz DOM tree to a PDF document with:
//! - Background colors and linear gradients
//! - Border-radius (rounded corners)
//! - Box shadows (outset and inset with blur approximation)
//! - Borders (per-edge colors)
//! - Text rendering with embedded fonts

use crate::config::RenderConfig;
use crate::error::{RenderError, RenderResult};

// ---------------------------------------------------------------------------
// Feature-gated imports
// ---------------------------------------------------------------------------

#[cfg(feature = "pdf")]
use blitz_dom::{BaseDocument, Node};
#[cfg(feature = "pdf")]
use blitz_html::HtmlDocument;
#[cfg(feature = "pdf")]
use krilla::color::rgb;
#[cfg(feature = "pdf")]
use krilla::geom::{Path, PathBuilder, Point, Size};
#[cfg(feature = "pdf")]
use krilla::num::NormalizedF32;
#[cfg(feature = "pdf")]
use krilla::page::PageSettings;
#[cfg(feature = "pdf")]
use krilla::paint::{Fill, FillRule};
#[cfg(feature = "pdf")]
use krilla::surface::Surface;
#[cfg(feature = "pdf")]
use krilla::text::{Font, GlyphId, KrillaGlyph};
#[cfg(feature = "pdf")]
use krilla::Document;
#[cfg(feature = "pdf")]
use linebender_resource_handle::FontData;
#[cfg(feature = "pdf")]
use parley::PositionedLayoutItem;
#[cfg(feature = "pdf")]
use std::collections::HashMap;
#[cfg(feature = "pdf")]
use style::color::AbsoluteColor;
#[cfg(feature = "pdf")]
use style::values::computed::{BorderCornerRadius, CSSPixelLength};
#[cfg(feature = "pdf")]
use style::values::generics::image::{GenericGradient, GenericGradientItem, GradientFlags};
#[cfg(feature = "pdf")]
use style::values::specified::position::{HorizontalPositionKeyword, VerticalPositionKeyword};

// ---------------------------------------------------------------------------
// PDF render entry point
// ---------------------------------------------------------------------------

/// Render a Blitz document to PDF bytes.
#[cfg(feature = "pdf")]
pub fn render_to_pdf(document: &HtmlDocument, config: &RenderConfig) -> RenderResult<Vec<u8>> {
    let width = config.width as f32;
    let height = if config.auto_height {
        get_content_height(document).unwrap_or(config.height as f32)
    } else {
        config.height as f32
    };

    let mut pdf_doc = Document::new();

    let size = Size::from_wh(width, height)
        .ok_or_else(|| RenderError::PdfCreate("Invalid page dimensions".to_string()))?;
    let page_settings = PageSettings::new(size);
    let mut page = pdf_doc.start_page_with(page_settings);
    let mut surface = page.surface();

    // Page background
    let [r, g, b, _a] = config.background;
    draw_rect(&mut surface, 0.0, 0.0, width, height, Rgb::new(r, g, b));

    let mut font_cache = FontCache::new();

    // Render DOM tree
    let doc = document.as_ref();
    let root = doc.root_element();
    render_node(&mut surface, doc, root, 0.0, 0.0, &mut font_cache)?;

    surface.finish();
    page.finish();

    pdf_doc
        .finish()
        .map_err(|e| RenderError::PdfCreate(format!("{:?}", e)))
}

#[cfg(not(feature = "pdf"))]
pub fn render_to_pdf(
    _document: &blitz_html::HtmlDocument,
    _config: &RenderConfig,
) -> RenderResult<Vec<u8>> {
    Err(RenderError::FormatNotEnabled("pdf"))
}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

#[cfg(feature = "pdf")]
#[derive(Clone, Copy, Default)]
struct Rgb {
    r: u8,
    g: u8,
    b: u8,
}

#[cfg(feature = "pdf")]
impl Rgb {
    fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

#[cfg(feature = "pdf")]
#[derive(Clone, Copy, Default)]
struct BorderRadii {
    top_left: (f32, f32),
    top_right: (f32, f32),
    bottom_right: (f32, f32),
    bottom_left: (f32, f32),
}

#[cfg(feature = "pdf")]
impl BorderRadii {
    fn has_any_radius(&self) -> bool {
        self.top_left != (0.0, 0.0)
            || self.top_right != (0.0, 0.0)
            || self.bottom_right != (0.0, 0.0)
            || self.bottom_left != (0.0, 0.0)
    }
}

#[cfg(feature = "pdf")]
type FontCache = HashMap<u64, Font>;

#[cfg(feature = "pdf")]
#[derive(Clone, Copy, Default)]
struct EdgeBorder {
    color: Rgb,
    alpha: f32,
    width: f32,
    visible: bool,
}

#[cfg(feature = "pdf")]
#[derive(Clone, Copy, Default)]
struct BorderWidths {
    top: f32,
    right: f32,
    bottom: f32,
    left: f32,
}

#[cfg(feature = "pdf")]
struct BoxShadowData {
    x_offset: f32,
    y_offset: f32,
    blur: f32,
    spread: f32,
    color: Rgb,
    alpha: f32,
    inset: bool,
}

// ---------------------------------------------------------------------------
// Drawing primitives
// ---------------------------------------------------------------------------

#[cfg(feature = "pdf")]
fn draw_rect(surface: &mut Surface, x: f32, y: f32, w: f32, h: f32, color: Rgb) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let mut builder = PathBuilder::new();
    builder.move_to(x, y);
    builder.line_to(x + w, y);
    builder.line_to(x + w, y + h);
    builder.line_to(x, y + h);
    builder.close();

    if let Some(path) = builder.finish() {
        let fill = Fill {
            paint: rgb::Color::new(color.r, color.g, color.b).into(),
            opacity: NormalizedF32::ONE,
            rule: FillRule::NonZero,
        };
        surface.set_fill(Some(fill));
        surface.draw_path(&path);
    }
}

#[cfg(feature = "pdf")]
fn build_rounded_rect_path(x: f32, y: f32, w: f32, h: f32, radii: &BorderRadii) -> Option<Path> {
    const KAPPA: f32 = 0.552_284_8;

    let mut builder = PathBuilder::new();

    let clamp_x = |r: f32| r.min(w / 2.0).max(0.0);
    let clamp_y = |r: f32| r.min(h / 2.0).max(0.0);

    let tl = (clamp_x(radii.top_left.0), clamp_y(radii.top_left.1));
    let tr = (clamp_x(radii.top_right.0), clamp_y(radii.top_right.1));
    let br = (
        clamp_x(radii.bottom_right.0),
        clamp_y(radii.bottom_right.1),
    );
    let bl = (clamp_x(radii.bottom_left.0), clamp_y(radii.bottom_left.1));

    builder.move_to(x + tl.0, y);
    builder.line_to(x + w - tr.0, y);

    if tr.0 > 0.0 && tr.1 > 0.0 {
        builder.cubic_to(
            x + w - tr.0 * (1.0 - KAPPA),
            y,
            x + w,
            y + tr.1 * (1.0 - KAPPA),
            x + w,
            y + tr.1,
        );
    }

    builder.line_to(x + w, y + h - br.1);

    if br.0 > 0.0 && br.1 > 0.0 {
        builder.cubic_to(
            x + w,
            y + h - br.1 * (1.0 - KAPPA),
            x + w - br.0 * (1.0 - KAPPA),
            y + h,
            x + w - br.0,
            y + h,
        );
    }

    builder.line_to(x + bl.0, y + h);

    if bl.0 > 0.0 && bl.1 > 0.0 {
        builder.cubic_to(
            x + bl.0 * (1.0 - KAPPA),
            y + h,
            x,
            y + h - bl.1 * (1.0 - KAPPA),
            x,
            y + h - bl.1,
        );
    }

    builder.line_to(x, y + tl.1);

    if tl.0 > 0.0 && tl.1 > 0.0 {
        builder.cubic_to(
            x,
            y + tl.1 * (1.0 - KAPPA),
            x + tl.0 * (1.0 - KAPPA),
            y,
            x + tl.0,
            y,
        );
    }

    builder.close();
    builder.finish()
}

// ---------------------------------------------------------------------------
// Style extraction
// ---------------------------------------------------------------------------

#[cfg(feature = "pdf")]
fn extract_border_radii(
    style: &style::properties::ComputedValues,
    width: f32,
    height: f32,
) -> BorderRadii {
    let border = style.get_border();
    let resolve_w = CSSPixelLength::new(width);
    let resolve_h = CSSPixelLength::new(height);

    let resolve = |radius: &BorderCornerRadius| -> (f32, f32) {
        (
            radius.0.width.0.resolve(resolve_w).px(),
            radius.0.height.0.resolve(resolve_h).px(),
        )
    };

    BorderRadii {
        top_left: resolve(&border.border_top_left_radius),
        top_right: resolve(&border.border_top_right_radius),
        bottom_right: resolve(&border.border_bottom_right_radius),
        bottom_left: resolve(&border.border_bottom_left_radius),
    }
}

#[cfg(feature = "pdf")]
fn extract_borders(
    style: &style::properties::ComputedValues,
    border_widths: BorderWidths,
    current_color: &AbsoluteColor,
) -> [EdgeBorder; 4] {
    use style::values::specified::BorderStyle;

    let border = style.get_border();

    let convert_style =
        |s: BorderStyle| -> bool { !matches!(s, BorderStyle::None | BorderStyle::Hidden) };

    let extract_edge =
        |color: &style::values::computed::Color, width: f32, style: BorderStyle| -> EdgeBorder {
            if width <= 0.0 || !convert_style(style) {
                return EdgeBorder::default();
            }

            let abs_color = color.resolve_to_absolute(current_color);
            let srgb = abs_color.to_color_space(style::color::ColorSpace::Srgb);

            EdgeBorder {
                color: Rgb::new(
                    (srgb.components.0.clamp(0.0, 1.0) * 255.0) as u8,
                    (srgb.components.1.clamp(0.0, 1.0) * 255.0) as u8,
                    (srgb.components.2.clamp(0.0, 1.0) * 255.0) as u8,
                ),
                alpha: srgb.alpha.clamp(0.0, 1.0),
                width,
                visible: true,
            }
        };

    [
        extract_edge(&border.border_top_color, border_widths.top, border.border_top_style),
        extract_edge(
            &border.border_right_color,
            border_widths.right,
            border.border_right_style,
        ),
        extract_edge(
            &border.border_bottom_color,
            border_widths.bottom,
            border.border_bottom_style,
        ),
        extract_edge(
            &border.border_left_color,
            border_widths.left,
            border.border_left_style,
        ),
    ]
}

#[cfg(feature = "pdf")]
fn extract_box_shadows(
    style: &style::properties::ComputedValues,
    current_color: &AbsoluteColor,
) -> Vec<BoxShadowData> {
    let effects = style.get_effects();
    effects
        .box_shadow
        .0
        .iter()
        .map(|shadow| {
            let color = shadow.base.color.resolve_to_absolute(current_color);
            let srgb = color.to_color_space(style::color::ColorSpace::Srgb);

            BoxShadowData {
                x_offset: shadow.base.horizontal.px(),
                y_offset: shadow.base.vertical.px(),
                blur: shadow.base.blur.px(),
                spread: shadow.spread.px(),
                color: Rgb::new(
                    (srgb.components.0.clamp(0.0, 1.0) * 255.0) as u8,
                    (srgb.components.1.clamp(0.0, 1.0) * 255.0) as u8,
                    (srgb.components.2.clamp(0.0, 1.0) * 255.0) as u8,
                ),
                alpha: srgb.alpha.clamp(0.0, 1.0),
                inset: shadow.inset,
            }
        })
        .collect()
}

#[cfg(feature = "pdf")]
fn extract_color(
    color: &style::values::computed::color::Color,
) -> Option<(f32, f32, f32, f32)> {
    use style::values::generics::color::Color as GenericColor;

    match color {
        GenericColor::Absolute(abs) => {
            let srgb = abs.to_color_space(style::color::ColorSpace::Srgb);
            Some((srgb.components.0, srgb.components.1, srgb.components.2, srgb.alpha))
        }
        GenericColor::CurrentColor => Some((0.0, 0.0, 0.0, 1.0)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Node rendering
// ---------------------------------------------------------------------------

#[cfg(feature = "pdf")]
fn render_node(
    surface: &mut Surface,
    doc: &BaseDocument,
    node: &Node,
    offset_x: f32,
    offset_y: f32,
    font_cache: &mut FontCache,
) -> RenderResult<()> {
    let layout = &node.final_layout;
    let x = offset_x + layout.location.x;
    let y = offset_y + layout.location.y;
    let width = layout.size.width;
    let height = layout.size.height;

    if width <= 0.0 || height <= 0.0 {
        if let Some(paint_children) = &*node.paint_children.borrow() {
            for child_id in paint_children.iter() {
                if let Some(child) = doc.get_node(*child_id) {
                    render_node(surface, doc, child, x, y, font_cache)?;
                }
            }
        }
        return Ok(());
    }

    let border_widths = BorderWidths {
        top: layout.border.top,
        right: layout.border.right,
        bottom: layout.border.bottom,
        left: layout.border.left,
    };

    let (radii, current_color, shadows, borders) = if let Some(style) = node.primary_styles() {
        let radii = extract_border_radii(&style, width, height);
        let current_color = style
            .get_inherited_text()
            .color
            .to_color_space(style::color::ColorSpace::Srgb);
        let shadows = extract_box_shadows(&style, &current_color);
        let borders = extract_borders(&style, border_widths, &current_color);
        (radii, current_color, shadows, borders)
    } else {
        (
            BorderRadii::default(),
            AbsoluteColor::BLACK,
            Vec::new(),
            [EdgeBorder::default(); 4],
        )
    };

    let has_radius = radii.has_any_radius();

    // 1. Outset box shadows
    for shadow in shadows.iter().filter(|s| !s.inset) {
        draw_outset_box_shadow(surface, x, y, width, height, shadow, &radii);
    }

    // 2. Clip for rounded corners
    if has_radius {
        if let Some(clip_path) = build_rounded_rect_path(x, y, width, height, &radii) {
            surface.push_clip_path(&clip_path, &FillRule::NonZero);
        }
    }

    // 3. Backgrounds
    if let Some(style) = node.primary_styles() {
        let bg = style.get_background();

        // Background color
        if let Some((r, g, b, a)) = extract_color(&bg.background_color) {
            if a > 0.0 {
                draw_rect(
                    surface,
                    x,
                    y,
                    width,
                    height,
                    Rgb::new((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8),
                );
            }
        }

        // Background gradients
        for bg_image in bg.background_image.0.iter() {
            if let style::values::generics::image::GenericImage::Gradient(gradient) = bg_image {
                if let GenericGradient::Linear {
                    direction,
                    items,
                    flags,
                    ..
                } = &**gradient
                {
                    if let Some(linear_grad) = convert_linear_gradient(
                        direction,
                        &items,
                        *flags,
                        width,
                        height,
                        &current_color,
                    ) {
                        draw_gradient_rect(surface, x, y, width, height, linear_grad);
                    }
                }
            }
        }
    }

    // 4. Inset box shadows
    for shadow in shadows.iter().filter(|s| s.inset) {
        draw_inset_box_shadow(surface, x, y, width, height, shadow, &radii);
    }

    // 5. Borders
    draw_borders(surface, x, y, width, height, &borders);

    // 6. Text
    if let Some(element_data) = node.element_data() {
        if let Some(text_layout) = &element_data.inline_layout_data {
            let content_x = x + layout.padding.left + layout.border.left;
            let content_y = y + layout.padding.top + layout.border.top;
            render_text(surface, doc, text_layout, content_x, content_y, font_cache)?;
        }
    }

    // 7. Paint children
    if let Some(paint_children) = &*node.paint_children.borrow() {
        for child_id in paint_children.iter() {
            if let Some(child) = doc.get_node(*child_id) {
                render_node(surface, doc, child, x, y, font_cache)?;
            }
        }
    }

    // Pop clip
    if has_radius {
        surface.pop();
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Text rendering
// ---------------------------------------------------------------------------

#[cfg(feature = "pdf")]
fn render_text(
    surface: &mut Surface,
    doc: &BaseDocument,
    text_layout: &blitz_dom::node::TextLayout,
    pos_x: f32,
    pos_y: f32,
    font_cache: &mut FontCache,
) -> RenderResult<()> {
    let text = &text_layout.text;
    let layout = &text_layout.layout;

    for line in layout.lines() {
        for item in line.items() {
            if let PositionedLayoutItem::GlyphRun(glyph_run) = item {
                let run = glyph_run.run();
                let font_data: FontData = run.font().clone();
                let font_size = run.font_size();
                let style = glyph_run.style();

                let (raw_data, font_id) = font_data.data.into_raw_parts();
                let krilla_font = if let Some(font) = font_cache.get(&font_id) {
                    font.clone()
                } else {
                    let data: krilla::Data = raw_data.into();
                    let font = Font::new(data, font_data.index).ok_or_else(|| {
                        RenderError::Font("failed to load font from data".to_string())
                    })?;
                    font_cache.insert(font_id, font.clone());
                    font
                };

                let text_color = doc
                    .get_node(style.brush.id)
                    .and_then(|n| n.primary_styles())
                    .map(|styles| {
                        let inherited = styles.get_inherited_text();
                        let srgb = inherited.color.to_color_space(style::color::ColorSpace::Srgb);
                        (srgb.components.0, srgb.components.1, srgb.components.2, srgb.alpha)
                    })
                    .unwrap_or((0.0, 0.0, 0.0, 1.0));

                let (r, g, b, _a) = text_color;
                surface.set_fill(Some(Fill {
                    paint: rgb::Color::new((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
                        .into(),
                    opacity: NormalizedF32::ONE,
                    rule: FillRule::NonZero,
                }));

                let mut glyphs: Vec<KrillaGlyph> = Vec::new();
                let baseline = glyph_run.baseline();

                for cluster in run.visual_clusters() {
                    if cluster.is_ligature_continuation() {
                        if let Some(glyph) = glyphs.last_mut() {
                            glyph.text_range.end = cluster.text_range().end;
                        }
                        continue;
                    }

                    let text_range = cluster.text_range();
                    for glyph in cluster.glyphs() {
                        glyphs.push(KrillaGlyph::new(
                            GlyphId::new(glyph.id),
                            glyph.advance / font_size,
                            glyph.x / font_size,
                            glyph.y / font_size,
                            0.0,
                            text_range.clone(),
                            None,
                        ));
                    }
                }

                if !glyphs.is_empty() {
                    let draw_x = pos_x + glyph_run.offset();
                    let draw_y = pos_y + baseline;

                    surface.draw_glyphs(
                        Point::from_xy(draw_x, draw_y),
                        &glyphs,
                        krilla_font,
                        text,
                        font_size,
                        false,
                    );
                }
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Gradient, shadow, and border helpers
// ---------------------------------------------------------------------------

#[cfg(feature = "pdf")]
fn convert_linear_gradient(
    direction: &style::values::computed::LineDirection,
    items: &[GenericGradientItem<
        style::values::generics::color::GenericColor<style::values::computed::Percentage>,
        style::values::computed::LengthPercentage,
    >],
    flags: GradientFlags,
    rect_width: f32,
    rect_height: f32,
    current_color: &AbsoluteColor,
) -> Option<krilla::paint::LinearGradient> {
    use krilla::geom::Transform;
    use krilla::paint::{LinearGradient, SpreadMethod};
    use style::values::computed::LineDirection;

    let (x1, y1, x2, y2) = match direction {
        LineDirection::Angle(angle) => {
            let radians = -angle.radians() + std::f32::consts::PI;
            let center_x = rect_width / 2.0;
            let center_y = rect_height / 2.0;
            let offset_len =
                rect_width / 2.0 * radians.sin().abs() + rect_height / 2.0 * radians.cos().abs();
            (
                center_x - offset_len * radians.sin(),
                center_y - offset_len * radians.cos(),
                center_x + offset_len * radians.sin(),
                center_y + offset_len * radians.cos(),
            )
        }
        LineDirection::Horizontal(horizontal) => {
            let mid_y = rect_height / 2.0;
            match horizontal {
                HorizontalPositionKeyword::Right => (0.0, mid_y, rect_width, mid_y),
                HorizontalPositionKeyword::Left => (rect_width, mid_y, 0.0, mid_y),
            }
        }
        LineDirection::Vertical(vertical) => {
            let mid_x = rect_width / 2.0;
            match vertical {
                VerticalPositionKeyword::Top => (mid_x, rect_height, mid_x, 0.0),
                VerticalPositionKeyword::Bottom => (mid_x, 0.0, mid_x, rect_height),
            }
        }
        LineDirection::Corner(horizontal, vertical) => {
            let (start_x, end_x) = match horizontal {
                HorizontalPositionKeyword::Right => (0.0, rect_width),
                HorizontalPositionKeyword::Left => (rect_width, 0.0),
            };
            let (start_y, end_y) = match vertical {
                VerticalPositionKeyword::Top => (rect_height, 0.0),
                VerticalPositionKeyword::Bottom => (0.0, rect_height),
            };
            (start_x, start_y, end_x, end_y)
        }
    };

    let gradient_length = ((x2 - x1).powi(2) + (y2 - y1).powi(2)).sqrt();
    let gradient_length_css = CSSPixelLength::new(gradient_length);

    let stops = convert_gradient_stops(items, gradient_length_css, current_color);
    if stops.is_empty() {
        return None;
    }

    let repeating = flags.contains(GradientFlags::REPEATING);

    Some(LinearGradient {
        x1,
        y1,
        x2,
        y2,
        transform: Transform::identity(),
        spread_method: if repeating {
            SpreadMethod::Repeat
        } else {
            SpreadMethod::Pad
        },
        stops,
        anti_alias: true,
    })
}

#[cfg(feature = "pdf")]
fn convert_gradient_stops(
    items: &[GenericGradientItem<
        style::values::generics::color::GenericColor<style::values::computed::Percentage>,
        style::values::computed::LengthPercentage,
    >],
    gradient_length: CSSPixelLength,
    current_color: &AbsoluteColor,
) -> Vec<krilla::paint::Stop> {
    use style::values::specified::percentage::ToPercentage;

    let mut stops = Vec::new();
    let num_items = items
        .iter()
        .filter(|item| !matches!(item, GenericGradientItem::InterpolationHint(_)))
        .count();

    let mut color_stop_idx = 0;
    for item in items.iter() {
        match item {
            GenericGradientItem::SimpleColorStop(color) => {
                let offset = if num_items > 1 {
                    color_stop_idx as f32 / (num_items - 1) as f32
                } else {
                    0.0
                };
                color_stop_idx += 1;
                if let Some(stop) = color_to_krilla_stop(color, offset, current_color) {
                    stops.push(stop);
                }
            }
            GenericGradientItem::ComplexColorStop { color, position } => {
                if let Some(percentage) = position.to_percentage_of(gradient_length) {
                    let offset = percentage.to_percentage();
                    color_stop_idx += 1;
                    if let Some(stop) = color_to_krilla_stop(color, offset, current_color) {
                        stops.push(stop);
                    }
                }
            }
            GenericGradientItem::InterpolationHint(_) => {}
        }
    }

    stops
}

#[cfg(feature = "pdf")]
fn color_to_krilla_stop(
    color: &style::values::generics::color::GenericColor<style::values::computed::Percentage>,
    offset: f32,
    current_color: &AbsoluteColor,
) -> Option<krilla::paint::Stop> {

    let abs_color = color.resolve_to_absolute(current_color);
    let srgb = abs_color.to_color_space(style::color::ColorSpace::Srgb);

    let r = (srgb.components.0.clamp(0.0, 1.0) * 255.0) as u8;
    let g = (srgb.components.1.clamp(0.0, 1.0) * 255.0) as u8;
    let b = (srgb.components.2.clamp(0.0, 1.0) * 255.0) as u8;
    let alpha = srgb.alpha.clamp(0.0, 1.0);

    Some(krilla::paint::Stop {
        offset: NormalizedF32::new(offset.clamp(0.0, 1.0))?,
        color: rgb::Color::new(r, g, b).into(),
        opacity: NormalizedF32::new(alpha).unwrap_or(NormalizedF32::ONE),
    })
}

#[cfg(feature = "pdf")]
fn draw_gradient_rect(
    surface: &mut Surface,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    gradient: krilla::paint::LinearGradient,
) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }

    let translated_gradient = krilla::paint::LinearGradient {
        x1: x + gradient.x1,
        y1: y + gradient.y1,
        x2: x + gradient.x2,
        y2: y + gradient.y2,
        ..gradient
    };

    let mut builder = PathBuilder::new();
    builder.move_to(x, y);
    builder.line_to(x + w, y);
    builder.line_to(x + w, y + h);
    builder.line_to(x, y + h);
    builder.close();

    if let Some(path) = builder.finish() {
        let fill = Fill {
            paint: translated_gradient.into(),
            opacity: NormalizedF32::ONE,
            rule: FillRule::NonZero,
        };
        surface.set_fill(Some(fill));
        surface.draw_path(&path);
    }
}

#[cfg(feature = "pdf")]
fn draw_borders(
    surface: &mut Surface,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    borders: &[EdgeBorder; 4],
) {
    let [top, right, bottom, left] = borders;

    if top.visible && top.alpha > 0.0 {
        draw_border_edge(
            surface,
            [(x, y), (x + width, y)],
            [
                (x + left.width, y + top.width),
                (x + width - right.width, y + top.width),
            ],
            top.color,
            top.alpha,
        );
    }
    if right.visible && right.alpha > 0.0 {
        draw_border_edge(
            surface,
            [(x + width, y), (x + width, y + height)],
            [
                (x + width - right.width, y + top.width),
                (x + width - right.width, y + height - bottom.width),
            ],
            right.color,
            right.alpha,
        );
    }
    if bottom.visible && bottom.alpha > 0.0 {
        draw_border_edge(
            surface,
            [(x + width, y + height), (x, y + height)],
            [
                (x + width - right.width, y + height - bottom.width),
                (x + left.width, y + height - bottom.width),
            ],
            bottom.color,
            bottom.alpha,
        );
    }
    if left.visible && left.alpha > 0.0 {
        draw_border_edge(
            surface,
            [(x, y + height), (x, y)],
            [
                (x + left.width, y + height - bottom.width),
                (x + left.width, y + top.width),
            ],
            left.color,
            left.alpha,
        );
    }
}

#[cfg(feature = "pdf")]
fn draw_border_edge(
    surface: &mut Surface,
    outer: [(f32, f32); 2],
    inner: [(f32, f32); 2],
    color: Rgb,
    alpha: f32,
) {
    if alpha <= 0.0 {
        return;
    }

    let mut builder = PathBuilder::new();
    builder.move_to(outer[0].0, outer[0].1);
    builder.line_to(outer[1].0, outer[1].1);
    builder.line_to(inner[1].0, inner[1].1);
    builder.line_to(inner[0].0, inner[0].1);
    builder.close();

    if let Some(path) = builder.finish() {
        let fill = Fill {
            paint: rgb::Color::new(color.r, color.g, color.b).into(),
            opacity: NormalizedF32::new(alpha).unwrap_or(NormalizedF32::ONE),
            rule: FillRule::NonZero,
        };
        surface.set_fill(Some(fill));
        surface.draw_path(&path);
    }
}

#[cfg(feature = "pdf")]
fn draw_outset_box_shadow(
    surface: &mut Surface,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    shadow: &BoxShadowData,
    radii: &BorderRadii,
) {
    if shadow.alpha <= 0.0 {
        return;
    }

    let shadow_x = x + shadow.x_offset - shadow.spread;
    let shadow_y = y + shadow.y_offset - shadow.spread;
    let shadow_w = width + shadow.spread * 2.0;
    let shadow_h = height + shadow.spread * 2.0;

    if shadow.blur <= 0.0 {
        draw_rect_with_alpha(
            surface,
            shadow_x,
            shadow_y,
            shadow_w,
            shadow_h,
            shadow.color,
            shadow.alpha,
            radii,
            shadow.spread,
        );
    } else {
        let blur_steps = (shadow.blur / 3.0).ceil().clamp(2.0, 8.0) as usize;
        let step_expand = shadow.blur * 2.5 / blur_steps as f32;

        for i in 0..blur_steps {
            let expand = i as f32 * step_expand;
            let layer_x = shadow_x - expand / 2.0;
            let layer_y = shadow_y - expand / 2.0;
            let layer_w = shadow_w + expand;
            let layer_h = shadow_h + expand;

            let progress = i as f32 / blur_steps as f32;
            let layer_alpha = shadow.alpha * (1.0 - progress * progress) / blur_steps as f32 * 2.0;

            if layer_alpha > 0.001 {
                draw_rect_with_alpha(
                    surface,
                    layer_x,
                    layer_y,
                    layer_w,
                    layer_h,
                    shadow.color,
                    layer_alpha,
                    radii,
                    shadow.spread + expand / 2.0,
                );
            }
        }
    }
}

#[cfg(feature = "pdf")]
fn draw_inset_box_shadow(
    surface: &mut Surface,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    shadow: &BoxShadowData,
    radii: &BorderRadii,
) {
    if shadow.alpha <= 0.0 {
        return;
    }

    if let Some(clip_path) = build_rounded_rect_path(x, y, width, height, radii) {
        surface.push_clip_path(&clip_path, &FillRule::NonZero);
    }

    let blur_steps = if shadow.blur > 0.0 {
        (shadow.blur / 3.0).ceil().clamp(2.0, 8.0) as usize
    } else {
        1
    };

    let spread = shadow.spread.abs();
    let inset_depth = shadow.blur * 1.5 + spread;

    for i in 0..blur_steps {
        let progress = i as f32 / blur_steps as f32;
        let depth = inset_depth * (1.0 - progress);
        let layer_alpha = shadow.alpha * (1.0 - progress) / blur_steps as f32;

        if layer_alpha > 0.001 && depth > 0.0 {
            if shadow.y_offset > 0.0 {
                let edge_h = depth.min(shadow.y_offset + depth);
                draw_rect_simple(surface, x + shadow.x_offset, y + shadow.y_offset, width, edge_h, shadow.color, layer_alpha);
            }
            if shadow.y_offset < 0.0 {
                let edge_h = depth.min(-shadow.y_offset + depth);
                draw_rect_simple(surface, x + shadow.x_offset, y + height + shadow.y_offset - edge_h, width, edge_h, shadow.color, layer_alpha);
            }
            if shadow.x_offset > 0.0 {
                let edge_w = depth.min(shadow.x_offset + depth);
                draw_rect_simple(surface, x + shadow.x_offset, y + shadow.y_offset, edge_w, height, shadow.color, layer_alpha);
            }
            if shadow.x_offset < 0.0 {
                let edge_w = depth.min(-shadow.x_offset + depth);
                draw_rect_simple(surface, x + width + shadow.x_offset - edge_w, y + shadow.y_offset, edge_w, height, shadow.color, layer_alpha);
            }
        }
    }

    surface.pop();
}

#[cfg(feature = "pdf")]
#[allow(clippy::too_many_arguments)]
fn draw_rect_with_alpha(
    surface: &mut Surface,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    color: Rgb,
    alpha: f32,
    radii: &BorderRadii,
    spread: f32,
) {
    if w <= 0.0 || h <= 0.0 || alpha <= 0.0 {
        return;
    }

    let scaled_radii = if radii.has_any_radius() && spread != 0.0 {
        let scale = |r: f32| (r + spread).max(0.0);
        BorderRadii {
            top_left: (scale(radii.top_left.0), scale(radii.top_left.1)),
            top_right: (scale(radii.top_right.0), scale(radii.top_right.1)),
            bottom_right: (scale(radii.bottom_right.0), scale(radii.bottom_right.1)),
            bottom_left: (scale(radii.bottom_left.0), scale(radii.bottom_left.1)),
        }
    } else {
        *radii
    };

    let path = if scaled_radii.has_any_radius() {
        build_rounded_rect_path(x, y, w, h, &scaled_radii)
    } else {
        let mut builder = PathBuilder::new();
        builder.move_to(x, y);
        builder.line_to(x + w, y);
        builder.line_to(x + w, y + h);
        builder.line_to(x, y + h);
        builder.close();
        builder.finish()
    };

    if let Some(path) = path {
        let fill = Fill {
            paint: rgb::Color::new(color.r, color.g, color.b).into(),
            opacity: NormalizedF32::new(alpha).unwrap_or(NormalizedF32::ZERO),
            rule: FillRule::NonZero,
        };
        surface.set_fill(Some(fill));
        surface.draw_path(&path);
    }
}

#[cfg(feature = "pdf")]
fn draw_rect_simple(surface: &mut Surface, x: f32, y: f32, w: f32, h: f32, color: Rgb, alpha: f32) {
    if w <= 0.0 || h <= 0.0 || alpha <= 0.0 {
        return;
    }

    let mut builder = PathBuilder::new();
    builder.move_to(x, y);
    builder.line_to(x + w, y);
    builder.line_to(x + w, y + h);
    builder.line_to(x, y + h);
    builder.close();

    if let Some(path) = builder.finish() {
        let fill = Fill {
            paint: rgb::Color::new(color.r, color.g, color.b).into(),
            opacity: NormalizedF32::new(alpha).unwrap_or(NormalizedF32::ZERO),
            rule: FillRule::NonZero,
        };
        surface.set_fill(Some(fill));
        surface.draw_path(&path);
    }
}

#[cfg(feature = "pdf")]
fn get_content_height(document: &HtmlDocument) -> Option<f32> {
    let doc = document.as_ref();
    let root = doc.root_element();
    Some(root.final_layout.size.height)
}