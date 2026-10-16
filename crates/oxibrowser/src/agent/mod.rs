//! Agent plane — the browser as the agent's workstation.
//!
//! Execution design: `docs/designs/2026-10-16-agent-plane-execution.md`.
//! M0: observation ([`obs`]). M1: the Playwright-shaped REPL
//! ([`world`] boa thread + [`repl`] async host + [`locators`] +
//! [`gates`]). M2: steering events ([`events`]). M3: the agent loop
//! ([`loop_`] + [`model`] + [`prompt`]). Memory (M4) and CUA fallback (M5)
//! land here as they are built.

pub mod cli;
pub mod events;
pub mod gates;
pub mod locators;
pub mod loop_;
pub mod model;
pub mod obs;
pub mod prompt;
pub mod repl;
pub mod site;
pub mod world;
