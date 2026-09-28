//! Credential broker surface for the CDP OXI domain (M4 — design
//! `2026-09-27` §6.3, `2026-09-28` §7.3).
//!
//! Bundles a [`CredentialProvider`] with a [`PolicyEngine`] and owns the
//! pending-confirmation table backing `OXI.fillCredential` ↔
//! `OXI.resolveConfirmation`. Secret values never cross this module's API:
//! fills resolve inside the session-injection path, and only handles,
//! fingerprints, and masked acks reach the CDP boundary.
//!
//! Confirmation lifecycle (design §6.3 — timeout/missing = denied, implicit
//! approval forbidden):
//!
//! ```text
//! fillCredential ──RequireConfirmation──▶ register_pending
//!         │                              + OXI.confirmationRequired event
//!         │                                     │
//! resolveConfirmation {approved} ◀─────────────┘
//!         ├─ approved  → verify_confirmation → fill → {resolved, approved}
//!         ├─ rejected  → audit Deny          → {resolved, denied}
//!         └─ expired/unknown (TTL purge)     → denied / invalid request
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use oxibrowser_core::security::audit::{AuditDecision, AuditEventKind, CredentialRef};
use oxibrowser_credentials::policy::CONFIRMATION_TTL;
use oxibrowser_credentials::{
    ConfirmationToken, CredError, CredentialAction, CredentialId, CredentialMeta,
    CredentialProvider, PolicyEngine, UseRequest,
};
use serde_json::{Value, json};

use crate::protocol::CdpError;

// ── Field kind (design §6.3 `fieldKind`) ────────────────────────────────────

/// Which form slot a credential fills.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Password,
    Totp,
    ApiKey,
}

impl FieldKind {
    /// Parse the wire form. Unknown kinds reject as an invalid parameter.
    pub fn parse(s: &str) -> Result<Self, CdpError> {
        match s {
            "password" => Ok(Self::Password),
            "totp" => Ok(Self::Totp),
            "apiKey" => Ok(Self::ApiKey),
            other => Err(CdpError {
                code: -32602,
                message: format!(
                    "invalid fieldKind '{other}' — expected \"password\", \"totp\" or \"apiKey\""
                ),
            }),
        }
    }

    /// Wire form of the kind.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::Totp => "totp",
            Self::ApiKey => "apiKey",
        }
    }

    /// Policy action class filled by this kind (design §4.3): a password slot
    /// is a login, a TOTP slot is MFA, an API-key slot is `fill-api-key`.
    pub fn action(self) -> CredentialAction {
        match self {
            Self::Password => CredentialAction::Login,
            Self::Totp => CredentialAction::Mfa,
            Self::ApiKey => CredentialAction::FillApiKey,
        }
    }
}

// ── Pending confirmations ───────────────────────────────────────────────────

/// A `fillCredential` awaiting its confirmation card resolution.
///
/// Carries everything needed to finish the fill after approval: the exact
/// [`UseRequest`] the confirmation token was minted from (hash binding — any
/// change voids the approval), the ref's CSS selector, and the originating
/// session key.
#[derive(Debug, Clone)]
pub struct PendingConfirmation {
    pub request_id: String,
    pub request: UseRequest,
    pub token: ConfirmationToken,
    /// `Session::id` string the fill was requested against.
    pub session_key: String,
    /// CSS selector of the ref'd element at observation time.
    pub selector: String,
    pub credential: CredentialId,
    pub field_kind: FieldKind,
    /// Broker-side hard deadline. Enforced independently of the engine
    /// token's own expiry — the shorter of the two wins, so an expired card
    /// can never approve.
    expires_at: Instant,
}

impl PendingConfirmation {
    /// Whether this pending is past its broker-side deadline.
    pub fn expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }
}

// ── Broker ──────────────────────────────────────────────────────────────────

