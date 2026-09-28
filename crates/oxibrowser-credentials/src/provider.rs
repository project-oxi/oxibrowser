//! Credential handles, metadata, records, and the [`CredentialProvider`] trait
//! (lower design §4.3 `provider.rs`, §5.1 key conventions, §5.2 record JSON).

use std::collections::HashMap;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::error::CredError;
use crate::secret::SecretBox;

/// Keychain service key prefix — the fixed part of the §5.1 convention.
pub const SERVICE_PREFIX: &str = "com.oxibrowser.agent";

/// Full service key for a credential item: `com.oxibrowser.agent/<agent-id>/<scope>`.
pub fn service_key(agent_id: &str, scope: &str) -> String {
    format!("{SERVICE_PREFIX}/{agent_id}/{scope}")
}

/// Keychain account key: `<kind>/<slug>`.
pub fn account_key(kind: CredentialKind, slug: &str) -> String {
    format!("{}/{}", kind.as_str(), slug)
}

/// Validate one handle component: non-empty, no `/` (the handle separator).
fn check_component(value: &str, what: &str) -> Result<(), CredError> {
    if value.is_empty() {
        return Err(CredError::Invalid(format!("{what} must not be empty")));
    }
    if value.contains('/') {
        return Err(CredError::Invalid(format!(
            "{what} must not contain '/': {value}"
        )));
    }
    Ok(())
}

/// Log-safe credential handle: `kch:<agent-id>/<scope>/<kind>/<slug>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CredentialId(pub String);

/// Parsed components of a [`CredentialId`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialParts {
    pub agent_id: String,
    pub scope: String,
    pub kind: CredentialKind,
    pub slug: String,
}

impl CredentialId {
    /// Build a handle from validated components.
    pub fn format(
        agent_id: &str,
        scope: &str,
        kind: CredentialKind,
        slug: &str,
    ) -> Result<Self, CredError> {
        check_component(agent_id, "agent_id")?;
        check_component(scope, "scope")?;
        check_component(slug, "slug")?;
        Ok(Self(format!(
            "kch:{agent_id}/{scope}/{}/{slug}",
            kind.as_str()
        )))
    }

