//! OS keychain backend — [`KeyringProvider`] (lower design §4.3 `keyring.rs`,
//! §5.1 service/account conventions).
//!
//! Built on `keyring` 4.x with its default `v1` feature: on macOS that links
//! the Apple Keychain store (`apple-native-keyring-store`/keychain), on Linux
//! the Secret Service, on Windows Credential Manager. The v1 module registers
//! the platform store as the `keyring-core` default, which is also what makes
//! [`keyring_core::Entry::search`] — the only enumeration path — work for
//! [`CredentialProvider::list`].
//!
//! Key layout (design §5.1): `service = <prefix>/<agent-id>/<scope>`,
//! `account = <kind>/<slug>`. Origin restriction lives in the stored record's
//! `allowed_origins`, never in the key.

use std::collections::HashMap;

use zeroize::Zeroizing;

use oxibrowser_core::security::audit::{
    AuditDecision, AuditEventKind, CredentialRef, event as audit_event, record as audit_record,
    secret_fingerprint,
};

use crate::error::CredError;
use crate::provider::{
    CredentialId, CredentialMeta, CredentialParts, CredentialProvider, NewCredential,
    SERVICE_PREFIX, account_key, build_record, parse_record, record_value,
};
use crate::secret::SecretBox;

/// OS-backed credential store. Stateless per call — safe to share via `Arc`.
#[derive(Debug, Clone)]
pub struct KeyringProvider {
    service_prefix: String,
}

impl Default for KeyringProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyringProvider {
    /// Provider with the standard `com.oxibrowser.agent` service prefix.
    pub fn new() -> Self {
        Self {
            service_prefix: SERVICE_PREFIX.to_string(),
        }
    }

    /// Provider under a custom service prefix (tests, per-profile isolation).
    pub fn with_service_prefix(prefix: impl Into<String>) -> Self {
        Self {
            service_prefix: prefix.into(),
        }
    }

    fn service_of(&self, agent_id: &str, scope: &str) -> String {
        format!("{}/{agent_id}/{scope}", self.service_prefix)
    }

    /// Service prefix that identifies our items: `<prefix>/`.
    fn item_prefix(&self) -> String {
        format!("{}/", self.service_prefix)
    }
}

/// Force the v1 module's one-time platform store initialization.
fn ready() -> Result<(), CredError> {
    match keyring::v1::Entry::store_status() {
        Ok(()) => Ok(()),
        Err(e) => Err(CredError::KeyStoreUnavailable(e.to_string())),
    }
}

/// Map a keyring error, keeping the design's error taxonomy: missing entries
/// are `NotFound`, storage-level failures are `KeyStoreUnavailable` (FM-7),
/// the rest stay internal.
fn map_keyring_err(id: Option<&CredentialId>, e: keyring_core::Error) -> CredError {
    use keyring_core::Error as KE;
    match e {
        KE::NoEntry => CredError::NotFound(
            id.map(|i| i.0.clone())
                .unwrap_or_else(|| "keychain entry".to_string()),
        ),
        KE::NoDefaultStore | KE::NotSupportedByStore(_) => {
            CredError::KeyStoreUnavailable(e.to_string())
        }
        KE::PlatformFailure(_) | KE::NoStorageAccess(_) => {
            CredError::KeyStoreUnavailable(e.to_string())
        }
        _ => CredError::Keyring(e.to_string()),
    }
}

/// Store-absence family (mirrors the `KeyStoreUnavailable` grouping in
/// [`map_keyring_err`]): the platform offers no usable default credential
/// store — headless CI has no Secret Service, a locked keychain surfaces as
/// `NoStorageAccess`/`PlatformFailure`. Tests that merely construct entries
/// skip on these instead of panicking.
#[cfg(test)]
fn is_store_absent(e: &keyring_core::Error) -> bool {
    use keyring_core::Error as KE;
    matches!(
        e,
        KE::NoDefaultStore
            | KE::NotSupportedByStore(_)
            | KE::PlatformFailure(_)
            | KE::NoStorageAccess(_)
    )
}

