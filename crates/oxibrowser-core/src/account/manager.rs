//! [`AccountManager`] — account lifecycle over the registry and the session
//! store (upper design §4, M-B).
//!
//! Responsibilities:
//!
//! - [`AccountManager::capture_session`] — seal a live session into the
//!   account's envelope (export + current fingerprint), refresh the session
//!   horizon, move the account to `valid`, audit `session_capture`.
//! - [`AccountManager::restore`] — load an envelope (fingerprint mismatch
//!   denied by default) and inject it into a live session, audit
//!   `session_restore`.
//! - [`AccountManager::verify_with_probe`] — restore + [`ValidationProbe`];
//!   `valid` / `stale` / `challenge` per the §4.2 machine with the §4.4
//!   false-positive defense.
//! - [`AccountManager::mark_stale`] / [`AccountManager::logout`] — state
//!   transitions and envelope disposal, audited.
//!
//! Every state transition is audited (§6.4): values never enter the log —
//! only handles, scopes, and counts.

use std::sync::Arc;

use crate::error::{CoreError, Result};
use crate::security::audit::{self, AuditDecision, AuditEvent, AuditEventKind, AuditLog};
use crate::session::Session;
use crate::storage::session_store::{FingerprintMeta, KeyProvider, SessionEnvelope, SessionStore};

use super::detector::LoginDetector;
use super::probe::{ProbeOutcome, ProbeVerdict, ValidationProbe};
use super::record::{AccountRecord, AccountState, SessionSummary, now_rfc3339};
use super::registry::AccountRegistry;

/// Account lifecycle driver: registry + audit sink.
pub struct AccountManager {
    registry: AccountRegistry,
    /// `None` → the process-global audit log (library default).
    audit: Option<Arc<AuditLog>>,
}

impl AccountManager {
    /// Manager over `registry`, auditing to the process-global log.
    pub fn new(registry: AccountRegistry) -> Self {
        AccountManager {
            registry,
            audit: None,
        }
    }

    /// Manager auditing into an explicit log (tests, embedded audit setups).
    pub fn with_audit(registry: AccountRegistry, audit: Arc<AuditLog>) -> Self {
        AccountManager {
            registry,
            audit: Some(audit),
        }
    }

    /// The underlying registry.
    pub fn registry(&self) -> &AccountRegistry {
        &self.registry
    }

    /// Seal the live state of `session` as `account_id`'s envelope: export
    /// the scope state, fingerprint it with the session's effective UA,
    /// save atomically, refresh `session_summary`, and move the account to
    /// [`AccountState::Valid`] (capture is direct evidence of validity).
    pub fn capture_session(
        &self,
        account_id: &str,
        session: &Session,
        keys: &dyn KeyProvider,
    ) -> Result<AccountRecord> {
        let mut record = self.registry.get(account_id)?;
        let state = session.export_state_for_scope(&record.scope);
        let envelope = SessionEnvelope::new(
            record.scope.clone(),
            self.current_fingerprint(session),
            record.egress.clone(),
            state,
        );
        self.session_store(account_id)?.save(&envelope, keys)?;

        let origins: Vec<String> = envelope
            .state
            .origins
            .iter()
            .map(|o| o.origin.clone())
            .collect();
        let cookie_count = envelope.state.cookies.len() as u64;
        record.session_summary = SessionSummary {
            updated_at: Some(now_rfc3339()),
            cookie_count,
            earliest_expiry: earliest_expiry_rfc3339(&envelope),
            origins: origins.clone(),
        };
        let from = record.state;
        record.transition(AccountState::Valid, Some("captured".into()))?;
        self.registry.save(&record)?;

        self.emit(
            AuditEventKind::SessionCapture,
            AuditDecision::Allow,
            Some("capture".into()),
            format!(
                "account={account_id} scope={} cookies={cookie_count} origins={}",
                record.scope,
                origins.len()
            ),
            Some(record.scope.clone()),
        );
        self.emit_state(account_id, from, record.state, "captured");
        Ok(record)
    }

    /// Load `account_id`'s envelope and inject it into `session`.
    ///
    /// `current` is the fingerprint the session currently runs under; `Some`
    /// enforces the match (default deny on drift, fail-closed per FM-L2),
    /// `None` overrides the check (the explicit CLI `--fingerprint-override`
    /// path). Envelope injection is audited as `session_restore` — allow on
    /// success, deny on the fingerprint gate.
    pub fn restore(
        &self,
        account_id: &str,
        session: &mut Session,
        keys: &dyn KeyProvider,
        current: Option<&FingerprintMeta>,
    ) -> Result<SessionEnvelope> {
        let envelope = self.load_envelope(account_id, keys, current)?;
        self.inject_envelope(account_id, session, envelope)
    }

