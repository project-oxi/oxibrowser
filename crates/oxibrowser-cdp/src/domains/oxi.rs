//! OXI domain — OxiBrowser AI agent extensions.
//!
//! Provides AI-agent-friendly methods beyond standard CDP:
//! - `OXI.getMarkdown` — page content as Markdown
//! - `OXI.getPageInfo` — URL, title, status
//! - `OXI.getStructuredPage` — headings, links, meta as structured JSON
//! - `OXI.getAccessibilityTree` — semantic tree of what's on the page
//! - `OXI.getInteractiveElements` — interactive elements in document order,
//!   each with a stable `ref` for the `OXI.*Ref` actions
//! - `OXI.getBoxModelScreenshot` — PNG with colored boxes for each element
//! - `OXI.clickRef` / `OXI.fillRef` / `OXI.waitRef` — act on stable refs
//!   (generation + fingerprint validated; drift answers "stale ref —
//!   re-observe")
//! - `OXI.ariaSnapshot` — Playwright-style YAML of the visible tree, with
//!   `[ref=eN]` markers on interactive elements
//! - `OXI.exportStorageState` / `OXI.importStorageState` — cookies +
//!   localStorage snapshot round-trip (Playwright-compatible `StorageState`)
//! - `OXI.credentialList` / `OXI.fillCredential` /
//!   `OXI.resolveConfirmation` — credential broker surface (M4, design
//!   `2026-09-27` §6.3). Values never cross the wire: fills inject broker
//!   -resolved secrets, responses carry `{filled: true, masked: true}` and
//!   handles only. Requires the server to be built with `with_credentials`.

use crate::credential::{self, CredentialBroker, FieldKind};
use crate::domains::{DispatchContext, DomainResult};
use crate::protocol::CdpError;
use crate::refs::RefRegistry;
use oxibrowser_core::account::AccountRecord;
use oxibrowser_core::network::origin_policy::{Decision, Origin, OriginPolicy};
use oxibrowser_credentials::{CredentialId, UseRequest};
use serde_json::{Value, json};
use std::sync::Arc;

/// Handle OXI domain methods.
pub async fn handle(method: &str, params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    match method {
        "getMarkdown" => get_markdown(ctx).await,
        "getPageInfo" => get_page_info(ctx).await,
        "getStructuredPage" => get_structured_page(params, ctx).await,
        "getAccessibilityTree" => get_accessibility_tree(ctx).await,
        "getInteractiveElements" => get_interactive_elements(ctx).await,
        "getBoxModelScreenshot" => get_box_model_screenshot(params, ctx).await,
        "clickRef" => click_ref(params, ctx).await,
        "fillRef" => fill_ref(params, ctx).await,
        "waitRef" => wait_ref(params, ctx).await,
        "ariaSnapshot" => aria_snapshot(params, ctx).await,
        "exportStorageState" => export_storage_state(ctx).await,
        "importStorageState" => import_storage_state(params, ctx).await,
        "getApiGaps" => get_api_gaps(params, ctx).await,
        "credentialList" => credential_list(params, ctx).await,
        "fillCredential" => fill_credential(params, ctx).await,
        "resolveConfirmation" => resolve_confirmation(params, ctx).await,
        "accountList" => account_list(params, ctx).await,
        "beginLogin" => begin_login(params, ctx).await,
        "loginWithAccount" => login_with_account(params, ctx).await,
        "endLogin" => end_login(params, ctx).await,
        "captureSession" => capture_session_cmd(params, ctx).await,
        "reportLoginSuccess" => report_login_success(params, ctx).await,
        _ => Err(CdpError {
            code: -32601,
            message: format!("unknown method: OXI.{}", method),
        }),
    }
}

async fn get_markdown(ctx: &DispatchContext) -> DomainResult {
    let guard = ctx.session.read().await;
    let markdown = guard.page().map(|p| p.to_markdown()).unwrap_or_default();
    Ok(Some(json!({ "markdown": markdown })))
}

async fn get_page_info(ctx: &DispatchContext) -> DomainResult {
    let guard = ctx.session.read().await;
    let url = guard
        .current_url()
        .map(|u| u.to_string())
        .unwrap_or_default();
    let title = guard
        .page()
        .and_then(|p| p.title().map(|t| t.to_string()))
        .unwrap_or_default();
    let status = guard.page().map(|p| p.status()).unwrap_or(0);
    Ok(Some(json!({
        "url": url,
        "title": title,
        "status": status,
        "readyState": "complete"
    })))
}

/// OXI.getStructuredPage — return structured page data.
///
/// Returns headings, links, meta tags, and basic page info as JSON.
/// This is optimized for AI agent consumption.
///
/// Optional params:
/// - `maxLinks` (number): limit number of links returned (default: 200)
async fn get_structured_page(_params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let max_links = _params
        .as_ref()
        .and_then(|p| p.get("maxLinks"))
        .and_then(|v| v.as_u64())
        .unwrap_or(200) as usize;

    let mut guard = ctx.session.write().await;
    let url = guard
        .current_url()
        .map(|u| u.to_string())
        .unwrap_or_default();
    let snapshot = guard.dom_snapshot().await?;
    let title = snapshot
        .as_ref()
        .map(|s| s.title.clone())
        .unwrap_or_default();

    let (headings, links, meta) = match snapshot {
        Some(s) => {
            let headings: Vec<Value> = s
                .headings()
                .into_iter()
                .map(|(level, text)| json!({ "level": level, "text": text }))
                .collect();
            let links: Vec<Value> = s
                .links()
                .into_iter()
                .take(max_links)
                .map(|(text, href)| json!({ "text": text, "href": href }))
                .collect();
            let meta: Value = s
                .meta_tags()
                .into_iter()
                .map(|(k, v)| (k, json!(v)))
                .collect();
            (headings, links, meta)
        }
        None => (vec![], vec![], json!({})),
    };

    Ok(Some(json!({
        "url": url,
        "title": title,
        "headings": headings,
        "links": links,
        "meta": meta,
        "linkCount": links.len(),
        "headingCount": headings.len(),
    })))
}

