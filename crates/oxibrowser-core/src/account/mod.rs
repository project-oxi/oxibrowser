//! Account registry, lifecycle management, login detection, and validation
//! probes (upper design `2026-09-28-account-login-session-management.md` §4,
//! milestone M-B).
//!
//! Layout:
//!
//! - [`record`] — [`AccountRecord`] (`account.json` schema §4.1) and the
//!   lifecycle state machine (§4.2).
//! - [`registry`] — [`AccountRegistry`], the on-disk store under
//!   `~/.oxibrowser/accounts/` (§6.1): one directory per account, atomic
//!   0600 record writes, 0700 directories, per-account [`SessionStore`].
//! - [`manager`] — [`AccountManager`]: capture/restore/logout lifecycle
//!   over [`crate::session::Session`] + [`crate::storage::SessionStore`],
//!   every transition audited (§6.4).
//! - [`detector`] — [`LoginDetector`]: broker-computed login-success signals
//!   (§4.3) — page text is never trusted on its own.
//! - [`probe`] — [`ValidationProbe`]: post-detection false-positive defense
//!   (§4.4).
//!
//! The registry holds **no secrets**: credential handles are opaque strings
//! (the credentials crate owns their meaning), session envelopes are sealed
//! by [`crate::storage::SessionStore`] with keys from a [`KeyProvider`].

pub mod agent_login;
pub mod detector;
pub mod manager;
pub mod orchestrator;
pub mod probe;
pub mod record;
pub mod registry;

pub use agent_login::{
    AgentLoginEngine, AgentLoginOutcome, AgentLoginProgress, CredentialSource, FieldInput,
    LoginForm, MfaForbidden, MfaStep, ResolvedLogin, SourceError,
};

pub use detector::{
    Detection, DetectionInput, LoginDetector, PreLoginSnapshot, Signal, SignalKind,
};
pub use manager::AccountManager;
pub use orchestrator::{
    AccountEvent, CompleteVerdict, EndOutcome, ImportOutcome, LoginEndState, LoginHandle,
    LoginMode, LoginOrchestrator, LoginOutcome, ProbeVerdictSummary,
};
pub use probe::{ProbeOutcome, ProbeVerdict, ValidationProbe};
pub use record::{
    AccountRecord, AccountState, IdentityCard, ProbeConfig, SessionSummary, validate_account_id,
};
pub use registry::AccountRegistry;

pub use crate::storage::session_store::KeyProvider;
