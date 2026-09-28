//! Credential broker failures (lower design §4.3 `error.rs`).

/// Errors surfaced by the credential broker.
///
/// Variants mirror the CDP error codes the surface layer must emit
/// (design §6.3): `consentRequired`, `originMismatch`, `credentialNotFound`.
#[derive(Debug, thiserror::Error)]
pub enum CredError {
    /// Secure keystore absent or unusable. No plaintext fallback is ever
    /// attempted — the broker stops instead (design §9 FM-7).
    #[error("credential keystore unavailable: {0}")]
    KeyStoreUnavailable(String),
    /// No credential under the requested handle.
    #[error("credential not found: {0}")]
    NotFound(String),
    /// Policy requires an explicit consent grant that is absent, expired, or
    /// exhausted. CDP maps this to `consentRequired`.
    #[error("consent required: {credential} may not be used at {origin} for action `{action}`")]
    ConsentRequired {
        credential: String,
        origin: String,
        action: String,
    },
    /// Requested use origin is not in the credential's exact-origin
    /// allowlist. CDP maps this to `originMismatch`.
    #[error("origin mismatch: credential {credential} is not allowed at {origin}")]
    OriginMismatch { credential: String, origin: String },
    /// Stored session fingerprint does not match the current one — refusal,
    /// never a warning (fail-closed, design §4.3 `SessionStore::load`).
    #[error("fingerprint mismatch: stored {expected}, current {found}")]
    FingerprintMismatch { expected: String, found: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Keyring backend failure other than a missing entry or an unavailable
    /// store (internal mapping of `keyring_core::Error`).
    #[error("keyring backend error: {0}")]
    Keyring(String),
    /// TOTP generation or decoding failure.
    #[error("totp error: {0}")]
    Totp(String),
    /// Malformed handle, record, URI, or configuration.
    #[error("invalid credential input: {0}")]
    Invalid(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_carry_context_without_values() {
        let e = CredError::ConsentRequired {
            credential: "kch:main/cloudflare.com/password/dashboard".into(),
            origin: "https://dash.cloudflare.com".into(),
            action: "login".into(),
        };
        let msg = e.to_string();
        assert!(msg.contains("kch:main/cloudflare.com/password/dashboard"));
        assert!(msg.contains("consent required"));
        // Errors reference handles, never secret values — nothing to leak by
        // construction, but keep the guarantee visible.
        let io_err = CredError::Io(std::io::Error::other("disk full"));
        assert!(io_err.to_string().contains("disk full"));
    }
}