/// OXI.getAccessibilityTree — return semantic tree of page content.
///
/// Shows what a user (or screen reader) would perceive:
/// roles, labels, visibility, interactivity, approximate positions.
async fn get_accessibility_tree(ctx: &DispatchContext) -> DomainResult {
    let mut guard = ctx.session.write().await;
    let snapshot = guard.dom_snapshot().await?;

    let tree = match snapshot {
        Some(s) => oxibrowser_core::css::render_accessibility_tree(&s),
        None => "(no page loaded)".into(),
    };

    Ok(Some(json!({ "tree": tree })))
}

/// OXI.getInteractiveElements — list interactive elements in document order.
///
/// One entry per element that is interactive: tag in
/// a/button/input/select/textarea, an `onclick` attribute, an interactive
/// `role` (button/link/tab/checkbox/radio), or `tabindex >= 0`. Detection and
/// role/selector computation live in `oxibrowser_core::js::dom_snapshot`.
///
/// Each entry additionally carries a stable `ref` (`e{N}`) usable with
/// `OXI.clickRef` / `OXI.fillRef` / `OXI.waitRef`; the top-level response
/// includes the page `generation` the refs were issued against.
async fn get_interactive_elements(ctx: &DispatchContext) -> DomainResult {
    let mut guard = ctx.session.write().await;
    let session_key = guard.id().to_string();
    let snapshot = guard.dom_snapshot().await?;
    let generation = guard.page().map(|p| p.generation()).unwrap_or(0);

    let elements = match &snapshot {
        Some(s) => {
            let items = s.interactive_elements();
            let mut out = Vec::with_capacity(items.len());
            for el in items {
                let r#ref = RefRegistry::allocate(
                    &session_key,
                    el.node_id,
                    generation,
                    s.fingerprint(el.node_id),
                    el.selector.clone(),
                );
                let mut value = serde_json::to_value(&el).unwrap_or(Value::Null);
                if let Value::Object(map) = &mut value {
                    map.insert("ref".into(), json!(r#ref));
                }
                out.push(value);
            }
            out
        }
        None => vec![],
    };

    Ok(Some(
        json!({ "elements": elements, "generation": generation }),
    ))
}

/// OXI.getBoxModelScreenshot — PNG with colored boxes for each element.
///
/// Uses LayoutEngine to estimate positions and draws:
/// - Background-colored rectangles for each visible element
/// - Text content inside boxes
/// - Element borders
async fn get_box_model_screenshot(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_default();
    let viewport_width = params
        .get("viewportWidth")
        .and_then(|v| v.as_u64())
        .unwrap_or(1280) as u32;

    let mut guard = ctx.session.write().await;
    let snapshot = guard.dom_snapshot().await?;

    let png_bytes = match snapshot {
        Some(s) => {
            oxibrowser_core::css::render_box_model_png(&s, viewport_width).unwrap_or_default()
        }
        None => Vec::new(),
    };

    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD.encode(&png_bytes);

    Ok(Some(json!({
        "data": data,
        "metadata": {
            "pageScaleFactor": 1,
            "deviceWidth": viewport_width,
        }
    })))
}

// ── Stable refs (OXI.*Ref) ──────────────────────────────────────────────────

/// The `stale ref` rejection: the page (generation or element content) drifted
/// since the ref was issued; the agent must re-observe.
fn stale_ref_error() -> CdpError {
    CdpError {
        code: -32000,
        message: "stale ref — re-observe".to_string(),
    }
}

fn string_param<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params.get(key).and_then(|v| v.as_str())
}

fn missing_param(param: &str) -> CdpError {
    CdpError {
        code: -32602,
        message: format!("requires a string '{param}' parameter"),
    }
}

/// Shared ref-validation flow: resolve → rebuild the current snapshot →
/// compare the recorded generation + fingerprint. Any drift (navigation,
/// mutation) rejects with "stale ref — re-observe"; on match the entry and the
/// fresh snapshot are returned for the action.
async fn resolve_and_validate(
    session: &mut oxibrowser_core::session::Session,
    session_key: &str,
    r#ref: &str,
) -> Result<(crate::refs::RefEntry, oxibrowser_core::js::DomSnapshot), CdpError> {
    let entry = RefRegistry::resolve(session_key, r#ref).map_err(|e| CdpError {
        code: -32602,
        message: e,
    })?;
    let generation = session.page().map(|p| p.generation()).unwrap_or(0);
    let snapshot = session.dom_snapshot().await?.ok_or_else(stale_ref_error)?;
    if generation != entry.generation || snapshot.fingerprint(entry.node_id) != entry.fingerprint {
        return Err(stale_ref_error());
    }
    Ok((entry, snapshot))
}

