//! OxiBrowser CDP — Chrome DevTools Protocol server.
//!
//! Implements the CDP WebSocket protocol so that tools like Puppeteer and
//! Playwright can connect to OxiBrowser, just like they connect to Chrome.
//!

pub mod account;
pub mod core_event;
pub mod credential;
pub mod event;
pub mod protocol;
pub mod refs;
pub mod server;
pub mod session;

pub mod domains;

pub use account::LoginSurface;
pub use server::CdpServer;
