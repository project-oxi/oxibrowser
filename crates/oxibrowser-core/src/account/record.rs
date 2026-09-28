//! [`AccountRecord`] — the `account.json` schema (upper design §4.1) — and
//! the account lifecycle state machine (§4.2).
//!
//! An account record is metadata only: it never carries secret values.
//! Credential references are opaque handle strings (`kch:…`) whose meaning
//! lives in the credentials crate; the session summary carries counts and
//! expiry horizons, never cookie values.

use crate::error::{CoreError, Result};
use crate::storage::session_store::{EgressMeta, FingerprintMeta};
use serde::{Deserialize, Serialize};

/// Maximum account-id slug length (design §4.1: `[a-z0-9-]{1,32}`).
pub const ACCOUNT_ID_MAX: usize = 32;

/// Validate an account id slug: `[a-z0-9-]{1,32}`.
pub fn validate_account_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= ACCOUNT_ID_MAX
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !id.starts_with('-')
        && !id.ends_with('-');
    if ok {
        Ok(())
    } else {
        Err(account_error(format!(
            "invalid account id {id:?}: expected [a-z0-9-]{{1,{ACCOUNT_ID_MAX}}}"
        )))
    }
}

/// Validate a scope: a registrable domain (public-suffix list), stored
/// lowercase with no scheme, path, or port — it is the session-file key and
/// the cookie-scope filter everywhere else.
pub fn validate_scope(scope: &str) -> Result<()> {
    if scope.is_empty() || scope.len() > 255 || !scope.is_ascii() {
        return Err(account_error(format!("invalid scope {scope:?}")));
    }
    if scope != scope.to_ascii_lowercase() {
        return Err(account_error(format!("scope must be lowercase: {scope:?}")));
    }
    // A scope must be a bare registrable domain: rejecting separators up
    // front keeps it safe as a file-name component and a jar filter.
    if scope.contains(['/', '\\', ':', '?', '#', '@', ' ']) || scope.starts_with('.') {
        return Err(account_error(format!(
            "scope must be a bare domain, got {scope:?}"
        )));
    }
    if !scope.contains('.') {
        return Err(account_error(format!(
            "scope {scope:?} is not a domain (needs a dot)"
        )));
    }
    let registrable = crate::network::cookie::registrable_domain(scope);
    if registrable != scope {
        return Err(account_error(format!(
            "scope {scope:?} is not a registrable domain (registrable part: {registrable:?})"
        )));
    }
    Ok(())
}

pub(crate) fn account_error(msg: impl Into<String>) -> CoreError {
    CoreError::SessionError(format!("account: {}", msg.into()))
}

/// Lifecycle states (design §4.2). Serialized snake_case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountState {
    /// No usable session envelope — login required.
    NeedsLogin,
    /// A login flow is in progress.
    LoggingIn,
    /// Captured session passed detection (+ probe when configured).
    Valid,
    /// Session no longer proves authentication (probe failure, expiry).
    Stale,
    /// Bot-management challenge blocks verification (challenge.rs).
    Challenge,
    /// Account revoked — records/envelopes/credentials destroyed.
    Revoked,
}

impl AccountState {
    pub fn as_str(self) -> &'static str {
        match self {
            AccountState::NeedsLogin => "needs_login",
            AccountState::LoggingIn => "logging_in",
            AccountState::Valid => "valid",
            AccountState::Stale => "stale",
            AccountState::Challenge => "challenge",
            AccountState::Revoked => "revoked",
        }
    }

    /// Legal transitions per the §4.2 machine. Same-state updates are legal
    /// (they refresh `state_detail` only); `revoked` is terminal.
    pub fn can_transition_to(self, to: AccountState) -> bool {
        use AccountState::*;
        if self == to {
            return true;
        }
        match self {
            NeedsLogin => matches!(to, LoggingIn | Valid),
            LoggingIn => matches!(to, Valid | NeedsLogin),
            Valid => matches!(to, Stale | Challenge | NeedsLogin),
            Stale => matches!(to, Valid | Challenge | LoggingIn | NeedsLogin),
            Challenge => matches!(to, Valid | NeedsLogin),
            Revoked => false,
        }
    }
}

impl std::fmt::Display for AccountState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Non-secret identity card (§1): safe to display to any surface. Card
/// contents come from pages, so they are never used for security decisions.
/// Flattened into the record's JSON (top-level keys, §4.1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IdentityCard {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login_hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
}

/// Value-free session horizon (§4.1 `session_summary`): counts and expiry
/// only — the envelope itself stays sealed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionSummary {
    /// RFC 3339 timestamp of the last envelope capture.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    pub cookie_count: u64,
    /// Earliest cookie expiry in the envelope, RFC 3339 (`None` when the
    /// envelope holds only session cookies).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub earliest_expiry: Option<String>,
    /// Origins the envelope carries localStorage for.
    pub origins: Vec<String>,
}

/// Validation probe configuration (§4.4): GET `url`, require `marker`
/// (CSS-ish selector or literal substring) and no login form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeConfig {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub marker: Option<String>,
}

