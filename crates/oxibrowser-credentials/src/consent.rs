//! Consent records and the append-only JSONL store (lower design §5.4, with
//! the account-grant subject extension from the upper design §7.3/M4).
//!
//! The file is append-only: a grant appends one record, a revocation appends
//! a tombstone, and a use-counter increment appends a fresh copy of the
//! record. Readers replay the file — the **last record per `consent_id`
//! wins**, tombstones remove. Nothing is ever mutated in place.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::error::CredError;
use crate::provider::CredentialId;

/// Wall-clock type used across consent records and confirmation tokens.
pub type Timestamp = DateTime<Utc>;

/// Default grant lifetime — unlimited consents are forbidden (design §5.4).
pub const DEFAULT_CONSENT_TTL: chrono::Duration = chrono::Duration::days(14);
/// Default use budget per grant (design §5.4).
pub const DEFAULT_MAX_USES: u64 = 50;

/// What a grant authorizes: a credential handle or an account slug
/// (upper design §7.3 — `subject: {credential} | {account}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ConsentSubject {
    /// Credential-plane grant, keyed by handle.
    Credential { credential: CredentialId },
    /// Account-plane grant, keyed by account slug **and** the agent it
    /// authorizes (upper design §1: grants are `agent_id × account_id`).
    Account { account: String, agent: String },
}

impl ConsentSubject {
    /// The credential handle, when this is a credential grant.
    pub fn credential(&self) -> Option<&CredentialId> {
        match self {
            ConsentSubject::Credential { credential } => Some(credential),
            ConsentSubject::Account { .. } => None,
        }
    }

    /// The account slug, when this is an account grant.
    pub fn account(&self) -> Option<&str> {
        match self {
            ConsentSubject::Credential { .. } => None,
            ConsentSubject::Account { account, .. } => Some(account),
        }
    }

    /// The agent this account grant is bound to. Account grants are always
    /// agent-scoped — a grant for one agent never authorizes another.
    pub fn agent(&self) -> Option<&str> {
        match self {
            ConsentSubject::Credential { .. } => None,
            ConsentSubject::Account { agent, .. } => Some(agent),
        }
    }
}

impl std::fmt::Display for ConsentSubject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConsentSubject::Credential { credential } => write!(f, "credential:{credential}"),
            ConsentSubject::Account { account, agent } => {
                write!(f, "account:{account}/agent:{agent}")
            }
        }
    }
}

/// One consent grant (design §5.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsentRecord {
    pub version: u32,
    pub consent_id: String,
    pub subject: ConsentSubject,
    /// Exact origin the grant covers (normalized at construction).
    pub origin: String,
    /// Action strings this grant covers (`login`, `mfa`, `fill-api-key`, or
    /// the account-plane `navigate`/`interact`/`irreversible`).
    pub actions: Vec<String>,
    pub granted_at: Timestamp,
    pub granted_by: String,
    /// Mandatory — no indefinite grants.
    pub expires_at: Timestamp,
    pub max_uses: u64,
    pub uses: u64,
    /// Caller-supplied correlation tag (`--ref`) — joins the grant with
    /// `audit.jsonl` lines and external task/run receipts. Absent on
    /// pre-schema grants.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub ref_tag: Option<String>,
}

impl ConsentRecord {
    /// Construct a fresh grant with a generated `consent_id`, `granted_by`
    /// `user:local`, and zero uses.
    pub fn new(
        subject: ConsentSubject,
        origin: &str,
        actions: &[&str],
        ttl: chrono::Duration,
        max_uses: u64,
    ) -> Self {
        let now = Utc::now();
        Self {
            version: 1,
            consent_id: Self::generate_id(&subject, origin, now),
            subject,
            origin: normalize_origin(origin),
            actions: actions.iter().map(|a| a.to_string()).collect(),
            granted_at: now,
            granted_by: "user:local".to_string(),
            expires_at: now + ttl,
            max_uses,
            uses: 0,
            ref_tag: None,
        }
    }