/// Provider + policy engine + pending-confirmation table, shared by every
/// credential-aware OXI handler via [`crate::domains::DispatchContext`].
pub struct CredentialBroker {
    /// Credential storage backend (values only ever leave via `resolve`).
    pub provider: Arc<dyn CredentialProvider>,
    /// Deny-first use policy engine (audits every decision itself).
    pub engine: Arc<PolicyEngine>,
    /// Pending confirmations keyed by request id. Server-lifetime; expired
    /// entries are purged lazily on every access.
    pending: Mutex<HashMap<String, PendingConfirmation>>,
    /// Confirmation card TTL (broker side).
    confirmation_ttl: Duration,
    /// Monotonic request-id counter (`conf-<n>`).
    counter: AtomicU64,
}

impl CredentialBroker {
    /// Build a broker with the design-default confirmation TTL
    /// ([`CONFIRMATION_TTL`], 60 s).
    pub fn new(provider: Arc<dyn CredentialProvider>, engine: Arc<PolicyEngine>) -> Self {
        Self {
            provider,
            engine,
            pending: Mutex::new(HashMap::new()),
            confirmation_ttl: CONFIRMATION_TTL.to_std().unwrap_or(Duration::from_secs(60)),
            counter: AtomicU64::new(1),
        }
    }

    /// Override the confirmation card TTL (tests, tightened deployments).
    pub fn with_confirmation_ttl(mut self, ttl: Duration) -> Self {
        self.confirmation_ttl = ttl;
        self
    }

    /// The confirmation TTL advertised on `OXI.confirmationRequired`.
    pub fn confirmation_ttl(&self) -> Duration {
        self.confirmation_ttl
    }

    /// Register a pending confirmation; returns the fresh `requestId`.
    pub fn register_pending(
        &self,
        request: UseRequest,
        token: ConfirmationToken,
        session_key: String,
        selector: String,
        credential: CredentialId,
        field_kind: FieldKind,
    ) -> String {
        let request_id = format!("conf-{}", self.counter.fetch_add(1, Ordering::SeqCst));
        let entry = PendingConfirmation {
            request_id: request_id.clone(),
            expires_at: Instant::now() + self.confirmation_ttl,
            request,
            token,
            session_key,
            selector,
            credential,
            field_kind,
        };
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(request_id.clone(), entry);
        request_id
    }

    /// Clone-and-drop: look the pending up and remove it. Expired pendings
    /// count as missing (timeout = denial, never implicit approval).
    pub fn take(&self, request_id: &str) -> Option<PendingConfirmation> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending
            .remove(request_id)
            .filter(|p| !p.expired())
            .or_else(|| {
                // Drop any other expired entries while the lock is held.
                pending.retain(|_, p| !p.expired());
                None
            })
    }

    /// Remove a pending outright (used after a resolution is recorded).
    pub fn remove(&self, request_id: &str) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(request_id);
    }

    /// Number of live (non-expired) pendings — the sweeper is lazy, so this
    /// purges first. Visible for tests.
    pub fn live_pending_count(&self) -> usize {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.retain(|_, p| !p.expired());
        pending.len()
    }
}

// ── CDP error mapping (design §6.3 error codes) ─────────────────────────────

/// `credentialsUnavailable` — no provider/engine wired into the server, or
/// the keystore itself is unusable.
pub fn credentials_unavailable(detail: impl std::fmt::Display) -> CdpError {
    CdpError {
        code: -32000,
        message: format!("credentialsUnavailable: {detail}"),
    }
}

/// `credentialNotFound` — unknown handle, or the credential lacks the value
/// slot the requested `fieldKind` needs.
pub fn credential_not_found(detail: impl std::fmt::Display) -> CdpError {
    CdpError {
        code: -32000,
        message: format!("credentialNotFound: {detail}"),
    }
}

/// `originMismatch` — the page origin is not in the credential's exact-origin
/// allowlist (or no usable page origin exists).
pub fn origin_mismatch(detail: impl std::fmt::Display) -> CdpError {
    CdpError {
        code: -32000,
        message: format!("originMismatch: {detail}"),
    }
}

