//! [`AgentLoginEngine`] — the M-D unattended agent login flow (upper design
//! §5.3).
//!
//! The engine drives a dedicated session in an account context end-to-end:
//!
//! 1. **scope jar restore** — an existing envelope is injected and judged by
//!    the [`ValidationProbe`]; a probe-valid jar re-captures and the flow
//!    ends before any credential is touched. A stale/absent/mismatched jar
//!    falls through to a fresh login.
//! 2. **login page discovery** — the probe URL, the scope root, and the
//!    `/login` `/signin` `/session` `/auth` paths are tried in order until a
//!    snapshot yields a login form ([`detect_login_form`]: exactly one
//!    password input plus a text/email companion and a submit control).
//! 3. **frame-origin gate (core M1, fail-closed)** — the form's hosting
//!    origin is evaluated against the credential's exact allowlist before
//!    resolution. v1 forms are main-document: iframe-hosted forms are
//!    invisible to the snapshot and fail closed by absence.
//! 4. **credential resolution** — through the [`CredentialSource`] port
//!    (implemented over the credentials-crate broker): deny rules → consent
//!    → resolve, with the `credential_read`/`credential_use` audit trail.
//! 5. **injection + submit** — values go straight into the page and the
//!    form POST; they never reach a log, event, or response.
//! 6. **TOTP** — a post-submit one-time-code form is filled from the
//!    account's TOTP credential. SMS/email-code steps are on the forbidden
//!    list: the flow aborts with an escalation instead of retrying.
//! 7. **judgment** — [`LoginDetector`] over the post-restore baseline, one
//!    retry of the inject+submit step at most.
//! 8. **redirect verdict** — the sequence's final origin is checked against
//!    the credential allowlist ([`OriginPolicy::redirect_verdict`]); an exit
//!    outside it vetoes capture and audits a `policy_violation` (v1 checks
//!    the final URL — there are no request hooks to watch mid-flight).
//!
//! Abort mapping: Interactive/Blocked challenges → account `challenge` +
//! escalation (never retried); SMS/email 2FA → immediate escalation;
//! form-not-found / no evidence / consent denial → `needs_login` + detail.
//!
//! The engine is reusable from any host (CDP server, CLI, REPL): callers
//! wire a fresh session, an [`AccountManager`], a [`CredentialSource`], and
//! an event sink; every transition is audited here, values never are.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::broadcast;
use zeroize::Zeroizing;

use crate::challenge::ChallengeKind;
use crate::error::{CoreError, Result};
use crate::js::dom_snapshot::{DomSnapshot, InteractiveElement};
use crate::network::client::ChallengeOutcome;
use crate::network::origin_policy::{Origin, OriginPolicy, RedirectVerdict};
use crate::security::audit::{self, AuditDecision, AuditEvent, AuditEventKind, CredentialRef};
use crate::session::Session;
use crate::storage::session_store::{FingerprintMeta, KeyProvider};
use url::Url;

use super::detector::LoginDetector;
use super::manager::AccountManager;
use super::orchestrator::AccountEvent;
use super::probe::{ProbeVerdict, ValidationProbe};
use super::record::{AccountRecord, AccountState, account_error};

/// Default whole-flow deadline (§7.3 login-window default).
pub const DEFAULT_AGENT_LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
/// Login paths tried under the scope root, in order (§5.3).
pub const LOGIN_PATHS: [&str; 4] = ["login", "signin", "session", "auth"];
/// Inject+submit attempts (initial + one retry, §5.3).
pub const MAX_SUBMIT_ATTEMPTS: u32 = 2;

// ---------------------------------------------------------------------------
// Credential source port (implemented over the credentials-crate broker)
// ---------------------------------------------------------------------------

/// Why a credential could not be supplied. The engine maps these onto flow
/// aborts; no value ever rides an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceError {
    /// Deny rule, missing/expired/exhausted grant, or an unanswered
    /// confirmation — unattended flows cannot confirm mid-flight.
    ConsentRequired { reason: String },
    /// The form origin is not in the credential's exact allowlist (M1).
    OriginMismatch { reason: String },
    /// No credential of the requested role for this scope.
    NotFound { reason: String },
    /// Keystore unusable.
    Unavailable { reason: String },
}

/// One resolved login: values live only between `resolve` and injection.
pub struct ResolvedLogin {
    /// Log-safe handle (`kch:…`) for the audit trail.
    pub handle: String,
    /// Username hint (not a secret) from the credential.
    pub username: Option<String>,
    /// The password value — zeroized on drop, never logged.
    pub password: Zeroizing<String>,
    /// `"sha256:…"` of the password, correlating use with `credential_read`.
    pub fingerprint: String,
    /// Exact origins the credential may be used at (drives the redirect
    /// verdict at sequence end).
    pub allowed_origins: Vec<Origin>,
}

/// What the engine needs from the credential plane. Implemented by
/// `oxibrowser_credentials::BrokerSource`; `resolve_*` must evaluate policy
/// (deny → consent), resolve the value, and audit the read + use — values
/// appear only in the return value.
pub trait CredentialSource: Send + Sync {
    /// Authorize + resolve the account's password credential at `origin`
    /// (the login form's hosting origin — fail-closed frame semantics).
    fn resolve_login(
        &self,
        handles: &[String],
        scope: &str,
        origin: &Origin,
    ) -> std::result::Result<ResolvedLogin, SourceError>;

    /// Authorize + resolve a current TOTP code from the account's TOTP
    /// credential. [`SourceError::NotFound`] here means "the account has no
    /// TOTP credential" — an MFA escalation, not a retryable miss.
    fn resolve_totp(
        &self,
        handles: &[String],
        scope: &str,
        origin: &Origin,
    ) -> std::result::Result<String, SourceError>;

    /// Absolute login-page origins the credentials for `scope` are pinned
    /// to (`allowed_origins` — scheme + host + port preserved). Candidate
    /// discovery tries these before the derived `https://<scope>/` bases.
    /// Values are metadata, never secrets. Default: none.
    fn origin_hints(&self, _handles: &[String], _scope: &str) -> Vec<String> {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// Pure form detection (unit-tested without a browser)
// ---------------------------------------------------------------------------

/// A fillable input: CSS selector + POST field name (`name` attr, else `id`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldInput {
    pub selector: String,
    pub field_name: String,
}

/// A detected login form (main document only — iframe content is invisible
/// to the snapshot, so iframe-hosted forms fail closed by absence).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginForm {
    pub username: FieldInput,
    pub password: FieldInput,
    pub submit: FieldInput,
    /// Raw `action` attribute of the enclosing `<form>`, when present.
    pub action: Option<String>,
}

/// What the post-submit snapshot asks the engine to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MfaStep {
    /// No second step visible.
    None,
    /// A one-time-code input the account's TOTP credential can fill.
    Totp {
        input: FieldInput,
        submit: Option<FieldInput>,
        /// Raw `action` attribute of the enclosing `<form>`, when present.
        action: Option<String>,
    },
    /// A second factor on the forbidden list — never automated.
    Escalate(MfaForbidden),
}

/// Forbidden second factors (§5.3: 즉시 중단 + 상승).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MfaForbidden {
    /// Code sent by SMS / phone / text message.
    Sms,
    /// Code sent by email.
    Email,
}

impl MfaForbidden {
    pub fn as_str(self) -> &'static str {
        match self {
            MfaForbidden::Sms => "sms",
            MfaForbidden::Email => "email",
        }
    }
}

/// Strong one-time-code tokens — a text/tel/number input whose name, id,
/// placeholder, or aria-label contains one of these is treated as the OTP
/// field. `code` alone is deliberately absent (too many false positives).
const OTP_TOKENS: [&str; 11] = [
    "otp",
    "totp",
    "one-time",
    "one_time",
    "one time",
    "two-factor",
    "two_factor",
    "two factor",
    "2fa",
    "verification code",
    "authenticator",
];

/// Forbidden-factor tokens, checked before [`OTP_TOKENS`] on the same input.
const SMS_TOKENS: [&str; 5] = ["sms", "text message", "texted", "phone", "mobile"];
const EMAIL_TOKENS: [&str; 2] = ["email", "e-mail"];