fn entry_for(
    provider: &KeyringProvider,
    parts: &CredentialParts,
) -> Result<keyring::v1::Entry, CredError> {
    ready()?;
    let service = provider.service_of(&parts.agent_id, &parts.scope);
    let account = account_key(parts.kind, &parts.slug);
    keyring::v1::Entry::new(&service, &account).map_err(|e| map_keyring_err(None, e))
}

fn audit_credential_read(id: &CredentialId, value: &[u8]) {
    let mut event = audit_event(
        AuditEventKind::CredentialRead,
        AuditDecision::Allow,
        "broker_resolve",
    );
    event.credential = Some(CredentialRef {
        id: id.0.clone(),
        fingerprint: secret_fingerprint(value),
    });
    // Global sink: no-op unless the binary initialized audit (core §4.1).
    audit_record(event);
}

impl CredentialProvider for KeyringProvider {
    fn put(&self, cred: NewCredential) -> Result<CredentialId, CredError> {
        let record = build_record(cred)?;
        let parts = record.meta.id.parse()?;
        let entry = entry_for(self, &parts)?;
        let json = serde_json::to_string(&record)
            .map_err(|e| CredError::Invalid(format!("record serialization failed: {e}")))?;
        entry
            .set_password(&json)
            .map_err(|e| map_keyring_err(None, e))?;
        Ok(record.meta.id.clone())
    }

    fn resolve(&self, id: &CredentialId) -> Result<(CredentialMeta, SecretBox), CredError> {
        let parts = id.parse()?;
        let entry = entry_for(self, &parts)?;
        let json = entry
            .get_password()
            .map_err(|e| map_keyring_err(Some(id), e))?;
        let record = parse_record(&json)?;
        let value = record_value(&record)?.to_string();
        audit_credential_read(id, value.as_bytes());
        Ok((record.meta, SecretBox::from_string(value)))
    }

    fn metadata(&self, id: &CredentialId) -> Result<CredentialMeta, CredError> {
        let parts = id.parse()?;
        let entry = entry_for(self, &parts)?;
        let json = entry
            .get_password()
            .map_err(|e| map_keyring_err(Some(id), e))?;
        Ok(parse_record(&json)?.into_meta())
    }

    fn list(&self, agent_id: Option<&str>) -> Result<Vec<CredentialMeta>, CredError> {
        ready()?;
        // keyring-core search has exact service/user keys only; an empty spec
        // enumerates the store and we filter by our service prefix before
        // touching any password bytes.
        let found =
            keyring_core::Entry::search(&HashMap::new()).map_err(|e| map_keyring_err(None, e))?;
        let prefix = self.item_prefix();
        let mut metas = Vec::new();
        for entry in found {
            let Some((service, _account)) = entry.get_specifiers() else {
                continue;
            };
            let Some(rest) = service.strip_prefix(&prefix) else {
                continue;
            };
            if let Some(agent) = agent_id
                && rest.split('/').next() != Some(agent)
            {
                continue;
            }
            let Ok(json) = entry.get_password() else {
                continue;
            };
            if let Ok(record) = parse_record(&json) {
                metas.push(record.into_meta());
            }
        }
        metas.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        Ok(metas)
    }

    fn delete(&self, id: &CredentialId) -> Result<(), CredError> {
        let parts = id.parse()?;
        let entry = entry_for(self, &parts)?;
        entry
            .delete_credential()
            .map_err(|e| map_keyring_err(Some(id), e))
    }
}

// ---------------------------------------------------------------------------
// Session AEAD keys — keychain-backed KeyProvider (lower design §5.1)
// ---------------------------------------------------------------------------

const SESSION_KEY_SERVICE: &str = "_session-keys";
const SESSION_KEY_ACCOUNT: &str = "aead-key";

/// OS-keychain session-key provider for the account session envelopes
/// (upper design §3.5: this implementation lives in the credentials crate;
/// surfaces like the CLI reuse it instead of carrying their own copy).
///
/// Keys are created once per scope and reused; there is no plaintext
/// fallback (FM-6). Key layout (§5.1): `service = <prefix>/_session-keys/<scope>`,
/// `account = "aead-key"`.
#[derive(Debug, Clone)]
pub struct KeyringKeyProvider {
    service_prefix: String,
}