/// Irreversible-action pattern gate (roadmap item 16, upper design §8.2 /
/// FM-L8). In account-bound contexts, interactions whose descriptor
/// (selector, page URL, enclosing form action, aria-label, visible text)
/// matches the built-in + per-account pattern list require the context's
/// `irreversible` capability — held by CLI-launched contexts (local user
/// direct) and by `createBrowserContext` contexts whose agent carries an
/// `irreversible` grant. Best-effort by design; the deny audit line records
/// the matched pattern.
async fn irreversible_gate(
    ctx: &DispatchContext,
    session: &mut oxibrowser_core::session::Session,
    selector: &str,
    enrich_element: bool,
) -> Result<(), CdpError> {
    use oxibrowser_core::account::irreversible::matched_pattern;
    use oxibrowser_core::security::audit::{
        self, AuditDecision, AuditEvent, AuditEventKind,
    };

    if !ctx.browser_context.credential_mode() {
        return Ok(());
    }
    let Some(surface) = ctx.logins.clone() else {
        return Ok(());
    };
    let ctx_id = ctx.browser_context.id().as_str();
    let Some(account_id) = surface.account_for_context(ctx_id) else {
        return Ok(());
    };
    let record = match surface.orchestrator().shared_manager().registry().get(&account_id) {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    let extra = record.irreversible_patterns.clone().unwrap_or_default();
    // Only http(s) URLs carry signal: `data:` URLs embed the whole page
    // source, so every descriptor would match every button label through
    // the URL component alone.
    let url = session
        .current_url()
        .map(|u| u.to_string())
        .unwrap_or_default();
    let url = if url.starts_with("http://") || url.starts_with("https://") {
        url
    } else {
        String::new()
    };
    let mut descriptor = format!("{selector}\n{url}");
    if enrich_element {
        let sel_json = serde_json::to_string(selector).unwrap_or_default();
        let js = format!(
            r#"(function() {{
                var el = document.querySelector({sel_json});
                if (!el) return "";
                var form = el.form || (el.closest ? el.closest("form") : null);
                return [form ? (form.getAttribute("action") || "") : "",
                        el.getAttribute("aria-label") || "",
                        (el.textContent || el.innerText || "").slice(0, 120),
                        el.id || "",
                        el.className || ""].join("\n");
            }})()"#
        );
        if let Ok(v) = session.evaluate_js(&js).await
            && let Some(text) = v.value.as_ref().and_then(|t| t.as_str())
        {
            descriptor.push('\n');
            descriptor.push_str(text);
        }
    }
    let Some(pattern) = matched_pattern(&descriptor, &extra) else {
        return Ok(());
    };
    let allowed = surface.irreversible_allowed(ctx_id);
    surface
        .orchestrator()
        .shared_manager()
        .record_event(AuditEvent {
        action: Some("irreversible_gate".to_string()),
        origin: Some(record.scope.clone()),
        ..audit::event(
            AuditEventKind::PolicyViolation,
            if allowed {
                AuditDecision::Allow
            } else {
                AuditDecision::Deny
            },
            format!("account={account_id} pattern={pattern:?} allowed={allowed}"),
        )
    });
    if allowed {
        return Ok(());
    }
    Err(CdpError {
        code: -32000,
        message: format!(
            "irreversibleActionRequiresGrant: pattern '{pattern}' — grant with \
             `account grant {account_id} --agent <A> --actions irreversible` \
             and recreate the context"
        ),
    })
}

/// Click JS mirroring `oxibrowser_core::tab::Tab::click` — the core Tab path
/// isn't reachable from the CDP layer, so the click snippet runs directly via
/// session evaluate (same pattern as the `Input.*` CdpStubs handlers).
fn click_js(selector: &str) -> String {
    let sel_json = serde_json::to_string(selector).unwrap_or_default();
    format!(
        r#"(function() {{
            var el = document.querySelector({sel_json});
            if (!el) return null;
            var rect = el.getBoundingClientRect
                ? el.getBoundingClientRect()
                : {{ left: 0, top: 0, width: 0, height: 0 }};
            var x = rect.left + rect.width / 2;
            var y = rect.top + rect.height / 2;
            el.dispatchEvent(new MouseEvent('click', {{
                bubbles: true,
                cancelable: true,
                clientX: x,
                clientY: y,
                button: 0
            }}));
            return el.tagName;
        }})()"#,
    )
}