/// Visible, enabled text-ish inputs with their attribute haystack (name, id,
/// placeholder, aria-label — lowercased, space-joined).
fn text_inputs(snapshot: &DomSnapshot) -> Vec<(InteractiveElement, String)> {
    let mut out = Vec::new();
    for el in snapshot.interactive_elements() {
        if el.tag != "input" || el.disabled {
            continue;
        }
        let ty = el
            .input_type
            .as_deref()
            .unwrap_or("text")
            .to_ascii_lowercase();
        if !matches!(ty.as_str(), "text" | "email" | "tel" | "number" | "") {
            continue;
        }
        let Some(node) = snapshot.nodes.get(&el.node_id) else {
            continue;
        };
        let haystack = [
            el.name_attr.as_deref(),
            node.attributes.get("id").map(String::as_str),
            el.placeholder.as_deref(),
            el.aria_label.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
        out.push((el, haystack));
    }
    out
}

fn field_name(el: &InteractiveElement, snapshot: &DomSnapshot) -> String {
    el.name_attr
        .clone()
        .or_else(|| {
            snapshot
                .nodes
                .get(&el.node_id)
                .and_then(|n| n.attributes.get("id").cloned())
        })
        .unwrap_or_else(|| el.selector.clone())
}

/// Locate a login form in a snapshot: exactly one password input (two means
/// registration — fail closed), a text/email companion, and a submit control.
pub fn detect_login_form(snapshot: &DomSnapshot) -> Option<LoginForm> {
    let elements = snapshot.interactive_elements();

    let is_password = |el: &InteractiveElement| {
        el.tag == "input" && !el.disabled && el.input_type.as_deref() == Some("password")
    };
    let password_idx: Vec<usize> = elements
        .iter()
        .enumerate()
        .filter(|(_, el)| is_password(el))
        .map(|(i, _)| i)
        .collect();
    if password_idx.len() != 1 {
        return None;
    }
    let pw_idx = password_idx[0];
    let pw = &elements[pw_idx];
    let password = FieldInput {
        selector: pw.selector.clone(),
        field_name: field_name(pw, snapshot),
    };

    // Username companion: the closest text/email input *before* the password
    // field (login order), else any text/email input at all.
    let is_username_type = |el: &InteractiveElement| {
        let ty = el
            .input_type
            .as_deref()
            .unwrap_or("text")
            .to_ascii_lowercase();
        matches!(ty.as_str(), "text" | "email" | "tel" | "")
    };
    let username_el = elements
        .iter()
        .take(pw_idx)
        .rev()
        .find(|el| el.tag == "input" && !el.disabled && is_username_type(el))
        .or_else(|| {
            elements
                .iter()
                .enumerate()
                .filter(|(i, el)| {
                    *i != pw_idx && el.tag == "input" && !el.disabled && is_username_type(el)
                })
                .map(|(_, el)| el)
                .next()
        })?;
    let username = FieldInput {
        selector: username_el.selector.clone(),
        field_name: field_name(username_el, snapshot),
    };

    let submit = detect_submit(&elements)?;
    let action = enclosing_form_action_of(snapshot, pw.node_id);

    Some(LoginForm {
        username,
        password,
        submit,
        action,
    })
}

/// Find the submit control: `input[type=submit]`, then the first usable
/// `button` (explicit `type=submit`, inviting text, or plain button role).
fn detect_submit(elements: &[InteractiveElement]) -> Option<FieldInput> {
    let mut fallback = None;
    for el in elements {
        if el.disabled {
            continue;
        }
        let ty = el.input_type.as_deref().unwrap_or("").to_ascii_lowercase();
        if el.tag == "input" && ty == "submit" {
            return Some(FieldInput {
                selector: el.selector.clone(),
                field_name: el.name_attr.clone().unwrap_or_default(),
            });
        }
        if el.tag == "button" && fallback.is_none() {
            let text = el.text.to_ascii_lowercase();
            if ty == "submit"
                || el.role == "button"
                || text.contains("sign in")
                || text.contains("log in")
                || text.contains("login")
                || text.contains("submit")
                || text.contains("continue")
            {
                fallback = Some(FieldInput {
                    selector: el.selector.clone(),
                    field_name: el.name_attr.clone().unwrap_or_default(),
                });
            }
        }
    }
    fallback
}

/// The `action` attribute of the nearest `<form>` ancestor of `node_id`.
fn enclosing_form_action_of(snapshot: &DomSnapshot, node_id: u32) -> Option<String> {
    let mut current = snapshot.nodes.get(&node_id)?.parent;
    while let Some(id) = current {
        let node = snapshot.nodes.get(&id)?;
        if node.tag.eq_ignore_ascii_case("form") {
            return node
                .attributes
                .get("action")
                .filter(|a| !a.trim().is_empty())
                .cloned();
        }
        current = node.parent;
    }
    None
}

/// Classify the post-submit page: TOTP input, forbidden factor, or nothing.
pub fn detect_mfa_step(snapshot: &DomSnapshot) -> MfaStep {
    let elements = snapshot.interactive_elements();
    for (el, haystack) in text_inputs(snapshot) {
        if SMS_TOKENS.iter().any(|t| haystack.contains(t)) {
            return MfaStep::Escalate(MfaForbidden::Sms);
        }
        if EMAIL_TOKENS.iter().any(|t| haystack.contains(t)) {
            return MfaStep::Escalate(MfaForbidden::Email);
        }
        if OTP_TOKENS.iter().any(|t| haystack.contains(t)) {
            let input = FieldInput {
                selector: el.selector.clone(),
                field_name: field_name(&el, snapshot),
            };
            return MfaStep::Totp {
                input,
                submit: detect_submit(&elements),
                action: enclosing_form_action_of(snapshot, el.node_id),
            };
        }
    }
    MfaStep::None
}

/// The URL a form posts to: the form's `action` resolved against the page
/// URL, else the page URL itself (HTML default action).
pub fn form_post_url(page_url: &Url, action: &Option<String>) -> Result<Url> {
    match action {
        Some(raw) => page_url
            .join(raw)
            .map_err(|e| account_error(format!("form action {raw:?} does not resolve: {e}"))),
        None => Ok(page_url.clone()),
    }
}

/// `application/x-www-form-urlencoded` body for the given field pairs.
pub fn form_body(pairs: &[(&str, &str)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        serializer.append_pair(k, v);
    }
    serializer.finish()
}

/// Candidate login URLs, in order: the credential `origin_hints` (scheme +
/// host + port exactly as the user pinned them at `credential put
/// --origin`), the probe URL, then the scope-root base with `/login`
/// `/signin` `/session` `/auth` — deduplicated.
pub fn login_candidates(record: &AccountRecord, hints: &[String]) -> Vec<Url> {
    let probe_url = record.probe.as_ref().and_then(|p| Url::parse(&p.url).ok());
    let hint_urls: Vec<Url> = hints
        .iter()
        .filter_map(|h| Url::parse(h).ok())
        .map(|mut u| {
            u.set_path("");
            u.set_query(None);
            u.set_fragment(None);
            u
        })
        .collect();
    let base = probe_url
        .clone()
        .map(|mut u| {
            u.set_path("");
            u.set_query(None);
            u.set_fragment(None);
            u
        })
        .or_else(|| Url::parse(&format!("https://{}/", record.scope)).ok());
    let Some(base) = base else {
        return Vec::new();
    };

    let mut out: Vec<Url> = Vec::new();
    let mut push = |u: Url| {
        if !out.iter().any(|e| e.as_str() == u.as_str()) {
            out.push(u);
        }
    };
    if let Some(u) = probe_url {
        push(u);
    }
    for hint in &hint_urls {
        push(hint.clone());
        for path in LOGIN_PATHS {
            let mut u = hint.clone();
            u.set_path(&format!("/{path}"));
            push(u);
        }
    }
    push(base.clone());
    for path in LOGIN_PATHS {
        let mut u = base.clone();
        u.set_path(&format!("/{path}"));
        push(u);
    }
    out
}

// ---------------------------------------------------------------------------
// Outcome & progress
// ---------------------------------------------------------------------------

/// How an unattended login ended.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum AgentLoginOutcome {
    /// Detector confirmed + envelope captured (`valid`).
    Captured {
        record: AccountRecord,
        /// Detector score; `None` when a restored jar probed valid without a
        /// fresh detection.
        score: Option<u32>,
    },
    /// A second factor the engine may not automate — human escalation.
    MfaEscalation {
        record: AccountRecord,
        kind: &'static str,
    },
    /// Bot-management challenge needs a human (never retried).
    Challenge {
        record: Box<AccountRecord>,
        detail: String,
    },
    /// The sequence ended outside the credential's allowed origins — capture
    /// vetoed, `policy_violation` audited.
    PolicyViolation {
        record: AccountRecord,
        origin: String,
    },
    /// Back to `needs_login` with a reason (no form, no evidence, consent…).
    NeedsLogin {
        record: AccountRecord,
        reason: String,
    },
}

