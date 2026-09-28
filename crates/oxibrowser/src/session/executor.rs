//! Session command executor.
//!
//! Takes a parsed `SessionCommand` and executes it against the browser
//! and tab manager, returning a `CliResponse`.

use crate::output::CliResponse;
use crate::session::parser::SessionCommand;
use crate::session::tab_manager::TabManager;
use oxibrowser_core::Browser;
use oxibrowser_core::account::{
    AccountManager, AccountRegistry, AccountState, AgentLoginOutcome, LoginMode, LoginOrchestrator,
};
use serde_json::Value;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

/// The account record an agent-login outcome carried (all variants hold one).
fn outcome_record(outcome: &AgentLoginOutcome) -> Option<&oxibrowser_core::account::AccountRecord> {
    match outcome {
        AgentLoginOutcome::Challenge { record, .. } => Some(record.as_ref()),
        AgentLoginOutcome::Captured { record, .. }
        | AgentLoginOutcome::MfaEscalation { record, .. }
        | AgentLoginOutcome::PolicyViolation { record, .. }
        | AgentLoginOutcome::NeedsLogin { record, .. } => Some(record),
    }
}

/// REPL-side account state: the login orchestrator (lazily built over the
/// REPL browser) plus at most one active login window.
pub struct AccountRuntime {
    browser: Arc<Browser>,
    orch: Option<Arc<LoginOrchestrator>>,
    active: Option<ActiveReplLogin>,
}

struct ActiveReplLogin {
    login_id: String,
    account_id: String,
    session: Arc<RwLock<oxibrowser_core::session::Session>>,
}

impl AccountRuntime {
    pub fn new(browser: Arc<Browser>) -> Self {
        AccountRuntime {
            browser,
            orch: None,
            active: None,
        }
    }

    fn orch(&mut self) -> Result<Arc<LoginOrchestrator>, String> {
        if let Some(orch) = &self.orch {
            return Ok(orch.clone());
        }
        let base = AccountRegistry::default_dir()
            .ok_or_else(|| "HOME is not set — cannot locate ~/.oxibrowser".to_string())?;
        let registry = AccountRegistry::open(base).map_err(|e| e.to_string())?;
        let manager = Arc::new(AccountManager::new(registry));
        let orch = Arc::new(LoginOrchestrator::new(
            manager,
            self.browser.clone(),
            Arc::new(oxibrowser_credentials::KeyringKeyProvider::new()),
        ));
        self.orch = Some(orch.clone());
        Ok(orch)
    }

    /// Credential plane for unattended logins (M-D): keychain provider +
    /// deny-first policy engine over `~/.oxibrowser/consents.jsonl`, auditing
    /// to the default audit path.
    fn agent_source(&self) -> Result<Arc<oxibrowser_credentials::BrokerSource>, String> {
        let provider: Arc<dyn oxibrowser_credentials::CredentialProvider> =
            Arc::new(oxibrowser_credentials::KeyringProvider::new());
        let consents = oxibrowser_credentials::ConsentStore::open_default()
            .map_err(|e| format!("cannot open consent store: {e}"))?;
        let audit_path = oxibrowser_core::security::audit::default_path()
            .ok_or_else(|| "HOME is not set — cannot locate the audit log".to_string())?;
        let audit = Arc::new(
            oxibrowser_core::security::audit::AuditLog::open(audit_path)
                .map_err(|e| format!("cannot open audit log: {e}"))?,
        );
        let engine = Arc::new(oxibrowser_credentials::PolicyEngine::new(
            Vec::new(),
            consents,
            audit,
        ));
        Ok(Arc::new(oxibrowser_credentials::BrokerSource::new(
            provider, engine,
        )))
    }
}

/// Execute a session command and return a CliResponse.
pub async fn execute(
    cmd: SessionCommand,
    browser: &Browser,
    manager: &mut TabManager,
    accounts: &mut AccountRuntime,
) -> CliResponse {
    let start = Instant::now();
    let result = execute_inner(cmd, browser, manager, accounts).await;
    let elapsed_ms = start.elapsed().as_millis() as u64;

    match result {
        Ok((data, tab_id)) => CliResponse::success_with_meta(data, tab_id, elapsed_ms),
        Err(resp) => resp,
    }
}

