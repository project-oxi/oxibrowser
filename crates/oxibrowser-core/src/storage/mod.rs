//! Persistent account-scoped storage (design M6′).
//!
//! [`session_store::SessionStore`] persists one encrypted [`session_store::SessionEnvelope`]
//! per account scope as an OXSESS1 AEAD file; encryption keys are injected via the
//! [`session_store::KeyProvider`] trait so the core crate never touches the OS keychain.

pub mod session_store;

pub use session_store::{
    ClientHints, EgressMeta, FingerprintMeta, KeyProvider, SESSION_FILE_MAGIC, ScopeSummary,
    SessionEnvelope, SessionStore, StaticKeyProvider,
};