    /// `c-` + 6 hex chars derived from the grant identity and current time.
    /// Collisions collapse harmlessly under last-wins.
    fn generate_id(subject: &ConsentSubject, origin: &str, now: Timestamp) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let mut hasher = sha2::Sha256::new();
        hasher.update(b"oxibrowser-consent-v1\n");
        hasher.update(subject.to_string().as_bytes());
        hasher.update(origin.as_bytes());
        hasher.update(now.to_rfc3339().as_bytes());
        hasher.update(nanos.to_le_bytes());
        let digest = hasher.finalize();
        let hex: String = digest.iter().take(3).map(|b| format!("{b:02x}")).collect();
        format!("c-{hex}")
    }

    /// Does this grant cover `action`?
    pub fn covers(&self, action: &str) -> bool {
        self.actions.iter().any(|a| a == action)
    }

    /// Valid at `now` and not exhausted?
    pub fn active_at(&self, now: Timestamp) -> bool {
        now <= self.expires_at && self.uses < self.max_uses
    }
}

/// A revocation tombstone (design §5.4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevokeTombstone {
    pub version: u32,
    pub revoke: String,
    pub revoked_at: Timestamp,
    pub reason: String,
}

/// One physical line of `consents.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum ConsentLine {
    Grant(ConsentRecord),
    Revoke(RevokeTombstone),
}

/// Append-only consent store. `open`/`grant`/`revoke` touch the file;
/// `active_for*` replays it.
pub struct ConsentStore {
    path: PathBuf,
}

impl ConsentStore {
    /// Store at `~/.oxibrowser/consents.jsonl`, creating parent directories.
    pub fn open_default() -> std::io::Result<Self> {
        let home = std::env::var_os("HOME").ok_or_else(|| {
            std::io::Error::other("HOME is not set — cannot locate ~/.oxibrowser")
        })?;
        let mut path = PathBuf::from(home);
        path.push(".oxibrowser");
        path.push("consents.jsonl");
        Self::open(path)
    }

