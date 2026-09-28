//! Use policy — deny rules → consent → confirmation, with a full audit trail
//! (lower design §4.3 `policy.rs`, §5.4/§5.5; upper design §1 gate order).
//!
//! The engine decides *whether* a credential may be used. The exact-origin
//! allowlist check against the record (`OriginPolicy::evaluate`, core M1)
//! happens in the injection layer before/around resolution — the two gates
//! compose deny-biased: both must pass.

use std::sync::Arc;

use chrono::Utc;
use oxibrowser_core::network::origin_policy::{Decision, Origin, OriginRule, RuleMode};
use oxibrowser_core::security::audit::{
    AuditDecision, AuditEventKind, AuditLog, CredentialRef, event as audit_event,
};
use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::consent::{ConsentStore, Timestamp};
use crate::error::CredError;
use crate::provider::{CredentialId, CredentialKind};

/// How long a confirmation approval stays valid. Silence at the card is a
/// denial — timeout produces `AuditDecision::Timeout` and a `Deny`.
pub const CONFIRMATION_TTL: chrono::Duration = chrono::Duration::seconds(60);

/// What the agent wants to do with the credential (design §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialAction {
    Login,
    Mfa,
    FillApiKey,
}

impl CredentialAction {
    pub fn as_str(self) -> &'static str {
        match self {
            CredentialAction::Login => "login",
            CredentialAction::Mfa => "mfa",
            CredentialAction::FillApiKey => "fill-api-key",
        }
    }
}

impl std::fmt::Display for CredentialAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One credential-use request.
///
/// `value_fingerprint` is the log-safe fingerprint (`"sha256:ab12cd34"`) of
/// the value when the caller already has it loaded — it correlates the
/// `credential_use` audit line with its `credential_read` pair (§5.5) and
/// binds confirmations to the exact value (a swapped secret voids approval).
/// Pre-resolution authorizations leave it `None`.
#[derive(Debug, Clone)]
pub struct UseRequest {
    pub credential: CredentialId,
    /// The page the agent believes it is on.
    pub top_level: Origin,
    /// The origin of the frame actually hosting the form. `None` is
    /// fail-closed downstream (core M1) and never matches deny rules' frame
    /// arm here.
    pub frame: Option<Origin>,
    pub action: CredentialAction,
    pub value_fingerprint: Option<String>,
}

impl UseRequest {
    pub fn new(credential: CredentialId, top_level: Origin, action: CredentialAction) -> Self {
        Self {
            credential,
            top_level,
            frame: None,
            action,
            value_fingerprint: None,
        }
    }
}

/// Approval for one `UseRequest`, minted when the confirmation card is
/// accepted. Verifying re-computes the request hash — any change to the
/// request (origin, action, value) invalidates the approval (design §4.3
/// `verify_confirmation`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfirmationToken {
    pub(crate) request_hash: [u8; 32],
    pub issued_at: Timestamp,
    pub expires_at: Timestamp,
}

impl ConfirmationToken {
    /// Hex of the bound request hash — audit/debug safe (only non-secret
    /// request fields feed the hash).
    pub fn request_hash_hex(&self) -> String {
        self.request_hash
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    pub fn expired_at(&self, now: Timestamp) -> bool {
        now > self.expires_at
    }
}

/// Deny-first use policy engine.
///
/// Evaluation order on [`PolicyEngine::authorize_use`]:
/// 1. static deny rules (top-level or frame origin),
/// 2. passkey guard — passkey policy is undefined until P2-M7 (FM-2),
/// 3. consent store (expiry and use-count filtered),
/// 4. `RequireConfirmation`.
///
/// Every verdict is recorded on the audit log as a `credential_use` event
/// carrying the handle and — when known — the value fingerprint only.
pub struct PolicyEngine {
    pub deny_rules: Vec<OriginRule>,
    pub consents: ConsentStore,
    pub audit: Arc<AuditLog>,
}

impl PolicyEngine {
    pub fn new(deny_rules: Vec<OriginRule>, consents: ConsentStore, audit: Arc<AuditLog>) -> Self {
        Self {
            deny_rules,
            consents,
            audit,
        }
    }

    pub fn authorize_use(&self, req: &UseRequest) -> Decision {
        self.authorize_use_at(req, Utc::now())
    }