/// OXI.clickRef — click an element through a stable ref.
///
/// Resolves `ref` (session key: the `sessionKey` param when given, else this
/// session's id), validates generation + fingerprint against the current
/// snapshot, then dispatches a click MouseEvent at the element's center.
async fn click_ref(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.ok_or_else(|| CdpError {
        code: -32602,
        message: "clickRef requires parameters".to_string(),
    })?;
    let r#ref = string_param(&params, "ref")
        .ok_or_else(|| missing_param("ref"))?
        .to_string();

    let mut guard = ctx.session.write().await;
    let session_key = string_param(&params, "sessionKey")
        .map(str::to_string)
        .unwrap_or_else(|| guard.id().to_string());

    let (entry, _snapshot) = resolve_and_validate(&mut guard, &session_key, &r#ref).await?;
    irreversible_gate(ctx, &mut guard, &entry.selector, true).await?;

    let js = click_js(&entry.selector);
    let result = guard.evaluate_js(&js).await?;
    if result.value.as_ref().is_none_or(|v| v.is_null()) {
        return Err(CdpError {
            code: -32000,
            message: format!("clickRef: no element matching '{}'", entry.selector),
        });
    }
    Ok(Some(json!({ "clicked": true, "ref": r#ref })))
}

/// OXI.fillRef — fill a form control through a stable ref.
///
/// Same resolve + generation/fingerprint validation as `clickRef`, then runs
/// the core `js_fill` snippet against the recorded selector.
async fn fill_ref(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.ok_or_else(|| CdpError {
        code: -32602,
        message: "fillRef requires parameters".to_string(),
    })?;
    let r#ref = string_param(&params, "ref")
        .ok_or_else(|| missing_param("ref"))?
        .to_string();
    let value = string_param(&params, "value")
        .ok_or_else(|| missing_param("value"))?
        .to_string();

    let mut guard = ctx.session.write().await;
    let session_key = string_param(&params, "sessionKey")
        .map(str::to_string)
        .unwrap_or_else(|| guard.id().to_string());

    let (entry, _snapshot) = resolve_and_validate(&mut guard, &session_key, &r#ref).await?;
    irreversible_gate(ctx, &mut guard, &entry.selector, false).await?;

    // Credential-mode literal gate (design §6.2): a literal value into an
    // `input[type=password]` bypasses the broker's audited fill path — route
    // to `OXI.fillCredential` instead.
    if ctx.browser_context.credential_mode()
        && guard.selector_targets_password(&entry.selector).await
    {
        return Err(CdpError {
            code: -32000,
            message: "passwordFillRequiresCredential: fill password fields via OXI.fillCredential"
                .to_string(),
        });
    }

    let js = oxibrowser_core::js::form::js_fill(&entry.selector, &value);
    guard.evaluate_js(&js).await?;
    Ok(Some(json!({ "filled": true, "ref": r#ref })))
}

/// OXI.waitRef — wait until the ref's selector appears in the snapshot.
///
/// Polls `dom_snapshot` every 50 ms (the `Tab::wait_for` equivalent) until the
/// selector matches or `timeoutMs` (default 5000) elapses. Unlike
/// click/fill, absence is not stale — it is what waiting is for.
async fn wait_ref(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.ok_or_else(|| CdpError {
        code: -32602,
        message: "waitRef requires parameters".to_string(),
    })?;
    let r#ref = string_param(&params, "ref")
        .ok_or_else(|| missing_param("ref"))?
        .to_string();
    let timeout_ms = params
        .get("timeoutMs")
        .and_then(|v| v.as_u64())
        .unwrap_or(5000);

    let mut guard = ctx.session.write().await;
    let session_key = string_param(&params, "sessionKey")
        .map(str::to_string)
        .unwrap_or_else(|| guard.id().to_string());

    let entry = RefRegistry::resolve(&session_key, &r#ref).map_err(|e| CdpError {
        code: -32602,
        message: e,
    })?;

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        let found = guard
            .dom_snapshot()
            .await?
            .is_some_and(|s| s.query_selector(&entry.selector).is_some());
        if found {
            return Ok(Some(json!({ "found": true, "ref": r#ref })));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(CdpError {
                code: -32000,
                message: format!(
                    "waitRef: timeout after {timeout_ms}ms waiting for '{}'",
                    entry.selector
                ),
            });
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// OXI.ariaSnapshot — Playwright-style YAML of the visible tree.
///
/// Renders visible elements as `- role "name" [flags]` / `- role:` container
/// lines (`oxibrowser_core::js::DomSnapshot::render_aria_yaml`). Interactive
/// elements additionally get stable refs from the same registry as
/// `getInteractiveElements`, so the snapshot is directly actionable via the
/// `OXI.*Ref` methods.
async fn aria_snapshot(_params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let mut guard = ctx.session.write().await;
    let session_key = guard.id().to_string();
    let generation = guard.page().map(|p| p.generation()).unwrap_or(0);
    let snapshot = guard.dom_snapshot().await?;

    let yaml = match snapshot {
        Some(s) => s.render_aria_yaml(&mut |node_id| {
            let node = s.nodes.get(&node_id)?;
            if !oxibrowser_core::js::dom_snapshot::is_interactive_element(node) {
                return None;
            }
            Some(RefRegistry::allocate(
                &session_key,
                node_id,
                generation,
                s.fingerprint(node_id),
                s.css_selector_path(node_id),
            ))
        }),
        None => String::new(),
    };

    Ok(Some(json!({ "snapshot": yaml })))
}

// ── Storage state (OXI.exportStorageState / OXI.importStorageState) ─────────

/// OXI.exportStorageState — cookies + per-origin localStorage snapshot
/// (Playwright-compatible `StorageState`).
///
/// Denied with `deniedInCredentialMode` while the session's context is in
/// credential mode (design §6.3 CDP gating): a state export IS a cookie
/// export, which would bypass the broker.
async fn export_storage_state(ctx: &DispatchContext) -> DomainResult {
    super::deny_in_credential_mode(ctx)?;
    let guard = ctx.session.read().await;
    let state = guard.export_state();
    let value = serde_json::to_value(&state).map_err(|e| CdpError {
        code: -32603,
        message: format!("exportStorageState: {e}"),
    })?;
    Ok(Some(json!({ "state": value })))
}

/// OXI.importStorageState — seed cookies + localStorage from a prior export.
///
/// Denied with `deniedInCredentialMode` while the session's context is in
/// credential mode (export/import symmetry): injecting attacker-known
/// session cookies into the account jar is a session-fixation vector.
async fn import_storage_state(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    super::deny_in_credential_mode(ctx)?;
    let params = params.ok_or_else(|| CdpError {
        code: -32602,
        message: "importStorageState requires parameters".to_string(),
    })?;
    let state_value = params.get("state").cloned().ok_or_else(|| CdpError {
        code: -32602,
        message: "importStorageState requires a 'state' object".to_string(),
    })?;
    let state: oxibrowser_core::StorageState =
        serde_json::from_value(state_value).map_err(|e| CdpError {
            code: -32602,
            message: format!("importStorageState: invalid state — {e}"),
        })?;

    let mut guard = ctx.session.write().await;
    guard.import_state(&state)?;
    Ok(Some(json!({})))
}

/// OXI.getApiGaps — JS/DOM API coverage telemetry (#15).
///
/// Returns the process-global snapshot of unsupported/polyfilled Web API
/// accesses recorded when the browser runs with
/// [`oxibrowser_core::BrowserConfig::telemetry`]: reads of `window`
/// properties the engine does not implement, keyed by property name.
/// Response: `{"gaps": [{"name", "count"}, …]}` sorted by count
/// (descending). The snapshot is read-only — counters are **not** reset,
/// so repeated calls observe the cumulative process totals.
async fn get_api_gaps(_params: Option<Value>, _ctx: &DispatchContext) -> DomainResult {
    let gaps = api_gaps_payload();
    Ok(Some(gaps))
}

/// Assemble the `getApiGaps` response body from the core telemetry
/// snapshot (count-descending).
fn api_gaps_payload() -> Value {
    let gaps: Vec<Value> = oxibrowser_core::js::runtime::telemetry_snapshot()
        .into_iter()
        .map(|(name, count)| json!({ "name": name, "count": count }))
        .collect();
    json!({ "gaps": gaps })
}

// ── Credential surface (OXI.credentialList / fillCredential /
//    resolveConfirmation, design `2026-09-27` §6.3) ──────────────────────────

/// Missing broker error: the server was not built with `with_credentials`.
fn credentials_unavailable() -> CdpError {
    credential::credentials_unavailable("no credential provider configured on this server")
}

// ── Account surface (OXI.accountList / beginLogin / endLogin /
//    reportLoginSuccess, design `2026-09-28` §7.3) ────────────────────────────

/// Missing login surface error: the server was not built with `with_login`.
fn accounts_unavailable() -> CdpError {
    CdpError {
        code: -32000,
        message: "accountsUnavailable".to_string(),
    }
}

fn missing_login_surface(
    ctx: &DispatchContext,
) -> Result<Arc<crate::account::LoginSurface>, CdpError> {
    ctx.logins.clone().ok_or_else(accounts_unavailable)
}

/// OXI.accountList — metadata + state only (§6.2: account.json is the one
/// account artifact agents may see). `params.agent` is accepted for
/// forward-compatibility and currently ignored (summaries carry no
/// per-agent grants).
async fn account_list(_params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let surface = missing_login_surface(ctx)?;
    let records = surface.orchestrator().manager().registry().list()?;
    let accounts: Vec<Value> = records
        .iter()
        .map(|r| {
            serde_json::json!({
                "account_id": r.account_id,
                "scope": r.scope,
                "state": r.state.as_str(),
                "state_detail": r.state_detail,
                "login_hint": r.identity.login_hint,
                "display_name": r.identity.display_name,
                "session_summary": r.session_summary,
            })
        })
        .collect();
    Ok(Some(json!({ "accounts": accounts })))
}

/// OXI.captureSession — explicit envelope capture of a bound account's live
/// context (roadmap item 3): the "stop the work" path. The operator
/// captures the freshest cookies before tearing a child down, instead of
/// waiting for a detector-gated auto-save. Audited as `session_capture` by
/// the manager; agent-role only (viewers are outside the allowlist).
///
/// Response: `{accountId, state, sessionSummary}`.
async fn capture_session_cmd(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let surface = missing_login_surface(ctx)?;
    let params = params.unwrap_or(Value::Null);
    let account_id = string_param(&params, "accountId")
        .ok_or_else(|| missing_param("accountId"))?
        .to_string();
    let record = surface
        .capture_bound(&account_id)
        .await
        .map_err(|e| CdpError {
            code: -32000,
            message: format!("captureFailed: {e}"),
        })?;
    Ok(Some(json!({
        "accountId": record.account_id,
        "state": record.state.as_str(),
        "sessionSummary": record.session_summary,
    })))
}

/// OXI.beginLogin — open a login window (§5.1). `mode: "user"` issues a
/// one-time viewer token delivered **out-of-band only** (CLI
/// `account login --json` / host channel); `mode: "agent"` leaves the
/// driving to the connected agent (M-D automates it later).
///
/// The token is never echoed in this response — any connected agent could
/// otherwise mint viewer credentials in-band (VIEWER-TOKEN-INBAND, §5.2).
///
/// Response: `{loginId, timeoutMs}`.
async fn begin_login(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let surface = missing_login_surface(ctx)?;
    let params = params.unwrap_or(Value::Null);
    let account_id = string_param(&params, "accountId")
        .ok_or_else(|| missing_param("accountId"))?
        .to_string();
    let mode = match string_param(&params, "mode") {
        Some("user") | None => oxibrowser_core::account::LoginMode::User,
        Some("agent") => oxibrowser_core::account::LoginMode::Agent,
        Some(other) => {
            return Err(CdpError {
                code: -32602,
                message: format!("invalid mode {other:?} (expected \"user\" or \"agent\")"),
            });
        }
    };
    let timeout = params
        .get("timeoutMs")
        .and_then(|v| v.as_u64())
        .map(std::time::Duration::from_millis);

    let (handle, _ctx) = surface
        .begin_login(&account_id, mode, timeout)
        .await
        .map_err(|e| CdpError {
            code: -32000,
            message: e.to_string(),
        })?;

    Ok(Some(json!({
        "loginId": handle.login_id,
        "timeoutMs": handle.timeout_ms,
    })))
}

/// OXI.loginWithAccount — start an **unattended** agent login (M-D, design
/// §5.3): the broker drives the account's login form itself (credentials +
/// TOTP injected from the keystore), and progress + the terminal state flow
/// back as `OXI.loginStateChanged` events.
///
/// Start-time gates (synchronous errors):
/// - unknown account → `invalidAccount`,
/// - no credential broker wired → `credentialsUnavailable`,
/// - no active `login` grant for the account's credentials (deny rules and
///   consent consulted non-consumingly) → `accountAccessDenied`
///   {consentRequired}.
///
/// Escalations arrive as events, not errors: `challenge` (Interactive /
/// Blocked bot-management), `mfa_escalation` (SMS/email 2FA — forbidden
/// list), `policy_violation` (final origin left the credential allowlist).
async fn login_with_account(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let surface = missing_login_surface(ctx)?;
    let broker = ctx
        .credentials
        .clone()
        .ok_or_else(credentials_unavailable)?;
    let params = params.unwrap_or(Value::Null);
    let account_id = string_param(&params, "accountId")
        .ok_or_else(|| missing_param("accountId"))?
        .to_string();
    let agent_id = string_param(&params, "agentId")
        .unwrap_or("main")
        .to_string();
    let timeout = params
        .get("timeoutMs")
        .and_then(|v| v.as_u64())
        .map(std::time::Duration::from_millis);

    // Unknown account → invalidAccount (§7.3 error code).
    let record = surface
        .orchestrator()
        .manager()
        .registry()
        .get(&account_id)
        .map_err(|_| CdpError {
            code: -32000,
            message: format!("invalidAccount: {account_id}"),
        })?;

    // Start gate: a non-consuming `login` preflight over the origins the
    // flow can actually authorize at (probe origin, scope root). The
    // consuming decision happens inside the engine at resolution time.
    if !agent_login_preflight(&broker, &record) {
        return Err(CdpError {
            code: -32000,
            message: "accountAccessDenied: consentRequired — no active login grant for this account's credentials".to_string(),
        });
    }

    let login_id = surface
        .start_agent_login(&account_id, &agent_id, broker, timeout)
        .map_err(|e| CdpError {
            code: -32000,
            message: e.to_string(),
        })?;

    Ok(Some(json!({
        "loginId": login_id,
        "accountId": account_id,
        "agentId": agent_id,
        "state": "started",
    })))
}

/// Non-consuming grant peek for the M-D start gate: any of the account's
/// credential handles with an active `login` grant at one of the flow's
/// candidate origins.
fn agent_login_preflight(broker: &CredentialBroker, record: &AccountRecord) -> bool {
    let mut origins: Vec<String> = Vec::new();
    if let Some(probe) = &record.probe
        && let Ok(url) = url::Url::parse(&probe.url)
    {
        origins.push(
            format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default())
                + &url.port().map(|p| format!(":{p}")).unwrap_or_default(),
        );
    }
    origins.push(format!("https://{}", record.scope));

    for handle in &record.credentials {
        for origin in &origins {
            let Ok(top) = Origin::parse(origin) else {
                continue;
            };
            let mut request = UseRequest::new(
                CredentialId(handle.clone()),
                top,
                oxibrowser_credentials::CredentialAction::Login,
            );
            request.frame = Some(request.top_level.clone());
            if broker.engine.preflight(&request) {
                return true;
            }
        }
    }
    false
}

/// OXI.endLogin — host-explicit end: `outcome: "done"` judges the window as
/// explicit success (capture); `outcome: "abort"` reverts to needs_login.
async fn end_login(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let surface = missing_login_surface(ctx)?;
    let params = params.unwrap_or(Value::Null);
    let login_id = string_param(&params, "loginId")
        .ok_or_else(|| missing_param("loginId"))?
        .to_string();
    let outcome = match string_param(&params, "outcome") {
        Some("done") => oxibrowser_core::account::EndOutcome::Done,
        Some("abort") | None => oxibrowser_core::account::EndOutcome::Abort,
        Some(other) => {
            return Err(CdpError {
                code: -32602,
                message: format!("invalid outcome {other:?} (expected \"done\" or \"abort\")"),
            });
        }
    };

    let result = surface
        .end_login(&login_id, outcome)
        .await
        .map_err(|e| CdpError {
            code: -32000,
            message: e.to_string(),
        })?;
    Ok(Some(json!({
        "loginId": login_id,
        "state": result.end_state,
        "accountState": result.record.as_ref().map(|r| r.state.as_str()),
    })))
}

/// OXI.reportLoginSuccess — the §4.3 highest-trust signal: capture the
/// login window's session (explicit success ⇒ detector confirms).
async fn report_login_success(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let surface = missing_login_surface(ctx)?;
    let params = params.unwrap_or(Value::Null);
    let login_id = string_param(&params, "loginId")
        .ok_or_else(|| missing_param("loginId"))?
        .to_string();

    let result = surface
        .report_success(&login_id)
        .await
        .map_err(|e| CdpError {
            code: -32000,
            message: e.to_string(),
        })?;
    Ok(Some(json!({
        "loginId": login_id,
        "state": result.end_state,
        "accountState": result.record.as_ref().map(|r| r.state.as_str()),
    })))
}

/// OXI.credentialList — metadata-only credential listing.
///
/// `params.agent` optionally filters by agent id. The payload is the
/// value-free [`oxibrowser_credentials::CredentialMeta`] projection, so no
/// secret can appear by construction.
async fn credential_list(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let broker = ctx
        .credentials
        .clone()
        .ok_or_else(credentials_unavailable)?;
    let agent = params.as_ref().and_then(|p| string_param(p, "agent"));
    credential::credential_list_payload(&broker, agent).map(Some)
}

/// OXI.fillCredential — inject a broker-resolved credential value through the
/// session fill path.
///
/// Flow (design §6.3): validate the ref → derive the top origin from the
/// session's current URL (the ref came from the main-document snapshot, so
/// the hosting frame is the top frame) → exact-origin allowlist check →
/// [`PolicyEngine::authorize_use`]:
///
/// - `Allow` → resolve + inject → `{filled: true, masked: true}` (no value in
///   the response),
/// - `RequireConfirmation` → register a pending card, emit
///   `OXI.confirmationRequired`, answer `consentRequired`,
/// - `Deny` → `consentRequired`.
///
/// Errors: `refStale`, `originMismatch`, `credentialNotFound`,
/// `consentRequired`, `credentialsUnavailable`.
async fn fill_credential(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let broker = ctx
        .credentials
        .clone()
        .ok_or_else(credentials_unavailable)?;
    let params = params.ok_or_else(|| CdpError {
        code: -32602,
        message: "fillCredential requires parameters".to_string(),
    })?;
    let r#ref = string_param(&params, "ref")
        .ok_or_else(|| missing_param("ref"))?
        .to_string();
    let credential_id = string_param(&params, "credentialId")
        .ok_or_else(|| missing_param("credentialId"))?
        .to_string();
    let field_kind = FieldKind::parse(
        string_param(&params, "fieldKind").ok_or_else(|| missing_param("fieldKind"))?,
    )?;

    let mut guard = ctx.session.write().await;
    let session_key = guard.id().to_string();

    // 1. The ref must still resolve against the live snapshot — any drift
    //    rejects with `refStale` (the agent re-observes).
    let (entry, _snapshot) = resolve_and_validate(&mut guard, &session_key, &r#ref).await?;

    // 2. Origin of the page hosting the form, from the session's current URL.
    let url = guard
        .current_url()
        .cloned()
        .ok_or_else(|| credential::origin_mismatch("no page loaded"))?;
    let origin = Origin::parse(url.as_str()).map_err(|_| {
        credential::origin_mismatch(format!("page URL {url} has no usable web origin"))
    })?;

    // 3. The credential must exist (handle known to the broker).
    let cred_id = CredentialId(credential_id);
    let meta = broker
        .provider
        .metadata(&cred_id)
        .map_err(credential::cred_error_to_cdp)?;

    // 4. Exact-origin allowlist (core M1) — deny-biased with the engine's
    //    verdict below. Fail-closed on unparseable stored origins.
    let mut allowed = Vec::with_capacity(meta.allowed_origins.len());
    for o in &meta.allowed_origins {
        allowed.push(Origin::parse(o).map_err(|e| {
            credential::origin_mismatch(format!(
                "credential {} has unusable allowed origin {o}: {e}",
                meta.id
            ))
        })?);
    }
    if OriginPolicy::default().evaluate(&allowed, &origin, Some(&origin)) != Decision::Allow {
        return Err(credential::origin_mismatch(format!(
            "credential {} is not allowed at {}",
            meta.id,
            origin.as_str()
        )));
    }

    // 5. Policy engine: deny rules → consent → confirmation.
    let mut request = UseRequest::new(meta.id.clone(), origin.clone(), field_kind.action());
    request.frame = Some(origin.clone());
    match broker.engine.authorize_use(&request) {
        Decision::Allow => {
            let fingerprint =
                fill_with_credential(&mut guard, &broker, &meta.id, field_kind, &entry.selector)
                    .await?;
            credential::audit_fill(&broker.engine, &request, &fingerprint);
            Ok(Some(json!({ "filled": true, "masked": true })))
        }
        Decision::RequireConfirmation { reason } => {
            let token = broker.engine.issue_confirmation(&request);
            let request_id = broker.register_pending(
                request.clone(),
                token,
                session_key,
                entry.selector,
                meta.id.clone(),
                field_kind,
            );
            ctx.events.send_event(
                "OXI.confirmationRequired",
                json!({
                    "requestId": request_id,
                    "action": field_kind.action().as_str(),
                    "origin": origin.as_str(),
                    "summary": {
                        // Handles only — card rendering never sees values.
                        "values": [meta.id.to_string()],
                        "irreversible": false,
                    },
                    "timeoutMs": broker.confirmation_ttl().as_millis() as u64,
                }),
            );
            Err(credential::consent_required(format!(
                "confirmation required for {} fill at {} ({})",
                field_kind.as_str(),
                origin.as_str(),
                reason
            )))
        }
        Decision::Deny { reason } => Err(credential::consent_required(format!(
            "{} fill at {} denied: {reason}",
            field_kind.as_str(),
            origin.as_str()
        ))),
    }
}

/// OXI.resolveConfirmation — resolve a pending confirmation card.
///
/// **Viewer connections only.** Approval is the human's out-of-band act on
/// the token-authenticated mirror channel; agent connections are rejected at
/// the role gate (`confirmationRequiresViewerRole`) and re-checked here.
///
/// `approved: true` re-validates via [`PolicyEngine::verify_confirmation`]
/// (request-hash binding + TTL + deny rules) and, on `Allow`, completes the
/// fill. Missing `approved` is a denial; an unknown or expired `requestId` is
/// invalid — timeout never approves implicitly (design §6.3).
///
/// The minting session's binding check (`pending.session_key`) applies only
/// to non-viewer resolvers; a viewer connection runs its own mirror session
/// in the shared context, so a mismatch there is surfaced as response detail
/// instead of a rejection.
async fn resolve_confirmation(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    if ctx.role != crate::session::RoleKind::Viewer {
        return Err(CdpError {
            code: -32000,
            message: "confirmationRequiresViewerRole".to_string(),
        });
    }
    let broker = ctx
        .credentials
        .clone()
        .ok_or_else(credentials_unavailable)?;
    let params = params.ok_or_else(|| CdpError {
        code: -32602,
        message: "resolveConfirmation requires parameters".to_string(),
    })?;
    let request_id = string_param(&params, "requestId")
        .ok_or_else(|| missing_param("requestId"))?
        .to_string();
    // Missing `approved` = denial (implicit approval is forbidden).
    let approved = params
        .get("approved")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Expired or unknown → treated as missing: nothing to approve.
    let pending = broker.take(&request_id).ok_or_else(|| CdpError {
        code: -32602,
        message: format!("unknown or expired confirmation requestId: {request_id}"),
    })?;

    // Non-viewer resolvers may only resolve on the minting session (fail
    // closed); for the viewer the binding is audit detail, not a gate — the
    // mirror session shares the context but has its own session id.
    let session_bound = {
        let guard = ctx.session.read().await;
        let bound = guard.id().to_string() != pending.session_key;
        if bound && ctx.role != crate::session::RoleKind::Viewer {
            return Err(CdpError {
                code: -32000,
                message: format!(
                    "confirmation {request_id} is bound to a different session — re-request the fill"
                ),
            });
        }
        bound
    };

    if !approved {
        credential::audit_rejection(
            &broker.engine,
            &pending.request,
            "confirmation rejected by user",
        );
        return Ok(Some(json!({ "resolved": true, "outcome": "denied" })));
    }

    match broker
        .engine
        .verify_confirmation(&pending.token, &pending.request)
    {
        Decision::Allow => {
            let mut guard = ctx.session.write().await;
            let fingerprint = fill_with_credential(
                &mut guard,
                &broker,
                &pending.credential,
                pending.field_kind,
                &pending.selector,
            )
            .await?;
            credential::audit_fill(&broker.engine, &pending.request, &fingerprint);
            Ok(Some(json!({
                "resolved": true,
                "outcome": "approved",
                "filled": true,
                "masked": true,
                "sessionBound": session_bound,
            })))
        }
        Decision::Deny { reason } | Decision::RequireConfirmation { reason } => Ok(Some(
            json!({ "resolved": true, "outcome": "denied", "reason": reason }),
        )),
    }
}

/// Resolve the credential value and inject it through the session fill path
/// ([`oxibrowser_core::js::form::js_fill`] against the ref's selector).
///
/// The secret exists only between `resolve` and the JS snippet — it never
/// reaches a response, event, or log. Returns the value fingerprint for the
/// audit correlation line.
async fn fill_with_credential(
    session: &mut oxibrowser_core::session::Session,
    broker: &CredentialBroker,
    credential_id: &CredentialId,
    field_kind: FieldKind,
    selector: &str,
) -> Result<String, CdpError> {
    let (meta, secret) = broker
        .provider
        .resolve(credential_id)
        .map_err(credential::cred_error_to_cdp)?;
    let value = match field_kind {
        FieldKind::Totp => {
            if !meta.has_totp {
                return Err(credential::credential_not_found(format!(
                    "{credential_id} carries no otpauth value"
                )));
            }
            let uri = secret
                .expose_str()
                .map_err(credential::cred_error_to_cdp)?
                .to_string();
            // The TOTP code is generated in-broker and injected directly —
            // never returned over CDP (design §6.3).
            oxibrowser_credentials::TotpGenerator::from_otpauth(&uri)
                .map_err(credential::cred_error_to_cdp)?
                .current()
                .map_err(credential::cred_error_to_cdp)?
                .0
        }
        FieldKind::Password | FieldKind::ApiKey => secret
            .expose_str()
            .map_err(credential::cred_error_to_cdp)?
            .to_string(),
    };
    let fingerprint = secret.fingerprint();
    drop(secret);
    let js = oxibrowser_core::js::form::js_fill(selector, &value);
    session.evaluate_js(&js).await?;
    Ok(fingerprint)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// OXI.getApiGaps reflects the core telemetry snapshot: entries ordered
    /// by count descending, shape `{"gaps": [{"name", "count"}]}`, and
    /// read-only (snapshotting does not reset the counters).
    #[test]
    fn test_get_api_gaps_payload_sorted_and_read_only() {
        oxibrowser_core::js::runtime::telemetry_reset();
        let rec = oxibrowser_core::js::runtime::telemetry_record;
        rec("IntersectionObserver");
        rec("webkitRequestAnimationFrame");
        rec("webkitRequestAnimationFrame");

        let payload = api_gaps_payload();
        let gaps = payload["gaps"].as_array().expect("gaps array");
        assert!(gaps.len() >= 2, "both recorded names present");
        assert_eq!(gaps[0]["name"], "webkitRequestAnimationFrame");
        assert_eq!(gaps[0]["count"], 2, "highest count first");
        assert_eq!(gaps[1]["name"], "IntersectionObserver");
        assert_eq!(gaps[1]["count"], 1);

        // Read-only: a second snapshot still sees the same totals.
        let again = api_gaps_payload();
        assert_eq!(again["gaps"][0]["count"], 2, "snapshot does not reset");

        oxibrowser_core::js::runtime::telemetry_reset();
    }
}
