//! `account` and `credential` CLI subcommands (upper design §7.1, lower
//! design §6.1).
//!
//! Secrets rules enforced here:
//!
//! - `credential put` never takes the value from argv (shell history) —
//!   `--stdin` or `--prompt` only (prompt is the default on a terminal).
//! - `credential get` prints the raw value to stdout and has **no `--json`**
//!   flag (design §6.1: values never leave as JSON).
//! - Keychain access is real ([`KeyringProvider`]); `get` is gated through
//!   the real [`PolicyEngine`] + [`ConsentStore`] at `~/.oxibrowser/consents.jsonl`
//!   (the local user is the confirmation authority; deny rules still win).
//! - Session AEAD keys for `account status --probe` come from the OS
//!   keychain via
//!   [`KeyringKeyProvider`] (`service="com.oxibrowser.agent/_session-keys/<scope>"`,
//!   `account="aead-key"`, create-once — lower design §5.1). The
//!   implementation lives in the credentials crate (§3.5); the CLI only
//!   calls it.

use std::io::IsTerminal;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::Subcommand;

use oxibrowser_core::account::{
    AccountManager, AccountRecord, AccountRegistry, LoginMode, LoginOrchestrator,
};
use oxibrowser_core::error::CoreError;
use oxibrowser_core::network::origin_policy::Decision;
use oxibrowser_core::security::audit::{self, AuditDecision, AuditEvent, AuditEventKind};
use oxibrowser_core::storage::session_store::{FingerprintMeta, SessionStore};
use oxibrowser_core::storage_state::StorageState;
use oxibrowser_core::{Browser, BrowserConfig};

use oxibrowser_cdp::{CdpServer, LoginSurface};

use oxibrowser_credentials::keyring::KeyringProvider;
use oxibrowser_credentials::provider::{SERVICE_PREFIX, service_key};
use oxibrowser_credentials::{
    ConsentRecord, ConsentStore, ConsentSubject, CredentialAction, CredentialId, CredentialKind,
    CredentialProvider, DEFAULT_CONSENT_TTL, DEFAULT_MAX_USES, NewCredential, PolicyEngine,
    SecretBox, TotpGenerator, UseRequest,
};

use crate::output::CliResponse;
use crate::print_error;

// ---------------------------------------------------------------------------
// Session AEAD keys — keychain-backed KeyProvider (lower design §5.1)
// ---------------------------------------------------------------------------

/// Session-key provider: the credentials crate's keychain implementation
/// (§3.5 — the CLI carries no key material logic of its own).
pub(crate) fn session_keys() -> oxibrowser_credentials::KeyringKeyProvider {
    oxibrowser_credentials::KeyringKeyProvider::new()
}

// ---------------------------------------------------------------------------
// Audit context (respects global --audit/--no-audit)
// ---------------------------------------------------------------------------

/// Audit sink configuration handed over from `main` (already resolved
/// against `--audit`/`--no-audit`).
#[derive(Debug, Clone)]
pub(crate) struct AuditContext {
    /// `None` = audit disabled (`--no-audit`).
    pub path: Option<PathBuf>,
}

impl AuditContext {
    /// A log for components requiring an owned [`Arc<AuditLog>`]
    /// ([`PolicyEngine`]). When audit is disabled the sink is `/dev/null`
    /// (unix) or a scratch file — the user asked for no audit trail, so a
    /// discarded sink is honest.
    fn engine_log(&self) -> Result<Arc<audit::AuditLog>, String> {
        let path = match &self.path {
            Some(p) => p.clone(),
            None => {
                #[cfg(unix)]
                {
                    PathBuf::from("/dev/null")
                }
                #[cfg(not(unix))]
                {
                    std::env::temp_dir().join("oxibrowser-audit-disabled.jsonl")
                }
            }
        };
        audit::AuditLog::open(&path)
            .map(Arc::new)
            .map_err(|e| format!("cannot open audit log at {}: {e}", path.display()))
    }
}

// ---------------------------------------------------------------------------
// Clap surface
// ---------------------------------------------------------------------------

