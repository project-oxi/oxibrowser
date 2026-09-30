//! OxiBrowser credential broker — secrets never leave the broker as values;
//! surfaces only ever see handles plus fingerprint prefixes.
//!
//! Implements the lower design (`docs/designs/2026-09-27-agent-auth-implementation.md`
//! §4.3, §5.1–§5.5, §4.4) and the account-grant subject extension from the
//! upper design (`docs/designs/2026-09-28-account-login-session-management.md`
//! §7.3 / M4). Dependency directions: `credentials → core`, one-way.
//!
//! Layering, in the order the broker touches a credential:
//!
//! 1. [`SecretBox`] — the only value container; no `Debug`/`Display`/`Serialize`.
//! 2. [`CredentialProvider`] — storage backends ([`KeyringProvider`] on the OS
//!    keychain, [`InMemoryProvider`] for tests/headless); `resolve` is the only
//!    value-returning path and audits a `credential_read`.
//! 3. [`TotpGenerator`] — RFC 6238 codes from an `otpauth://` URI or base32
//!    secret; bumps to the next window while fewer than 3 seconds remain.
//! 4. [`ConsentStore`] — append-only JSONL grants, last-wins, tombstone
//!    revocations, expiry/use-count filtering.
//! 5. [`PolicyEngine`] — deny rules → consent → confirmation, every decision
//!    audited as a `credential_use` event with handle + fingerprint only.

pub mod agent;
pub mod consent;
pub mod error;
pub mod keyring;
pub mod policy;
pub mod provider;
pub mod secret;
pub mod totp;

pub use agent::BrokerSource;

pub use consent::{
    ConsentRecord, ConsentStore, ConsentSubject, DEFAULT_CONSENT_TTL, DEFAULT_MAX_USES, Timestamp,
};
pub use error::CredError;
pub use keyring::{KeyringKeyProvider, KeyringProvider};
pub use oxibrowser_core::storage::session_store::KeyProvider;
pub use policy::{CONFIRMATION_TTL, ConfirmationToken, CredentialAction, PolicyEngine, UseRequest};
pub use provider::{
    CredentialId, CredentialKind, CredentialMeta, CredentialProvider, CredentialRecord,
    InMemoryProvider, NewCredential, SERVICE_PREFIX,
};
pub use secret::SecretBox;
pub use totp::TotpGenerator;

#[cfg(test)]
pub(crate) fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "oxi-{tag}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}