    /// Load `account_id`'s envelope without touching a session — the
    /// blocking (keychain + file IO) half of [`AccountManager::restore`],
    /// split out so async callers can run it under
    /// `tokio::task::spawn_blocking` and inject separately. Fingerprint
    /// mismatch is denied by default (fail-closed, FM-L2) and audited.
    pub(crate) fn load_envelope(
        &self,
        account_id: &str,
        keys: &dyn KeyProvider,
        current: Option<&FingerprintMeta>,
    ) -> Result<SessionEnvelope> {
        let record = self.registry.get(account_id)?;
        match self
            .session_store(account_id)?
            .load(&record.scope, keys, current)
        {
            Ok(env) => Ok(env),
            Err(err @ CoreError::SessionFingerprintMismatch(_)) => {
                self.emit(
                    AuditEventKind::SessionRestore,
                    AuditDecision::Deny,
                    Some("restore".into()),
                    format!(
                        "account={account_id} scope={} fingerprint_mismatch",
                        record.scope
                    ),
                    Some(record.scope.clone()),
                );
                Err(err)
            }
            Err(e) => Err(e),
        }
    }

    /// Inject an already-loaded envelope into `session` and audit
    /// `session_restore` (allow). The in-memory half of
    /// [`AccountManager::restore`].
    pub(crate) fn inject_envelope(
        &self,
        account_id: &str,
        session: &mut Session,
        envelope: SessionEnvelope,
    ) -> Result<SessionEnvelope> {
        let record = self.registry.get(account_id)?;
        session.import_state(&envelope.state)?;
        self.emit(
            AuditEventKind::SessionRestore,
            AuditDecision::Allow,
            Some("restore".into()),
            format!(
                "account={account_id} scope={} cookies={} origins={}",
                record.scope,
                envelope.state.cookies.len(),
                envelope.state.origins.len()
            ),
            Some(record.scope.clone()),
        );
        Ok(envelope)
    }

    /// Restore + [`ValidationProbe`] (§4.4): the only path that may confirm
    /// a `stale` account back to `valid`, and the detector's false-positive
    /// defense at capture time.
    ///
    /// Verdict mapping: `Valid` → `valid`; `Invalid` → `stale` + reason;
    /// `Challenge` → `challenge` + `challenge:<vendor>:<kind>`;
    /// `Unreachable` → state unchanged (transport failure is not evidence).
    pub async fn verify_with_probe(
        &self,
        account_id: &str,
        session: &mut Session,
        keys: &dyn KeyProvider,
        current: Option<&FingerprintMeta>,
    ) -> Result<(AccountRecord, ProbeOutcome)> {
        let record = self.registry.get(account_id)?;
        let probe = ValidationProbe::from_record(record.probe.as_ref(), &record.scope)?;
        self.restore(account_id, session, keys, current)?;
        let outcome = probe.run(session.http_client().as_ref()).await;
        match &outcome.verdict {
            ProbeVerdict::Valid => {
                let from = self.registry.get(account_id)?.state;
                let updated = self.registry.set_state(
                    account_id,
                    AccountState::Valid,
                    Some("probe_ok".into()),
                )?;
                self.emit_state(account_id, from, updated.state, "probe_ok");
            }
            ProbeVerdict::Invalid { reason } => {
                self.mark_stale_inner(account_id, reason)?;
            }
            ProbeVerdict::Challenge { challenge } => {
                let kind = match challenge.kind {
                    crate::challenge::ChallengeKind::Managed => "managed",
                    crate::challenge::ChallengeKind::JsCheck => "js_check",
                    crate::challenge::ChallengeKind::Interactive => "interactive",
                    crate::challenge::ChallengeKind::Blocked => "blocked",
                    crate::challenge::ChallengeKind::Unknown => "unknown",
                };
                let detail = format!("challenge:{}:{}", challenge.vendor.as_str(), kind);
                let from = self.registry.get(account_id)?.state;
                let updated = self.registry.set_state(
                    account_id,
                    AccountState::Challenge,
                    Some(detail.clone()),
                )?;
                self.emit_state(account_id, from, updated.state, &detail);
            }
            ProbeVerdict::Unreachable { reason } => {
                tracing::warn!(account = %account_id, reason = %reason, "probe unreachable; state unchanged");
            }
        }
        let updated = self.registry.get(account_id)?;
        Ok((updated, outcome))
    }