#[derive(Subcommand)]
pub(crate) enum AccountCommand {
    /// Register an account (record + session sandbox), state=needs_login.
    Add {
        /// Site the account belongs to (domain or URL; normalized to its
        /// registrable domain).
        #[arg(long)]
        site: String,
        /// Account slug ([a-z0-9-]{1,32}); derived from the scope when
        /// omitted (conflicts get -2, -3, …).
        #[arg(long)]
        id: Option<String>,
        /// Login hint (username/email — not a secret).
        #[arg(long)]
        login: Option<String>,
        /// Display name for the identity card.
        #[arg(long)]
        display: Option<String>,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Account status board (Codex "account manager", CLI edition).
    List {
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show one account; `--probe` re-validates the stored session live.
    Status {
        id: String,
        /// Run the validation probe (network + keychain) and update state.
        #[arg(long)]
        probe: bool,
        /// Allow private/loopback targets when probing.
        #[arg(long)]
        allow_private_ips: bool,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Revoke an account: record, session envelopes, sandbox — all gone.
    Rm {
        id: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Run a login flow for an account (upper design §5.1).
    ///
    /// `--mode user` (default) opens a login window: with `--json`, serve a
    /// temporary CDP endpoint and print one `{ws_url, viewer_token, login_id}`
    /// block (the host connects as viewer); without, run the terminal wizard
    /// (`goto/type/click/screenshot/done/abort`). `--mode import` seeds the
    /// account from `--storage-state` (Playwright JSON) or `--cookies`
    /// (Netscape cookies.txt) and validates with the probe. `--mode agent`
    /// runs the unattended M-D flow: the broker fills the login form (and
    /// TOTP) itself from the keychain — no window, no values on screen.
    Login {
        id: String,
        /// user (default), import, or agent.
        #[arg(long, value_parser = ["user", "import", "agent"])]
        mode: Option<String>,
        /// [import] Playwright storageState JSON file.
        #[arg(long, value_name = "FILE")]
        storage_state: Option<PathBuf>,
        /// [import] Netscape cookies.txt file.
        #[arg(long, value_name = "FILE")]
        cookies: Option<PathBuf>,
        /// [agent] Acting agent id (audit + events; default "main").
        #[arg(long, value_name = "ID")]
        agent: Option<String>,
        /// Host contract: print the {ws_url, viewer_token, login_id} block
        /// and block until the window ends.
        #[arg(long)]
        json: bool,
        /// Login window timeout in seconds (default 300).
        #[arg(long)]
        timeout: Option<u64>,
        /// Allow private/loopback network targets (local consoles, dev
        /// servers) — same switch as `fetch --allow-private-ips`.
        #[arg(long)]
        allow_private_ips: bool,
    },
    /// Dispose the stored envelope and return the account to needs_login
    /// (credentials are kept).
    Logout {
        id: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Export the stored envelope as Playwright storageState JSON (§6.3
    /// exception path — audited and warned; never exposed over CDP).
    ExportState {
        id: String,
        /// Output file (written 0600).
        #[arg(long)]
        out: PathBuf,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Grant an agent access to this account (agent-scoped consent —
    /// `Target.createBrowserContext {oxiAccount}` and agent login gate).
    Grant {
        id: String,
        /// Agent id the grant is bound to (upper design §1:
        /// grants are `agent_id × account_id`).
        #[arg(long, value_name = "ID")]
        agent: String,
        /// Granted actions (comma list of navigate|interact|irreversible|login).
        #[arg(long, value_delimiter = ',', default_values = ["navigate", "interact"])]
        actions: Vec<String>,
        /// Grant lifetime in seconds (default 14 days).
        #[arg(long, value_name = "SEC")]
        ttl: Option<u64>,
        /// Max uses (default 50).
        #[arg(long)]
        max_uses: Option<u64>,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Revoke an agent's grants for this account (all actions).
    Revoke {
        id: String,
        /// Agent id whose grants are revoked.
        #[arg(long, value_name = "ID")]
        agent: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum CredentialCommand {
    /// Store a credential in the OS keychain.
    ///
    /// The value is read from `--stdin` or `--prompt` (default on a
    /// terminal) — never argv. For `--kind totp` the value is an `otpauth://`
    /// URI or a bare base32 secret.
    Put {
        /// Agent namespace for the handle (`kch:<agent>/<scope>/…`).
        #[arg(long)]
        agent: String,
        /// Site the credential belongs to.
        #[arg(long)]
        site: String,
        /// Credential kind: password, totp, api-key, note.
        #[arg(long)]
        kind: String,
        /// Account/purpose discriminator (default: `default`).
        #[arg(long)]
        slug: Option<String>,
        /// Login hint (username — not a secret).
        #[arg(long)]
        login: Option<String>,
        /// Exact origin this credential may be used at (repeatable).
        #[arg(long = "origin")]
        origins: Vec<String>,
        /// Read the value from stdin.
        #[arg(long, conflicts_with = "prompt")]
        stdin: bool,
        /// Read the value with a hidden terminal prompt.
        #[arg(long)]
        prompt: bool,
    },
    /// Print one secret field to stdout (no --json by design).
    Get {
        /// Credential handle (`kch:…`).
        #[arg(long)]
        id: String,
        /// Which field: `password` or `otpauth`.
        #[arg(long)]
        field: String,
    },
    /// List credential metadata (never values).
    List {
        /// Filter by agent namespace.
        #[arg(long)]
        agent: Option<String>,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Delete a credential from the keychain.
    Rm {
        #[arg(long)]
        id: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print the current TOTP code for a totp credential (human output).
    Totp {
        #[arg(long)]
        id: String,
    },
    /// Keychain onboarding guidance: ACL notes and diagnostics script.
    Onboard {
        #[arg(long)]
        agent: String,
        #[arg(long)]
        site: String,
    },
    /// Grant a credential-plane consent: allow `--actions` at `--origin`
    /// (lower design §6.1 REPL `credential_authorize` — CLI edition).
    Authorize {
        /// Credential handle (`kch:…`).
        #[arg(long)]
        id: String,
        /// Exact origin the grant covers.
        #[arg(long)]
        origin: String,
        /// Actions (comma list of login|mfa|fill-api-key).
        #[arg(long, value_delimiter = ',', default_values = ["login"])]
        actions: Vec<String>,
        /// Lifetime in seconds (default 14 days).
        #[arg(long, value_name = "SEC")]
        ttl: Option<u64>,
        /// Max uses (default 50).
        #[arg(long)]
        max_uses: Option<u64>,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Revoke a credential-plane consent (tombstone).
    Forget {
        #[arg(long)]
        id: String,
        #[arg(long)]
        origin: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

pub(crate) fn oxi_accounts_registry() -> Result<AccountRegistry, String> {
    let base = AccountRegistry::default_dir()
        .ok_or_else(|| "HOME is not set — cannot locate ~/.oxibrowser".to_string())?;
    AccountRegistry::open(base).map_err(|e| e.to_string())
}

/// Normalize `--site` (domain or URL) to a registrable domain scope.
fn scope_of_site(site: &str) -> Result<String, String> {
    let host = if site.contains("://") {
        url::Url::parse(site)
            .map_err(|e| format!("invalid --site {site:?}: {e}"))?
            .host_str()
            .ok_or_else(|| format!("--site {site:?} has no host"))?
            .to_string()
    } else {
        site.split(['/']).next().unwrap_or(site).to_string()
    };
    let host = host.trim_start_matches('.').to_ascii_lowercase();
    // IP literals (loopback consoles, LAN dashboards) have no registrable
    // domain — psl suffix-matching mangles them ("127.0.0.1" → "0.1").
    if host.parse::<std::net::IpAddr>().is_ok() {
        return Ok(host);
    }
    let scope = psl::domain_str(&host).unwrap_or(&host).to_ascii_lowercase();
    if scope.is_empty() {
        return Err(format!("cannot derive a registrable domain from {site:?}"));
    }
    Ok(scope)
}

/// Derive an unused account id from the scope (`github-com`, `github-com-2`…).
fn derive_account_id(registry: &AccountRegistry, scope: &str) -> String {
    let base = scope.replace('.', "-");
    if !registry.exists(&base) {
        return base;
    }
    for n in 2.. {
        let candidate = format!("{base}-{n}");
        if !registry.exists(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

fn record_json(rec: &AccountRecord) -> serde_json::Value {
    serde_json::to_value(rec).unwrap_or(serde_json::Value::Null)
}

fn audit_sensitive(action: &str, reason: &str) {
    audit::record(AuditEvent {
        action: Some(action.to_string()),
        ..audit::event(
            AuditEventKind::SensitiveAction,
            AuditDecision::Allow,
            reason,
        )
    });
}

fn provider() -> KeyringProvider {
    KeyringProvider::new()
}

fn policy_engine(audit: &AuditContext) -> Result<PolicyEngine, String> {
    let consents =
        ConsentStore::open_default().map_err(|e| format!("cannot open consent store: {e}"))?;
    let log = audit.engine_log()?;
    Ok(PolicyEngine::new(Vec::new(), consents, log))
}

// ---------------------------------------------------------------------------
// account …
// ---------------------------------------------------------------------------

/// Build the CDP [`CredentialBroker`] for `serve`: keychain provider +
/// policy engine over the shared consent store + audit sink. `Ok(None)`
/// means "no consent store / audit sink could be opened" — the server then
/// runs without the credential surface (commands answer
/// `credentialsUnavailable`, fail-closed).
pub(crate) fn serve_credential_broker(
    audit: AuditContext,
) -> Result<Option<Arc<oxibrowser_cdp::credential::CredentialBroker>>, String> {
    let engine = match policy_engine(&audit) {
        Ok(e) => e,
        Err(_) => return Ok(None),
    };
    let provider: Arc<dyn CredentialProvider> = Arc::new(provider());
    Ok(Some(Arc::new(
        oxibrowser_cdp::credential::CredentialBroker::new(provider, Arc::new(engine)),
    )))
}

pub(crate) async fn run_account(command: AccountCommand, audit: AuditContext) -> i32 {
    match command {
        AccountCommand::Add {
            site,
            id,
            login,
            display,
            json,
        } => account_add(
            &site,
            id.as_deref(),
            login.as_deref(),
            display.as_deref(),
            json,
        ),
        AccountCommand::List { json } => account_list(json),
        AccountCommand::Status {
            id,
            probe,
            allow_private_ips,
            json,
        } => account_status(&id, probe, allow_private_ips, json).await,
        AccountCommand::Rm { id, json } => account_rm(&id, json),
        AccountCommand::Login {
            id,
            mode,
            storage_state,
            cookies,
            agent,
            json,
            timeout,
            allow_private_ips,
        } => {
            account_login(
                &id,
                mode.as_deref().unwrap_or("user"),
                storage_state.as_deref(),
                cookies.as_deref(),
                agent.as_deref(),
                json,
                timeout,
                allow_private_ips,
                &audit,
            )
            .await
        }
        AccountCommand::Logout { id, json } => account_logout(&id, json),
        AccountCommand::ExportState { id, out, json } => account_export_state(&id, &out, json),
        AccountCommand::Grant {
            id,
            agent,
            actions,
            ttl,
            max_uses,
            json,
        } => account_grant(&id, &agent, &actions, ttl, max_uses, json),
        AccountCommand::Revoke { id, agent, json } => account_revoke(&id, &agent, json),
    }
}

fn account_add(
    site: &str,
    id: Option<&str>,
    login: Option<&str>,
    display: Option<&str>,
    json: bool,
) -> i32 {
    let registry = match oxi_accounts_registry() {
        Ok(r) => r,
        Err(e) => return print_error(&e, "RUNTIME_ERROR", json),
    };
    let scope = match scope_of_site(site) {
        Ok(s) => s,
        Err(e) => return print_error(&e, "INPUT_VALIDATION", json),
    };
    if let Some(custom) = id {
        if let Err(e) = oxibrowser_core::account::validate_account_id(custom) {
            return print_error(&e.to_string(), "INPUT_VALIDATION", json);
        }
        if registry.exists(custom) {
            return print_error(
                &format!("account {custom:?} already exists"),
                "ACCOUNT_EXISTS",
                json,
            );
        }
    }
    let account_id = id
        .map(str::to_string)
        .unwrap_or_else(|| derive_account_id(&registry, &scope));

    let mut record = match AccountRecord::new(&account_id, &scope) {
        Ok(r) => r,
        Err(e) => return print_error(&e.to_string(), "INPUT_VALIDATION", json),
    };
    record.identity.login_hint = login.map(str::to_string);
    record.identity.display_name = display.map(str::to_string);
    let record = match registry.add(record) {
        Ok(r) => r,
        Err(e) => return print_error(&e.to_string(), "RUNTIME_ERROR", json),
    };

    if json {
        CliResponse::success(record_json(&record)).print_json();
    } else {
        println!(
            "account added: {account_id} (scope: {scope}, state: {})",
            record.state
        );
    }
    0
}

fn account_list(json: bool) -> i32 {
    let registry = match oxi_accounts_registry() {
        Ok(r) => r,
        Err(e) => return print_error(&e, "RUNTIME_ERROR", json),
    };
    let records = match registry.list() {
        Ok(r) => r,
        Err(e) => return print_error(&e.to_string(), "RUNTIME_ERROR", json),
    };
    if json {
        CliResponse::success(
            serde_json::json!({ "accounts": records.iter().map(record_json).collect::<Vec<_>>() }),
        )
        .print_json();
        return 0;
    }
    if records.is_empty() {
        println!("no accounts — add one with `oxibrowser account add --site <domain>`");
        return 0;
    }
    let mut board = Board::new(&["ID", "SCOPE", "STATE", "LOGIN HINT", "SESSION UPDATED"]);
    for r in &records {
        board.row([
            r.account_id.as_str(),
            r.scope.as_str(),
            r.state.as_str(),
            r.identity.login_hint.as_deref().unwrap_or("-"),
            r.session_summary.updated_at.as_deref().unwrap_or("-"),
        ]);
    }
    board.print();
    0
}

async fn account_status(id: &str, probe: bool, allow_private: bool, json: bool) -> i32 {
    let registry = match oxi_accounts_registry() {
        Ok(r) => r,
        Err(e) => return print_error(&e, "RUNTIME_ERROR", json),
    };
    let mut record = match registry.get(id) {
        Ok(r) => r,
        Err(_) => {
            return print_error(
                &format!("account {id:?} not found"),
                "ACCOUNT_NOT_FOUND",
                json,
            );
        }
    };

    let mut probe_json = None;
    if probe {
        // No captured envelope yet → probing would just fail on the file
        // read; say so directly.
        let store: SessionStore = match registry.session_store(id) {
            Ok(s) => s,
            Err(e) => return print_error(&e.to_string(), "INPUT_VALIDATION", json),
        };
        if !store.path_for(&record.scope).exists() {
            let msg = format!(
                "account {id} has no captured session yet (state: {}) — nothing to probe",
                record.state
            );
            return print_error(&msg, "NO_SESSION", json);
        }
        let manager = AccountManager::new(registry.clone());
        let mut config = BrowserConfig::headless();
        if allow_private {
            config.enable_ssrf_filter = false;
        }
        let browser = match Browser::new(config).await {
            Ok(b) => b,
            Err(e) => {
                return print_error(&format!("browser init failed: {e}"), "RUNTIME_ERROR", json);
            }
        };
        let session = match browser.new_session().await {
            Ok(s) => s,
            Err(e) => {
                return print_error(&format!("session init failed: {e}"), "RUNTIME_ERROR", json);
            }
        };
        let mut guard = session.write().await;
        let current = FingerprintMeta {
            user_agent: guard.effective_ua(),
            ..FingerprintMeta::default()
        };
        match manager
            .verify_with_probe(id, &mut guard, &session_keys(), Some(&current))
            .await
        {
            Ok((updated, outcome)) => {
                record = updated;
                probe_json = Some(serde_json::json!({
                    "verdict": verdict_str(&outcome.verdict),
                    "http_status": outcome.http_status,
                    "marker_found": outcome.marker_found,
                    "login_form_present": outcome.login_form_present,
                }));
            }
            Err(e) => {
                return print_error(&format!("probe failed: {e}"), probe_error_code(&e), json);
            }
        }
    }

    if json {
        let mut payload = serde_json::json!({ "account": record_json(&record) });
        if let Some(p) = probe_json {
            payload["probe"] = p;
        }
        CliResponse::success(payload).print_json();
        return 0;
    }

    println!("account   {} ({})", record.account_id, record.scope);
    match (&record.state, &record.state_detail) {
        (state, Some(detail)) => println!("state     {state} — {detail}"),
        (state, None) => println!("state     {state}"),
    }
    let s = &record.session_summary;
    println!(
        "session   {} cookie(s) · earliest expiry {} · updated {}",
        s.cookie_count,
        s.earliest_expiry.as_deref().unwrap_or("-"),
        s.updated_at.as_deref().unwrap_or("-")
    );
    if record.credentials.is_empty() {
        println!("credentials none");
    } else {
        println!("credentials {} handle(s)", record.credentials.len());
        for h in &record.credentials {
            println!("    {h}");
        }
    }
    match &record.probe {
        Some(p) => println!(
            "probe     {} (marker: {})",
            p.url,
            p.marker.as_deref().unwrap_or("-")
        ),
        None => println!(
            "probe     fallback (https://{}/ + login-form absence)",
            record.scope
        ),
    }
    if let Some(p) = probe_json {
        println!(
            "last probe  {} (http {:?}, marker {:?}, login form {:?})",
            p["verdict"].as_str().unwrap_or("?"),
            p["http_status"],
            p["marker_found"],
            p["login_form_present"]
        );
    }
    0
}

fn verdict_str(verdict: &oxibrowser_core::account::ProbeVerdict) -> &'static str {
    use oxibrowser_core::account::ProbeVerdict::*;
    match verdict {
        Valid => "valid",
        Invalid { .. } => "invalid",
        Challenge { .. } => "challenge",
        Unreachable { .. } => "unreachable",
    }
}

fn probe_error_code(err: &CoreError) -> &'static str {
    match err {
        CoreError::SessionFingerprintMismatch(_) => "FINGERPRINT_MISMATCH",
        CoreError::SessionStoreIo(_) => "KEYSTORE_UNAVAILABLE",
        _ => "RUNTIME_ERROR",
    }
}

fn account_rm(id: &str, json: bool) -> i32 {
    let registry = match oxi_accounts_registry() {
        Ok(r) => r,
        Err(e) => return print_error(&e, "RUNTIME_ERROR", json),
    };
    let record = match registry.get(id) {
        Ok(r) => r,
        Err(_) => {
            return print_error(
                &format!("account {id:?} not found"),
                "ACCOUNT_NOT_FOUND",
                json,
            );
        }
    };
    match registry.remove(id) {
        Ok(true) => {
            audit_sensitive(
                "account_rm",
                &format!("account={id} scope={}", record.scope),
            );
            if json {
                CliResponse::success(serde_json::json!({"removed": id, "scope": record.scope}))
                    .print_json();
            } else {
                println!(
                    "account removed: {id} (envelopes and sandbox deleted; keychain credentials kept)"
                );
            }
            0
        }
        Ok(false) => print_error(
            &format!("account {id:?} not found"),
            "ACCOUNT_NOT_FOUND",
            json,
        ),
        Err(e) => print_error(&e.to_string(), "RUNTIME_ERROR", json),
    }
}

// ---------------------------------------------------------------------------
// account login / logout / export-state (M-C, §5.1)
// ---------------------------------------------------------------------------

/// Manager + browser + orchestrator bundle for login flows (audit goes to
/// the process-global sink, like the other `account` subcommands).
pub(crate) async fn login_stack(
    registry: &AccountRegistry,
    allow_private: bool,
) -> Result<(Arc<AccountManager>, Arc<Browser>, Arc<LoginOrchestrator>), String> {
    let manager = Arc::new(AccountManager::new(registry.clone()));
    let mut config = BrowserConfig::headless();
    if allow_private {
        config.enable_ssrf_filter = false;
    }
    let browser = Arc::new(
        Browser::new(config)
            .await
            .map_err(|e| format!("browser init failed: {e}"))?,
    );
    let orch = Arc::new(LoginOrchestrator::new(
        manager.clone(),
        browser.clone(),
        Arc::new(session_keys()),
    ));
    Ok((manager, browser, orch))
}

async fn account_login(
    id: &str,
    mode: &str,
    storage_state: Option<&Path>,
    cookies: Option<&Path>,
    agent: Option<&str>,
    json: bool,
    timeout: Option<u64>,
    allow_private: bool,
    audit: &AuditContext,
) -> i32 {
    match mode {
        "import" => account_login_import(id, storage_state, cookies, allow_private, json).await,
        "user" if json => account_login_host(id, timeout, allow_private).await,
        "user" => account_login_wizard(id, timeout, allow_private).await,
        "agent" => account_login_agent(id, agent, json, timeout, allow_private, audit).await,
        other => print_error(
            &format!("invalid --mode {other:?} (expected \"user\", \"agent\" or \"import\")"),
            "INPUT_VALIDATION",
            json,
        ),
    }
}

/// Agent-mode login (M-D §5.3): the unattended flow — scope jar restore,
/// login-page discovery, broker-gated credential + TOTP injection, detector,
/// capture. Values never reach the terminal; the outcome is a state.
async fn account_login_agent(
    id: &str,
    agent: Option<&str>,
    json: bool,
    timeout: Option<u64>,
    allow_private: bool,
    audit: &AuditContext,
) -> i32 {
    let agent_id = agent.unwrap_or("main").to_string();
    let registry = match oxi_accounts_registry() {
        Ok(r) => r,
        Err(e) => return print_error(&e, "RUNTIME_ERROR", json),
    };
    let (manager, _browser, orch) = match login_stack(&registry, allow_private).await {
        Ok(s) => s,
        Err(e) => return print_error(&e, "RUNTIME_ERROR", json),
    };

    // Credential plane: keychain provider + deny-first policy engine over
    // ~/.oxibrowser/consents.jsonl (the same wiring `credential put/get` uses).
    let provider: std::sync::Arc<dyn oxibrowser_credentials::CredentialProvider> =
        std::sync::Arc::new(provider());
    let engine = match policy_engine(audit) {
        Ok(e) => std::sync::Arc::new(e),
        Err(e) => return print_error(&e, "RUNTIME_ERROR", json),
    };
    let source = std::sync::Arc::new(oxibrowser_credentials::BrokerSource::new(provider, engine));

    let mut agent_engine = oxibrowser_core::account::AgentLoginEngine::new(
        manager,
        orch.shared_keys(),
        source,
        agent_id,
    )
    .with_events(orch.event_sender());
    if let Some(secs) = timeout {
        agent_engine = agent_engine.with_timeout(Duration::from_secs(secs));
    }

    let (_ctx, session) = match orch.open_account_context(id).await {
        Ok(pair) => pair,
        Err(e) => return print_error(&e.to_string(), "RUNTIME_ERROR", json),
    };
    let mut guard = session.write().await;
    let outcome = agent_engine.login(id, &mut guard).await;
    drop(guard);

    use oxibrowser_core::account::AgentLoginOutcome;
    match outcome {
        Ok(outcome) => {
            if json {
                let payload = serde_json::json!({
                    "account_id": outcome_record(&outcome).map(|r| r.account_id.clone()),
                    "state": outcome.state(),
                    "detail": outcome.detail(),
                    "retryable": outcome.retryable(),
                });
                CliResponse::success(payload).print_json();
            } else {
                match &outcome {
                    AgentLoginOutcome::Captured { record, .. } => {
                        println!(
                            "account {id}: {} (agent login captured, cookies={})",
                            record.state, record.session_summary.cookie_count
                        );
                    }
                    AgentLoginOutcome::MfaEscalation { kind, .. } => {
                        eprintln!(
                            "agent login stopped: {kind} second factor needs a human — complete it once via `account login` (user mode)"
                        );
                    }
                    AgentLoginOutcome::Challenge { detail, .. } => {
                        eprintln!(
                            "agent login stopped: challenge needs a human ({detail}) — escalation, not retry"
                        );
                    }
                    AgentLoginOutcome::PolicyViolation { origin, .. } => {
                        eprintln!(
                            "agent login stopped: final origin {origin} left the credential allowlist — capture vetoed"
                        );
                    }
                    AgentLoginOutcome::NeedsLogin { reason, .. } => {
                        eprintln!("agent login failed: {reason}");
                    }
                }
            }
            if matches!(outcome, AgentLoginOutcome::Captured { .. }) {
                0
            } else {
                1
            }
        }
        Err(e) => print_error(&e.to_string(), "RUNTIME_ERROR", json),
    }
}

fn outcome_record(outcome: &oxibrowser_core::account::AgentLoginOutcome) -> Option<&AccountRecord> {
    use oxibrowser_core::account::AgentLoginOutcome;
    match outcome {
        AgentLoginOutcome::Challenge { record, .. } => Some(record.as_ref()),
        AgentLoginOutcome::Captured { record, .. }
        | AgentLoginOutcome::MfaEscalation { record, .. }
        | AgentLoginOutcome::PolicyViolation { record, .. }
        | AgentLoginOutcome::NeedsLogin { record, .. } => Some(record),
    }
}

/// Load the import source: Playwright `storageState` JSON or Netscape
/// `cookies.txt`. Exactly one must be given.
fn load_import_state(
    storage_state: Option<&Path>,
    cookies: Option<&Path>,
) -> Result<StorageState, String> {
    match (storage_state, cookies) {
        (Some(path), None) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            serde_json::from_str(&text).map_err(|e| {
                format!(
                    "{} is not Playwright storageState JSON: {e}",
                    path.display()
                )
            })
        }
        (None, Some(path)) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            Ok(StorageState::from_netscape(&text))
        }
        (Some(_), Some(_)) => Err("--storage-state and --cookies are mutually exclusive".into()),
        (None, None) => Err(
            "import mode needs exactly one of --storage-state <FILE> or --cookies <FILE>".into(),
        ),
    }
}

async fn account_login_import(
    id: &str,
    storage_state: Option<&Path>,
    cookies: Option<&Path>,
    allow_private: bool,
    json: bool,
) -> i32 {
    let state = match load_import_state(storage_state, cookies) {
        Ok(s) => s,
        Err(e) => return print_error(&e, "INPUT_VALIDATION", json),
    };
    let registry = match oxi_accounts_registry() {
        Ok(r) => r,
        Err(e) => return print_error(&e, "RUNTIME_ERROR", json),
    };
    let (_manager, _browser, orch) = match login_stack(&registry, allow_private).await {
        Ok(s) => s,
        Err(e) => return print_error(&e, "RUNTIME_ERROR", json),
    };

    match orch.import(id, &state).await {
        Ok(outcome) => {
            if outcome.fingerprint_baseline_adopted {
                eprintln!(
                    "⚠ Imported session captured under THIS device's fingerprint baseline — \
                     state minted on another device may not survive (FM-L4)."
                );
                audit_sensitive(
                    "account_import",
                    &format!(
                        "account={id} state={} cookies={}",
                        outcome.record.state, outcome.record.session_summary.cookie_count
                    ),
                );
            }
            if json {
                let payload = serde_json::json!({
                    "account": record_json(&outcome.record),
                    "probe": outcome.probe,
                    "fingerprint_baseline_adopted": outcome.fingerprint_baseline_adopted,
                });
                CliResponse::success(payload).print_json();
            } else {
                println!(
                    "account {id}: {} (probe: {})",
                    outcome.record.state, outcome.probe.verdict
                );
            }
            if outcome.record.state == oxibrowser_core::account::AccountState::Valid {
                0
            } else {
                1
            }
        }
        Err(e) => print_error(&format!("import failed: {e}"), "RUNTIME_ERROR", json),
    }
}

/// User-mode login under the host contract (§5.1 user-mirror): temporary
/// CDP serve, one `{ws_url, viewer_token, login_id}` block on stdout, then
/// block until the window ends; the final outcome follows as a second JSON
/// line.
async fn account_login_host(id: &str, timeout: Option<u64>, allow_private: bool) -> i32 {
    let registry = match oxi_accounts_registry() {
        Ok(r) => r,
        Err(e) => return print_error(&e, "RUNTIME_ERROR", true),
    };
    let (_manager, browser, orch) = match login_stack(&registry, allow_private).await {
        Ok(s) => s,
        Err(e) => return print_error(&e, "RUNTIME_ERROR", true),
    };
    let surface = LoginSurface::new(orch.clone(), browser.clone());

    let (handle, ctx) = match surface
        .begin_login(id, LoginMode::User, timeout.map(Duration::from_secs))
        .await
    {
        Ok(h) => h,
        Err(e) => return print_error(&e.to_string(), "RUNTIME_ERROR", true),
    };

    let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let server = Arc::new(
        CdpServer::new(addr, browser.clone())
            .with_primary_context(Arc::clone(&ctx))
            .with_login(surface.clone()),
    );
    let bound = match server.start().await {
        Ok(a) => a,
        Err(e) => {
            let _ = orch.abort(&handle.login_id).await;
            return print_error(&format!("serve failed: {e}"), "RUNTIME_ERROR", true);
        }
    };

    // The host contract: exactly one connection block on stdout, then block.
    println!(
        "{}",
        serde_json::json!({
            "ws_url": format!("ws://{bound}/ws"),
            "viewer_token": handle.viewer_token,
            "login_id": handle.login_id,
            "account_id": id,
            "timeout_ms": handle.timeout_ms,
        })
    );
    use std::io::Write as _;
    let _ = std::io::stdout().lock().flush();

    let outcome = orch.wait(&handle.login_id).await;
    server.shutdown();
    browser.close().await.ok();

    match outcome {
        Some(o) => {
            println!(
                "{}",
                serde_json::json!({
                    "login_id": o.login_id,
                    "end_state": o.end_state,
                    "account_state": o.record.as_ref().map(|r| r.state.as_str()),
                })
            );
            if o.end_state == oxibrowser_core::account::LoginEndState::Captured {
                0
            } else {
                1
            }
        }
        None => print_error("login window channel closed", "RUNTIME_ERROR", true),
    }
}

const WIZARD_HELP: &str = "commands: goto <url> | type <selector> <value> | click <selector> | \
screenshot [file] | status | done | abort";

/// User-mode login in the terminal wizard (§5.1 row 3): a reduced REPL over
/// the account context. Typed values go straight from stdin into the page —
/// responses carry status only, never the values; `screenshot` writes a
/// 0600 file and prints the path only (audited).
async fn account_login_wizard(id: &str, timeout: Option<u64>, allow_private: bool) -> i32 {
    let registry = match oxi_accounts_registry() {
        Ok(r) => r,
        Err(e) => return print_error(&e, "RUNTIME_ERROR", false),
    };
    let (_manager, browser, orch) = match login_stack(&registry, allow_private).await {
        Ok(s) => s,
        Err(e) => return print_error(&e, "RUNTIME_ERROR", false),
    };

    let handle = match orch.begin_login(id, LoginMode::User, timeout.map(Duration::from_secs)) {
        Ok(h) => h,
        Err(e) => return print_error(&e.to_string(), "RUNTIME_ERROR", false),
    };
    let (ctx, session) = match orch.open_account_context(id).await {
        Ok(s) => s,
        Err(e) => {
            let _ = orch.abort(&handle.login_id).await;
            return print_error(&e.to_string(), "RUNTIME_ERROR", false);
        }
    };
    let tab = match browser.new_tab_in(&ctx).await {
        Ok(t) => t,
        Err(e) => {
            let _ = orch.abort(&handle.login_id).await;
            return print_error(&format!("tab init failed: {e}"), "RUNTIME_ERROR", false);
        }
    };

    eprintln!(
        "Login wizard for {id} (window timeout: {}s)",
        handle.timeout_ms / 1000
    );
    eprintln!("{WIZARD_HELP}");

    let stdin = std::io::stdin();
    let end_state;
    loop {
        eprint!("wizard> ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        if stdin.read_line(&mut line).unwrap_or(0) == 0 {
            if let Err(e) = orch.abort(&handle.login_id).await {
                eprintln!("error: {e}");
            }
            eprintln!("\n(stdin closed — aborting login)");
            end_state = Some(());
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut tokens = line.splitn(3, ' ');
        let cmd = tokens.next().unwrap_or_default();
        match cmd {
            "goto" => match tokens.next() {
                Some(url) => match tab.goto(url).await {
                    Ok(nav) => eprintln!("→ {} ({})", nav.url, nav.status),
                    Err(e) => eprintln!("error: {e}"),
                },
                None => eprintln!("goto <url>"),
            },
            "type" => {
                let rest = tokens.next().unwrap_or_default();
                let mut sel_val = rest.splitn(2, ' ');
                match (sel_val.next(), sel_val.next()) {
                    (Some(sel), Some(value)) => match tab.fill(sel, value).await {
                        Ok(()) => eprintln!("filled"),
                        Err(e) => eprintln!("error: {e}"),
                    },
                    _ => eprintln!("type <selector> <value>"),
                }
            }
            "click" => match tokens.next() {
                Some(sel) => match tab.click(sel).await {
                    Ok(()) => eprintln!("clicked"),
                    Err(e) => eprintln!("error: {e}"),
                },
                None => eprintln!("click <selector>"),
            },
            "screenshot" => {
                let path = tokens.next().map(PathBuf::from).unwrap_or_else(|| {
                    std::env::temp_dir().join(format!(
                        "oxi-wizard-{}-{}.png",
                        id,
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or_default()
                    ))
                });
                match screenshot_to_file(&session, &path).await {
                    Ok(()) => {
                        audit_sensitive(
                            "login_wizard_screenshot",
                            &format!("account={id} path={}", path.display()),
                        );
                        // Path only — the image never enters the REPL
                        // response channel (§5.2).
                        println!("{}", path.display());
                    }
                    Err(e) => eprintln!("error: {e}"),
                }
            }
            "status" => match registry.get(id) {
                Ok(record) => eprintln!("state: {}", record.state),
                Err(e) => eprintln!("error: {e}"),
            },
            "done" => {
                let mut guard = session.write().await;
                let verdict = orch
                    .complete_login(&handle.login_id, &mut guard, true)
                    .await;
                drop(guard);
                match verdict {
                    Ok(_) => eprintln!("captured"),
                    Err(e) => eprintln!("error: {e}"),
                }
                end_state = Some(());
                break;
            }
            "abort" => {
                if let Err(e) = orch.abort(&handle.login_id).await {
                    eprintln!("error: {e}");
                }
                end_state = Some(());
                break;
            }
            "help" | "?" => eprintln!("{WIZARD_HELP}"),
            other => eprintln!("unknown command {other:?} — {WIZARD_HELP}"),
        }
        // A window that vanished under us timed out.
        if orch.login_info(&handle.login_id).is_none() {
            eprintln!("login window closed (timeout?)");
            end_state = None;
            break;
        }
    }

    browser.close().await.ok();
    // The record is the truth — the window events are one-shot broadcasts.
    let state = registry
        .get(id)
        .map(|r| r.state)
        .unwrap_or(oxibrowser_core::account::AccountState::NeedsLogin);
    let _ = end_state;
    if state == oxibrowser_core::account::AccountState::Valid {
        println!("state: valid");
        0
    } else {
        println!("state: {state}");
        1
    }
}

/// Write a PNG screenshot as a 0600 file (the wizard's user-channel
/// exception — §5.2).
async fn screenshot_to_file(
    session: &Arc<tokio::sync::RwLock<oxibrowser_core::session::Session>>,
    path: &Path,
) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let png = {
        let mut guard = session.write().await;
        guard
            .capture_screenshot_png(1280)
            .await
            .map_err(|e| format!("screenshot failed: {e}"))?
    };
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    file.write_all(&png)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(())
}

fn account_logout(id: &str, json: bool) -> i32 {
    let registry = match oxi_accounts_registry() {
        Ok(r) => r,
        Err(e) => return print_error(&e, "RUNTIME_ERROR", json),
    };
    if !registry.exists(id) {
        return print_error(
            &format!("account {id:?} not found"),
            "ACCOUNT_NOT_FOUND",
            json,
        );
    }
    let manager = AccountManager::new(registry.clone());
    match manager.logout(id) {
        Ok(record) => {
            if json {
                CliResponse::success(record_json(&record)).print_json();
            } else {
                println!(
                    "account {id}: {} (envelope discarded; credentials kept)",
                    record.state
                );
            }
            0
        }
        Err(e) => print_error(&e.to_string(), "RUNTIME_ERROR", json),
    }
}

fn account_export_state(id: &str, out: &Path, json: bool) -> i32 {
    use std::os::unix::fs::OpenOptionsExt;

    let registry = match oxi_accounts_registry() {
        Ok(r) => r,
        Err(e) => return print_error(&e, "RUNTIME_ERROR", json),
    };
    let record = match registry.get(id) {
        Ok(r) => r,
        Err(_) => {
            return print_error(
                &format!("account {id:?} not found"),
                "ACCOUNT_NOT_FOUND",
                json,
            );
        }
    };
    let store = match registry.session_store(id) {
        Ok(s) => s,
        Err(e) => return print_error(&e.to_string(), "INPUT_VALIDATION", json),
    };
    if !store.path_for(&record.scope).exists() {
        return print_error(
            &format!(
                "account {id} has no captured session (state: {})",
                record.state
            ),
            "NO_SESSION",
            json,
        );
    }
    let envelope = match store.load(&record.scope, &session_keys(), None) {
        Ok(env) => env,
        Err(e) => return print_error(&e.to_string(), probe_error_code(&e), json),
    };
    let body = match serde_json::to_string_pretty(&envelope.state) {
        Ok(b) => b,
        Err(e) => return print_error(&format!("serialization failed: {e}"), "RUNTIME_ERROR", json),
    };
    if let Err(e) = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(out)
        .and_then(|mut f| f.write_all(body.as_bytes()))
    {
        return print_error(
            &format!("cannot write {}: {e}", out.display()),
            "RUNTIME_ERROR",
            json,
        );
    }

    // §6.3 exception path: caller is the local user; audit + warn loudly.
    audit_sensitive(
        "export_storage_state",
        &format!(
            "account={id} out={} cookies={}",
            out.display(),
            envelope.state.cookies.len()
        ),
    );
    eprintln!(
        "⚠ export-state writes session cookies + localStorage in PLAINTEXT to {} — \
         treat the file as a password.",
        out.display()
    );
    if json {
        CliResponse::success(serde_json::json!({
            "exported": out,
            "cookies": envelope.state.cookies.len(),
            "origins": envelope.state.origins.len(),
        }))
        .print_json();
    } else {
        println!(
            "exported account {id} → {} ({} cookies, {} origins)",
            out.display(),
            envelope.state.cookies.len(),
            envelope.state.origins.len()
        );
    }
    0
}

/// Account-plane actions a grant may carry (§8.2 + `login` for agent login).
const ACCOUNT_GRANT_ACTIONS: [&str; 4] = ["navigate", "interact", "irreversible", "login"];

fn account_grant(
    id: &str,
    agent: &str,
    actions: &[String],
    ttl: Option<u64>,
    max_uses: Option<u64>,
    json: bool,
) -> i32 {
    let registry = match oxi_accounts_registry() {
        Ok(r) => r,
        Err(e) => return print_error(&e, "RUNTIME_ERROR", json),
    };
    let record = match registry.get(id) {
        Ok(r) => r,
        Err(_) => {
            return print_error(
                &format!("account {id:?} not found"),
                "ACCOUNT_NOT_FOUND",
                json,
            );
        }
    };
    let mut normalized: Vec<&str> = Vec::new();
    for a in actions {
        let a = a.trim().to_ascii_lowercase();
        if !ACCOUNT_GRANT_ACTIONS.contains(&a.as_str()) {
            return print_error(
                &format!(
                    "unknown action {a:?} — allowed: {}",
                    ACCOUNT_GRANT_ACTIONS.join("|")
                ),
                "INPUT_VALIDATION",
                json,
            );
        }
        if !normalized.iter().any(|x| *x == a) {
            normalized.push(match ACCOUNT_GRANT_ACTIONS.iter().find(|x| **x == a) {
                Some(x) => *x,
                None => unreachable!(),
            });
        }
    }
    if normalized.is_empty() {
        return print_error("at least one action required", "INPUT_VALIDATION", json);
    }
    let ttl = ttl
        .map(|secs| chrono::Duration::seconds(secs as i64))
        .unwrap_or(DEFAULT_CONSENT_TTL);
    // Indefinite grants are forbidden (§5.4); a CLI ttl can still exceed the
    // 90-day ceiling we impose for sanity.
    if ttl > chrono::Duration::days(90) {
        return print_error("ttl must be ≤ 90 days", "INPUT_VALIDATION", json);
    }
    let max_uses = max_uses.unwrap_or(DEFAULT_MAX_USES);

    // The scope-root origin — the same key `createBrowserContext {oxiAccount}`
    // checks against (target.rs `create_account_context`).
    let scope_origin = format!("https://{}", record.scope);
    let rec = ConsentRecord::new(
        ConsentSubject::Account {
            account: id.to_string(),
            agent: agent.to_string(),
        },
        &scope_origin,
        &normalized,
        ttl,
        max_uses,
    );
    let consents = match ConsentStore::open_default() {
        Ok(s) => s,
        Err(e) => {
            return print_error(
                &format!("cannot open consent store: {e}"),
                "RUNTIME_ERROR",
                json,
            );
        }
    };
    if let Err(e) = consents.grant(rec.clone()) {
        return print_error(&format!("grant failed: {e}"), "RUNTIME_ERROR", json);
    }
    audit::record(AuditEvent {
        action: Some("account_grant".to_string()),
        ..audit::event(
            AuditEventKind::AccountState,
            AuditDecision::Allow,
            format!(
                "grant account={id} agent={agent} actions={} expires={}",
                normalized.join("|"),
                rec.expires_at.to_rfc3339()
            ),
        )
    });
    if json {
        CliResponse::success(serde_json::json!({
            "granted": true,
            "consent_id": rec.consent_id,
            "account": id,
            "agent": agent,
            "actions": normalized,
            "expires_at": rec.expires_at.to_rfc3339(),
            "max_uses": rec.max_uses,
        }))
        .print_json();
    } else {
        println!(
            "granted {agent} → {id} [{}] (expires {}, max {} uses)",
            normalized.join(","),
            rec.expires_at.to_rfc3339(),
            rec.max_uses
        );
    }
    0
}

fn account_revoke(id: &str, agent: &str, json: bool) -> i32 {
    let registry = match oxi_accounts_registry() {
        Ok(r) => r,
        Err(e) => return print_error(&e, "RUNTIME_ERROR", json),
    };
    let record = match registry.get(id) {
        Ok(r) => r,
        Err(_) => {
            return print_error(
                &format!("account {id:?} not found"),
                "ACCOUNT_NOT_FOUND",
                json,
            );
        }
    };
    let consents = match ConsentStore::open_default() {
        Ok(s) => s,
        Err(e) => {
            return print_error(
                &format!("cannot open consent store: {e}"),
                "RUNTIME_ERROR",
                json,
            );
        }
    };
    let scope_origin = format!("https://{}", record.scope);
    for action in ACCOUNT_GRANT_ACTIONS {
        if let Err(e) = consents.revoke_account(id, agent, &scope_origin, action) {
            return print_error(&format!("revoke failed: {e}"), "RUNTIME_ERROR", json);
        }
    }
    audit::record(AuditEvent {
        action: Some("account_revoke".to_string()),
        ..audit::event(
            AuditEventKind::AccountState,
            AuditDecision::Deny,
            format!("revoke account={id} agent={agent} all_actions"),
        )
    });
    if json {
        CliResponse::success(serde_json::json!({
            "revoked": true, "account": id, "agent": agent,
        }))
        .print_json();
    } else {
        println!("revoked all grants: {agent} → {id}");
    }
    0
}

// ---------------------------------------------------------------------------
// account context binding (fetch/serve --account)
// ---------------------------------------------------------------------------

/// Create a context bound to `id`: credential mode on, stored envelope
/// restored (fingerprint-gated, fail-closed — FM-L2), audited as
/// `session_restore`. Shared by `fetch --account` and `serve --account`.
pub(crate) async fn bind_account_context(
    browser: &Arc<Browser>,
    id: &str,
) -> Result<Arc<oxibrowser_core::context::BrowserContext>, String> {
    let registry = oxi_accounts_registry()?;
    let record = registry
        .get(id)
        .map_err(|_| format!("account {id:?} not found"))?;
    let store = registry.session_store(id).map_err(|e| e.to_string())?;
    if !store.path_for(&record.scope).exists() {
        return Err(format!(
            "account {id} has no captured session (state: {}) — run `account login` first",
            record.state
        ));
    }

    let ctx = browser
        .new_context(oxibrowser_core::context::ContextConfig {
            label: Some(format!("account:{id}")),
            proxy: None,
        })
        .map_err(|e| e.to_string())?;
    ctx.set_credential_mode(true);

    let session = browser
        .new_session_in(&ctx)
        .await
        .map_err(|e| format!("session init failed: {e}"))?;
    let manager = AccountManager::new(registry.clone());
    let mut guard = session.write().await;
    let current = FingerprintMeta {
        user_agent: guard.effective_ua(),
        ..FingerprintMeta::default()
    };
    let envelope = manager
        .restore(id, &mut guard, &session_keys(), Some(&current))
        .map_err(|e| e.to_string())?;
    eprintln!(
        "account {id}: restored envelope ({} cookies, {} origins) into context {} \
         [credential mode on]",
        envelope.state.cookies.len(),
        envelope.state.origins.len(),
        ctx.id()
    );
    Ok(ctx)
}

// ---------------------------------------------------------------------------
// credential …
// ---------------------------------------------------------------------------

pub(crate) async fn run_credential(command: CredentialCommand, audit: AuditContext) -> i32 {
    match command {
        CredentialCommand::Put {
            agent,
            site,
            kind,
            slug,
            login,
            origins,
            stdin,
            prompt,
        } => credential_put(
            &agent,
            &site,
            &kind,
            slug.as_deref(),
            login.as_deref(),
            &origins,
            stdin,
            prompt,
        ),
        CredentialCommand::Get { id, field } => credential_get(&id, &field, &audit),
        CredentialCommand::List { agent, json } => credential_list(agent.as_deref(), json),
        CredentialCommand::Rm { id, json } => credential_rm(&id, json),
        CredentialCommand::Totp { id } => credential_totp(&id),
        CredentialCommand::Onboard { agent, site } => credential_onboard(&agent, &site),
        CredentialCommand::Authorize {
            id,
            origin,
            actions,
            ttl,
            max_uses,
            json,
        } => credential_authorize(&id, &origin, &actions, ttl, max_uses, json),
        CredentialCommand::Forget { id, origin, json } => credential_forget(&id, &origin, json),
    }
}

/// Read the secret value: `--stdin`, `--prompt`, or auto (prompt on a
/// terminal, stdin otherwise). argv is never consulted.
fn read_secret_value(stdin_flag: bool, prompt_flag: bool) -> Result<String, String> {
    let use_prompt = prompt_flag || (!stdin_flag && std::io::stdin().is_terminal());
    let value = if use_prompt {
        rpassword::prompt_password("credential value: ").map_err(|e| format!("read failed: {e}"))?
    } else {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| format!("stdin read failed: {e}"))?;
        buf.trim_end_matches(['\n', '\r']).to_string()
    };
    if value.is_empty() {
        return Err("empty credential value".to_string());
    }
    Ok(value)
}

/// Wrap a bare base32 secret into an otpauth URI; pass `otpauth://` through.
fn otpauth_or_wrap(value: &str, slug: &str, scope: &str) -> Result<String, String> {
    if value.starts_with("otpauth://") {
        return Ok(value.to_string());
    }
    let compact: String = value.chars().filter(|c| !c.is_whitespace()).collect();
    let valid = !compact.is_empty()
        && compact
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'=');
    if !valid {
        return Err("totp value must be an otpauth:// URI or a base32 secret".to_string());
    }
    Ok(format!(
        "otpauth://totp/{slug}?secret={compact}&issuer={scope}"
    ))
}

fn credential_put(
    agent: &str,
    site: &str,
    kind: &str,
    slug: Option<&str>,
    login: Option<&str>,
    origins: &[String],
    stdin_flag: bool,
    prompt_flag: bool,
) -> i32 {
    let json = false;
    let kind = match CredentialKind::from_str(kind) {
        Ok(k) => k,
        Err(e) => return print_error(&e.to_string(), "INPUT_VALIDATION", json),
    };
    let scope = match scope_of_site(site) {
        Ok(s) => s,
        Err(e) => return print_error(&e, "INPUT_VALIDATION", json),
    };
    let slug = slug.unwrap_or("default").to_string();
    if origins.is_empty() {
        return print_error(
            "at least one --origin is required (e.g. --origin https://github.com/)",
            "INPUT_VALIDATION",
            json,
        );
    }
    let value = match read_secret_value(stdin_flag, prompt_flag) {
        Ok(v) => v,
        Err(e) => return print_error(&e, "INPUT_VALIDATION", json),
    };

    let new = if kind == CredentialKind::Totp {
        let uri = match otpauth_or_wrap(&value, &slug, &scope) {
            Ok(u) => u,
            Err(e) => return print_error(&e, "INPUT_VALIDATION", json),
        };
        NewCredential {
            agent_id: agent.to_string(),
            scope: scope.clone(),
            kind,
            slug: slug.clone(),
            allowed_origins: origins.to_vec(),
            login_hint: login.map(str::to_string),
            password: None,
            otpauth_uri: Some(SecretBox::from_string(uri)),
        }
    } else {
        NewCredential {
            agent_id: agent.to_string(),
            scope: scope.clone(),
            kind,
            slug: slug.clone(),
            allowed_origins: origins.to_vec(),
            login_hint: login.map(str::to_string),
            password: Some(SecretBox::from_string(value)),
            otpauth_uri: None,
        }
    };

    let stored = match provider().put(new) {
        Ok(id) => id,
        Err(e) => return print_error(&e.to_string(), "KEYSTORE_UNAVAILABLE", json),
    };
    audit_sensitive(
        "credential_put",
        &format!(
            "handle={stored} agent={agent} scope={scope} kind={}",
            kind.as_str()
        ),
    );
    println!("{stored}");
    0
}

fn credential_get(id: &str, field: &str, audit: &AuditContext) -> i32 {
    // No --json flag exists for this command by design: values must never
    // flow out as JSON (lower design §6.1).
    let json = false;
    let handle = match CredentialId(id.to_string()).parse() {
        Ok(p) => p,
        Err(e) => return print_error(&e.to_string(), "INPUT_VALIDATION", json),
    };
    let field = match field {
        "password" => Field::Password,
        "otpauth" => Field::Otpauth,
        other => {
            return print_error(
                &format!("unknown field {other:?} (expected password|otpauth)"),
                "INPUT_VALIDATION",
                json,
            );
        }
    };

    // Real policy wiring: deny rules → consent → local-user confirmation.
    let engine = match policy_engine(audit) {
        Ok(e) => e,
        Err(e) => return print_error(&e, "RUNTIME_ERROR", json),
    };
    let top_level =
        match oxibrowser_core::network::Origin::parse(&format!("https://{}/", handle.scope)) {
            Ok(o) => o,
            Err(e) => {
                return print_error(
                    &format!("origin for {}: {e}", handle.scope),
                    "INPUT_VALIDATION",
                    json,
                );
            }
        };
    let action = match field {
        Field::Otpauth => CredentialAction::Mfa,
        Field::Password => CredentialAction::Login,
    };
    let req = UseRequest::new(CredentialId(id.to_string()), top_level, action);
    match engine.authorize_use(&req) {
        Decision::Allow => {}
        Decision::Deny { reason } => {
            return print_error(
                &format!("credential use denied by policy: {reason}"),
                "POLICY_DENIED",
                json,
            );
        }
        Decision::RequireConfirmation { .. } => {
            // The local user IS the confirmation authority; mint + verify.
            let token = engine.issue_confirmation(&req);
            if let Decision::Deny { reason } = engine.verify_confirmation(&token, &req) {
                return print_error(
                    &format!("confirmation invalid: {reason}"),
                    "POLICY_DENIED",
                    json,
                );
            }
            eprintln!("note: granted by local-user confirmation (no consent grant found)");
        }
    }

    let (meta, secret) = match provider().resolve(&CredentialId(id.to_string())) {
        Ok(v) => v,
        Err(e) => return print_error(&e.to_string(), "CREDENTIAL_NOT_FOUND", json),
    };
    // resolve hands back the record's primary value (otpauth for totp,
    // password otherwise) — reject field/kind mismatches before printing.
    let matches = match (field, meta.kind) {
        (Field::Otpauth, CredentialKind::Totp) => true,
        (Field::Password, k) => !matches!(k, CredentialKind::Totp | CredentialKind::Passkey),
        _ => false,
    };
    if !matches {
        return print_error(
            &format!(
                "credential {} is of kind {} — no {} value",
                id,
                meta.kind.as_str(),
                match field {
                    Field::Password => "password",
                    Field::Otpauth => "otpauth",
                }
            ),
            "FIELD_MISMATCH",
            json,
        );
    }
    match secret.expose_str() {
        Ok(v) => {
            println!("{v}");
            0
        }
        Err(e) => print_error(&e.to_string(), "RUNTIME_ERROR", json),
    }
}

#[derive(Clone, Copy)]
enum Field {
    Password,
    Otpauth,
}

fn credential_list(agent: Option<&str>, json: bool) -> i32 {
    let metas = match provider().list(agent) {
        Ok(m) => m,
        Err(e) => return print_error(&e.to_string(), "KEYSTORE_UNAVAILABLE", json),
    };
    if json {
        CliResponse::success(serde_json::json!({ "credentials": metas })).print_json();
        return 0;
    }
    if metas.is_empty() {
        println!("no credentials — store one with `oxibrowser credential put`");
        return 0;
    }
    let mut board = Board::new(&["ID", "KIND", "SCOPE", "SLUG", "LOGIN HINT"]);
    for m in &metas {
        board.row([
            m.id.as_str(),
            m.kind.as_str(),
            m.scope.as_str(),
            m.slug.as_str(),
            m.login_hint.as_deref().unwrap_or("-"),
        ]);
    }
    board.print();
    0
}

fn credential_rm(id: &str, json: bool) -> i32 {
    match provider().delete(&CredentialId(id.to_string())) {
        Ok(()) => {
            audit_sensitive("credential_rm", &format!("handle={id}"));
            if json {
                CliResponse::success(serde_json::json!({ "removed": id })).print_json();
            } else {
                println!("credential removed: {id}");
            }
            0
        }
        Err(e) => print_error(&e.to_string(), "CREDENTIAL_NOT_FOUND", json),
    }
}

fn credential_totp(id: &str) -> i32 {
    let json = false; // human output by design (lower design §6.1)
    let (meta, secret) = match provider().resolve(&CredentialId(id.to_string())) {
        Ok(v) => v,
        Err(e) => return print_error(&e.to_string(), "CREDENTIAL_NOT_FOUND", json),
    };
    if meta.kind != CredentialKind::Totp {
        return print_error(
            &format!(
                "credential {id} is of kind {} — not a totp credential",
                meta.kind.as_str()
            ),
            "FIELD_MISMATCH",
            json,
        );
    }
    let uri = match secret.expose_str() {
        Ok(u) => u,
        Err(e) => return print_error(&e.to_string(), "RUNTIME_ERROR", json),
    };
    let generator = match TotpGenerator::from_otpauth(uri) {
        Ok(g) => g,
        Err(e) => return print_error(&e.to_string(), "INPUT_VALIDATION", json),
    };
    match generator.current() {
        Ok((code, remaining)) => {
            println!("{code} (valid {}s)", remaining.as_secs());
            0
        }
        Err(e) => print_error(&e.to_string(), "RUNTIME_ERROR", json),
    }
}

fn credential_onboard(agent: &str, site: &str) -> i32 {
    let scope = scope_of_site(site).unwrap_or_else(|_| site.to_string());
    println!("OxiBrowser credential onboarding — OS keychain notes");
    println!();
    println!("Items live in your login keychain under:");
    println!("  service: {}", service_key(agent, &scope));
    println!("  account: <kind>/<slug>   (e.g. password/dashboard)");
    println!("  session keys: {SERVICE_PREFIX}/_session-keys/<scope> / aead-key (create-once)");
    println!();
    println!("First use:");
    println!("  - The first `credential put`/`get` may prompt for keychain access;");
    println!("    approving once grants this binary durable access (ACL).");
    println!("  - Unsigned/local builds re-prompt after every rebuild because the");
    println!("    ACL binds to the code signature. Use a stably signed install for");
    println!("    prompt-free agent operation (lower design FM-7).");
    println!();
    println!("Diagnostics:");
    println!(
        "  security dump-keychain ~/Library/Keychains/login.keychain-db | grep {SERVICE_PREFIX}"
    );
    println!(
        "  security find-generic-password -s \"{}\" -a \"password/dashboard\"",
        service_key(agent, &scope)
    );
    println!();
    println!("Smoke test (stores then removes a probe item):");
    println!(
        "  oxibrowser credential put --agent {agent} --site {scope} --kind note --slug onboard-check --origin https://{scope}/ --prompt"
    );
    println!(
        "  oxibrowser credential get --id \"kch:{agent}/{scope}/note/onboard-check\" --field password"
    );
    println!("  oxibrowser credential rm --id \"kch:{agent}/{scope}/note/onboard-check\"");
    0
}

const CREDENTIAL_GRANT_ACTIONS: [&str; 3] = ["login", "mfa", "fill-api-key"];

fn credential_authorize(
    id: &str,
    origin: &str,
    actions: &[String],
    ttl: Option<u64>,
    max_uses: Option<u64>,
    json: bool,
) -> i32 {
    let mut normalized: Vec<&str> = Vec::new();
    for a in actions {
        let a = a.trim().to_ascii_lowercase();
        match CREDENTIAL_GRANT_ACTIONS.iter().find(|x| **x == a) {
            Some(x) => {
                if !normalized.contains(x) {
                    normalized.push(x);
                }
            }
            None => {
                return print_error(
                    &format!(
                        "unknown action {a:?} — allowed: {}",
                        CREDENTIAL_GRANT_ACTIONS.join("|")
                    ),
                    "INPUT_VALIDATION",
                    json,
                );
            }
        }
    }
    if normalized.is_empty() {
        return print_error("at least one action required", "INPUT_VALIDATION", json);
    }
    let ttl = ttl
        .map(|secs| chrono::Duration::seconds(secs as i64))
        .unwrap_or(DEFAULT_CONSENT_TTL);
    if ttl > chrono::Duration::days(90) {
        return print_error("ttl must be ≤ 90 days", "INPUT_VALIDATION", json);
    }
    let max_uses = max_uses.unwrap_or(DEFAULT_MAX_USES);
    let rec = ConsentRecord::new(
        ConsentSubject::Credential {
            credential: CredentialId(id.to_string()),
        },
        origin,
        &normalized,
        ttl,
        max_uses,
    );
    let consents = match ConsentStore::open_default() {
        Ok(s) => s,
        Err(e) => {
            return print_error(
                &format!("cannot open consent store: {e}"),
                "RUNTIME_ERROR",
                json,
            );
        }
    };
    if let Err(e) = consents.grant(rec.clone()) {
        return print_error(&format!("grant failed: {e}"), "RUNTIME_ERROR", json);
    }
    audit::record(AuditEvent {
        action: Some("credential_authorize".to_string()),
        ..audit::event(
            AuditEventKind::SensitiveAction,
            AuditDecision::Allow,
            format!(
                "grant credential={id} origin={origin} actions={} expires={}",
                normalized.join("|"),
                rec.expires_at.to_rfc3339()
            ),
        )
    });
    if json {
        CliResponse::success(serde_json::json!({
            "granted": true,
            "consent_id": rec.consent_id,
            "credential": id,
            "origin": rec.origin,
            "actions": normalized,
            "expires_at": rec.expires_at.to_rfc3339(),
            "max_uses": rec.max_uses,
        }))
        .print_json();
    } else {
        println!(
            "granted {id} @ {} [{}] (expires {}, max {} uses)",
            rec.origin,
            normalized.join(","),
            rec.expires_at.to_rfc3339(),
            rec.max_uses
        );
    }
    0
}

fn credential_forget(id: &str, origin: &str, json: bool) -> i32 {
    let consents = match ConsentStore::open_default() {
        Ok(s) => s,
        Err(e) => {
            return print_error(
                &format!("cannot open consent store: {e}"),
                "RUNTIME_ERROR",
                json,
            );
        }
    };
    for action in CREDENTIAL_GRANT_ACTIONS {
        if let Err(e) = consents.revoke(&CredentialId(id.to_string()), origin, action) {
            return print_error(&format!("revoke failed: {e}"), "RUNTIME_ERROR", json);
        }
    }
    audit_sensitive(
        "credential_forget",
        &format!("credential={id} origin={origin} all_actions"),
    );
    if json {
        CliResponse::success(serde_json::json!({
            "revoked": true, "credential": id, "origin": origin,
        }))
        .print_json();
    } else {
        println!("revoked consents: {id} @ {origin}");
    }
    0
}

// ---------------------------------------------------------------------------
// Tiny aligned-table renderer for the human status boards
// ---------------------------------------------------------------------------

struct Board {
    headers: &'static [&'static str],
    rows: Vec<Vec<String>>,
    widths: Vec<usize>,
}

impl Board {
    fn new(headers: &'static [&'static str]) -> Self {
        let widths = headers.iter().map(|h| h.len()).collect();
        Board {
            headers,
            rows: Vec::new(),
            widths,
        }
    }

    fn row(&mut self, cells: [&str; 5]) {
        for (i, cell) in cells.iter().enumerate() {
            self.widths[i] = self.widths[i].max(cell.len());
        }
        self.rows
            .push(cells.iter().map(|c| (*c).to_string()).collect());
    }

    fn print(&self) {
        let header: Vec<String> = self
            .headers
            .iter()
            .enumerate()
            .map(|(i, h)| format!("{:<width$}", h, width = self.widths[i]))
            .collect();
        println!("{}", header.join("  "));
        for row in &self.rows {
            let line: Vec<String> = row
                .iter()
                .enumerate()
                .map(|(i, c)| format!("{:<width$}", c, width = self.widths[i]))
                .collect();
            println!("{}", line.join("  "));
        }
    }
}