impl AgentLoginOutcome {
    /// Terminal state string for events/surfaces.
    pub fn state(&self) -> &'static str {
        match self {
            AgentLoginOutcome::Captured { .. } => "captured",
            AgentLoginOutcome::MfaEscalation { .. } => "mfa_escalation",
            AgentLoginOutcome::Challenge { .. } => "challenge",
            AgentLoginOutcome::PolicyViolation { .. } => "policy_violation",
            AgentLoginOutcome::NeedsLogin { .. } => "needs_login",
        }
    }

    /// Whether the flow may be retried (escalations must not be).
    pub fn retryable(&self) -> bool {
        matches!(self, AgentLoginOutcome::NeedsLogin { .. })
    }

    /// Value-free terminal detail for events.
    pub fn detail(&self) -> Option<String> {
        match self {
            AgentLoginOutcome::Captured { score, .. } => score.map(|s| format!("score:{s}")),
            AgentLoginOutcome::MfaEscalation { kind, .. } => Some((*kind).to_string()),
            AgentLoginOutcome::Challenge { detail, .. } => Some(detail.clone()),
            AgentLoginOutcome::PolicyViolation { origin, .. } => Some(origin.clone()),
            AgentLoginOutcome::NeedsLogin { reason, .. } => Some(reason.clone()),
        }
    }
}

/// Mid-flow progress for surfaces (never carries values).
#[derive(Debug, Clone)]
pub enum AgentLoginProgress {
    Started,
    Restored,
    Fresh,
    Navigating { url: String },
    FormFound { url: String },
    CredentialsResolved { handle: String },
    Injected,
    Submitted,
    TotpInjected,
}

impl AgentLoginProgress {
    pub fn state(&self) -> &'static str {
        match self {
            AgentLoginProgress::Started => "started",
            AgentLoginProgress::Restored => "restored",
            AgentLoginProgress::Fresh => "fresh",
            AgentLoginProgress::Navigating { .. } => "navigating",
            AgentLoginProgress::FormFound { .. } => "form_found",
            AgentLoginProgress::CredentialsResolved { .. } => "credentials_resolved",
            AgentLoginProgress::Injected => "injected",
            AgentLoginProgress::Submitted => "submitted",
            AgentLoginProgress::TotpInjected => "totp_injected",
        }
    }

    fn detail(&self) -> Option<String> {
        match self {
            AgentLoginProgress::Navigating { url } | AgentLoginProgress::FormFound { url } => {
                Some(url.clone())
            }
            AgentLoginProgress::CredentialsResolved { handle } => Some(handle.clone()),
            _ => None,
        }
    }
}

/// Wiring for one unattended login run.
pub struct AgentLoginEngine {
    /// Correlation id for events (`alogin-…`), minted at construction.
    pub login_id: String,
    /// Registry + capture + audit.
    pub manager: Arc<AccountManager>,
    /// Envelope sealing keys (same provider the capture path uses).
    pub keys: Arc<dyn KeyProvider>,
    /// Credential plane (broker-backed).
    pub source: Arc<dyn CredentialSource>,
    /// Acting agent id (audit + events only).
    pub agent_id: String,
    /// Whole-flow deadline.
    pub timeout: Duration,
    /// Broadcast sink for [`AccountEvent`]s (state changes + progress); the
    /// CDP forwarders consume these. Best-effort — a closed sink never
    /// aborts the login.
    pub events: Option<broadcast::Sender<AccountEvent>>,
}

impl AgentLoginEngine {
    pub fn new(
        manager: Arc<AccountManager>,
        keys: Arc<dyn KeyProvider>,
        source: Arc<dyn CredentialSource>,
        agent_id: impl Into<String>,
    ) -> Self {
        AgentLoginEngine {
            login_id: format!("alogin-{}", uuid::Uuid::new_v4()),
            manager,
            keys,
            source,
            agent_id: agent_id.into(),
            timeout: DEFAULT_AGENT_LOGIN_TIMEOUT,
            events: None,
        }
    }

    /// Override the whole-flow deadline.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Attach an event sink (orchestrator broadcast or a fresh channel).
    pub fn with_events(mut self, events: broadcast::Sender<AccountEvent>) -> Self {
        self.events = Some(events);
        self
    }

    /// Run the unattended login for `account_id` on a fresh session in the
    /// account context. The account must be in `needs_login` / `stale`.
    pub async fn login(
        &self,
        account_id: &str,
        session: &mut Session,
    ) -> Result<AgentLoginOutcome> {
        let record = self.manager.registry().get(account_id)?;
        let mut record = record;
        match record.state {
            AccountState::NeedsLogin | AccountState::Stale => {}
            AccountState::Valid => {
                return Err(account_error(format!(
                    "account {account_id} is valid — run `account logout` before re-login"
                )));
            }
            // Crash recovery: `logging_in` persists in account.json, but the
            // process that owned the window is gone (each CLI/engine run is
            // a fresh process). Adopt the orphaned window instead of
            // bricking the account (audit says who and why).
            AccountState::LoggingIn => {
                self.manager.set_state_locked(
                    account_id,
                    AccountState::NeedsLogin,
                    Some("recovered_orphaned_login_window".to_string()),
                )?;
                record = self.manager.registry().get(account_id)?;
            }
            other => {
                return Err(account_error(format!(
                    "account {account_id} cannot start an agent login from state {}",
                    other.as_str()
                )));
            }
        }

        self.report(account_id, AgentLoginProgress::Started);
        self.enter_logging_in(account_id, &record)?;

        let deadline = Instant::now() + self.timeout;
        let outcome = self.run_inner(account_id, session, &record, deadline).await;
        let (state, detail) = match &outcome {
            Ok(outcome) => (outcome.state(), outcome.detail()),
            Err(e) => ("failed", Some(e.to_string())),
        };
        self.publish(AccountEvent::AgentLogin {
            login_id: self.login_id.clone(),
            account_id: account_id.to_string(),
            agent_id: self.agent_id.clone(),
            state,
            detail,
        });
        outcome
    }