    /// Mark `valid` (or already-`stale`) account as `stale` with a reason —
    /// probe failure, observed expiry, or server-side invalidation.
    pub fn mark_stale(&self, account_id: &str, detail: impl Into<String>) -> Result<AccountRecord> {
        self.mark_stale_inner(account_id, &detail.into())
    }

    /// Server-side logout best-effort is the caller's job; this disposes the
    /// stored envelope, clears the session horizon, and returns the account
    /// to [`AccountState::NeedsLogin`] — credentials are kept (§4.2).
    pub fn logout(&self, account_id: &str) -> Result<AccountRecord> {
        let record = self.registry.get(account_id)?;
        let store = self.session_store(account_id)?;
        let discarded = store.discard(&record.scope)?;
        let from = record.state;
        let mut updated = self.registry.get(account_id)?;
        updated.session_summary = SessionSummary {
            updated_at: Some(now_rfc3339()),
            ..SessionSummary::default()
        };
        updated.transition(AccountState::NeedsLogin, Some("logged_out".into()))?;
        self.registry.save(&updated)?;

        self.emit(
            AuditEventKind::SessionDiscard,
            AuditDecision::Allow,
            Some("logout".into()),
            format!(
                "account={account_id} scope={} discarded={discarded}",
                record.scope
            ),
            Some(record.scope.clone()),
        );
        self.emit_state(account_id, from, updated.state, "logged_out");
        Ok(updated)
    }

    /// Detector convenience for callers orchestrating a login flow: judge a
    /// live session against the pre-login baseline (§4.3).
    pub async fn detect_login(
        &self,
        account_id: &str,
        session: &mut Session,
        baseline: &super::detector::PreLoginSnapshot,
        explicit_success: bool,
    ) -> Result<super::detector::Detection> {
        let record = self.registry.get(account_id)?;
        let detector = LoginDetector::new(record.scope.clone())?;
        detector.assess(session, baseline, explicit_success).await
    }

    fn mark_stale_inner(&self, account_id: &str, reason: &str) -> Result<AccountRecord> {
        let from = self.registry.get(account_id)?.state;
        let updated =
            self.registry
                .set_state(account_id, AccountState::Stale, Some(reason.to_string()))?;
        self.emit_state(account_id, from, updated.state, reason);
        Ok(updated)
    }

    /// The fingerprint a session currently presents (effective UA; hints
    /// default). Same derivation on both sides of capture/restore.
    fn current_fingerprint(&self, session: &Session) -> FingerprintMeta {
        FingerprintMeta {
            user_agent: session.effective_ua(),
            ..FingerprintMeta::default()
        }
    }

    fn session_store(&self, account_id: &str) -> Result<SessionStore> {
        self.registry.session_store(account_id)
    }

    /// Audit + event emission for a lifecycle transition. `pub(crate)` so the
    /// login orchestrator (same crate) reuses the exact audit/event shape of
    /// manager-driven transitions.
    pub(crate) fn emit_state(
        &self,
        account_id: &str,
        from: AccountState,
        to: AccountState,
        detail: &str,
    ) {
        self.emit(
            AuditEventKind::AccountState,
            AuditDecision::Allow,
            Some(format!("{from}->{to}")),
            format!("account={account_id} {detail}"),
            None,
        );
    }

    fn emit(
        &self,
        kind: AuditEventKind,
        decision: AuditDecision,
        action: Option<String>,
        reason: String,
        origin: Option<String>,
    ) {
        let event = AuditEvent {
            origin,
            action,
            ..audit::event(kind, decision, reason)
        };
        match &self.audit {
            Some(log) => {
                if let Err(e) = log.record(event) {
                    tracing::warn!(error = %e, "account audit log write failed");
                }
            }
            None => audit::record(event),
        }
    }
}

