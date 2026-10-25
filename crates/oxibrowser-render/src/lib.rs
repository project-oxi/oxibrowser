//! CSS-aware HTML rendering pipeline for OxiBrowser, backed by Blitz.
//!
//! This crate is the **only** place in the workspace that depends on the Blitz
//! rendering stack (Stylo CSS engine, Taffy layout, Parley text, vello_cpu
//! paint). It exposes a [`RenderDocument`] that wraps a Blitz [`BaseDocument`]
//! and produces real CSS-laid-out PNG screenshots — replacing the legacy
//! text-based bitmap-font renderer in `oxibrowser-core::css`.
//!
//! ## Threading
//!
//! `BaseDocument` (Stylo-backed) is effectively `!Send`, so a `RenderDocument`
//! must live on a single thread. In the integrated design it is owned by the
//! JS thread alongside boa's `Context`; in this Phase-1 crate it is driven
//! directly by the caller. See `docs/designs/2026-08-07-blitz-rendering-integration.md`.

mod document;
mod paint;

#[cfg(feature = "webgl")]
pub mod gl;

// Re-export Blitz DOM types so downstream crates (oxibrowser-core) can walk the
// `BaseDocument` tree (returned by `RenderDocument::document()`) without adding
// a direct blitz-dom dependency.
pub use blitz_dom::{BaseDocument, NodeData};

pub use document::{
    CaptureOpts, NodeId, RenderDocument, RenderError, Viewport, color_scheme_override_dark,
    set_color_scheme_override,
};
pub use paint::blank_png;
pub use paint::encode_png;
pub use paint::png_to_pdf;
pub use paint::{PdfOrientation, PdfPageOptions, PdfPageSize, png_to_pdf_paged};