/// `consentRequired` — policy denied the use, or a confirmation card is
/// required and outstanding.
pub fn consent_required(detail: impl std::fmt::Display) -> CdpError {
    CdpError {
        code: -32000,
        message: format!("consentRequired: {detail}"),
    }
}

/// Map a broker [`CredError`] onto the §6.3 CDP error codes.
pub fn cred_error_to_cdp(e: CredError) -> CdpError {
    match e {
        CredError::NotFound(_) => credential_not_found(e),
        CredError::ConsentRequired { .. } => consent_required(e),
        CredError::OriginMismatch { .. } => origin_mismatch(e),
        // No plaintext fallback ever — an unusable keystore is unavailable
        // surface, not an internal error (design §9 FM-7).
        CredError::KeyStoreUnavailable(_) | CredError::Keyring(_) | CredError::Io(_) => {
            credentials_unavailable(e)
        }
        CredError::Invalid(_) | CredError::Totp(_) | CredError::FingerprintMismatch { .. } => {
            CdpError {
                code: -32603,
                message: format!("credentialError: {e}"),
            }
        }
    }
}

// ── Audit helpers (public core audit API — no audit.rs edits) ───────────────

/// Record the explicit user rejection of a confirmation card. The engine's
/// `verify_confirmation` path covers approvals, timeouts, and policy denials;
/// a flat "no" from the user never reaches it, so it is audited here.
pub fn audit_rejection(engine: &PolicyEngine, request: &UseRequest, reason: &str) {
    let mut event = oxibrowser_core::security::audit::event(
        AuditEventKind::CredentialUse,
        AuditDecision::Deny,
        reason,
    );
    event.origin = Some(request.top_level.as_str());
    event.action = Some(request.action.as_str().to_string());
    event.credential = Some(CredentialRef {
        id: request.credential.0.clone(),
        fingerprint: String::new(),
    });
    if let Err(e) = engine.audit.record(event) {
        tracing::warn!(error = %e, "confirmation rejection audit write failed");
    }
}

/// Correlate a completed injection with its `credential_read` line via the
/// value fingerprint (design §5.5 pairing).
pub fn audit_fill(engine: &PolicyEngine, request: &UseRequest, fingerprint: &str) {
    let mut event = oxibrowser_core::security::audit::event(
        AuditEventKind::CredentialUse,
        AuditDecision::Allow,
        "credential value injected into form",
    );
    event.origin = Some(request.top_level.as_str());
    event.action = Some(request.action.as_str().to_string());
    event.credential = Some(CredentialRef {
        id: request.credential.0.clone(),
        fingerprint: fingerprint.to_string(),
    });
    if let Err(e) = engine.audit.record(event) {
        tracing::warn!(error = %e, "credential fill audit write failed");
    }
}

// ── Payload assembly ────────────────────────────────────────────────────────

