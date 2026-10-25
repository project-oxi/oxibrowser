//! Presentation path: reads WebGL drawing buffers into the canvas nodes'
//! raster-image slots so blitz's normal image compositing paints them.
//!
//! * Screenshots (`Page.captureScreenshot`) call [`flush_canvas`] with
//!   `present = true` — this mirrors WebGL's clear-after-compositing rule
//!   (`preserveDrawingBuffer: false` leaves a cleared buffer afterwards).
//! * `toDataURL`/`toBlob` use `present = false` (not a compositing event).
//!
//! Always reading back (not just when "dirty") is deliberate: after a present
//! the drawing buffer is cleared, so a second screenshot without new draws
//! must show a cleared buffer, exactly like a real browser.

use std::sync::Arc;

use crate::js::webgl;
use oxibrowser_render::RenderDocument;

/// Reads back every live WebGL canvas in the current frame and writes the
/// bitmap into its canvas node. Must run on the JS thread before painting.
/// `present = true` mirrors compositing (clear-after-present semantics).
pub fn flush_canvas(doc: &mut RenderDocument, present: bool) {
    for (_, node_id) in webgl::entries() {
        let Some((w, h, rgba)) = webgl::readback_straight(node_id, present) else {
            continue;
        };
        if w == 0 || h == 0 {
            continue;
        }
        doc.set_canvas_bitmap(node_id, w, h, Arc::new(rgba));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn premultiplied_to_straight_round_trip() {
        // (128, 128, 128, 128) premultiplied ≈ (255, 255, 255, 128) straight.
        let mut px = [128u8, 128, 128, 128];
        crate::js::webgl::premultiplied_to_straight(&mut px);
        assert!(px[0] >= 253, "{px:?}");
        assert_eq!(px[3], 128);
        // Opaque and transparent stay exact.
        let mut op = [10u8, 20, 30, 255];
        crate::js::webgl::premultiplied_to_straight(&mut op);
        assert_eq!(op, [10, 20, 30, 255]);
        let mut tr = [0u8, 0, 0, 0];
        crate::js::webgl::premultiplied_to_straight(&mut tr);
        assert_eq!(tr, [0, 0, 0, 0]);
    }
}