    /// Non-consuming pre-check for unattended flows that must refuse a use
    /// *before* starting (M-D `OXI.loginWithAccount`): deny rules lose, and
    /// an active grant must exist. Unlike [`Self::authorize_use`] this never
    /// consumes a use nor prompts — the authoritative, consuming decision
    /// happens at resolution time.
    pub fn preflight(&self, req: &UseRequest) -> bool {
        if self.deny_hit(req).is_some() {
            return false;
        }
        self.consents
            .active_for(
                &req.credential,
                &req.top_level.as_str(),
                req.action.as_str(),
                Utc::now(),
            )
            .is_some()
    }

    /// Deterministic variant — inject the clock for tests and replay.
    pub fn authorize_use_at(&self, req: &UseRequest, now: Timestamp) -> Decision {
        if let Some(reason) = self.deny_hit(req) {
            self.log(req, AuditDecision::Deny, reason.clone());
            return Decision::Deny { reason };
        }
        // Passkey handling is reserved for P2-M7; until then deny (FM-2).
        if matches!(
            req.credential.parse().map(|p| p.kind),
            Ok(CredentialKind::Passkey)
        ) {
            let reason = "passkey policy is undefined until WebAuthn support (P2-M7)".to_string();
            self.log(req, AuditDecision::Deny, reason.clone());
            return Decision::Deny { reason };
        }
        match self.consents.active_for(
            &req.credential,
            &req.top_level.as_str(),
            req.action.as_str(),
            now,
        ) {
            Some(rec) => {
                // Consumption is a write; if accounting fails, deny —
                // a grant whose use cannot be counted cannot be enforced.
                match self.consents.consume(&rec.consent_id) {
                    Ok(()) => {
                        let reason = format!("consent:{}", rec.consent_id);
                        self.log(req, AuditDecision::Allow, reason.clone());
                        Decision::Allow
                    }
                    Err(e) => {
                        let reason = format!("consent accounting failed: {e}");
                        self.log(req, AuditDecision::Deny, reason.clone());
                        Decision::Deny { reason }
                    }
                }
            }
            None => {
                let reason = format!(
                    "no active consent for {} at {} ({})",
                    req.credential, req.top_level, req.action
                );
                self.log(req, AuditDecision::Prompt, reason.clone());
                Decision::RequireConfirmation { reason }
            }
        }
    }

    /// Mint an approval token after the confirmation card is accepted.
    /// Binds the exact request; expires after [`CONFIRMATION_TTL`].
    pub fn issue_confirmation(&self, req: &UseRequest) -> ConfirmationToken {
        let now = Utc::now();
        ConfirmationToken {
            request_hash: request_hash(req),
            issued_at: now,
            expires_at: now + CONFIRMATION_TTL,
        }
    }

    pub fn verify_confirmation(&self, token: &ConfirmationToken, req: &UseRequest) -> Decision {
        self.verify_confirmation_at(token, req, Utc::now())
    }

    /// Re-validate immediately before execution. Hash mismatch or expiry
    /// voids the approval; deny rules added after issuance still win.
    pub fn verify_confirmation_at(
        &self,
        token: &ConfirmationToken,
        req: &UseRequest,
        now: Timestamp,
    ) -> Decision {
        if request_hash(req) != token.request_hash {
            let reason = "request changed since confirmation — approval void".to_string();
            self.log(req, AuditDecision::Deny, reason.clone());
            return Decision::Deny { reason };
        }
        if token.expired_at(now) {
            let reason = "confirmation expired — timeout denies".to_string();
            self.log(req, AuditDecision::Timeout, reason.clone());
            return Decision::Deny { reason };
        }
        if let Some(reason) = self.deny_hit(req) {
            self.log(req, AuditDecision::Deny, reason.clone());
            return Decision::Deny { reason };
        }
        self.log(req, AuditDecision::Allow, "confirmed".to_string());
        Decision::Allow
    }

    /// First matching deny rule over the top-level or frame origin.
    fn deny_hit(&self, req: &UseRequest) -> Option<String> {
        self.deny_rules
            .iter()
            .filter(|r| r.mode == RuleMode::Deny)
            .find(|r| {
                r.origin.exact_eq(&req.top_level)
                    || req.frame.as_ref().is_some_and(|f| r.origin.exact_eq(f))
            })
            .map(|r| format!("deny rule for {}", r.origin))
    }