/// Earliest absolute cookie expiry in an envelope, RFC 3339 (`None` when the
/// envelope holds only session cookies).
fn earliest_expiry_rfc3339(envelope: &SessionEnvelope) -> Option<String> {
    let min = envelope
        .state
        .cookies
        .iter()
        .filter_map(|c| c.expiry)
        .min()?;
    chrono::DateTime::from_timestamp(min, 0)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::AccountRecord;
    use crate::config::BrowserConfig;
    use crate::network::cookie::{CookieEntry, SameSite};
    use crate::security::audit::AuditLog;
    use crate::session::RequestOverrides;
    use crate::storage::session_store::StaticKeyProvider;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "oxi-account-mgr-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn keys() -> StaticKeyProvider {
        StaticKeyProvider::new([7u8; 32])
    }

    /// (manager, audit.jsonl path) with the registry under a temp base.
    fn setup(tag: &str) -> (AccountManager, PathBuf) {
        let base = temp_dir(tag);
        let audit_path = base.join("audit.jsonl");
        let log = Arc::new(AuditLog::open(&audit_path).unwrap());
        let registry = AccountRegistry::open(base.join("accounts")).unwrap();
        let manager = AccountManager::with_audit(registry, log);
        (manager, audit_path)
    }

    fn read_audit(audit_path: &PathBuf) -> Vec<AuditEvent> {
        std::fs::read_to_string(audit_path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str::<AuditEvent>(l).unwrap())
            .collect()
    }

    fn session_cookie() -> CookieEntry {
        CookieEntry {
            name: "user_session".into(),
            value: "totally-secret".into(),
            path: Some("/".into()),
            domain: Some("github.com".into()),
            secure: true,
            http_only: true,
            same_site: Some(SameSite::Lax),
            expires: None,
            max_age: Some(3600),
            expiry: Some(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64
                    + 3600,
            ),
            partitioned: false,
            partition_key: None,
        }
    }

    async fn new_session() -> Arc<tokio::sync::RwLock<Session>> {
        let browser = crate::Browser::new(BrowserConfig::headless())
            .await
            .unwrap();
        browser.new_session().await.unwrap()
    }

    fn with_ua(ua: &str) -> RequestOverrides {
        RequestOverrides {
            user_agent: Some(ua.into()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn capture_restore_round_trip() {
        let (manager, audit_path) = setup("roundtrip");
        manager
            .registry()
            .add(AccountRecord::new("gh", "github.com").unwrap())
            .unwrap();

        // Capture side: one session cookie, UA override → fingerprint.
        let s1 = new_session().await;
        s1.write().await.set_overrides(with_ua("RoundTrip/1"));
        s1.write()
            .await
            .cookie_jar()
            .write()
            .insert_entry(session_cookie());

        let rec = manager
            .capture_session("gh", &*s1.read().await, &keys())
            .unwrap();
        assert_eq!(rec.state, AccountState::Valid);
        assert_eq!(rec.session_summary.cookie_count, 1);
        assert!(rec.session_summary.earliest_expiry.is_some());
        // envelope on disk, sealed
        let store = manager.registry().session_store("gh").unwrap();
        assert!(store.path_for("github.com").is_file());

        // Restore side: fresh session, same fingerprint expected.
        let s2 = new_session().await;
        s2.write().await.set_overrides(with_ua("RoundTrip/1"));
        {
            let mut guard = s2.write().await;
            let current = FingerprintMeta {
                user_agent: guard.effective_ua(),
                ..FingerprintMeta::default()
            };
            let env = manager
                .restore("gh", &mut guard, &keys(), Some(&current))
                .unwrap();
            assert_eq!(env.scope, "github.com");
            assert_eq!(env.state.cookies.len(), 1);
        }
        assert!(
            s2.read()
                .await
                .cookie_jar()
                .read()
                .get_all()
                .iter()
                .any(|c| c.name == "user_session")
        );

        // Audit trail: capture → state(valid) → restore, no values leaked.
        let events = read_audit(&audit_path);
        let kinds: Vec<_> = events.iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&AuditEventKind::SessionCapture));
        assert!(kinds.contains(&AuditEventKind::SessionRestore));
        assert!(events.iter().all(|e| !e.reason.contains("totally-secret")));
        let capture = events
            .iter()
            .find(|e| e.kind == AuditEventKind::SessionCapture)
            .unwrap();
        assert_eq!(capture.decision, AuditDecision::Allow);
        assert_eq!(capture.origin.as_deref(), Some("github.com"));
    }

    #[tokio::test]
    async fn restore_rejects_fingerprint_drift_and_audits_deny() {
        let (manager, audit_path) = setup("drift");
        manager
            .registry()
            .add(AccountRecord::new("gh", "github.com").unwrap())
            .unwrap();

        let s1 = new_session().await;
        s1.write().await.set_overrides(with_ua("Captured/1"));
        s1.write()
            .await
            .cookie_jar()
            .write()
            .insert_entry(session_cookie());
        manager
            .capture_session("gh", &*s1.read().await, &keys())
            .unwrap();

        let s2 = new_session().await;
        let mut guard = s2.write().await;
        let drifted = FingerprintMeta {
            user_agent: "Drifted/9".into(),
            ..FingerprintMeta::default()
        };
        let err = manager.restore("gh", &mut guard, &keys(), Some(&drifted));
        assert!(matches!(err, Err(CoreError::SessionFingerprintMismatch(_))));
        // no cookies injected on the denied path
        assert!(guard.cookie_jar().read().is_empty());

        // explicit override (None) is the documented escape hatch
        manager.restore("gh", &mut guard, &keys(), None).unwrap();
        assert!(!guard.cookie_jar().read().is_empty());

        let events = read_audit(&audit_path);
        let denies: Vec<_> = events
            .iter()
            .filter(|e| {
                e.kind == AuditEventKind::SessionRestore && e.decision == AuditDecision::Deny
            })
            .collect();
        assert_eq!(denies.len(), 1, "fingerprint mismatch must audit a deny");
        assert!(denies[0].reason.contains("fingerprint_mismatch"));
    }

    #[tokio::test]
    async fn logout_discards_envelope_and_resets_state() {
        let (manager, audit_path) = setup("logout");
        manager
            .registry()
            .add(AccountRecord::new("gh", "github.com").unwrap())
            .unwrap();
        let s1 = new_session().await;
        s1.write()
            .await
            .cookie_jar()
            .write()
            .insert_entry(session_cookie());
        manager
            .capture_session("gh", &*s1.read().await, &keys())
            .unwrap();

        let rec = manager.logout("gh").unwrap();
        assert_eq!(rec.state, AccountState::NeedsLogin);
        assert_eq!(rec.session_summary.cookie_count, 0);
        assert!(rec.session_summary.origins.is_empty());
        let store = manager.registry().session_store("gh").unwrap();
        assert!(
            !store.path_for("github.com").exists(),
            "envelope must be gone"
        );

        let events = read_audit(&audit_path);
        assert!(events.iter().any(
            |e| e.kind == AuditEventKind::SessionDiscard && e.decision == AuditDecision::Allow
        ));
        assert!(events.iter().any(|e| {
            e.kind == AuditEventKind::AccountState
                && e.action.as_deref() == Some("valid->needs_login")
        }));
    }

    #[tokio::test]
    async fn mark_stale_records_transition() {
        let (manager, audit_path) = setup("stale");
        manager
            .registry()
            .add(AccountRecord::new("gh", "github.com").unwrap())
            .unwrap();
        let s1 = new_session().await;
        manager
            .capture_session("gh", &*s1.read().await, &keys())
            .unwrap();

        let rec = manager.mark_stale("gh", "probe_401").unwrap();
        assert_eq!(rec.state, AccountState::Stale);
        assert_eq!(rec.state_detail.as_deref(), Some("probe_401"));
        assert_eq!(
            manager.registry().get("gh").unwrap().state,
            AccountState::Stale
        );

        // stale → stale refreshes detail without an illegal transition
        let rec = manager.mark_stale("gh", "probe_401_again").unwrap();
        assert_eq!(rec.state_detail.as_deref(), Some("probe_401_again"));

        let events = read_audit(&audit_path);
        assert!(events.iter().any(|e| e.kind == AuditEventKind::AccountState
            && e.action.as_deref() == Some("valid->stale")
            && e.reason.contains("probe_401")));
    }

    #[tokio::test]
    async fn capture_twice_updates_horizon_atomically() {
        let (manager, _audit_path) = setup("twice");
        manager
            .registry()
            .add(AccountRecord::new("gh", "github.com").unwrap())
            .unwrap();
        let s1 = new_session().await;
        manager
            .capture_session("gh", &*s1.read().await, &keys())
            .unwrap();
        s1.write()
            .await
            .cookie_jar()
            .write()
            .insert_entry(session_cookie());
        let rec = manager
            .capture_session("gh", &*s1.read().await, &keys())
            .unwrap();
        assert_eq!(rec.session_summary.cookie_count, 1);
        // the store still reads back as one valid envelope
        let env = manager
            .registry()
            .session_store("gh")
            .unwrap()
            .load("github.com", &keys(), None)
            .unwrap();
        assert_eq!(env.state.cookies.len(), 1);
    }

    #[test]
    fn unknown_account_errors() {
        let (manager, _audit_path) = setup("unknown");
        assert!(manager.registry().get("nope").is_err());
        // malformed ids are rejected; well-formed ids get a lazy store (the
        // directory is only materialized by `add` or the first save)
        assert!(manager.registry().session_store("Bad_ID").is_err());
        assert!(manager.registry().session_store("never-added").is_ok());
        assert!(!manager.registry().account_dir("never-added").exists());
        // capture on an unknown account surfaces the registry error
        assert!(manager.mark_stale("nope", "x").is_err());
    }
}