    /// Split a handle back into its components.
    pub fn parse(&self) -> Result<CredentialParts, CredError> {
        let rest = self.0.strip_prefix("kch:").ok_or_else(|| {
            CredError::Invalid(format!("handle must start with 'kch:': {}", self.0))
        })?;
        let mut segs = rest.split('/');
        let agent_id = segs.next().unwrap_or_default().to_string();
        let scope = segs.next().unwrap_or_default().to_string();
        let kind = CredentialKind::from_str(
            segs.next()
                .ok_or_else(|| CredError::Invalid(format!("handle missing kind: {}", self.0)))?,
        )?;
        let slug = segs.next().unwrap_or_default().to_string();
        if segs.next().is_some() {
            return Err(CredError::Invalid(format!(
                "handle has too many segments: {}",
                self.0
            )));
        }
        check_component(&agent_id, "agent_id")?;
        check_component(&scope, "scope")?;
        check_component(&slug, "slug")?;
        Ok(CredentialParts {
            agent_id,
            scope,
            kind,
            slug,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CredentialId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Credential category (design §5.2). `Passkey` is a reserved variant: storage
/// and policy handling arrive with P2-M7, everything rejects it before then.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialKind {
    Password,
    Totp,
    ApiKey,
    Note,
    Passkey,
}

impl CredentialKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CredentialKind::Password => "password",
            CredentialKind::Totp => "totp",
            CredentialKind::ApiKey => "api-key",
            CredentialKind::Note => "note",
            CredentialKind::Passkey => "passkey",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Result<Self, CredError> {
        match s {
            "password" => Ok(Self::Password),
            "totp" => Ok(Self::Totp),
            "api-key" => Ok(Self::ApiKey),
            "note" => Ok(Self::Note),
            "passkey" => Ok(Self::Passkey),
            other => Err(CredError::Invalid(format!(
                "unknown credential kind: {other}"
            ))),
        }
    }
}

/// Value-free metadata — safe to send to any surface (CDP/REPL/CLI list).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialMeta {
    pub id: CredentialId,
    pub kind: CredentialKind,
    pub agent_id: String,
    /// Registrable domain, e.g. `cloudflare.com`.
    pub scope: String,
    /// Account/purpose discriminator within the scope.
    pub slug: String,
    /// Absolute origins (scheme+host+port) this credential may be used at
    /// (design §5.2). Exact matching only — no eTLD+1 expansion.
    pub allowed_origins: Vec<String>,
    /// Username hint — not a secret.
    pub login_hint: Option<String>,
    /// True when the record carries an `otpauth` URI.
    pub has_totp: bool,
    pub created_at: String,
    /// Set by future broker wiring on use; v1 writes never touch it.
    pub last_used_at: Option<String>,
}

/// The JSON stored inside the keychain item (`kSecValueData`, design §5.2).
///
/// Unlike [`CredentialMeta`] this type carries secret values, so `Debug` is
/// hand-written to redact them; `Serialize` exists because serializing *is*
/// how the record reaches the keystore — the record never crosses to logs,
/// CDP responses, or HAR output.
#[derive(Clone, Serialize, Deserialize)]
pub struct CredentialRecord {
    pub version: u32,
    #[serde(flatten)]
    pub meta: CredentialMeta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Password / API key / note value — the generic value slot.
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Google Key URI Format TOTP configuration.
    pub otpauth: Option<String>,
}

impl CredentialRecord {
    /// Project the value-free metadata.
    pub fn meta(&self) -> &CredentialMeta {
        &self.meta
    }

