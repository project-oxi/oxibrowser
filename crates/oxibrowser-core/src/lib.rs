//! OxiBrowser Core — Browser lifecycle, Session, Page, and Frame management.
//!
//! Uses Servo's html5ever for HTML parsing and boa_engine for JavaScript execution.

pub mod browse_result;
pub mod browser;
pub mod challenge;
pub mod config;
pub mod context;
pub mod css;
pub mod dom_link;
pub mod event;
pub mod extract;
pub mod fonts;
pub mod frame;
pub mod page;
pub mod script;
pub mod security;
pub mod session;
pub mod storage_state;
pub mod tab;

pub mod js;
pub mod network;

pub mod encoding;

pub mod error;

/// Blank white PNG fallback (re-exported from the render crate).
pub use oxibrowser_render::blank_png;
pub use oxibrowser_render::png_to_pdf;
pub use oxibrowser_render::{PdfOrientation, PdfPageOptions, PdfPageSize, png_to_pdf_paged};

pub use browse_result::BrowseResult;
pub use browser::Browser;
pub use config::BrowserConfig;
pub use config::BrowserConfigBuilder;
pub use context::{BrowserContext, ContextConfig, ContextId};
pub use error::Result;
pub use event::BrowserEvent;
pub use storage_state::{OriginState, StorageState};
pub use tab::Tab;