type ExecResult = Result<(Value, Option<String>), CliResponse>;

// ExecResult's error variant embeds a full CliResponse payload by design —
// the REPL surfaces structured errors to the agent, not strings.
#[allow(clippy::result_large_err)]
async fn execute_inner(
    cmd: SessionCommand,
    browser: &Browser,
    manager: &mut TabManager,
    accounts: &mut AccountRuntime,
) -> ExecResult {
    match cmd {
        // ---- Account surface (§7.2) ----
        SessionCommand::AccountList => {
            let orch = accounts
                .orch()
                .map_err(|e| CliResponse::error(e, "RUNTIME_ERROR"))?;
            let records = orch
                .manager()
                .registry()
                .list()
                .map_err(|e| CliResponse::error(e.to_string(), "RUNTIME_ERROR"))?;
            let accounts_json: Vec<Value> = records
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "account_id": r.account_id,
                        "scope": r.scope,
                        "state": r.state.as_str(),
                        "state_detail": r.state_detail,
                        "login_hint": r.identity.login_hint,
                    })
                })
                .collect();
            Ok((serde_json::json!({ "accounts": accounts_json }), None))
        }

        SessionCommand::AccountStatus { id } => {
            let orch = accounts
                .orch()
                .map_err(|e| CliResponse::error(e, "RUNTIME_ERROR"))?;
            let record = orch.manager().registry().get(&id).map_err(|_| {
                CliResponse::error(format!("account {id:?} not found"), "ACCOUNT_NOT_FOUND")
            })?;
            Ok((
                serde_json::json!({
                    "account_id": record.account_id,
                    "state": record.state.as_str(),
                    "detail": record.state_detail,
                    "session_summary": record.session_summary,
                }),
                None,
            ))
        }

        SessionCommand::AccountLogout { id } => {
            let orch = accounts
                .orch()
                .map_err(|e| CliResponse::error(e, "RUNTIME_ERROR"))?;
            let record = orch
                .manager()
                .logout(&id)
                .map_err(|e| CliResponse::error(e.to_string(), "RUNTIME_ERROR"))?;
            Ok((
                serde_json::json!({ "account_id": id, "state": record.state.as_str() }),
                None,
            ))
        }

        SessionCommand::AccountLogin {
            id,
            mode,
            storage_state,
            cookies,
            agent,
        } => {
            let orch = accounts
                .orch()
                .map_err(|e| CliResponse::error(e, "RUNTIME_ERROR"))?;
            let mode = mode.as_deref().unwrap_or("user");
            match mode {
                "agent" => {
                    let source = accounts
                        .agent_source()
                        .map_err(|e| CliResponse::error(e, "RUNTIME_ERROR"))?;
                    let engine = oxibrowser_core::account::AgentLoginEngine::new(
                        orch.shared_manager(),
                        orch.shared_keys(),
                        source,
                        agent.as_deref().unwrap_or("main"),
                    )
                    .with_events(orch.event_sender());
                    let (_ctx, session) = orch
                        .open_account_context(&id)
                        .await
                        .map_err(|e| CliResponse::error(e.to_string(), "RUNTIME_ERROR"))?;
                    let mut guard = session.write().await;
                    let outcome = engine.login(&id, &mut guard).await;
                    drop(guard);
                    match outcome {
                        Ok(outcome) => Ok((
                            serde_json::json!({
                                "account_id": outcome_record(&outcome).map(|r| r.account_id.clone()),
                                "state": outcome.state(),
                                "detail": outcome.detail(),
                                "retryable": outcome.retryable(),
                            }),
                            None,
                        )),
                        Err(e) => Err(CliResponse::error(e.to_string(), "RUNTIME_ERROR")),
                    }
                }
                "import" => {
                    let state = match (storage_state.as_deref(), cookies.as_deref()) {
                        (Some(path), None) => {
                            let text = std::fs::read_to_string(path).map_err(|e| {
                                CliResponse::error(
                                    format!("cannot read {path}: {e}"),
                                    "INPUT_VALIDATION",
                                )
                            })?;
                            serde_json::from_str(&text).map_err(|e| {
                                CliResponse::error(
                                    format!("{path} is not storageState JSON: {e}"),
                                    "INPUT_VALIDATION",
                                )
                            })?
                        }
                        (None, Some(path)) => {
                            let text = std::fs::read_to_string(path).map_err(|e| {
                                CliResponse::error(
                                    format!("cannot read {path}: {e}"),
                                    "INPUT_VALIDATION",
                                )
                            })?;
                            oxibrowser_core::storage_state::StorageState::from_netscape(&text)
                        }
                        _ => {
                            return Err(CliResponse::error(
                                "choose exactly one of --storage-state / --cookies",
                                "INPUT_VALIDATION",
                            ));
                        }
                    };
                    let outcome = orch
                        .import(&id, &state)
                        .await
                        .map_err(|e| CliResponse::error(e.to_string(), "RUNTIME_ERROR"))?;
                    Ok((
                        serde_json::json!({
                            "account_id": id,
                            "state": outcome.record.state.as_str(),
                            "detail": outcome.record.state_detail,
                            "probe": outcome.probe,
                        }),
                        None,
                    ))
                }
                "user" => {
                    if accounts.active.is_some() {
                        return Err(CliResponse::error(
                            "a login window is already active — finish it with `takeover done|abort`",
                            "RUNTIME_ERROR",
                        ));
                    }
                    let handle = orch
                        .begin_login(&id, LoginMode::User, None)
                        .map_err(|e| CliResponse::error(e.to_string(), "RUNTIME_ERROR"))?;
                    let (ctx, login_session) = match orch.open_account_context(&id).await {
                        Ok(s) => s,
                        Err(e) => {
                            let _ = orch.abort(&handle.login_id).await;
                            return Err(CliResponse::error(e.to_string(), "RUNTIME_ERROR"));
                        }
                    };
                    // A drivable tab in the same account context (shared
                    // jar/storage) — the REPL's hands for the login flow.
                    let tab = match accounts.browser.new_tab_in(&ctx).await {
                        Ok(t) => t,
                        Err(e) => {
                            let _ = orch.abort(&handle.login_id).await;
                            return Err(CliResponse::error(
                                format!("tab init failed: {e}"),
                                "RUNTIME_ERROR",
                            ));
                        }
                    };
                    let tab_id = manager.insert_tab(tab);
                    accounts.active = Some(ActiveReplLogin {
                        login_id: handle.login_id.clone(),
                        account_id: id.clone(),
                        session: login_session,
                    });
                    Ok((
                        serde_json::json!({
                            "login_id": handle.login_id,
                            "account_id": id,
                            "tab_id": tab_id,
                            "state": "logging_in",
                            "timeout_ms": handle.timeout_ms,
                            "next": "drive goto/type/click on the tab, then `takeover done` or `takeover abort`",
                        }),
                        Some(tab_id),
                    ))
                }
                other => Err(CliResponse::error(
                    format!("invalid --mode {other:?} (expected \"user\" or \"import\")"),
                    "INPUT_VALIDATION",
                )),
            }
        }

        SessionCommand::Takeover { action } => {
            let orch = accounts
                .orch()
                .map_err(|e| CliResponse::error(e, "RUNTIME_ERROR"))?;
            match action.as_deref() {
                None | Some("status") => {
                    let active = accounts.active.as_ref().map(|a| {
                        serde_json::json!({
                            "login_id": a.login_id,
                            "account_id": a.account_id,
                        })
                    });
                    Ok((
                        serde_json::json!({ "active": active.is_some(), "login": active }),
                        None,
                    ))
                }
                Some("done") => {
                    let active = accounts.active.take().ok_or_else(|| {
                        CliResponse::error(
                            "no active login — start one with account_login",
                            "RUNTIME_ERROR",
                        )
                    })?;
                    let mut guard = active.session.write().await;
                    let verdict = orch
                        .complete_login(&active.login_id, &mut guard, true)
                        .await;
                    drop(guard);
                    match verdict {
                        Ok(_) => {
                            let record = orch.manager().registry().get(&active.account_id).ok();
                            Ok((
                                serde_json::json!({
                                    "login_id": active.login_id,
                                    "state": record.as_ref().map(|r| r.state.as_str()).unwrap_or("valid"),
                                }),
                                None,
                            ))
                        }
                        Err(e) => Err(CliResponse::error(e.to_string(), "RUNTIME_ERROR")),
                    }
                }
                Some("abort") => {
                    let active = accounts.active.take().ok_or_else(|| {
                        CliResponse::error(
                            "no active login — start one with account_login",
                            "RUNTIME_ERROR",
                        )
                    })?;
                    orch.abort(&active.login_id)
                        .await
                        .map_err(|e| CliResponse::error(e.to_string(), "RUNTIME_ERROR"))?;
                    Ok((
                        serde_json::json!({ "login_id": active.login_id, "state": AccountState::NeedsLogin.as_str() }),
                        None,
                    ))
                }
                Some(other) => Err(CliResponse::error(
                    format!("unknown takeover action {other:?} (expected done|abort|status)"),
                    "INPUT_VALIDATION",
                )),
            }
        }

        // ---- Tab lifecycle ----
        SessionCommand::New => {
            let tab_id = manager
                .create_tab(browser)
                .await
                .map_err(|e| CliResponse::error(e, "RUNTIME_ERROR"))?;
            Ok((serde_json::json!({ "tab_id": tab_id }), Some(tab_id)))
        }

        SessionCommand::Close { tab_id } => {
            manager
                .close_tab(&tab_id)
                .await
                .map_err(|e| CliResponse::error(e, "RUNTIME_ERROR"))?;
            Ok((serde_json::json!({ "closed": tab_id }), None))
        }

        SessionCommand::CloseAll => {
            let count = manager.len();
            manager.close_all().await;
            Ok((serde_json::json!({ "closed_all": count }), None))
        }

        SessionCommand::List => {
            let tabs = manager.list();
            Ok((serde_json::json!({ "tabs": tabs }), None))
        }

        // ---- Navigation ----
        SessionCommand::Goto {
            tab_id,
            url,
            wait_selector,
            timeout_ms,
        } => {
            let tab = get_tab(manager, &tab_id)?;
            let nav = tab.goto(&url).await.map_err(|e| {
                CliResponse::error(format!("{e}"), crate::output::core_error_code(&e))
            })?;

            // Wait for selector if requested
            if let Some(sel) = wait_selector {
                let timeout = timeout_ms.unwrap_or(5000);
                tab.wait_for(&sel, timeout).await.map_err(|e| {
                    CliResponse::error(format!("{e}"), crate::output::core_error_code(&e))
                })?;
            }

            let data = serde_json::json!({
                "url": nav.url,
                "title": nav.title,
                "status": nav.status,
            });
            Ok((data, Some(tab_id)))
        }

        SessionCommand::Back { tab_id } => {
            let tab = get_tab(manager, &tab_id)?;
            let nav = tab.back().await.map_err(|e| {
                CliResponse::error(format!("{e}"), crate::output::core_error_code(&e))
            })?;
            Ok((
                serde_json::json!({
                    "url": nav.url,
                    "title": nav.title,
                    "status": nav.status,
                }),
                Some(tab_id),
            ))
        }

        SessionCommand::Forward { tab_id } => {
            let tab = get_tab(manager, &tab_id)?;
            let nav = tab.forward().await.map_err(|e| {
                CliResponse::error(format!("{e}"), crate::output::core_error_code(&e))
            })?;
            Ok((
                serde_json::json!({
                    "url": nav.url,
                    "title": nav.title,
                    "status": nav.status,
                }),
                Some(tab_id),
            ))
        }

        SessionCommand::Reload { tab_id } => {
            let tab = get_tab(manager, &tab_id)?;
            let nav = tab.reload().await.map_err(|e| {
                CliResponse::error(format!("{e}"), crate::output::core_error_code(&e))
            })?;
            Ok((
                serde_json::json!({
                    "url": nav.url,
                    "title": nav.title,
                    "status": nav.status,
                }),
                Some(tab_id),
            ))
        }

        // ---- Interaction ----
        SessionCommand::Click { tab_id, selector } => {
            let tab = get_tab(manager, &tab_id)?;
            tab.click(&selector).await.map_err(|e| {
                CliResponse::error(format!("{e}"), crate::output::core_error_code(&e))
            })?;
            Ok((serde_json::json!({ "clicked": selector }), Some(tab_id)))
        }

        SessionCommand::Fill {
            tab_id,
            selector,
            value,
        } => {
            let tab = get_tab(manager, &tab_id)?;
            // Credential-mode literal gate (design §6.2): passwords flow only
            // through the audited credential path.
            if tab.in_credential_mode().await && tab.selector_targets_password(&selector).await {
                return Err(CliResponse::error(
                    "passwordFillRequiresCredential: fill password fields via the credential path (`account login` / OXI.fillCredential)",
                    "POLICY_DENIED",
                ));
            }
            tab.fill(&selector, &value).await.map_err(|e| {
                CliResponse::error(format!("{e}"), crate::output::core_error_code(&e))
            })?;
            Ok((
                serde_json::json!({
                    "filled": selector,
                    "value_length": value.len(),
                }),
                Some(tab_id),
            ))
        }

        SessionCommand::Press { tab_id, key } => {
            let tab = get_tab(manager, &tab_id)?;
            tab.press(&key).await.map_err(|e| {
                CliResponse::error(format!("{e}"), crate::output::core_error_code(&e))
            })?;
            Ok((serde_json::json!({ "pressed": key }), Some(tab_id)))
        }

        SessionCommand::Type {
            tab_id,
            selector,
            text,
        } => {
            let tab = get_tab(manager, &tab_id)?;
            // Same credential-mode literal gate as `Fill` — typed keystrokes
            // into a password field are a literal secret too.
            if tab.in_credential_mode().await && tab.selector_targets_password(&selector).await {
                return Err(CliResponse::error(
                    "passwordFillRequiresCredential: fill password fields via the credential path (`account login` / OXI.fillCredential)",
                    "POLICY_DENIED",
                ));
            }
            tab.r#type(&selector, &text).await.map_err(|e| {
                CliResponse::error(format!("{e}"), crate::output::core_error_code(&e))
            })?;
            Ok((
                serde_json::json!({
                    "typed": selector,
                    "text_length": text.len(),
                }),
                Some(tab_id),
            ))
        }

        SessionCommand::Select {
            tab_id,
            selector,
            value,
        } => {
            let tab = get_tab(manager, &tab_id)?;
            tab.select_option(&selector, &value).await.map_err(|e| {
                CliResponse::error(format!("{e}"), crate::output::core_error_code(&e))
            })?;
            Ok((
                serde_json::json!({
                    "selected": selector,
                    "value": value,
                }),
                Some(tab_id),
            ))
        }

        SessionCommand::Check { tab_id, selector } => {
            let tab = get_tab(manager, &tab_id)?;
            tab.check(&selector).await.map_err(|e| {
                CliResponse::error(format!("{e}"), crate::output::core_error_code(&e))
            })?;
            Ok((serde_json::json!({ "checked": selector }), Some(tab_id)))
        }

        SessionCommand::Uncheck { tab_id, selector } => {
            let tab = get_tab(manager, &tab_id)?;
            tab.uncheck(&selector).await.map_err(|e| {
                CliResponse::error(format!("{e}"), crate::output::core_error_code(&e))
            })?;
            Ok((serde_json::json!({ "unchecked": selector }), Some(tab_id)))
        }

        SessionCommand::Scroll { tab_id, dx, dy } => {
            let tab = get_tab(manager, &tab_id)?;
            tab.scroll(dx, dy).await.map_err(|e| {
                CliResponse::error(format!("{e}"), crate::output::core_error_code(&e))
            })?;
            Ok((
                serde_json::json!({ "scrolled": { "dx": dx, "dy": dy } }),
                Some(tab_id),
            ))
        }

        // ---- JS evaluation ----
        SessionCommand::Eval {
            tab_id,
            expression,
            await_promise,
        } => {
            let tab = get_tab(manager, &tab_id)?;
            let value = if await_promise {
                tab.evaluate_await(&expression).await.map_err(|e| {
                    CliResponse::error(format!("{e}"), crate::output::core_error_code(&e))
                })?
            } else {
                tab.evaluate(&expression).await.map_err(|e| {
                    CliResponse::error(format!("{e}"), crate::output::core_error_code(&e))
                })?
            };
            Ok((serde_json::json!({ "value": value }), Some(tab_id)))
        }

        // ---- Extraction ----
        SessionCommand::Extract {
            tab_id,
            selector,
            all,
            attrs,
            links,
            title,
            text,
            markdown,
            max_bytes,
        } => {
            let tab = get_tab(manager, &tab_id)?;
            let content = tab.content().await.map_err(|e| {
                CliResponse::error(format!("{e}"), crate::output::core_error_code(&e))
            })?;

            let mut data = serde_json::Map::new();

            if title {
                data.insert("title".into(), Value::String(content.title.clone()));
            }
            if links {
                let hrefs = tab.query_all("a[href]").await.map_err(|e| {
                    CliResponse::error(format!("{e}"), crate::output::core_error_code(&e))
                })?;
                data.insert(
                    "links".into(),
                    Value::Array(hrefs.into_iter().map(Value::String).collect()),
                );
            }
            if text {
                data.insert("text".into(), Value::String(content.markdown.clone()));
            }
            if markdown {
                data.insert("markdown".into(), Value::String(content.markdown.clone()));
            }

            if let Some(ref sel) = selector {
                let matches = tab.query_all(sel).await.map_err(|e| {
                    CliResponse::error(format!("{e}"), crate::output::core_error_code(&e))
                })?;
                let requested_attrs: Vec<&str> = attrs
                    .as_deref()
                    .map(|a| a.split(',').map(|s| s.trim()).collect())
                    .unwrap_or_default();

                if all {
                    let items: Vec<Value> = matches
                        .into_iter()
                        .map(|t| {
                            if requested_attrs.is_empty() {
                                Value::String(t)
                            } else {
                                serde_json::json!({ "text": t })
                            }
                        })
                        .collect();
                    data.insert("selector".into(), Value::String(sel.clone()));
                    data.insert("count".into(), serde_json::json!(items.len()));
                    data.insert("items".into(), Value::Array(items));
                } else {
                    let m = matches.first().cloned().unwrap_or_default();
                    data.insert("selector".into(), Value::String(sel.clone()));
                    data.insert("match".into(), Value::String(m));
                }
            }

            // Default: title + text if nothing specific requested
            if !title && !links && !text && !markdown && selector.is_none() {
                data.insert("title".into(), Value::String(content.title.clone()));
                data.insert("text".into(), Value::String(content.markdown.clone()));
            }

            let mut data_val = Value::Object(data);
            if let Some(mb) = max_bytes {
                crate::output::truncate_fields(&mut data_val, mb);
            }

            Ok((data_val, Some(tab_id)))
        }

        // ---- Content ----
        SessionCommand::Content {
            tab_id,
            format,
            max_bytes,
        } => {
            let tab = get_tab(manager, &tab_id)?;
            let content = tab.content().await.map_err(|e| {
                CliResponse::error(format!("{e}"), crate::output::core_error_code(&e))
            })?;

            let mut data = match format.as_str() {
                "html" => serde_json::json!({
                    "url": content.url,
                    "title": content.title,
                    "status": content.status,
                    "html": content.html,
                }),
                "text" => serde_json::json!({
                    "url": content.url,
                    "title": content.title,
                    "status": content.status,
                    "text": content.markdown,
                }),
                _ => serde_json::json!({
                    "url": content.url,
                    "title": content.title,
                    "status": content.status,
                    "markdown": content.markdown,
                }),
            };

            if let Some(mb) = max_bytes {
                crate::output::truncate_fields(&mut data, mb);
            }

            Ok((data, Some(tab_id)))
        }

        // ---- Screenshot ----
        SessionCommand::Screenshot {
            tab_id,
            output_path,
            width,
        } => {
            let tab = get_tab(manager, &tab_id)?;
            let w = width.unwrap_or(800);
            let png = tab.screenshot(w).await.map_err(|e| {
                CliResponse::error(format!("{e}"), crate::output::core_error_code(&e))
            })?;

            match output_path {
                Some(path) => {
                    std::fs::write(&path, &png).map_err(|e| {
                        CliResponse::error(format!("write failed: {e}"), "IO_ERROR")
                    })?;
                    Ok((
                        serde_json::json!({
                            "saved": path,
                            "size": png.len(),
                            "width": w,
                        }),
                        Some(tab_id),
                    ))
                }
                None => {
                    // Return base64-encoded PNG
                    use std::io::Write;
                    let mut buf = Vec::new();
                    {
                        let mut encoder = base64::write::EncoderWriter::new(
                            &mut buf,
                            &base64::engine::general_purpose::STANDARD,
                        );
                        encoder.write_all(&png).map_err(|e| {
                            CliResponse::error(format!("base64 encode failed: {e}"), "INTERNAL")
                        })?;
                    }
                    let b64 = String::from_utf8(buf).map_err(|e| {
                        CliResponse::error(format!("base64 encode failed: {e}"), "INTERNAL")
                    })?;
                    Ok((
                        serde_json::json!({
                            "screenshot": b64,
                            "size": png.len(),
                            "width": w,
                            "encoding": "base64",
                        }),
                        Some(tab_id),
                    ))
                }
            }
        }

        // ---- Wait ----
        SessionCommand::Wait {
            tab_id,
            selector,
            timeout_ms,
        } => {
            let tab = get_tab(manager, &tab_id)?;
            let timeout = timeout_ms.unwrap_or(5000);
            tab.wait_for(&selector, timeout).await.map_err(|e| {
                CliResponse::error(format!("{e}"), crate::output::core_error_code(&e))
            })?;
            Ok((
                serde_json::json!({
                    "waited": selector,
                    "timeout_ms": timeout,
                }),
                Some(tab_id),
            ))
        }

        // ---- Storage state ----
        SessionCommand::SaveState { path } => {
            let (tab, tab_id) = get_active_tab(manager)?;
            let state = tab.export_storage_state().await.map_err(|e| {
                CliResponse::error(format!("{e}"), crate::output::core_error_code(&e))
            })?;
            let cookies = state["cookies"].as_array().map(|a| a.len()).unwrap_or(0);
            let origins = state["origins"].as_array().map(|a| a.len()).unwrap_or(0);
            let json = serde_json::to_string_pretty(&state)
                .map_err(|e| CliResponse::error(format!("serialize failed: {e}"), "INTERNAL"))?;
            let bytes = json.len();
            std::fs::write(&path, &json)
                .map_err(|e| CliResponse::error(format!("write failed: {e}"), "IO_ERROR"))?;
            Ok((
                serde_json::json!({
                    "saved": path,
                    "bytes": bytes,
                    "cookies": cookies,
                    "origins": origins,
                }),
                Some(tab_id),
            ))
        }

        SessionCommand::LoadState { path } => {
            let (tab, tab_id) = get_active_tab(manager)?;
            let text = std::fs::read_to_string(&path)
                .map_err(|e| CliResponse::error(format!("read failed: {e}"), "IO_ERROR"))?;
            let state: Value = serde_json::from_str(&text)
                .map_err(|e| CliResponse::error(format!("parse failed: {e}"), "PARSE_ERROR"))?;
            let cookies = state["cookies"].as_array().map(|a| a.len()).unwrap_or(0);
            let origins = state["origins"].as_array().map(|a| a.len()).unwrap_or(0);
            tab.import_storage_state(state).await.map_err(|e| {
                CliResponse::error(format!("{e}"), crate::output::core_error_code(&e))
            })?;
            Ok((
                serde_json::json!({
                    "loaded": path,
                    "cookies": cookies,
                    "origins": origins,
                }),
                Some(tab_id),
            ))
        }

        // ---- Help ----
        SessionCommand::Help => Ok((
            serde_json::json!({
                "commands": [
                    "new",
                    "goto <tab_id> <url> [--wait <selector>] [--timeout <ms>]",
                    "back <tab_id>",
                    "forward <tab_id>",
                    "reload <tab_id>",
                    "click <tab_id> <selector>",
                    "fill <tab_id> <selector> <value>",
                    "press <tab_id> <key>",
                    "type <tab_id> <selector> <text>",
                    "select <tab_id> <selector> <value>",
                    "check <tab_id> <selector>",
                    "uncheck <tab_id> <selector>",
                    "scroll <tab_id> <dx> <dy>",
                    "eval <tab_id> <expression> [--await]",
                    "extract <tab_id> [--selector <s>] [--all] [--attrs a,b] [--links] [--title] [--text] [--markdown] [--max-bytes N]",
                    "content <tab_id> [--format markdown|html|text] [--max-bytes N]",
                    "screenshot <tab_id> [-o path] [--width N]",
                    "wait <tab_id> <selector> [--timeout <ms>]",
                    "close <tab_id>",
                    "close --all",
                    "list",
                    "save-state <path>",
                    "load-state <path>",
                    "account_list",
                    "account_status <id>",
                    "account_logout <id>",
                    "account_login <id> [--mode user|agent|import] [--agent ID] [--storage-state F] [--cookies F]",
                    "takeover [status|done|abort]",
                    "help",
                    "exit",
                ]
            }),
            None,
        )),

        // ---- Exit ----
        SessionCommand::Exit => {
            // Not reached in normal flow (handled by caller), but provide a response anyway.
            Ok((serde_json::json!({ "exit": true }), None))
        }
    }
}