impl Default for KeyringKeyProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyringKeyProvider {
    /// Provider under the design service prefix (`com.oxibrowser.agent`).
    pub fn new() -> Self {
        KeyringKeyProvider {
            service_prefix: SERVICE_PREFIX.to_string(),
        }
    }

    /// Provider under an explicit prefix (tests, embedded setups).
    pub fn with_service_prefix(prefix: impl Into<String>) -> Self {
        KeyringKeyProvider {
            service_prefix: prefix.into(),
        }
    }

    fn service_of(&self, scope: &str) -> String {
        format!("{}/{SESSION_KEY_SERVICE}/{scope}", self.service_prefix)
    }
}

impl crate::KeyProvider for KeyringKeyProvider {
    fn session_key(
        &self,
        scope: &str,
    ) -> Result<Zeroizing<[u8; 32]>, oxibrowser_core::error::CoreError> {
        use base64::Engine as _;
        use oxibrowser_core::error::CoreError;

        let service = self.service_of(scope);
        let entry = keyring::v1::Entry::new(&service, SESSION_KEY_ACCOUNT)
            .map_err(|e| CoreError::SessionStoreIo(format!("keychain unavailable: {e}")))?;
        match entry.get_password() {
            Ok(stored) => decode_session_key(&stored),
            Err(keyring_core::Error::NoEntry) => {
                // create-once: 32 random bytes, base64 in the password slot
                let mut raw = [0u8; 32];
                getrandom::fill(&mut raw)
                    .map_err(|e| CoreError::SessionStoreAead(format!("keygen failed: {e}")))?;
                let encoded = base64::engine::general_purpose::STANDARD.encode(&raw[..]);
                entry.set_password(&encoded).map_err(|e| {
                    CoreError::SessionStoreIo(format!("keychain write failed: {e}"))
                })?;
                Ok(Zeroizing::new(raw))
            }
            Err(e) => Err(CoreError::SessionStoreIo(format!(
                "session key unavailable: {e}"
            ))),
        }
    }
}