    async fn run_inner(
        &self,
        account_id: &str,
        session: &mut Session,
        record: &AccountRecord,
        deadline: Instant,
    ) -> Result<AgentLoginOutcome> {
        let detector = LoginDetector::new(record.scope.clone())?;

        // -- 1. scope jar restore ------------------------------------------
        match self.restore_jar(account_id, session, record).await? {
            JarRestore::Valid { record } => {
                self.report(account_id, AgentLoginProgress::Restored);
                return Ok(AgentLoginOutcome::Captured {
                    record: *record,
                    score: None,
                });
            }
            JarRestore::Challenge { record, detail } => {
                return Ok(AgentLoginOutcome::Challenge { record, detail });
            }
            JarRestore::Fresh => {
                self.report(account_id, AgentLoginProgress::Fresh);
            }
        }
        // Baseline AFTER the restore: restored cookies are the floor — only
        // login-produced state counts as new evidence (§4.3).
        let baseline = detector.snapshot_session(session);

        // -- 2. find the login page ----------------------------------------
        let (mut form, mut form_url) = match self.find_login_page(session, record, deadline).await {
            PageProbe::Found(found) => found,
            PageProbe::Challenge { record, detail } => {
                return Ok(AgentLoginOutcome::Challenge { record, detail });
            }
            PageProbe::None => {
                let reason = "login form not found".to_string();
                let record = self.revert_to_needs_login(account_id, &reason)?;
                return Ok(AgentLoginOutcome::NeedsLogin { record, reason });
            }
        };
        let page_origin = Origin::parse(form_url.as_str()).map_err(|e| {
            account_error(format!("login page {form_url} has no usable origin: {e}"))
        })?;

        // -- 3./4. origin gate + authorize + resolve (inside the source) ---
        let resolved =
            match self
                .source
                .resolve_login(&record.credentials, &record.scope, &page_origin)
            {
                Ok(r) => r,
                Err(err) => return Ok(self.abort_on_source_error(account_id, err, "login")),
            };
        self.report(
            account_id,
            AgentLoginProgress::CredentialsResolved {
                handle: resolved.handle.clone(),
            },
        );
        self.audit_credential(
            account_id,
            &resolved.handle,
            &resolved.fingerprint,
            "resolve",
            format!(
                "agent={} scope={} origin={}",
                self.agent_id, record.scope, page_origin
            ),
        );

        // Some username must exist to fill the form.
        let username = match resolved
            .username
            .clone()
            .or_else(|| record.identity.login_hint.clone())
        {
            Some(u) => u,
            None => {
                let reason =
                    "no username available (credential login hint and account record both empty)"
                        .to_string();
                let record = self.revert_to_needs_login(account_id, &reason)?;
                return Ok(AgentLoginOutcome::NeedsLogin { record, reason });
            }
        };

        // -- 5./6. inject + submit (+ TOTP), at most MAX_SUBMIT_ATTEMPTS ---
        let mut logged_in = false;
        let mut verdict_score: Option<u32> = None;
        for attempt in 1..=MAX_SUBMIT_ATTEMPTS {
            if Instant::now() >= deadline {
                break;
            }
            if attempt > 1 {
                // A failed submit usually re-renders the form — re-detect.
                match self.snapshot(session).await.as_ref() {
                    Some(snapshot) => {
                        if let Some(next) = detect_login_form(snapshot) {
                            form = next;
                            if let Some(url) = session.current_url() {
                                form_url = url.clone();
                            }
                        } else {
                            break;
                        }
                    }
                    None => break,
                }
            }
            let post_url = match form_post_url(&form_url, &form.action) {
                Ok(u) => u,
                Err(e) => {
                    let record = self.revert_to_needs_login(account_id, &e.to_string())?;
                    return Ok(AgentLoginOutcome::NeedsLogin {
                        record,
                        reason: e.to_string(),
                    });
                }
            };

            // Injection: values only ever flow session-ward.
            for (field, value) in [
                (&form.username, username.as_str()),
                (&form.password, resolved.password.as_str()),
            ] {
                let js = crate::js::form::js_fill(&field.selector, value);
                if let Err(e) = session.evaluate_js(&js).await {
                    let reason = format!("credential injection failed: {e}");
                    let record = self.revert_to_needs_login(account_id, &reason)?;
                    return Ok(AgentLoginOutcome::NeedsLogin { record, reason });
                }
            }
            self.report(account_id, AgentLoginProgress::Injected);
            self.audit_credential(
                account_id,
                &resolved.handle,
                &resolved.fingerprint,
                "inject",
                format!("agent={} attempt={attempt}", self.agent_id),
            );

            // Submit at the HTTP layer (shared jar, redirect-following) —
            // the pure-Rust page does not execute form-submission navigation.
            let body = form_body(&[
                (form.username.field_name.as_str(), username.as_str()),
                (
                    form.password.field_name.as_str(),
                    resolved.password.as_str(),
                ),
            ]);
            if let Err(e) = session
                .post(
                    post_url.as_str(),
                    &body,
                    "application/x-www-form-urlencoded",
                )
                .await
            {
                let reason = format!("login submit failed: {e}");
                let record = self.revert_to_needs_login(account_id, &reason)?;
                return Ok(AgentLoginOutcome::NeedsLogin { record, reason });
            }
            self.report(account_id, AgentLoginProgress::Submitted);
            self.settle_dom(session).await;

            // -- TOTP step ------------------------------------------------
            let Some(snapshot) = self.snapshot(session).await else {
                break;
            };
            match detect_mfa_step(&snapshot) {
                MfaStep::Escalate(kind) => {
                    let reason = format!("mfa_escalation:{}", kind.as_str());
                    let record = self.revert_to_needs_login(account_id, &reason)?;
                    return Ok(AgentLoginOutcome::MfaEscalation {
                        record,
                        kind: kind.as_str(),
                    });
                }
                MfaStep::Totp {
                    input,
                    submit,
                    action,
                } => {
                    let code = match self.source.resolve_totp(
                        &record.credentials,
                        &record.scope,
                        &page_origin,
                    ) {
                        Ok(c) => c,
                        // Second factor detected, no TOTP credential — a
                        // human must complete the step (never retried).
                        Err(SourceError::NotFound { reason }) => {
                            let reason = format!("mfa_escalation:missing_totp:{reason}");
                            let record = self.revert_to_needs_login(account_id, &reason)?;
                            return Ok(AgentLoginOutcome::MfaEscalation {
                                record,
                                kind: "missing_totp",
                            });
                        }
                        Err(err) => {
                            return Ok(self.abort_on_source_error(account_id, err, "totp"));
                        }
                    };
                    let js = crate::js::form::js_fill(&input.selector, &code);
                    if let Err(e) = session.evaluate_js(&js).await {
                        let reason = format!("totp injection failed: {e}");
                        let record = self.revert_to_needs_login(account_id, &reason)?;
                        return Ok(AgentLoginOutcome::NeedsLogin { record, reason });
                    }
                    // The one-time-code form posts through the same jar.
                    if let Ok(totp_url) = form_post_url(&form_url, &action) {
                        let totp_body = form_body(&[(input.field_name.as_str(), code.as_str())]);
                        let _ = session
                            .post(
                                totp_url.as_str(),
                                &totp_body,
                                "application/x-www-form-urlencoded",
                            )
                            .await;
                        let _ = submit;
                    }
                    self.report(account_id, AgentLoginProgress::TotpInjected);
                }
                MfaStep::None => {}
            }

            // -- 7. judge --------------------------------------------------
            // Judge against the DOM this loop just settled on — a second
            // internal snapshot read can lag the render pipeline by a page.
            let dom_html = self.snapshot(session).await.map(|s| s.to_html());
            let input = detector.input_from_session(session, &baseline, dom_html, false);
            let verdict = detector.assess_input(&input);
            self.audit_flow(
                account_id,
                "detect",
                format!(
                    "agent={} attempt={} score={} signals={}",
                    self.agent_id,
                    attempt,
                    verdict.score,
                    verdict.signals.len()
                ),
            );
            if verdict.logged_in {
                logged_in = true;
                verdict_score = Some(verdict.score);
                break;
            }
        }

        if !logged_in {
            let reason = "no login evidence after submit (detector below threshold)".to_string();
            let record = self.revert_to_needs_login(account_id, &reason)?;
            return Ok(AgentLoginOutcome::NeedsLogin { record, reason });
        }

        // -- 8. redirect verdict on the sequence's final URL ----------------
        let Some(final_url) = session.current_url().cloned() else {
            let reason = "no final URL after login".to_string();
            let record = self.revert_to_needs_login(account_id, &reason)?;
            return Ok(AgentLoginOutcome::NeedsLogin { record, reason });
        };
        let final_origin = Origin::parse(final_url.as_str()).map_err(|e| {
            account_error(format!("final URL {final_url} has no usable origin: {e}"))
        })?;
        if OriginPolicy::default().redirect_verdict(&resolved.allowed_origins, &final_origin)
            == RedirectVerdict::InvalidateAndEscalate
        {
            self.audit_policy_violation(
                account_id,
                &resolved.handle,
                format!(
                    "final origin {} outside credential allowlist (agent={})",
                    final_origin, self.agent_id
                ),
            );
            let reason = format!("redirect_policy_violation:{}", final_origin);
            let record = self.revert_to_needs_login(account_id, &reason)?;
            return Ok(AgentLoginOutcome::PolicyViolation {
                record,
                origin: final_origin.as_str(),
            });
        }

        // -- capture --------------------------------------------------------
        let record = self.capture(account_id, session)?;
        Ok(AgentLoginOutcome::Captured {
            record,
            score: verdict_score,
        })
    }

    // -- phases -------------------------------------------------------------

