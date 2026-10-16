//! Agent plane — the browser as the agent's workstation.
//!
//! Execution design: `docs/designs/2026-10-16-agent-plane-execution.md`.
//! M0: observation ([`obs`]). M1: the Playwright-shaped REPL
//! ([`world`] boa thread + [`repl`] async host + [`locators`] +
//! [`gates`]). M2: steering events ([`events`]). The agent loop (M3),
//! memory (M4) and CUA fallback (M5) land here as they are built.

pub mod events;
pub mod gates;
pub mod locators;
pub mod obs;
pub mod repl;
pub mod world;