/// `OXI.credentialList` payload: metadata only — [`CredentialMeta`] is the
/// value-free projection, so no secret can appear by construction.
pub fn credential_list_payload(
    broker: &CredentialBroker,
    agent: Option<&str>,
) -> Result<Value, CdpError> {
    let metas: Vec<CredentialMeta> = broker.provider.list(agent).map_err(cred_error_to_cdp)?;
    let credentials = serde_json::to_value(&metas).map_err(|e| CdpError {
        code: -32603,
        message: format!("credentialList: {e}"),
    })?;
    Ok(json!({ "credentials": credentials }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxibrowser_credentials::{CredentialKind, InMemoryProvider, NewCredential};
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        // Mirrors the credentials crate's unique temp-dir helper for tests.
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "oxi-cdp-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn test_engine(dir: PathBuf) -> Arc<PolicyEngine> {
        let audit = Arc::new(
            oxibrowser_core::security::audit::AuditLog::open(dir.join("audit.jsonl")).unwrap(),
        );
        Arc::new(PolicyEngine::new(
            Vec::new(),
            oxibrowser_credentials::ConsentStore::open(dir.join("consents.jsonl")).unwrap(),
            audit,
        ))
    }

    fn test_broker() -> Arc<CredentialBroker> {
        Arc::new(CredentialBroker::new(
            Arc::new(InMemoryProvider::new()),
            test_engine(temp_dir("broker")),
        ))
    }

    fn put_password(provider: &InMemoryProvider, origins: &[&str]) -> CredentialId {
        provider
            .put(NewCredential {
                agent_id: "main".into(),
                scope: "cloudflare.com".into(),
                kind: CredentialKind::Password,
                slug: "dashboard".into(),
                allowed_origins: origins.iter().map(|s| s.to_string()).collect(),
                login_hint: Some("user@example.com".into()),
                password: Some(oxibrowser_credentials::SecretBox::from_string(
                    "hunter2".into(),
                )),
                otpauth_uri: None,
            })
            .unwrap()
    }

    // -- fieldKind --

    #[test]
    fn field_kind_parses_wire_forms_and_maps_actions() {
        assert_eq!(FieldKind::parse("password").unwrap(), FieldKind::Password);
        assert_eq!(FieldKind::parse("totp").unwrap(), FieldKind::Totp);
        assert_eq!(FieldKind::parse("apiKey").unwrap(), FieldKind::ApiKey);
        assert!(FieldKind::parse("api-key").is_err(), "wire form is apiKey");
        assert!(FieldKind::parse("").is_err());

        assert_eq!(FieldKind::Password.action().as_str(), "login");
        assert_eq!(FieldKind::Totp.action().as_str(), "mfa");
        assert_eq!(FieldKind::ApiKey.action().as_str(), "fill-api-key");
        assert_eq!(FieldKind::ApiKey.as_str(), "apiKey");
    }

    // -- error tokens --

    #[test]
    fn cred_errors_map_to_design_error_codes() {
        let not_found = cred_error_to_cdp(CredError::NotFound("kch:main/a.com/password/x".into()));
        assert_eq!(not_found.code, -32000);
        assert!(not_found.message.starts_with("credentialNotFound"));

        let consent = cred_error_to_cdp(CredError::ConsentRequired {
            credential: "kch:main/a.com/password/x".into(),
            origin: "https://a.com".into(),
            action: "login".into(),
        });
        assert!(consent.message.starts_with("consentRequired"));

        let origin = cred_error_to_cdp(CredError::OriginMismatch {
            credential: "kch:main/a.com/password/x".into(),
            origin: "https://a.com".into(),
        });
        assert!(origin.message.starts_with("originMismatch"));

        let store = cred_error_to_cdp(CredError::KeyStoreUnavailable("no keychain".into()));
        assert!(store.message.starts_with("credentialsUnavailable"));

        let bad = cred_error_to_cdp(CredError::Invalid("malformed".into()));
        assert_eq!(bad.code, -32603);
        assert!(bad.message.starts_with("credentialError"));
    }

    // -- pending lifecycle --

    #[test]
    fn pending_roundtrip_and_take_removes() {
        let broker = test_broker();
        let id = CredentialId("kch:main/cloudflare.com/password/dashboard".into());
        let origin =
            oxibrowser_core::network::Origin::parse("https://dash.cloudflare.com").unwrap();
        let request = UseRequest::new(id.clone(), origin.clone(), CredentialAction::Login);
        let token = broker.engine.issue_confirmation(&request);

        let request_id = broker.register_pending(
            request,
            token,
            "session-1".into(),
            "#pw".into(),
            id.clone(),
            FieldKind::Password,
        );
        assert_eq!(broker.live_pending_count(), 1);

        let taken = broker.take(&request_id).expect("live pending");
        assert_eq!(taken.session_key, "session-1");
        assert_eq!(taken.selector, "#pw");
        assert_eq!(taken.field_kind, FieldKind::Password);
        assert_eq!(taken.credential, id);
        assert!(broker.take(&request_id).is_none(), "take removes");
        assert_eq!(broker.live_pending_count(), 0);
    }

    #[test]
    fn expired_pending_is_denied_and_purged() {
        let broker = CredentialBroker::new(
            Arc::new(InMemoryProvider::new()),
            test_engine(temp_dir("broker-ttl")),
        )
        .with_confirmation_ttl(std::time::Duration::from_millis(30));
        let id = CredentialId("kch:main/cloudflare.com/password/dashboard".into());
        let origin =
            oxibrowser_core::network::Origin::parse("https://dash.cloudflare.com").unwrap();
        let request = UseRequest::new(id.clone(), origin, CredentialAction::Login);
        let token = broker.engine.issue_confirmation(&request);
        let request_id = broker.register_pending(
            request,
            token,
            "session-1".into(),
            "#pw".into(),
            id,
            FieldKind::Totp,
        );

        std::thread::sleep(std::time::Duration::from_millis(50));
        // Timeout = denial: the pending resolves to "missing".
        assert!(broker.take(&request_id).is_none());
        assert_eq!(broker.live_pending_count(), 0, "expired entries purged");
    }

    // -- credentialList payload --

    #[test]
    fn credential_list_payload_is_metadata_only() {
        let provider = InMemoryProvider::new();
        let _id = put_password(&provider, &["https://dash.cloudflare.com"]);
        let broker = CredentialBroker::new(Arc::new(provider), test_engine(temp_dir("list")));

        let payload = credential_list_payload(&broker, None).unwrap();
        let creds = payload["credentials"].as_array().unwrap();
        assert_eq!(creds.len(), 1);
        let entry = &creds[0];
        assert_eq!(entry["id"], "kch:main/cloudflare.com/password/dashboard");
        assert_eq!(entry["kind"], "password");
        assert_eq!(entry["scope"], "cloudflare.com");
        assert_eq!(entry["slug"], "dashboard");
        assert_eq!(entry["agent_id"], "main");
        assert_eq!(entry["has_totp"], false);
        assert_eq!(
            entry["allowed_origins"],
            json!(["https://dash.cloudflare.com"])
        );
        assert!(
            !payload.to_string().contains("hunter2"),
            "values must never appear in the list payload"
        );
        assert!(
            entry.get("password").is_none() && entry.get("otpauth").is_none(),
            "metadata carries no value slots"
        );

        // Agent filter: a different agent id sees nothing.
        let other = credential_list_payload(&broker, Some("other-agent")).unwrap();
        assert_eq!(other["credentials"].as_array().unwrap().len(), 0);
    }

    // -- audit helpers --

    #[test]
    fn rejection_and_fill_audit_lines_are_recorded() {
        let dir = temp_dir("audit");
        let audit_path = dir.join("audit.jsonl");
        let engine = test_engine(dir.clone());
        let id = CredentialId("kch:main/cloudflare.com/password/dashboard".into());
        let origin =
            oxibrowser_core::network::Origin::parse("https://dash.cloudflare.com").unwrap();
        let request = UseRequest::new(id, origin, CredentialAction::Login);

        audit_rejection(&engine, &request, "confirmation rejected by user");
        audit_fill(&engine, &request, "sha256:f52fbd32");

        let lines = std::fs::read_to_string(&audit_path).unwrap();
        let mut found_reject = false;
        let mut found_fill = false;
        for line in lines.lines() {
            let v: Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["kind"], "credential_use");
            if v["decision"] == "deny" && v["reason"] == "confirmation rejected by user" {
                found_reject = true;
                assert_eq!(v["origin"], "https://dash.cloudflare.com");
                assert_eq!(v["action"], "login");
            }
            if v["decision"] == "allow" && v["reason"] == "credential value injected into form" {
                found_fill = true;
                assert_eq!(v["credential"]["fingerprint"], "sha256:f52fbd32");
            }
        }
        assert!(found_reject, "rejection audited as credential_use deny");
        assert!(found_fill, "fill audited as credential_use allow");
    }
}