    /// Store at an explicit path (tests, relocated state directories).
    pub fn open(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one grant (design §5.4 `grant`).
    pub fn grant(&self, rec: ConsentRecord) -> std::io::Result<()> {
        self.append_line(
            &serde_json::to_string(&rec)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?,
        )
    }

    /// Revoke every active credential grant covering `(id, origin, action)`
    /// by appending tombstones. Idempotent — zero matches is not an error.
    /// Revoking one action of a multi-action grant revokes the whole grant
    /// (conservative, deny-biased).
    pub fn revoke(&self, id: &CredentialId, origin: &str, action: &str) -> std::io::Result<()> {
        self.revoke_subject(
            &ConsentSubject::Credential {
                credential: id.clone(),
            },
            origin,
            action,
        )
    }

    /// Account-plane counterpart of [`ConsentStore::revoke`]. `agent` must
    /// match the grant's agent exactly.
    pub fn revoke_account(
        &self,
        account: &str,
        agent: &str,
        origin: &str,
        action: &str,
    ) -> std::io::Result<()> {
        self.revoke_subject(
            &ConsentSubject::Account {
                account: account.to_string(),
                agent: agent.to_string(),
            },
            origin,
            action,
        )
    }

    /// Revoke one grant by `consent_id` (tombstone with a free-form reason —
    /// `account exec` uses `exec_exit` / `exec_signal`). Idempotent — an
    /// unknown id appends nothing.
    pub fn revoke_by_id(&self, consent_id: &str, reason: &str) -> std::io::Result<()> {
        let exists = self
            .replay()?
            .into_iter()
            .any(|rec| rec.consent_id == consent_id);
        if !exists {
            return Ok(());
        }
        let tombstone = RevokeTombstone {
            version: 1,
            revoke: consent_id.to_string(),
            revoked_at: Utc::now(),
            reason: reason.to_string(),
        };
        self.append_line(
            &serde_json::to_string(&tombstone)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?,
        )
    }

    fn revoke_subject(
        &self,
        subject: &ConsentSubject,
        origin: &str,
        action: &str,
    ) -> std::io::Result<()> {
        let now = Utc::now();
        let matches: Vec<String> = self
            .replay()?
            .into_iter()
            .filter(|rec| {
                &rec.subject == subject && origin_matches(&rec.origin, origin) && rec.covers(action)
            })
            .map(|rec| rec.consent_id)
            .collect();
        for consent_id in matches {
            let tombstone = RevokeTombstone {
                version: 1,
                revoke: consent_id,
                revoked_at: now,
                reason: "user_revoked".to_string(),
            };
            self.append_line(
                &serde_json::to_string(&tombstone)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?,
            )?;
        }
        Ok(())
    }

    /// The active credential grant for `(id, origin, action)` at `now`, or
    /// `None` when absent, expired, revoked, or use-exhausted (design §5.4
    /// `active_for`). When several grants are alive, the most recently
    /// granted one wins.
    pub fn active_for(
        &self,
        id: &CredentialId,
        origin: &str,
        action: &str,
        now: Timestamp,
    ) -> Option<ConsentRecord> {
        self.active_for_subject(
            &ConsentSubject::Credential {
                credential: id.clone(),
            },
            origin,
            action,
            now,
        )
    }

    /// Account-plane counterpart of [`ConsentStore::active_for`], scoped to
    /// `(account, agent)`.
    pub fn active_for_account(
        &self,
        account: &str,
        agent: &str,
        origin: &str,
        action: &str,
        now: Timestamp,
    ) -> Option<ConsentRecord> {
        self.active_for_subject(
            &ConsentSubject::Account {
                account: account.to_string(),
                agent: agent.to_string(),
            },
            origin,
            action,
            now,
        )
    }

    /// Peek (non-consuming) whether any action in `actions` carries an
    /// active account grant for `(account, agent)` at `origin` — the
    /// `Target.createBrowserContext {oxiAccount}` gate (upper design §3.3,
    /// actions `navigate` / `interact`).
    pub fn active_account_any(
        &self,
        account: &str,
        agent: &str,
        origin: &str,
        actions: &[&str],
    ) -> Option<ConsentRecord> {
        let now = Utc::now();
        actions
            .iter()
            .find_map(|a| self.active_for_account(account, agent, origin, a, now))
    }

    fn active_for_subject(
        &self,
        subject: &ConsentSubject,
        origin: &str,
        action: &str,
        now: Timestamp,
    ) -> Option<ConsentRecord> {
        self.replay()
            .ok()?
            .into_iter()
            .filter(|rec| {
                &rec.subject == subject
                    && origin_matches(&rec.origin, origin)
                    && rec.covers(action)
                    && rec.active_at(now)
            })
            .max_by(|a, b| {
                a.granted_at
                    .cmp(&b.granted_at)
                    .then_with(|| a.consent_id.cmp(&b.consent_id))
            })
    }

    /// Record one use of a grant: appends the record with `uses + 1`
    /// (last-wins update). Errors when the consent is unknown.
    pub fn consume(&self, consent_id: &str) -> std::io::Result<()> {
        let rec = self
            .replay()?
            .into_iter()
            .find(|rec| rec.consent_id == consent_id)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("unknown consent: {consent_id}"),
                )
            })?;
        let mut updated = rec;
        updated.uses += 1;
        self.grant(updated)
    }

    /// The grant's current state, if it still exists.
    pub fn get(&self, consent_id: &str) -> Result<Option<ConsentRecord>, CredError> {
        Ok(self
            .replay()?
            .into_iter()
            .find(|rec| rec.consent_id == consent_id))
    }

    /// All live (unrevoked) grants for one account id, in file order —
    /// the `account grants <id>` listing. Expired or exhausted records are
    /// included so callers can render them as such; revoked ones are gone
    /// (their history lives in the audit log).
    pub fn list_for_account(&self, account: &str) -> std::io::Result<Vec<ConsentRecord>> {
        Ok(self
            .replay()?
            .into_iter()
            .filter(|rec| {
                matches!(
                    &rec.subject,
                    ConsentSubject::Account { account: a, .. } if a == account
                )
            })
            .collect())
    }

    /// Replay the file: last record per `consent_id` wins, tombstones remove,
    /// unparsable lines are skipped (append-only logs tolerate partial tails).
    fn replay(&self) -> std::io::Result<Vec<ConsentRecord>> {
        let content = match std::fs::read_to_string(&self.path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut order: Vec<String> = Vec::new();
        let mut by_id: HashMap<String, ConsentRecord> = HashMap::new();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<ConsentLine>(line) {
                Ok(ConsentLine::Grant(rec)) => {
                    if !by_id.contains_key(&rec.consent_id) {
                        order.push(rec.consent_id.clone());
                    }
                    by_id.insert(rec.consent_id.clone(), rec);
                }
                Ok(ConsentLine::Revoke(tombstone)) => {
                    if by_id.remove(&tombstone.revoke).is_some() {
                        order.retain(|id| *id != tombstone.revoke);
                    }
                }
                Err(_) => continue,
            }
        }
        Ok(order
            .into_iter()
            .filter_map(|id| by_id.remove(&id))
            .collect())
    }

    fn append_line(&self, line: &str) -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()
    }
}