/// Decode the stored base64 key back into 32 bytes.
fn decode_session_key(
    encoded: &str,
) -> Result<Zeroizing<[u8; 32]>, oxibrowser_core::error::CoreError> {
    use base64::Engine as _;
    use oxibrowser_core::error::CoreError;

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .map_err(|e| CoreError::SessionStoreAead(format!("stored session key is corrupt: {e}")))?;
    bytes.try_into().map(Zeroizing::new).map_err(|v: Vec<u8>| {
        CoreError::SessionStoreAead(format!(
            "stored session key must be 32 bytes, got {}",
            v.len()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{CredentialKind, service_key};

    #[test]
    fn service_prefix_isolates_agents() {
        let p = KeyringProvider::with_service_prefix("test.oxi");
        assert_eq!(
            p.service_of("main", "cloudflare.com"),
            "test.oxi/main/cloudflare.com"
        );
        assert_eq!(p.item_prefix(), "test.oxi/");
    }

    #[test]
    fn default_provider_uses_design_prefix() {
        let p = KeyringProvider::new();
        assert_eq!(p.service_of("a", "b"), service_key("a", "b"));
    }

    /// Full roundtrip against the real login keychain. Ignored by default —
    /// run manually with `cargo test -p oxibrowser-credentials -- --ignored`
    /// (design FM-7: first access may prompt depending on signing identity).
    #[test]
    #[ignore = "touches the real OS keychain — run manually"]
    fn keychain_roundtrip() {
        let provider = KeyringProvider::new();
        let slug = format!("smoke-{}", std::process::id());
        let id = provider
            .put(NewCredential {
                agent_id: "main".into(),
                scope: "example.com".into(),
                kind: CredentialKind::Password,
                slug: slug.clone(),
                allowed_origins: vec!["https://example.com".into()],
                login_hint: Some("smoke@example.com".into()),
                password: Some(SecretBox::from_string("keychain-smoke-test".into())),
                otpauth_uri: None,
            })
            .expect("put into keychain");

        let (meta, secret) = provider.resolve(&id).expect("resolve");
        assert_eq!(meta.id, id);
        assert_eq!(secret.expose(), b"keychain-smoke-test");

        let meta = provider.metadata(&id).expect("metadata");
        assert_eq!(meta.slug, slug);

        let listed = provider.list(Some("main")).expect("list");
        assert!(listed.iter().any(|m| m.id == id));

        provider.delete(&id).expect("delete");
        assert!(matches!(provider.resolve(&id), Err(CredError::NotFound(_))));
    }

    /// Missing entries surface as `NotFound` without touching the keystore
    /// only when it is unavailable — otherwise they resolve to NotFound.
    #[test]
    #[ignore = "touches the real OS keychain — run manually"]
    fn keychain_missing_entry_is_not_found() {
        let provider = KeyringProvider::new();
        let id = CredentialId::format("main", "example.com", CredentialKind::Note, "no-such-slug")
            .unwrap();
        match provider.resolve(&id) {
            Err(CredError::NotFound(_)) => {}
            Err(CredError::KeyStoreUnavailable(_)) => {}
            other => panic!(
                "expected NotFound or KeyStoreUnavailable, got {:?}",
                other.err()
            ),
        }
    }

    #[test]
    fn session_key_service_layout_matches_design() {
        let p = KeyringKeyProvider::new();
        assert_eq!(
            p.service_of("github.com"),
            "com.oxibrowser.agent/_session-keys/github.com"
        );
        let p = KeyringKeyProvider::with_service_prefix("test.oxi");
        assert_eq!(
            p.service_of("github.com"),
            "test.oxi/_session-keys/github.com"
        );
    }

    /// Session-key service layout parses through the real keyring Entry
    /// constructor (no store access — construction only).
    #[test]
    fn session_key_entry_constructs() {
        ready().ok(); // store init is best-effort here; construction is the point
        let p = KeyringKeyProvider::new();
        let entry = keyring::v1::Entry::new(&p.service_of("example.com"), SESSION_KEY_ACCOUNT);
        match entry {
            Ok(_) => {}
            // Store-less hosts (headless CI Linux has no Secret Service)
            // fail inside Entry::new itself. Construction semantics are only
            // observable where a store exists; the real-store roundtrips are
            // the #[ignore]d tests below — so skip instead of panicking.
            Err(e) if is_store_absent(&e) => {
                eprintln!("skipping entry-construction assert: no platform store: {e}");
            }
            Err(e) => panic!("entry construction failed: {e:?}"),
        }
    }

    /// The skip predicate covers exactly the store-absence family — a
    /// per-credential miss (`NoEntry`) is not a missing store and must
    /// still fail the test.
    #[test]
    fn store_absence_errors_are_recognized() {
        let absent = |msg: &str| -> Box<dyn std::error::Error + Send + Sync> {
            Box::<dyn std::error::Error + Send + Sync>::from(msg)
        };
        assert!(is_store_absent(&keyring_core::Error::NoDefaultStore));
        assert!(is_store_absent(&keyring_core::Error::PlatformFailure(
            absent("dbus not running")
        )));
        assert!(is_store_absent(&keyring_core::Error::NoStorageAccess(
            absent("keychain locked")
        )));
        assert!(!is_store_absent(&keyring_core::Error::NoEntry));
    }

    /// Create-once + reuse roundtrip against the real login keychain.
    #[test]
    #[ignore = "touches the real OS keychain — run manually"]
    fn session_key_create_once_roundtrip() {
        use oxibrowser_core::storage::session_store::KeyProvider as _;

        let provider = KeyringKeyProvider::new();
        let scope = format!("session-key-smoke-{}", std::process::id());
        let first = provider.session_key(&scope).expect("create-once key");
        let again = provider.session_key(&scope).expect("reuse key");
        assert_eq!(&*first, &*again, "session keys must be create-once");

        // cleanup
        let service = provider.service_of(&scope);
        let entry = keyring::v1::Entry::new(&service, SESSION_KEY_ACCOUNT).unwrap();
        let _ = entry.delete_credential();
    }
}