    /// Try the scope jar: restore + probe. `Valid` ends the flow early.
    async fn restore_jar(
        &self,
        account_id: &str,
        session: &mut Session,
        record: &AccountRecord,
    ) -> Result<JarRestore> {
        // P4: loading the envelope does synchronous keychain + file IO —
        // run it on the blocking pool so the tokio worker driving this
        // engine never stalls. Injection is in-memory and stays inline.
        let fingerprint = self.current_fingerprint(session);
        let loaded = {
            let manager = Arc::clone(&self.manager);
            let keys = Arc::clone(&self.keys);
            let account_id = account_id.to_string();
            tokio::task::spawn_blocking(move || {
                manager.load_envelope(&account_id, keys.as_ref(), Some(&fingerprint))
            })
            .await
            .map_err(|e| CoreError::SessionError(format!("session_restore task failed: {e}")))?
        };
        let envelope = match loaded {
            Ok(envelope) => envelope,
            // No jar / fingerprint drift → fresh login (fail-open into the
            // credential-gated path, never into trust).
            Err(_) => return Ok(JarRestore::Fresh),
        };
        let envelope = match self.manager.inject_envelope(account_id, session, envelope) {
            Ok(envelope) => envelope,
            // Same fail-open contract as the former whole-restore call.
            Err(_) => return Ok(JarRestore::Fresh),
        };
        self.audit_flow(
            account_id,
            "restore",
            format!(
                "agent={} cookies={} origins={}",
                self.agent_id,
                envelope.state.cookies.len(),
                envelope.state.origins.len()
            ),
        );

        let probe = ValidationProbe::from_record(record.probe.as_ref(), &record.scope)?;
        let outcome = probe.run(session.http_client().as_ref()).await;
        match &outcome.verdict {
            ProbeVerdict::Valid => {
                let record = self.capture(account_id, session)?;
                Ok(JarRestore::Valid {
                    record: Box::new(record),
                })
            }
            ProbeVerdict::Challenge { challenge } => {
                let detail = format!(
                    "challenge:{}:{}",
                    challenge.vendor.as_str(),
                    challenge_kind_str(&challenge.kind)
                );
                let record = self.mark_challenge(account_id, &detail)?;
                Ok(JarRestore::Challenge {
                    record: Box::new(record),
                    detail,
                })
            }
            // Invalid (stale jar) / Unreachable (transport is not evidence —
            // §4.4): fall through to the credential-gated fresh login.
            ProbeVerdict::Invalid { .. } | ProbeVerdict::Unreachable { .. } => {
                Ok(JarRestore::Fresh)
            }
        }
    }

    /// Try candidate URLs until one yields a login form. Challenge
    /// interstitials escalate (never retried); transport errors move on.
    async fn find_login_page(
        &self,
        session: &mut Session,
        record: &AccountRecord,
        deadline: Instant,
    ) -> PageProbe {
        let hints = self.source.origin_hints(&record.credentials, &record.scope);
        for url in login_candidates(record, &hints) {
            if Instant::now() >= deadline {
                break;
            }
            self.report(
                &record.account_id,
                AgentLoginProgress::Navigating {
                    url: url.as_str().to_string(),
                },
            );
            if session.navigate(url.as_str()).await.is_err() {
                continue;
            }
            if let Some(snapshot) = self.snapshot(session).await
                && let Some(form) = detect_login_form(&snapshot)
            {
                self.report(
                    &record.account_id,
                    AgentLoginProgress::FormFound {
                        url: url.as_str().to_string(),
                    },
                );
                return PageProbe::Found((form, url));
            }
            // No form here — is this a bot-management interstitial? An
            // un-cleared Interactive/Blocked challenge escalates at once.
            if let Ok(ChallengeOutcome {
                challenge: Some(c), ..
            }) = session
                .http_client()
                .fetch_with_challenge_retry(&url, 1)
                .await
                && matches!(c.kind, ChallengeKind::Interactive | ChallengeKind::Blocked)
            {
                let detail = format!(
                    "challenge:{}:{}",
                    c.vendor.as_str(),
                    challenge_kind_str(&c.kind)
                );
                if let Ok(record) = self.mark_challenge(&record.account_id, &detail) {
                    return PageProbe::Challenge {
                        record: Box::new(record),
                        detail,
                    };
                }
                return PageProbe::None;
            }
        }
        PageProbe::None
    }

    // -- small helpers ------------------------------------------------------

    async fn snapshot(&self, session: &mut Session) -> Option<DomSnapshot> {
        session.dom_snapshot().await.ok().flatten()
    }