    /// Consume into the value-free metadata.
    pub fn into_meta(self) -> CredentialMeta {
        self.meta
    }
}

impl std::fmt::Debug for CredentialRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialRecord")
            .field("version", &self.version)
            .field("meta", &self.meta)
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .field("otpauth", &self.otpauth.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

/// A new credential awaiting storage.
///
/// No `Debug`/`Clone`: it holds live [`SecretBox`] values.
pub struct NewCredential {
    pub agent_id: String,
    pub scope: String,
    pub kind: CredentialKind,
    pub slug: String,
    pub allowed_origins: Vec<String>,
    pub login_hint: Option<String>,
    /// Password / API key / note value.
    pub password: Option<SecretBox>,
    /// `otpauth://` URI for TOTP-capable credentials.
    pub otpauth_uri: Option<SecretBox>,
}

/// Build and validate the stored record from a [`NewCredential`].
pub(crate) fn build_record(cred: NewCredential) -> Result<CredentialRecord, CredError> {
    let now = crate::provider::now_rfc3339();
    let id = CredentialId::format(&cred.agent_id, &cred.scope, cred.kind, &cred.slug)?;
    if cred.allowed_origins.is_empty() {
        return Err(CredError::Invalid(format!(
            "credential {id} needs at least one allowed origin"
        )));
    }
    // Normalize every allowlist origin at rest — policy matching later is
    // exact-string equality over these (design §5.2).
    let allowed_origins = cred
        .allowed_origins
        .iter()
        .map(|o| {
            oxibrowser_core::network::Origin::parse(o).map_err(|e| {
                CredError::Invalid(format!("credential {id} has invalid allowed origin: {e}"))
            })
        })
        .map(|r| r.map(|o| o.as_str()))
        .collect::<Result<Vec<_>, _>>()?;
    if cred.kind == CredentialKind::Passkey {
        return Err(CredError::Invalid(
            "passkey storage is reserved for P2-M7 (design §5.2)".to_string(),
        ));
    }
    let password = match &cred.password {
        Some(s) => Some(s.expose_str()?.to_string()),
        None => None,
    };
    let otpauth = match &cred.otpauth_uri {
        Some(s) => Some(s.expose_str()?.to_string()),
        None => None,
    };
    match cred.kind {
        CredentialKind::Totp if otpauth.is_none() => {
            return Err(CredError::Invalid(format!(
                "credential {id} of kind totp requires an otpauth URI"
            )));
        }
        CredentialKind::Password | CredentialKind::ApiKey | CredentialKind::Note
            if password.is_none() =>
        {
            return Err(CredError::Invalid(format!(
                "credential {id} of kind {} requires a value",
                cred.kind.as_str()
            )));
        }
        _ => {}
    }
    Ok(CredentialRecord {
        version: 1,
        meta: CredentialMeta {
            id,
            kind: cred.kind,
            agent_id: cred.agent_id,
            scope: cred.scope,
            slug: cred.slug,
            allowed_origins,
            login_hint: cred.login_hint,
            has_totp: otpauth.is_some(),
            created_at: now,
            last_used_at: None,
        },
        password,
        otpauth,
    })
}

/// Parse a stored record JSON blob.
pub(crate) fn parse_record(json: &str) -> Result<CredentialRecord, CredError> {
    serde_json::from_str(json)
        .map_err(|e| CredError::Invalid(format!("credential record is not valid JSON: {e}")))
}

/// The record's primary value: the `otpauth` URI for TOTP credentials, the
/// generic value slot for everything else. `Passkey` never has a value here.
pub(crate) fn record_value(record: &CredentialRecord) -> Result<&str, CredError> {
    match record.meta.kind {
        CredentialKind::Totp => record
            .otpauth
            .as_deref()
            .ok_or_else(|| CredError::NotFound(format!("{} has no otpauth value", record.meta.id))),
        CredentialKind::Passkey => Err(CredError::Invalid(format!(
            "{} is a passkey — values are handled by the WebAuthn layer (P2-M7)",
            record.meta.id
        ))),
        _ => record
            .password
            .as_deref()
            .ok_or_else(|| CredError::NotFound(format!("{} has no value", record.meta.id))),
    }
}

pub(crate) fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Credential storage backend (lower design §4.3). Implementations are
/// interior-mutable and shared via `Arc`.
///
/// `resolve` is the only value-returning method; policy evaluation
/// ([`crate::PolicyEngine`]) must happen before it, and it records a
/// `credential_read` audit event.
pub trait CredentialProvider: Send + Sync {
    fn put(&self, cred: NewCredential) -> Result<CredentialId, CredError>;
    fn resolve(&self, id: &CredentialId) -> Result<(CredentialMeta, SecretBox), CredError>;
    fn metadata(&self, id: &CredentialId) -> Result<CredentialMeta, CredError>;
    fn list(&self, agent_id: Option<&str>) -> Result<Vec<CredentialMeta>, CredError>;
    fn delete(&self, id: &CredentialId) -> Result<(), CredError>;
}

/// Heap-backed provider for tests and headless environments.
///
/// Values live in plain `String` record fields in memory — same trust domain
/// as the process, no persistence, no keystore requirement.
#[derive(Debug, Default)]
pub struct InMemoryProvider {
    items: Mutex<HashMap<CredentialId, CredentialRecord>>,
}

impl InMemoryProvider {
    pub fn new() -> Self {
        Self::default()
    }
}

impl CredentialProvider for InMemoryProvider {
    fn put(&self, cred: NewCredential) -> Result<CredentialId, CredError> {
        let record = build_record(cred)?;
        let id = record.meta.id.clone();
        self.items.lock().insert(id.clone(), record);
        Ok(id)
    }

    fn resolve(&self, id: &CredentialId) -> Result<(CredentialMeta, SecretBox), CredError> {
        let items = self.items.lock();
        let record = items
            .get(id)
            .ok_or_else(|| CredError::NotFound(id.0.clone()))?;
        let value = record_value(record)?.to_string();
        Ok((record.meta.clone(), SecretBox::from_string(value)))
    }

    fn metadata(&self, id: &CredentialId) -> Result<CredentialMeta, CredError> {
        let items = self.items.lock();
        items
            .get(id)
            .map(|r| r.meta.clone())
            .ok_or_else(|| CredError::NotFound(id.0.clone()))
    }

    fn list(&self, agent_id: Option<&str>) -> Result<Vec<CredentialMeta>, CredError> {
        let items = self.items.lock();
        let mut metas: Vec<CredentialMeta> = items
            .values()
            .filter(|r| agent_id.is_none_or(|a| r.meta.agent_id == a))
            .map(|r| r.meta.clone())
            .collect();
        metas.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        Ok(metas)
    }

    fn delete(&self, id: &CredentialId) -> Result<(), CredError> {
        self.items
            .lock()
            .remove(id)
            .map(|_| ())
            .ok_or_else(|| CredError::NotFound(id.0.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_password() -> NewCredential {
        NewCredential {
            agent_id: "main".into(),
            scope: "cloudflare.com".into(),
            kind: CredentialKind::Password,
            slug: "dashboard".into(),
            allowed_origins: vec!["https://DASH.cloudflare.com".into()],
            login_hint: Some("user@example.com".into()),
            password: Some(SecretBox::from_string("hunter2".into())),
            otpauth_uri: None,
        }
    }

    #[test]
    fn key_conventions_match_design_5_1() {
        assert_eq!(
            service_key("main", "cloudflare.com"),
            "com.oxibrowser.agent/main/cloudflare.com"
        );
        assert_eq!(
            account_key(CredentialKind::Password, "dashboard"),
            "password/dashboard"
        );
        assert_eq!(account_key(CredentialKind::ApiKey, "ci"), "api-key/ci");
        assert_eq!(SERVICE_PREFIX, "com.oxibrowser.agent");
    }

    #[test]
    fn handle_format_parse_roundtrip() {
        let id =
            CredentialId::format("main", "cloudflare.com", CredentialKind::Totp, "work").unwrap();
        assert_eq!(id.as_str(), "kch:main/cloudflare.com/totp/work");
        let parts = id.parse().unwrap();
        assert_eq!(
            parts,
            CredentialParts {
                agent_id: "main".into(),
                scope: "cloudflare.com".into(),
                kind: CredentialKind::Totp,
                slug: "work".into(),
            }
        );
    }

    #[test]
    fn handle_rejects_bad_components() {
        assert!(CredentialId::format("", "a.com", CredentialKind::Note, "s").is_err());
        assert!(CredentialId::format("a/b", "a.com", CredentialKind::Note, "s").is_err());
        let bad = CredentialId("nope:main/a.com/password/s".into());
        assert!(matches!(bad.parse(), Err(CredError::Invalid(_))));
        let too_many = CredentialId("kch:main/a.com/password/s/extra".into());
        assert!(matches!(too_many.parse(), Err(CredError::Invalid(_))));
        let bad_kind = CredentialId("kch:main/a.com/secret/s".into());
        assert!(matches!(bad_kind.parse(), Err(CredError::Invalid(_))));
    }

    #[test]
    fn in_memory_roundtrip() {
        let p = InMemoryProvider::new();
        let id = p.put(new_password()).unwrap();
        let (meta, secret) = p.resolve(&id).unwrap();
        assert_eq!(meta.id, id);
        assert_eq!(meta.kind, CredentialKind::Password);
        assert_eq!(meta.scope, "cloudflare.com");
        assert_eq!(meta.allowed_origins, vec!["https://dash.cloudflare.com"]);
        assert!(!meta.has_totp);
        assert_eq!(secret.expose(), b"hunter2");

        let meta2 = p.metadata(&id).unwrap();
        assert_eq!(meta2.login_hint.as_deref(), Some("user@example.com"));

        assert_eq!(p.list(None).unwrap().len(), 1);
        assert_eq!(p.list(Some("main")).unwrap().len(), 1);
        assert!(p.list(Some("other")).unwrap().is_empty());

        p.delete(&id).unwrap();
        assert!(matches!(p.resolve(&id), Err(CredError::NotFound(_))));
        assert!(matches!(p.delete(&id), Err(CredError::NotFound(_))));
    }

    #[test]
    fn put_validates_kind_value_and_origins() {
        let invalid_password = |mutate: fn(&mut NewCredential)| {
            let mut cred = new_password();
            mutate(&mut cred);
            InMemoryProvider::new().put(cred)
        };
        assert!(matches!(
            invalid_password(|c| c.password = None),
            Err(CredError::Invalid(_))
        ));
        assert!(matches!(
            invalid_password(|c| c.allowed_origins = vec!["not an origin".into()]),
            Err(CredError::Invalid(_))
        ));
        assert!(matches!(
            invalid_password(|c| c.allowed_origins.clear()),
            Err(CredError::Invalid(_))
        ));
        assert!(matches!(
            invalid_password(|c| c.kind = CredentialKind::Passkey),
            Err(CredError::Invalid(_))
        ));
    }

    #[test]
    fn totp_kind_requires_otpauth_and_sets_has_totp() {
        let missing_uri = |mutate: fn(&mut NewCredential)| {
            let mut cred = new_password();
            mutate(&mut cred);
            InMemoryProvider::new().put(cred)
        };
        assert!(matches!(
            missing_uri(|c| {
                c.kind = CredentialKind::Totp;
                c.password = None;
            }),
            Err(CredError::Invalid(_))
        ));

        let with_uri = |mutate: fn(&mut NewCredential)| {
            let mut cred = new_password();
            mutate(&mut cred);
            cred.otpauth_uri = Some(SecretBox::from_string(
                "otpauth://totp/Cloudflare:user@example.com?secret=JBSWY3DPEHPK3PXP&issuer=Cloudflare"
                    .into(),
            ));
            cred
        };
        let p = InMemoryProvider::new();
        let id = p
            .put(with_uri(|c| {
                c.kind = CredentialKind::Totp;
                c.password = None;
            }))
            .unwrap();
        assert!(id.as_str().ends_with("/totp/dashboard"));
        let (meta, secret) = p.resolve(&id).unwrap();
        assert!(meta.has_totp);
        assert!(secret.expose_str().unwrap().starts_with("otpauth://"));
    }

    #[test]
    fn record_debug_redacts_values() {
        let record = build_record(new_password()).unwrap();
        let dbg = format!("{record:?}");
        assert!(!dbg.contains("hunter2"), "{dbg}");
        assert!(dbg.contains("[REDACTED]"));
        // Serialized record keeps the value — that is its purpose (keystore).
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains("hunter2"));
        // …and parses back into an equal record.
        let back = parse_record(&json).unwrap();
        assert_eq!(back.meta.id, record.meta.id);
        assert_eq!(back.password.as_deref(), Some("hunter2"));
    }

    #[test]
    fn record_json_matches_design_5_2_shape() {
        let record = build_record(new_password()).unwrap();
        let v: serde_json::Value = serde_json::to_value(&record).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(v["id"], "kch:main/cloudflare.com/password/dashboard");
        assert_eq!(v["kind"], "password");
        assert_eq!(v["agent_id"], "main");
        assert_eq!(v["scope"], "cloudflare.com");
        assert_eq!(v["slug"], "dashboard");
        assert_eq!(v["allowed_origins"][0], "https://dash.cloudflare.com");
        assert_eq!(v["login_hint"], "user@example.com");
        assert_eq!(v["has_totp"], false);
        assert_eq!(v["password"], "hunter2");
        assert!(v.get("otpauth").is_none());
        assert!(v["created_at"].is_string());
    }
}
