//! Agent plane — the browser as the agent's workstation.
//!
//! Execution design: `docs/designs/2026-10-16-agent-plane-execution.md`.
//! M0 ships this observation module; the REPL world (M1), steering events
//! (M2), the agent loop (M3), memory (M4) and CUA fallback (M5) land here
//! as they are built.

pub mod obs;