    fn log(&self, req: &UseRequest, decision: AuditDecision, reason: String) {
        let mut event = audit_event(AuditEventKind::CredentialUse, decision, reason);
        event.origin = Some(req.top_level.as_str());
        event.action = Some(req.action.as_str().to_string());
        event.credential = Some(CredentialRef {
            id: req.credential.0.clone(),
            fingerprint: req.value_fingerprint.clone().unwrap_or_default(),
        });
        if let Err(e) = self.audit.record(event) {
            tracing::warn!(error = %e, "credential_use audit write failed");
        }
    }
}

/// Domain-separated SHA-256 over the canonical request JSON.
fn request_hash(req: &UseRequest) -> [u8; 32] {
    let canonical = serde_json::json!({
        "action": req.action.as_str(),
        "credential": req.credential.0,
        "frame": req.frame.as_ref().map(|o| o.as_str()),
        "top_level": req.top_level.as_str(),
        "value_fingerprint": req.value_fingerprint,
    });
    let mut hasher = sha2::Sha256::new();
    hasher.update(b"oxibrowser-confirmation-v1\n");
    hasher.update(canonical.to_string().as_bytes());
    hasher.finalize().into()
}

/// Convenience wrapper for callers that map decisions onto `CredError`
/// (CDP `consentRequired` / `originMismatch` pairing, design §6.3).
pub fn decision_error(req: &UseRequest, decision: &Decision) -> CredError {
    match decision {
        Decision::Allow => CredError::Invalid("decision was Allow, not an error".to_string()),
        Decision::Deny { reason } => CredError::ConsentRequired {
            credential: req.credential.0.clone(),
            origin: req.top_level.as_str(),
            action: req.action.as_str().to_string(),
        }
        .merge_reason(reason),
        Decision::RequireConfirmation { .. } => CredError::ConsentRequired {
            credential: req.credential.0.clone(),
            origin: req.top_level.as_str(),
            action: req.action.as_str().to_string(),
        },
    }
}

impl CredError {
    fn merge_reason(self, reason: &str) -> CredError {
        match self {
            CredError::ConsentRequired {
                credential,
                origin,
                action,
            } => CredError::ConsentRequired {
                credential: format!("{credential} ({reason})"),
                origin,
                action,
            },
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::consent::{ConsentRecord, ConsentSubject, DEFAULT_CONSENT_TTL, DEFAULT_MAX_USES};
    use crate::provider::CredentialKind;

    fn setup() -> (PolicyEngine, std::path::PathBuf) {
        let dir = crate::unique_temp_dir("policy-test");
        let audit_path = dir.join("audit.jsonl");
        let _ = std::fs::remove_file(&audit_path);
        let audit = Arc::new(AuditLog::open(&audit_path).unwrap());
        let engine = PolicyEngine::new(
            Vec::new(),
            ConsentStore::open(dir.join("consents.jsonl")).unwrap(),
            audit,
        );
        (engine, audit_path)
    }

    fn cred_id(kind: CredentialKind, slug: &str) -> CredentialId {
        CredentialId::format("main", "cloudflare.com", kind, slug).unwrap()
    }

    fn req(id: CredentialId, origin: &str, action: CredentialAction) -> UseRequest {
        UseRequest::new(id, Origin::parse(origin).unwrap(), action)
    }

    fn grant(
        engine: &PolicyEngine,
        id: &CredentialId,
        actions: &[&str],
        max: u64,
    ) -> ConsentRecord {
        let rec = ConsentRecord::new(
            ConsentSubject::Credential {
                credential: id.clone(),
            },
            "https://dash.cloudflare.com",
            actions,
            DEFAULT_CONSENT_TTL,
            max,
        );
        engine.consents.grant(rec.clone()).unwrap();
        rec
    }

    fn read_audit(path: &std::path::Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn deny_rule_wins_over_active_consent() {
        let (mut engine, audit_path) = setup();
        let id = cred_id(CredentialKind::Password, "dash");
        grant(&engine, &id, &["login"], DEFAULT_MAX_USES);
        engine.deny_rules.push(OriginRule {
            origin: Origin::parse("https://dash.cloudflare.com").unwrap(),
            mode: RuleMode::Deny,
        });
        let now = Utc::now();
        match engine.authorize_use_at(
            &req(
                id.clone(),
                "https://dash.cloudflare.com",
                CredentialAction::Login,
            ),
            now,
        ) {
            Decision::Deny { reason } => assert!(reason.contains("deny rule")),
            other => panic!("expected Deny, got {other:?}"),
        }
        // Deny consumed nothing: the grant is still there and unused.
        assert!(
            engine
                .consents
                .active_for(&id, "https://dash.cloudflare.com", "login", now)
                .is_some()
        );
        let lines = read_audit(&audit_path);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["kind"], "credential_use");
        assert_eq!(lines[0]["decision"], "deny");
        assert_eq!(lines[0]["credential"]["id"], id.as_str());
    }

    #[test]
    fn deny_rule_on_frame_origin_blocks() {
        let (mut engine, _audit) = setup();
        let id = cred_id(CredentialKind::Password, "dash");
        engine.deny_rules.push(OriginRule {
            origin: Origin::parse("https://evil.example").unwrap(),
            mode: RuleMode::Deny,
        });
        let mut r = req(id, "https://dash.cloudflare.com", CredentialAction::Login);
        r.frame = Some(Origin::parse("https://evil.example").unwrap());
        assert!(matches!(engine.authorize_use(&r), Decision::Deny { .. }));
    }

    #[test]
    fn consent_hit_allows_and_consumes() {
        let (engine, audit_path) = setup();
        let id = cred_id(CredentialKind::Password, "dash");
        grant(&engine, &id, &["login", "mfa"], 1);
        let now = Utc::now();
        assert!(matches!(
            engine.authorize_use_at(
                &req(
                    id.clone(),
                    "https://dash.cloudflare.com",
                    CredentialAction::Login
                ),
                now
            ),
            Decision::Allow
        ));
        // Use budget exhausted → next request needs confirmation.
        assert!(matches!(
            engine.authorize_use_at(
                &req(
                    id.clone(),
                    "https://dash.cloudflare.com",
                    CredentialAction::Login
                ),
                now
            ),
            Decision::RequireConfirmation { .. }
        ));
        let lines = read_audit(&audit_path);
        assert_eq!(lines[0]["decision"], "allow");
        assert!(
            lines[0]["reason"]
                .as_str()
                .unwrap()
                .starts_with("consent:c-")
        );
        assert_eq!(lines[1]["decision"], "prompt");
        // Audit lines never carry secret values — only handle + fingerprint.
        assert_eq!(lines[0]["credential"]["fingerprint"], "");
    }

    #[test]
    fn fingerprinted_requests_correlate_in_audit() {
        let (engine, audit_path) = setup();
        let id = cred_id(CredentialKind::Password, "dash");
        grant(&engine, &id, &["fill-api-key"], DEFAULT_MAX_USES);
        let mut r = req(
            id,
            "https://dash.cloudflare.com",
            CredentialAction::FillApiKey,
        );
        r.value_fingerprint = Some("sha256:f52fbd5212".to_string());
        assert!(matches!(engine.authorize_use(&r), Decision::Allow));
        let lines = read_audit(&audit_path);
        assert_eq!(lines[0]["credential"]["fingerprint"], "sha256:f52fbd5212");
        // The raw value never enters the audit file.
        let raw = std::fs::read_to_string(&audit_path).unwrap();
        assert!(!raw.contains("hunter2"));
    }

    #[test]
    fn missing_consent_prompts() {
        let (engine, audit_path) = setup();
        let id = cred_id(CredentialKind::ApiKey, "ci");
        match engine.authorize_use(&req(
            id,
            "https://dash.cloudflare.com",
            CredentialAction::FillApiKey,
        )) {
            Decision::RequireConfirmation { reason } => {
                assert!(reason.contains("no active consent"));
            }
            other => panic!("expected RequireConfirmation, got {other:?}"),
        }
        assert_eq!(read_audit(&audit_path)[0]["decision"], "prompt");
    }

    #[test]
    fn passkey_kind_denied_until_m7() {
        let (engine, _audit) = setup();
        let id = cred_id(CredentialKind::Passkey, "web");
        assert!(matches!(
            engine.authorize_use(&req(
                id,
                "https://dash.cloudflare.com",
                CredentialAction::Login
            )),
            Decision::Deny { .. }
        ));
    }

    #[test]
    fn confirmation_flow_full_lifecycle() {
        let (engine, audit_path) = setup();
        let id = cred_id(CredentialKind::Password, "dash");
        let r = req(id, "https://dash.cloudflare.com", CredentialAction::Login);

        // No consent → prompt → user approves card.
        assert!(matches!(
            engine.authorize_use(&r),
            Decision::RequireConfirmation { .. }
        ));
        let token = engine.issue_confirmation(&r);
        assert!(matches!(
            engine.verify_confirmation(&token, &r),
            Decision::Allow
        ));

        // A changed request voids the approval.
        let mut r2 = r.clone();
        r2.action = CredentialAction::Mfa;
        assert!(matches!(
            engine.verify_confirmation(&token, &r2),
            Decision::Deny { .. }
        ));
        let mut r3 = r.clone();
        r3.top_level = Origin::parse("https://evil.example").unwrap();
        assert!(matches!(
            engine.verify_confirmation(&token, &r3),
            Decision::Deny { .. }
        ));

        // Timeout denies with the Timeout audit decision.
        let expired = ConfirmationToken {
            request_hash: token.request_hash,
            issued_at: token.issued_at,
            expires_at: token.issued_at,
        };
        assert!(matches!(
            engine.verify_confirmation_at(
                &expired,
                &r,
                token.issued_at + chrono::Duration::seconds(1)
            ),
            Decision::Deny { .. }
        ));

        let lines = read_audit(&audit_path);
        let decisions: Vec<&str> = lines
            .iter()
            .map(|l| l["decision"].as_str().unwrap())
            .collect();
        assert_eq!(
            decisions,
            vec!["prompt", "allow", "deny", "deny", "timeout"]
        );
    }

    #[test]
    fn deny_rule_added_after_issuance_still_wins() {
        let (mut engine, _audit) = setup();
        let id = cred_id(CredentialKind::Password, "dash");
        let r = req(id, "https://dash.cloudflare.com", CredentialAction::Login);
        let token = engine.issue_confirmation(&r);
        engine.deny_rules.push(OriginRule {
            origin: Origin::parse("https://dash.cloudflare.com").unwrap(),
            mode: RuleMode::Deny,
        });
        assert!(matches!(
            engine.verify_confirmation(&token, &r),
            Decision::Deny { .. }
        ));
    }

    #[test]
    fn value_swap_voids_confirmation() {
        let (engine, _audit) = setup();
        let id = cred_id(CredentialKind::Password, "dash");
        let mut r = req(id, "https://dash.cloudflare.com", CredentialAction::Login);
        r.value_fingerprint = Some("sha256:aaaaaaaa".to_string());
        let token = engine.issue_confirmation(&r);
        r.value_fingerprint = Some("sha256:bbbbbbbb".to_string());
        assert!(matches!(
            engine.verify_confirmation(&token, &r),
            Decision::Deny { .. }
        ));
    }

    #[test]
    fn token_hash_is_stable_and_serializable() {
        let (engine, _audit) = setup();
        let id = cred_id(CredentialKind::Password, "dash");
        let r = req(id, "https://dash.cloudflare.com", CredentialAction::Login);
        let t1 = engine.issue_confirmation(&r);
        let t2 = engine.issue_confirmation(&r);
        assert_eq!(t1.request_hash_hex(), t2.request_hash_hex());
        assert_eq!(t1.request_hash_hex().len(), 64);
        let json = serde_json::to_string(&t1).unwrap();
        let back: ConfirmationToken = serde_json::from_str(&json).unwrap();
        assert_eq!(back, t1);
    }

    #[test]
    fn account_grant_expires_and_exhausts_like_credentials() {
        let (engine, _audit) = setup();
        let rec = ConsentRecord::new(
            ConsentSubject::Account {
                account: "gh-work".into(),
                agent: "omp".into(),
            },
            "https://github.com",
            &["navigate"],
            DEFAULT_CONSENT_TTL,
            1,
        );
        engine.consents.grant(rec.clone()).unwrap();
        let now = Utc::now();
        assert!(
            engine
                .consents
                .active_for_account("gh-work", "omp", "https://github.com", "navigate", now)
                .is_some()
        );
        engine.consents.consume(&rec.consent_id).unwrap();
        assert!(
            engine
                .consents
                .active_for_account("gh-work", "omp", "https://github.com", "navigate", now)
                .is_none()
        );
        let later = now + chrono::Duration::days(15);
        assert!(
            engine
                .consents
                .active_for_account("gh-work", "omp", "https://github.com", "navigate", later)
                .is_none()
        );
    }
}