/// One account: identity card + credential handles + session horizon
/// (`~/.oxibrowser/accounts/<account-id>/account.json`, §4.1).
///
/// Identity fields (`account_id`, `scope`, `created_at`, `state`) are
/// required when deserializing — a record missing them is corrupt and must
/// fail to load, not silently default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountRecord {
    /// Schema version; this module writes 1.
    #[serde(default = "default_version")]
    pub version: u32,
    pub account_id: String,
    pub scope: String,
    #[serde(flatten)]
    pub identity: IdentityCard,
    /// RFC 3339 creation timestamp.
    pub created_at: String,
    pub state: AccountState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_detail: Option<String>,
    /// The virtual device this account's sessions are captured under.
    #[serde(default)]
    pub fingerprint: FingerprintMeta,
    /// Egress posture inherited by the account's contexts.
    #[serde(default)]
    pub egress: EgressMeta,
    /// Credential handles (`kch:…`) — values live in the OS keychain.
    #[serde(default)]
    pub credentials: Vec<String>,
    #[serde(default)]
    pub session_summary: SessionSummary,
    /// Validation probe; `None` → scope-root fallback (§4.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<ProbeConfig>,
}

fn default_version() -> u32 {
    1
}

impl AccountRecord {
    /// Fresh record in [`AccountState::NeedsLogin`] with schema version 1.
    pub fn new(account_id: impl Into<String>, scope: impl Into<String>) -> Result<Self> {
        let account_id = account_id.into();
        let scope = scope.into();
        validate_account_id(&account_id)?;
        validate_scope(&scope)?;
        Ok(AccountRecord {
            version: 1,
            account_id,
            scope,
            identity: IdentityCard::default(),
            created_at: now_rfc3339(),
            state: AccountState::NeedsLogin,
            state_detail: None,
            fingerprint: FingerprintMeta::default(),
            egress: EgressMeta::Unspecified,
            credentials: Vec::new(),
            session_summary: SessionSummary::default(),
            probe: None,
        })
    }

    /// Apply `to` to this record, enforcing the §4.2 machine. Same-state
    /// updates refresh `state_detail` only. Returns `Err` on illegal
    /// transitions without touching the record.
    pub fn transition(&mut self, to: AccountState, detail: Option<String>) -> Result<()> {
        if !self.state.can_transition_to(to) {
            return Err(account_error(format!(
                "illegal state transition {} -> {} for account {:?}",
                self.state, to, self.account_id
            )));
        }
        self.state = to;
        self.state_detail = detail;
        Ok(())
    }
}

pub(crate) fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_id_rules() {
        assert!(validate_account_id("gh-work").is_ok());
        assert!(validate_account_id("a").is_ok());
        assert!(validate_account_id("a-1").is_ok());
        assert!(validate_account_id("").is_err());
        assert!(validate_account_id("GH").is_err());
        assert!(validate_account_id("has_underscore").is_err());
        assert!(validate_account_id("-lead").is_err());
        assert!(validate_account_id("trail-").is_err());
        assert!(validate_account_id(&"a".repeat(33)).is_err());
        assert!(validate_account_id(&"a".repeat(32)).is_ok());
    }

    #[test]
    fn scope_rules() {
        assert!(validate_scope("github.com").is_ok());
        assert!(validate_scope("example.co.uk").is_ok());
        assert!(validate_scope("GitHub.com").is_err());
        assert!(validate_scope("https://github.com").is_err());
        assert!(validate_scope("github.com/path").is_err());
        assert!(validate_scope("localhost").is_err());
        assert!(validate_scope("com").is_err()); // bare public suffix
    }

    #[test]
    fn state_machine_edges() {
        use AccountState::*;
        // forward edges from §4.2
        assert!(NeedsLogin.can_transition_to(LoggingIn));
        assert!(NeedsLogin.can_transition_to(Valid)); // import/capture
        assert!(LoggingIn.can_transition_to(Valid));
        assert!(LoggingIn.can_transition_to(NeedsLogin));
        assert!(Valid.can_transition_to(Stale));
        assert!(Valid.can_transition_to(Challenge));
        assert!(Stale.can_transition_to(Valid));
        assert!(Challenge.can_transition_to(Valid));
        // logout: any live state → needs_login
        for s in [NeedsLogin, LoggingIn, Valid, Stale, Challenge] {
            assert!(s.can_transition_to(NeedsLogin), "{s} -> needs_login");
        }
        // revoked is terminal
        assert!(!Revoked.can_transition_to(Valid));
        assert!(!Valid.can_transition_to(LoggingIn));
        // same-state detail refresh is legal
        assert!(Valid.can_transition_to(Valid));
    }

    #[test]
    fn record_json_round_trip_flat_identity() {
        let mut rec = AccountRecord::new("gh-work", "github.com").unwrap();
        rec.identity.login_hint = Some("garden@corp.io".into());
        rec.credentials
            .push("kch:main/github.com/password/work".into());
        let json = serde_json::to_string(&rec).unwrap();
        // identity card flattens to top-level keys (§4.1 shape)
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["login_hint"], "garden@corp.io");
        assert_eq!(v["state"], "needs_login");
        assert_eq!(v["version"], 1);
        let back: AccountRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, rec);
    }

    #[test]
    fn transition_rejects_without_mutating() {
        let mut rec = AccountRecord::new("x", "example.com").unwrap();
        assert!(rec.transition(AccountState::Stale, None).is_err());
        assert_eq!(rec.state, AccountState::NeedsLogin);
        rec.transition(AccountState::Valid, Some("captured".into()))
            .unwrap();
        assert_eq!(rec.state, AccountState::Valid);
        assert_eq!(rec.state_detail.as_deref(), Some("captured"));
    }
}
