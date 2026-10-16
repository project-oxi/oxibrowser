//! Enforcement gates for REPL actions (M1) — native-path equivalents of the
//! CDP gates, with descriptors derived from the Rust snapshot instead of
//! page `querySelector` enrichment (page scripts cannot forge what the gate
//! matches on; the CDP `evaluate_js` enrichment in
//! `oxibrowser-cdp/src/domains/oxi.rs::irreversible_gate` remains the
//! legacy weaker path).
//!
//! Mirrors:
//! - password literal denial — `OXI.fillRef`'s
//!   `passwordFillRequiresCredential` (credential mode only);
//! - irreversible pattern gate — the deny-biased `matched_pattern` +
//!   `LoginSurface::irreversible_allowed` decision, audit line included.

use oxibrowser_cdp::account::LoginSurface;
use oxibrowser_core::context::BrowserContext;
use oxibrowser_core::js::dom_snapshot::{DomSnapshot, InteractiveElement};
use oxibrowser_core::security::audit::{self, AuditDecision, AuditEvent, AuditEventKind};
use serde::Serialize;

/// A denial with the M1 error contract shape.
#[derive(Debug, Clone, Serialize)]
pub struct GateError {
    pub code: &'static str,
    pub message: String,
}

/// Literal fill into a password field in credential mode — same condition
/// as `OXI.fillRef` (`selector_targets_password` under
/// `BrowserContext::credential_mode`), read from the snapshot instead of a
/// page query.
pub fn check_fill(context: &BrowserContext, el: &InteractiveElement) -> Result<(), GateError> {
    if context.credential_mode()
        && el
            .input_type
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case("password"))
    {
        return Err(GateError {
            code: "POLICY_DENIED",
            message: "passwordFillRequiresCredential: fill password fields via \
                      oxi.fillCredential(locator, account)"
                .to_string(),
        });
    }
    Ok(())
}

/// Irreversible-pattern gate for clicks — replicate of
/// `domains/oxi.rs::irreversible_gate` with snapshot-derived enrichment:
/// `selector \n url \n form-action \n aria-label \n text(120) \n id \n class`.
pub fn check_click(
    context: &BrowserContext,
    surface: Option<&LoginSurface>,
    session_url: &str,
    snapshot: &DomSnapshot,
    el: &InteractiveElement,
) -> Result<(), GateError> {
    use oxibrowser_core::account::irreversible::matched_pattern;

    if !context.credential_mode() {
        return Ok(());
    }
    let Some(surface) = surface else {
        return Ok(());
    };
    let ctx_id = context.id().as_str();
    let Some(account_id) = surface.account_for_context(ctx_id) else {
        return Ok(());
    };
    let record = match surface
        .orchestrator()
        .shared_manager()
        .registry()
        .get(&account_id)
    {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    let extra = record.irreversible_patterns.clone().unwrap_or_default();

    // Only http(s) URLs carry signal (`data:` pages embed the whole source,
    // so every label would match through the URL alone) — same rule as the
    // CDP gate.
    let url = if session_url.starts_with("http://") || session_url.starts_with("https://") {
        session_url
    } else {
        ""
    };
    let node_attrs = |id: u32| snapshot.nodes.get(&id).map(|n| &n.attributes);
    let attr = |id: u32, name: &str| -> String {
        node_attrs(id)
            .and_then(|a| a.get(name))
            .cloned()
            .unwrap_or_default()
    };
    let form_action = ancestor_form_action(snapshot, el.node_id);
    let text: String = el.text.chars().take(120).collect();
    let descriptor = format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n{}",
        el.selector,
        url,
        form_action,
        el.name.as_deref().unwrap_or_default(),
        text,
        attr(el.node_id, "id"),
        attr(el.node_id, "class"),
    );

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
    Err(GateError {
        code: "POLICY_DENIED",
        message: format!(
            "irreversibleActionRequiresGrant: pattern '{pattern}' — grant with \
             `account grant {account_id} --agent <A> --actions irreversible` \
             and recreate the context"
        ),
    })
}

/// Nearest ancestor `<form>`'s `action` attribute (snapshot walk — the
/// page-eval `el.form` enrichment equivalent without executing page script).
fn ancestor_form_action(snapshot: &DomSnapshot, mut node_id: u32) -> String {
    let mut current = Some(node_id);
    while let Some(id) = current {
        let Some(node) = snapshot.nodes.get(&id) else {
            break;
        };
        if node.node_type == 1 && node.tag.eq_ignore_ascii_case("form") {
            return node.attributes.get("action").cloned().unwrap_or_default();
        }
        current = node.parent;
        node_id = id;
    }
    let _ = node_id;
    String::new()
}