    /// Give the render pipeline a bounded window to settle after a
    /// navigation-producing submit: wait until two consecutive snapshots
    /// agree (the document handoff to the JS thread can lag one page behind
    /// on a loaded scheduler). Bounded to ~0.5 s; wiremock-grade pages
    /// settle on the first iteration.
    async fn settle_dom(&self, session: &mut Session) {
        let mut last: Option<String> = None;
        for _ in 0..10 {
            let Some(html) = self.snapshot(session).await.map(|s| s.to_html()) else {
                return;
            };
            if last.as_deref() == Some(html.as_str()) {
                return;
            }
            last = Some(html);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn current_fingerprint(&self, session: &Session) -> FingerprintMeta {
        FingerprintMeta {
            user_agent: session.effective_ua(),
            ..FingerprintMeta::default()
        }
    }

    fn enter_logging_in(&self, account_id: &str, record: &AccountRecord) -> Result<()> {
        let from = record.state;
        let updated = self.manager.set_state_locked(
            account_id,
            AccountState::LoggingIn,
            Some(format!("login_started:agent:{}", self.agent_id)),
        )?;
        self.manager
            .emit_state(account_id, from, updated.state, "login_started:agent");
        self.publish(AccountEvent::StateChanged {
            account_id: account_id.to_string(),
            from: from.as_str().into(),
            to: updated.state.as_str().into(),
            detail: Some(format!("login_started:agent:{}", self.agent_id)),
        });
        Ok(())
    }

    /// Probe-confirmed capture (`logging_in` → `valid`, audited in the
    /// manager, event published here).
    fn capture(&self, account_id: &str, session: &Session) -> Result<AccountRecord> {
        let from = AccountState::LoggingIn;
        let record = self
            .manager
            .capture_session(account_id, session, self.keys.as_ref())?;
        self.audit_flow(
            account_id,
            "capture",
            format!(
                "agent={} cookies={} origins={}",
                self.agent_id,
                record.session_summary.cookie_count,
                record.session_summary.origins.len()
            ),
        );
        self.publish(AccountEvent::StateChanged {
            account_id: account_id.to_string(),
            from: from.as_str().into(),
            to: record.state.as_str().into(),
            detail: Some(format!("agent_login_captured:{}", self.agent_id)),
        });
        Ok(record)
    }

    /// Failure exit from `logging_in` (§4.2: `Valid` | `NeedsLogin` only).
    fn revert_to_needs_login(&self, account_id: &str, detail: &str) -> Result<AccountRecord> {
        let from = AccountState::LoggingIn;
        let record = self.manager.set_state_locked(
            account_id,
            AccountState::NeedsLogin,
            Some(detail.to_string()),
        )?;
        self.manager
            .emit_state(account_id, from, record.state, detail);
        self.publish(AccountEvent::StateChanged {
            account_id: account_id.to_string(),
            from: from.as_str().into(),
            to: record.state.as_str().into(),
            detail: Some(detail.to_string()),
        });
        Ok(record)
    }

    fn mark_challenge(&self, account_id: &str, detail: &str) -> Result<AccountRecord> {
        let from = AccountState::LoggingIn;
        let record = self.manager.set_state_locked(
            account_id,
            AccountState::Challenge,
            Some(detail.to_string()),
        )?;
        self.manager
            .emit_state(account_id, from, record.state, detail);
        self.publish(AccountEvent::StateChanged {
            account_id: account_id.to_string(),
            from: from.as_str().into(),
            to: record.state.as_str().into(),
            detail: Some(detail.to_string()),
        });
        Ok(record)
    }

    /// Map a credential-source failure onto the matching flow abort.
    fn abort_on_source_error(
        &self,
        account_id: &str,
        err: SourceError,
        stage: &str,
    ) -> AgentLoginOutcome {
        let (reason, mfa_missing) = match &err {
            SourceError::ConsentRequired { reason } => {
                (format!("consent_required:{reason}"), false)
            }
            SourceError::OriginMismatch { reason } => (format!("origin_mismatch:{reason}"), false),
            SourceError::NotFound { reason } => {
                (format!("no_credential:{reason}"), stage == "totp")
            }
            SourceError::Unavailable { reason } => {
                (format!("credentials_unavailable:{reason}"), false)
            }
        };
        let record = self
            .revert_to_needs_login(account_id, &reason)
            .unwrap_or_else(|_| {
                // The account existed at flow start; the registry read cannot
                // fail here short of concurrent revocation.
                self.manager
                    .registry()
                    .get(account_id)
                    .expect("account vanished mid-login")
            });
        if mfa_missing {
            AgentLoginOutcome::MfaEscalation {
                record,
                kind: "missing_totp",
            }
        } else {
            AgentLoginOutcome::NeedsLogin { record, reason }
        }
    }

    fn audit_flow(&self, account_id: &str, action: &str, reason: String) {
        audit::record(AuditEvent {
            action: Some(format!("agent_login_{action}")),
            ..audit::event(
                AuditEventKind::AccountUse,
                AuditDecision::Allow,
                format!("account={account_id} {reason}"),
            )
        });
    }

    /// Handle + fingerprint only — never a value.
    fn audit_credential(
        &self,
        account_id: &str,
        handle: &str,
        fingerprint: &str,
        stage: &str,
        reason: String,
    ) {
        let event = AuditEvent {
            credential: Some(CredentialRef {
                id: handle.to_string(),
                fingerprint: fingerprint.to_string(),
            }),
            action: Some(format!("agent_login_{stage}")),
            ..audit::event(
                AuditEventKind::CredentialUse,
                AuditDecision::Allow,
                format!("account={account_id} {reason}"),
            )
        };
        audit::record(event);
    }

    fn audit_policy_violation(&self, account_id: &str, handle: &str, reason: String) {
        let event = AuditEvent {
            credential: Some(CredentialRef {
                id: handle.to_string(),
                fingerprint: String::new(),
            }),
            action: Some("agent_login".to_string()),
            ..audit::event(
                AuditEventKind::PolicyViolation,
                AuditDecision::Deny,
                format!("account={account_id} {reason}"),
            )
        };
        audit::record(event);
    }

    /// Progress → broadcast (best-effort; the login proceeds regardless).
    fn report(&self, account_id: &str, progress: AgentLoginProgress) {
        if let Some(tx) = &self.events {
            let _ = tx.send(AccountEvent::AgentLogin {
                login_id: self.login_id.clone(),
                account_id: account_id.to_string(),
                agent_id: self.agent_id.clone(),
                state: progress.state(),
                detail: progress.detail(),
            });
        }
        tracing::info!(
            account = %account_id,
            agent = %self.agent_id,
            step = progress.state(),
            "agent login progress"
        );
    }

    fn publish(&self, event: AccountEvent) {
        if let Some(tx) = &self.events {
            let _ = tx.send(event);
        }
    }
}

/// Restore-phase result.
enum JarRestore {
    /// Restored jar probed valid — re-captured, no credentials used.
    Valid { record: Box<AccountRecord> },
    /// Challenge classified during the restore probe.
    Challenge {
        record: Box<AccountRecord>,
        detail: String,
    },
    /// Fresh login required.
    Fresh,
}

/// Login-page discovery result.
#[allow(clippy::large_enum_variant)] // Found's (LoginForm, Url) dominates; cold path
enum PageProbe {
    Found((LoginForm, Url)),
    /// Challenge interstitial (account already moved to `challenge`).
    Challenge {
        record: Box<AccountRecord>,
        detail: String,
    },
    /// No candidate page carried a login form.
    None,
}

fn challenge_kind_str(kind: &ChallengeKind) -> &'static str {
    match kind {
        ChallengeKind::Managed => "managed",
        ChallengeKind::JsCheck => "js_check",
        ChallengeKind::Interactive => "interactive",
        ChallengeKind::Blocked => "blocked",
        ChallengeKind::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BrowserConfig;
    use crate::js::dom_snapshot::DomNode;
    use crate::security::audit::AuditLog;
    use crate::storage::session_store::StaticKeyProvider;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::sync::RwLock;

    // -- snapshot builders (pure detection tests) ---------------------------

    /// Minimal DOM tree builder over [`DomSnapshot`].
    struct Doc {
        snap: DomSnapshot,
        next: u32,
    }

    impl Doc {
        fn new() -> Self {
            let mut snap = DomSnapshot::empty();
            snap.root_id = 0;
            snap.nodes.insert(
                0,
                DomNode {
                    id: 0,
                    tag: "#document".into(),
                    attributes: HashMap::new(),
                    text_content: String::new(),
                    children: vec![1],
                    parent: None,
                    node_type: 9,
                },
            );
            snap.nodes.insert(
                1,
                DomNode {
                    id: 1,
                    tag: "html".into(),
                    attributes: HashMap::new(),
                    text_content: String::new(),
                    children: vec![2],
                    parent: Some(0),
                    node_type: 1,
                },
            );
            snap.nodes.insert(
                2,
                DomNode {
                    id: 2,
                    tag: "body".into(),
                    attributes: HashMap::new(),
                    text_content: String::new(),
                    children: Vec::new(),
                    parent: Some(1),
                    node_type: 1,
                },
            );
            Doc { snap, next: 3 }
        }

        fn add(&mut self, tag: &str, attrs: &[(&str, &str)], parent: u32, text: &str) -> u32 {
            let id = self.next;
            self.next += 1;
            self.snap.nodes.insert(
                id,
                DomNode {
                    id,
                    tag: tag.into(),
                    attributes: attrs
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                    text_content: text.into(),
                    children: Vec::new(),
                    parent: Some(parent),
                    node_type: 1,
                },
            );
            self.snap.nodes.get_mut(&parent).unwrap().children.push(id);
            id
        }

        fn form(mut self, inner: impl FnOnce(&mut Doc, u32)) -> DomSnapshot {
            let form = self.add("form", &[("action", "/do-login")], 2, "");
            inner(&mut self, form);
            self.snap
        }
    }

    fn login_doc() -> DomSnapshot {
        Doc::new().form(|doc, form| {
            doc.add(
                "input",
                &[("type", "text"), ("name", "user"), ("id", "user")],
                form,
                "",
            );
            doc.add(
                "input",
                &[("type", "password"), ("name", "pass"), ("id", "pass")],
                form,
                "",
            );
            doc.add("button", &[("type", "submit")], form, "Sign in");
        })
    }

    #[test]
    fn login_form_detected_with_companion_and_submit() {
        let snap = login_doc();
        let form = detect_login_form(&snap).expect("login form detected");
        assert_eq!(form.username.field_name, "user");
        assert_eq!(form.password.field_name, "pass");
        assert_eq!(form.action.as_deref(), Some("/do-login"));
        assert!(
            form.submit.selector.contains("button"),
            "submit is the button"
        );
    }

    #[test]
    fn password_only_or_registration_forms_fail_closed() {
        // No username companion.
        let snap = Doc::new().form(|doc, form| {
            doc.add("input", &[("type", "password"), ("name", "pass")], form, "");
            doc.add("button", &[("type", "submit")], form, "Sign in");
        });
        assert!(
            detect_login_form(&snap).is_none(),
            "password alone is not a login form"
        );

        // Two password fields → registration.
        let snap = Doc::new().form(|doc, form| {
            doc.add("input", &[("type", "text"), ("name", "user")], form, "");
            doc.add("input", &[("type", "password"), ("name", "pass")], form, "");
            doc.add(
                "input",
                &[("type", "password"), ("name", "confirm")],
                form,
                "",
            );
            doc.add("button", &[("type", "submit")], form, "Register");
        });
        assert!(
            detect_login_form(&snap).is_none(),
            "two password fields fail closed"
        );
    }

    #[test]
    fn iframe_hosted_form_is_invisible() {
        // A real main-document snapshot never contains the iframe's content
        // tree — the iframe is a leaf. Form detection must not conjure one.
        let snap = Doc::new().form(|doc, form| {
            doc.add(
                "iframe",
                &[("src", "https://accounts.example/login")],
                form,
                "",
            );
        });
        assert!(
            detect_login_form(&snap).is_none(),
            "iframe form fails closed"
        );
    }

    #[test]
    fn mfa_step_detection_matches_totp_and_forbidden_tokens() {
        let totp = Doc::new().form(|doc, form| {
            doc.add(
                "input",
                &[
                    ("type", "text"),
                    ("name", "otp"),
                    ("aria-label", "One-time code"),
                ],
                form,
                "",
            );
            doc.add("button", &[("type", "submit")], form, "Verify");
        });
        match detect_mfa_step(&totp) {
            MfaStep::Totp { input, submit, .. } => {
                assert_eq!(input.field_name, "otp");
                assert!(submit.is_some());
            }
            other => panic!("expected Totp step, got {other:?}"),
        }

        // two-factor via aria-label, no explicit name.
        let two_fa = Doc::new().form(|doc, form| {
            doc.add(
                "input",
                &[("type", "text"), ("placeholder", "2FA code")],
                form,
                "",
            );
        });
        assert!(matches!(detect_mfa_step(&two_fa), MfaStep::Totp { .. }));

        let sms = Doc::new().form(|doc, form| {
            doc.add(
                "input",
                &[
                    ("type", "text"),
                    ("name", "code"),
                    ("aria-label", "Code sent by SMS"),
                ],
                form,
                "",
            );
        });
        assert_eq!(detect_mfa_step(&sms), MfaStep::Escalate(MfaForbidden::Sms));

        let email = Doc::new().form(|doc, form| {
            doc.add(
                "input",
                &[
                    ("type", "text"),
                    ("name", "email_code"),
                    ("placeholder", "check your email"),
                ],
                form,
                "",
            );
        });
        assert_eq!(
            detect_mfa_step(&email),
            MfaStep::Escalate(MfaForbidden::Email)
        );

        // Ordinary text input: nothing.
        let plain = Doc::new().form(|doc, form| {
            doc.add(
                "input",
                &[("type", "text"), ("name", "q"), ("placeholder", "Search")],
                form,
                "",
            );
        });
        assert_eq!(detect_mfa_step(&plain), MfaStep::None);
    }

    #[test]
    fn login_candidates_order_and_dedup() {
        let mut record = AccountRecord::new("gh", "example.com").unwrap();
        record.probe = Some(super::super::record::ProbeConfig {
            url: "https://example.com/settings".into(),
            marker: None,
        });
        let urls = login_candidates(&record, &[]);
        let strs: Vec<&str> = urls.iter().map(|u| u.as_str()).collect();
        assert_eq!(strs[0], "https://example.com/settings", "probe URL first");
        assert_eq!(strs[1], "https://example.com/", "scope root second");
        assert_eq!(strs[2], "https://example.com/login");
        assert_eq!(strs[3], "https://example.com/signin");
        assert_eq!(strs[4], "https://example.com/session");
        assert_eq!(strs[5], "https://example.com/auth");
        // Probe URL was /settings — no duplicates anywhere.
        assert_eq!(
            strs.len(),
            strs.iter().collect::<std::collections::HashSet<_>>().len()
        );
    }

    #[test]
    fn login_candidates_prefer_pinned_origin_hints() {
        let record = AccountRecord::new("local", "127.0.0.1").unwrap();
        // A pinned origin (scheme + port preserved) beats the derived
        // https://<scope>/ base and carries the port through.
        let urls = login_candidates(&record, &["http://127.0.0.1:9344".to_string()]);
        let strs: Vec<&str> = urls.iter().map(|u| u.as_str()).collect();
        assert_eq!(strs[0], "http://127.0.0.1:9344/", "hint base first");
        assert_eq!(strs[1], "http://127.0.0.1:9344/login", "hint paths follow");
        // Derived https base is still present as fallback.
        assert!(strs.contains(&"https://127.0.0.1/"));
        assert_eq!(
            strs.len(),
            strs.iter().collect::<std::collections::HashSet<_>>().len()
        );
    }

    #[test]
    fn form_body_is_urlencoded() {
        let body = form_body(&[("user", "a&b=c"), ("pass", "p w")]);
        assert_eq!(body, "user=a%26b%3Dc&pass=p+w");
    }

    #[test]
    fn form_post_url_resolves_action_against_page() {
        let page = Url::parse("https://example.com/login").unwrap();
        assert_eq!(
            form_post_url(&page, &Some("/auth".into()))
                .unwrap()
                .as_str(),
            "https://example.com/auth"
        );
        assert_eq!(
            form_post_url(&page, &None).unwrap().as_str(),
            "https://example.com/login"
        );
    }

    // -- engine flow (wiremock) ----------------------------------------------

    fn temp_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "oxi-agent-login-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Test [`CredentialSource`]: fixed values, configurable failure mode.
    struct TestSource {
        username: Option<String>,
        password: Option<String>,
        /// Exact allowed origins reported on the resolved login.
        allowed: Vec<String>,
        totp: Option<String>,
        deny: Option<SourceError>,
    }

    impl TestSource {
        fn ok(origin: String) -> Self {
            TestSource {
                username: Some("garden".into()),
                password: Some("hunter2".into()),
                allowed: vec![origin],
                totp: None,
                deny: None,
            }
        }

        fn deny() -> Self {
            TestSource {
                username: None,
                password: None,
                allowed: vec![],
                totp: None,
                deny: Some(SourceError::ConsentRequired {
                    reason: "no grant".into(),
                }),
            }
        }
    }

    impl CredentialSource for TestSource {
        fn resolve_login(
            &self,
            _handles: &[String],
            _scope: &str,
            _origin: &Origin,
        ) -> std::result::Result<ResolvedLogin, SourceError> {
            if let Some(err) = &self.deny {
                return Err(err.clone());
            }
            Ok(ResolvedLogin {
                handle: "kch:test/scope/password/main".into(),
                username: self.username.clone(),
                password: Zeroizing::new(self.password.clone().unwrap()),
                fingerprint: "sha256:deadbeef".into(),
                allowed_origins: self
                    .allowed
                    .iter()
                    .map(|o| Origin::parse(o).unwrap())
                    .collect(),
            })
        }

        fn resolve_totp(
            &self,
            _handles: &[String],
            _scope: &str,
            _origin: &Origin,
        ) -> std::result::Result<String, SourceError> {
            self.totp.clone().ok_or_else(|| SourceError::NotFound {
                reason: "no totp credential".into(),
            })
        }
    }

    const LOGIN_FORM: &str = r#"<html><body>
<form action="/do-login">
  <input type="text" name="user" id="user">
  <input type="password" name="pass" id="pass">
  <button type="submit">Sign in</button>
</form>
</body></html>"#;

    const SIGNED_IN: &str = r#"<html><head><meta name="user-login" content="garden"></head>
<body><h1>Signed in</h1></body></html>"#;

    const MFA_SMS: &str = r#"<html><body>
<form action="/verify">
  <input type="text" name="code" aria-label="Code sent by SMS">
  <button type="submit">Verify</button>
</form>
</body></html>"#;

    /// Login-flow test server: `/` shows the login form (or the signed-in
    /// page once `user_session` is in the jar), `/do-login` accepts the form
    /// POST and sets the session cookie, `/mfa` is a forbidden SMS step.
    async fn login_flow_server() -> wiremock::MockServer {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/do-login"))
            .respond_with(
                wiremock::ResponseTemplate::new(303)
                    .insert_header("Location", "/")
                    .insert_header(
                        "Set-Cookie",
                        "user_session=oxy123abc; Path=/; HttpOnly; Secure",
                    ),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/mfa"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(MFA_SMS))
            .mount(&server)
            .await;
        // `/` inspects the Cookie header: session → signed-in page.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/"))
            .respond_with(|req: &wiremock::Request| {
                let has_session = req
                    .headers
                    .get("cookie")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|c| c.contains("user_session="));
                let body = if has_session { SIGNED_IN } else { LOGIN_FORM };
                wiremock::ResponseTemplate::new(200).set_body_string(body)
            })
            .mount(&server)
            .await;
        server
    }

    struct Harness {
        manager: Arc<AccountManager>,
        browser: Arc<crate::Browser>,
        keys: Arc<dyn KeyProvider>,
        base: PathBuf,
    }

    async fn harness(tag: &str) -> Harness {
        let base = temp_dir(tag);
        let audit = Arc::new(AuditLog::open(base.join("audit.jsonl")).unwrap());
        let registry = crate::account::AccountRegistry::open(base.join("accounts")).unwrap();
        let manager = Arc::new(AccountManager::with_audit(registry, audit));
        let mut config = BrowserConfig::headless();
        config.enable_ssrf_filter = false; // loopback wiremock
        let browser = Arc::new(crate::Browser::new(config).await.unwrap());
        Harness {
            manager,
            browser,
            keys: Arc::new(StaticKeyProvider::new([7u8; 32])),
            base,
        }
    }

    impl Harness {
        fn add_account(&self, id: &str, probe_url: &str) -> AccountRecord {
            let mut record = AccountRecord::new(id, "127.0.0.1").unwrap();
            record.probe = Some(super::super::record::ProbeConfig {
                url: probe_url.to_string(),
                marker: Some("Signed in".into()),
            });
            self.manager.registry().add(record.clone()).unwrap();
            record
        }

        async fn session(
            &self,
            account_id: &str,
        ) -> (Arc<crate::context::BrowserContext>, Arc<RwLock<Session>>) {
            let ctx = self
                .browser
                .new_context(crate::context::ContextConfig {
                    label: Some(format!("account:{account_id}")),
                    proxy: None,
                })
                .unwrap();
            ctx.set_credential_mode(true);
            let session = self.browser.new_session_in(&ctx).await.unwrap();
            (ctx, session)
        }

        async fn run(
            &self,
            account_id: &str,
            source: Arc<dyn CredentialSource>,
            session: &mut Session,
        ) -> AgentLoginOutcome {
            let engine = AgentLoginEngine::new(
                Arc::clone(&self.manager),
                Arc::clone(&self.keys),
                source,
                "test-agent",
            )
            .with_timeout(Duration::from_secs(30));
            engine.login(account_id, session).await.unwrap()
        }
    }

    #[tokio::test]
    async fn agent_login_captures_on_success() {
        let server = login_flow_server().await;
        let h = harness("ok").await;
        h.add_account("gh", &server.uri());

        let (_ctx, session) = h.session("gh").await;
        let mut guard = session.write().await;
        let source = Arc::new(TestSource::ok(server.uri()));
        let outcome = h.run("gh", source, &mut guard).await;
        drop(guard);

        let AgentLoginOutcome::Captured { record, score } = outcome else {
            panic!("expected capture, got {outcome:?}");
        };
        assert_eq!(record.state, AccountState::Valid);
        assert!(
            score.unwrap() >= 6,
            "cookie + corroboration required: {score:?}"
        );
        assert_eq!(
            record.session_summary.cookie_count, 1,
            "user_session captured"
        );

        // The envelope is real.
        let store = crate::account::AccountRegistry::open(h.base.join("accounts"))
            .unwrap()
            .session_store("gh")
            .unwrap();
        let envelope = store
            .load("127.0.0.1", &StaticKeyProvider::new([7u8; 32]), None)
            .unwrap();
        assert_eq!(envelope.state.cookies[0].name, "user_session");
    }

    #[tokio::test]
    async fn restored_valid_jar_short_circuits_before_credentials() {
        let server = login_flow_server().await;
        let h = harness("restore").await;
        h.add_account("gh", &server.uri());

        // First run: full login → valid.
        let (_ctx, session) = h.session("gh").await;
        {
            let mut guard = session.write().await;
            let outcome = h
                .run("gh", Arc::new(TestSource::ok(server.uri())), &mut guard)
                .await;
            assert!(matches!(outcome, AgentLoginOutcome::Captured { .. }));
        }
        // Age it to `stale` — the jar restore must revive it without ever
        // touching credentials (the source holds nothing to resolve).
        h.manager.mark_stale("gh", "probe_failed").unwrap();
        let (_ctx2, session2) = h.session("gh").await;
        let mut guard = session2.write().await;
        let outcome = h.run("gh", Arc::new(TestSource::deny()), &mut guard).await;
        drop(guard);
        let AgentLoginOutcome::Captured { record, score } = outcome else {
            panic!("expected restore capture, got {outcome:?}");
        };
        assert_eq!(record.state, AccountState::Valid);
        assert!(
            score.is_none(),
            "restored capture carries no detector score"
        );
    }

    #[tokio::test]
    async fn consent_denied_lands_needs_login_with_reason() {
        let server = login_flow_server().await;
        let h = harness("consent").await;
        h.add_account("gh", &server.uri());

        let (_ctx, session) = h.session("gh").await;
        let mut guard = session.write().await;
        let outcome = h.run("gh", Arc::new(TestSource::deny()), &mut guard).await;
        drop(guard);

        let AgentLoginOutcome::NeedsLogin { record, reason } = outcome else {
            panic!("expected needs_login, got {outcome:?}");
        };
        assert_eq!(record.state, AccountState::NeedsLogin);
        assert!(reason.starts_with("consent_required:"), "{reason}");
    }

    #[tokio::test]
    async fn sms_second_factor_is_an_immediate_escalation() {
        // Server that always lands on the SMS step. wiremock matches mounts
        // in insertion order: the specific `/mfa` GET must precede the
        // catch-all GET.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(303).insert_header("Location", "/mfa"))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/mfa"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(MFA_SMS))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(LOGIN_FORM))
            .mount(&server)
            .await;

        let h = harness("sms").await;
        h.add_account("gh", &server.uri());
        let (_ctx, session) = h.session("gh").await;
        let mut guard = session.write().await;
        let outcome = h
            .run("gh", Arc::new(TestSource::ok(server.uri())), &mut guard)
            .await;
        drop(guard);

        let AgentLoginOutcome::MfaEscalation { record, kind } = outcome else {
            panic!("expected mfa escalation, got {outcome:?}");
        };
        assert_eq!(kind, "sms");
        assert_eq!(record.state, AccountState::NeedsLogin);
        assert!(
            record
                .state_detail
                .as_deref()
                .is_some_and(|d| d.starts_with("mfa_escalation:"))
        );
    }

    #[tokio::test]
    async fn redirect_off_allowlist_vetoes_capture() {
        let server = login_flow_server().await;
        let h = harness("redirect").await;
        h.add_account("gh", &server.uri());

        // Credential allowed only at a foreign origin: the flow reaches the
        // in-scope page, but the final-origin verdict must veto capture.
        let source = Arc::new(TestSource {
            username: Some("garden".into()),
            password: Some("hunter2".into()),
            allowed: vec!["https://other.example".to_string()],
            totp: None,
            deny: None,
        });
        let (_ctx, session) = h.session("gh").await;
        let mut guard = session.write().await;
        let outcome = h.run("gh", source, &mut guard).await;
        drop(guard);

        let AgentLoginOutcome::PolicyViolation { record, origin } = outcome else {
            panic!("expected policy violation, got {outcome:?}");
        };
        assert!(origin.starts_with("http://127.0.0.1"), "{origin}");
        assert_eq!(record.state, AccountState::NeedsLogin);
        // Nothing was captured — no valid envelope exists.
        assert_eq!(
            h.manager.registry().get("gh").unwrap().state,
            AccountState::NeedsLogin
        );
    }

    #[tokio::test]
    async fn form_not_found_lands_needs_login() {
        // A site with no form anywhere.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_string("<html><body><h1>hello</h1></body></html>"),
            )
            .mount(&server)
            .await;
        let h = harness("noform").await;
        h.add_account("gh", &server.uri());

        let (_ctx, session) = h.session("gh").await;
        let mut guard = session.write().await;
        let outcome = h
            .run("gh", Arc::new(TestSource::ok(server.uri())), &mut guard)
            .await;
        drop(guard);

        let AgentLoginOutcome::NeedsLogin { record, reason } = outcome else {
            panic!("expected needs_login, got {outcome:?}");
        };
        assert!(reason.contains("login form not found"), "{reason}");
        assert_eq!(record.state, AccountState::NeedsLogin);
    }

    #[tokio::test]
    async fn valid_account_refuses_agent_login() {
        let h = harness("valid").await;
        h.manager
            .registry()
            .add(AccountRecord::new("gh", "example.com").unwrap())
            .unwrap();
        // Drive to `valid` through the legal machine: logging_in → capture.
        let (_ctx, session) = h.session("gh").await;
        h.manager
            .registry()
            .set_state("gh", AccountState::LoggingIn, None)
            .unwrap();
        h.manager
            .capture_session("gh", &*session.read().await, h.keys.as_ref())
            .unwrap();

        let engine = AgentLoginEngine::new(
            Arc::clone(&h.manager),
            Arc::clone(&h.keys),
            Arc::new(TestSource::deny()),
            "agent",
        );
        let (_c2, s2) = h.session("gh").await;
        let mut guard = s2.write().await;
        let err = engine.login("gh", &mut guard).await.unwrap_err();
        assert!(err.to_string().contains("valid"), "{err}");
    }

    #[test]
    fn session_state_strings_are_stable() {
        // The event surface depends on these spellings.
        let captured = AgentLoginOutcome::Captured {
            record: AccountRecord::new("x", "example.com").unwrap(),
            score: Some(6),
        };
        assert_eq!(captured.state(), "captured");
        assert_eq!(captured.detail().as_deref(), Some("score:6"));
    }
}