use std::collections::HashMap;

/// Exact-origin comparison over normalized origins; unparseable origins
/// match nothing (fail-closed).
fn origin_matches(stored: &str, requested: &str) -> bool {
    match (
        oxibrowser_core::network::Origin::parse(stored),
        oxibrowser_core::network::Origin::parse(requested),
    ) {
        (Ok(a), Ok(b)) => a.exact_eq(&b),
        _ => false,
    }
}

/// Normalize an origin at grant time; unparseable origins are stored raw and
/// will simply never match (fail-closed).
fn normalize_origin(origin: &str) -> String {
    oxibrowser_core::network::Origin::parse(origin)
        .map(|o| o.as_str())
        .unwrap_or_else(|_| origin.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> ConsentStore {
        let dir = crate::unique_temp_dir("consent-test");
        ConsentStore::open(dir.join("consents.jsonl")).unwrap()
    }

    fn cred_id(slug: &str) -> CredentialId {
        CredentialId::format("main", "cloudflare.com", CredentialKind::Password, slug).unwrap()
    }

    use crate::provider::{CredentialId, CredentialKind};

    fn grant_credential(
        s: &ConsentStore,
        slug: &str,
        actions: &[&str],
        ttl: chrono::Duration,
        max: u64,
    ) -> ConsentRecord {
        let rec = ConsentRecord::new(
            ConsentSubject::Credential {
                credential: cred_id(slug),
            },
            "https://dash.cloudflare.com",
            actions,
            ttl,
            max,
        );
        s.grant(rec.clone()).unwrap();
        rec
    }

    #[test]
    fn grant_then_active_hit() {
        let s = store();
        let rec = grant_credential(&s, "dash", &["login", "mfa"], DEFAULT_CONSENT_TTL, 50);
        let now = Utc::now();
        let hit = s
            .active_for(
                &cred_id("dash"),
                "https://dash.cloudflare.com",
                "login",
                now,
            )
            .expect("active");
        assert_eq!(hit.consent_id, rec.consent_id);
        assert!(hit.covers("mfa"));
        assert!(!hit.covers("fill-api-key"));
    }

    #[test]
    fn expired_consents_are_ignored() {
        let s = store();
        grant_credential(&s, "dash", &["login"], chrono::Duration::seconds(1), 50);
        // Well past expiry.
        let later = Utc::now() + chrono::Duration::hours(2);
        assert!(
            s.active_for(
                &cred_id("dash"),
                "https://dash.cloudflare.com",
                "login",
                later
            )
            .is_none()
        );
    }

    #[test]
    fn exhausted_uses_are_ignored() {
        let s = store();
        let rec = grant_credential(&s, "dash", &["login"], DEFAULT_CONSENT_TTL, 1);
        let now = Utc::now();
        assert!(
            s.active_for(
                &cred_id("dash"),
                "https://dash.cloudflare.com",
                "login",
                now
            )
            .is_some()
        );
        s.consume(&rec.consent_id).unwrap();
        assert!(
            s.active_for(
                &cred_id("dash"),
                "https://dash.cloudflare.com",
                "login",
                now
            )
            .is_none()
        );
        // The counter moved via an appended record.
        assert_eq!(s.get(&rec.consent_id).unwrap().unwrap().uses, 1);
    }

    #[test]
    fn list_for_account_filters_subject_and_keeps_ref() {
        let s = store();
        let mut rec = ConsentRecord::new(
            ConsentSubject::Account {
                account: "gh-work".into(),
                agent: "omp".into(),
            },
            "https://github.com",
            &["navigate", "interact"],
            DEFAULT_CONSENT_TTL,
            5,
        );
        rec.ref_tag = Some("task-7".into());
        s.grant(rec.clone()).unwrap();
        // Different account, same agent — must not leak into the listing.
        s.grant(ConsentRecord::new(
            ConsentSubject::Account {
                account: "gh-personal".into(),
                agent: "omp".into(),
            },
            "https://github.com",
            &["navigate"],
            DEFAULT_CONSENT_TTL,
            5,
        ))
        .unwrap();
        // Credential-plane grant — also out of scope for account listings.
        grant_credential(&s, "dash", &["login"], DEFAULT_CONSENT_TTL, 5);

        let list = s.list_for_account("gh-work").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].consent_id, rec.consent_id);
        assert_eq!(list[0].ref_tag.as_deref(), Some("task-7"));

        // Use accounting re-appends the record; the ref must survive.
        s.consume(&rec.consent_id).unwrap();
        let after = s.list_for_account("gh-work").unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].uses, 1);
        assert_eq!(after[0].ref_tag.as_deref(), Some("task-7"));

        // Revocation removes the grant from the live listing.
        s.revoke_account("gh-work", "omp", "https://github.com", "navigate")
            .unwrap();
        assert!(s.list_for_account("gh-work").unwrap().is_empty());
    }

    #[test]
    fn last_wins_on_regrant() {
        let s = store();
        let first = grant_credential(&s, "dash", &["login"], DEFAULT_CONSENT_TTL, 50);
        let second = grant_credential(&s, "dash", &["login"], DEFAULT_CONSENT_TTL, 1);
        let now = Utc::now();
        // The most recent covering grant wins.
        let hit = s
            .active_for(
                &cred_id("dash"),
                "https://dash.cloudflare.com",
                "login",
                now,
            )
            .expect("grant active");
        assert_eq!(hit.consent_id, second.consent_id);
        assert_ne!(hit.consent_id, first.consent_id);
        // Once the newer grant's budget is spent, the older still-valid grant
        // is consulted again (every grant is an explicit user allowance).
        s.consume(&second.consent_id).unwrap();
        let fallback = s
            .active_for(
                &cred_id("dash"),
                "https://dash.cloudflare.com",
                "login",
                now,
            )
            .expect("older grant falls back");
        assert_eq!(fallback.consent_id, first.consent_id);
        // Expiry still applies to the fallback.
        let later = now + chrono::Duration::days(15);
        assert!(
            s.active_for(
                &cred_id("dash"),
                "https://dash.cloudflare.com",
                "login",
                later
            )
            .is_none()
        );
    }

    #[test]
    fn tombstone_revokes() {
        let s = store();
        let rec = grant_credential(&s, "dash", &["login"], DEFAULT_CONSENT_TTL, 50);
        let now = Utc::now();
        s.revoke(&cred_id("dash"), "https://dash.cloudflare.com", "login")
            .unwrap();
        assert!(
            s.active_for(
                &cred_id("dash"),
                "https://dash.cloudflare.com",
                "login",
                now
            )
            .is_none()
        );
        // Revocation is idempotent.
        s.revoke(&cred_id("dash"), "https://dash.cloudflare.com", "login")
            .unwrap();
        // Re-grant after revoke is active again.
        grant_credential(&s, "dash", &["login"], DEFAULT_CONSENT_TTL, 50);
        assert!(
            s.active_for(
                &cred_id("dash"),
                "https://dash.cloudflare.com",
                "login",
                now
            )
            .is_some()
        );
        // Unknown consent_id consumption errors.
        assert!(s.consume(&rec.consent_id).is_err());
    }

    #[test]
    fn origin_must_match_exactly() {
        let s = store();
        grant_credential(&s, "dash", &["login"], DEFAULT_CONSENT_TTL, 50);
        let now = Utc::now();
        assert!(
            s.active_for(&cred_id("dash"), "https://api.cloudflare.com", "login", now)
                .is_none()
        );
        // Different port = different origin.
        assert!(
            s.active_for(
                &cred_id("dash"),
                "https://dash.cloudflare.com:8443",
                "login",
                now
            )
            .is_none()
        );
        // Normalized equivalent matches (explicit default port).
        assert!(
            s.active_for(
                &cred_id("dash"),
                "https://DASH.cloudflare.com:443",
                "login",
                now
            )
            .is_some()
        );
        // Unparseable origins match nothing.
        assert!(
            s.active_for(&cred_id("dash"), "javascript:alert(1)", "login", now)
                .is_none()
        );
    }

    #[test]
    fn account_grants_are_a_separate_plane() {
        let s = store();
        let rec = ConsentRecord::new(
            ConsentSubject::Account {
                account: "gh-work".to_string(),
                agent: "omp".to_string(),
            },
            "https://github.com",
            &["navigate", "interact"],
            DEFAULT_CONSENT_TTL,
            DEFAULT_MAX_USES,
        );
        s.grant(rec.clone()).unwrap();
        let now = Utc::now();

        let hit = s
            .active_for_account("gh-work", "omp", "https://github.com", "navigate", now)
            .expect("account grant active");
        assert_eq!(hit.consent_id, rec.consent_id);
        assert_eq!(hit.subject.account(), Some("gh-work"));
        assert!(hit.subject.credential().is_none());
        // Agent scoping: a different agent never sees the grant.
        assert!(
            s.active_for_account(
                "gh-work",
                "other-agent",
                "https://github.com",
                "navigate",
                now
            )
            .is_none()
        );

        // Credential-plane lookups never see account grants.
        assert!(
            s.active_for(&cred_id("gh-work"), "https://github.com", "navigate", now)
                .is_none()
        );

        // Account revoke tombstones.
        s.revoke_account("gh-work", "omp", "https://github.com", "navigate")
            .unwrap();
        assert!(
            s.active_for_account("gh-work", "omp", "https://github.com", "navigate", now)
                .is_none()
        );
    }

    #[test]
    fn defaults_match_design() {
        assert_eq!(DEFAULT_MAX_USES, 50);
        assert_eq!(DEFAULT_CONSENT_TTL, chrono::Duration::days(14));
    }

    #[test]
    fn unparsable_lines_are_skipped() {
        let dir = crate::unique_temp_dir("consent-corrupt");
        let s = ConsentStore::open(dir.join("consents.jsonl")).unwrap();
        grant_credential(&s, "dash", &["login"], DEFAULT_CONSENT_TTL, 50);
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(s.path())
            .unwrap();
        writeln!(f, "{{not json").unwrap();
        writeln!(f).unwrap();
        drop(f);
        assert!(
            s.active_for(
                &cred_id("dash"),
                "https://dash.cloudflare.com",
                "login",
                Utc::now()
            )
            .is_some()
        );
    }

    #[test]
    fn subject_serializes_as_task_shape() {
        let c = ConsentSubject::Credential {
            credential: cred_id("x"),
        };
        let v = serde_json::to_value(&c).unwrap();
        assert!(v.get("credential").is_some());
        let a = ConsentSubject::Account {
            account: "gh-work".into(),
            agent: "omp".into(),
        };
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(
            v,
            serde_json::json!({ "account": "gh-work", "agent": "omp" })
        );
    }

    #[test]
    fn empty_store_is_not_an_error() {
        let s = store();
        assert!(
            s.active_for(
                &cred_id("nope"),
                "https://dash.cloudflare.com",
                "login",
                Utc::now()
            )
            .is_none()
        );
        assert!(matches!(
            s.revoke(&cred_id("nope"), "https://dash.cloudflare.com", "login"),
            Ok(())
        ));
    }
}