/// Get a tab from the manager, returning an error response if not found.
#[allow(clippy::result_large_err)]
fn get_tab(manager: &TabManager, tab_id: &str) -> Result<oxibrowser_core::Tab, CliResponse> {
    manager
        .get(tab_id)
        .cloned()
        .ok_or_else(|| CliResponse::error(format!("tab not found: {tab_id}"), "TAB_NOT_FOUND"))
}

/// Get the active tab (most recently created, e.g. `t2` over `t1`) for
/// session-level commands that take no tab id. Errors if no tabs exist.
#[allow(clippy::result_large_err)]
fn get_active_tab(manager: &TabManager) -> Result<(oxibrowser_core::Tab, String), CliResponse> {
    let tab_id = manager
        .list()
        .last()
        .cloned()
        .ok_or_else(|| CliResponse::error("no active tab: run 'new' first", "TAB_NOT_FOUND"))?;
    let tab = get_tab(manager, &tab_id)?;
    Ok((tab, tab_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxibrowser_core::BrowserConfig;

    const FORM_HTML: &str = "data:text/html,<html><body><form>\
        <input id='user' type='text'><input id='pw' type='password'>\
        </form></body></html>";

    /// Credential-mode literal gate (design §6.2, F5): REPL `Fill`/`Type`
    /// into an `input[type=password]` is rejected while the tab's context is
    /// in credential mode; non-password targets and non-credential mode pass.
    #[tokio::test]
    async fn fill_and_type_reject_password_literals_in_credential_mode() {
        let browser = Arc::new(Browser::new(BrowserConfig::headless()).await.unwrap());
        browser.default_context().set_credential_mode(true);
        let mut manager = TabManager::new();
        let mut accounts = AccountRuntime::new(browser.clone());

        let tab = browser.new_tab().await.unwrap();
        tab.goto(FORM_HTML).await.unwrap();
        let tab_id = manager.insert_tab(tab);

        // Fill into the password field → gated.
        let resp = execute(
            SessionCommand::Fill {
                tab_id: tab_id.clone(),
                selector: "#pw".into(),
                value: "hunter2".into(),
            },
            &browser,
            &mut manager,
            &mut accounts,
        )
        .await;
        assert_eq!(
            resp.error_code.as_deref(),
            Some("POLICY_DENIED"),
            "{resp:?}"
        );
        assert!(
            resp.error
                .as_deref()
                .unwrap_or("")
                .contains("passwordFillRequiresCredential"),
            "{resp:?}"
        );

        // Type into the password field → gated too.
        let resp = execute(
            SessionCommand::Type {
                tab_id: tab_id.clone(),
                selector: "#pw".into(),
                text: "hunter2".into(),
            },
            &browser,
            &mut manager,
            &mut accounts,
        )
        .await;
        assert_eq!(
            resp.error_code.as_deref(),
            Some("POLICY_DENIED"),
            "{resp:?}"
        );

        // Non-password targets keep flowing.
        let resp = execute(
            SessionCommand::Fill {
                tab_id: tab_id.clone(),
                selector: "#user".into(),
                value: "user@example.com".into(),
            },
            &browser,
            &mut manager,
            &mut accounts,
        )
        .await;
        assert!(resp.ok, "non-password fill must pass: {resp:?}");

        // Field is untouched and mode off → the same literal passes.
        browser.default_context().set_credential_mode(false);
        let resp = execute(
            SessionCommand::Fill {
                tab_id: tab_id.clone(),
                selector: "#pw".into(),
                value: "hunter2".into(),
            },
            &browser,
            &mut manager,
            &mut accounts,
        )
        .await;
        assert!(resp.ok, "fill must pass outside credential mode: {resp:?}");
    }
}
