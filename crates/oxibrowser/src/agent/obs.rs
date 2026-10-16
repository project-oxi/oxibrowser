//! Token-budgeted page observation (execution design M0).
//!
//! Wraps `Session::dom_snapshot` + `DomSnapshot::render_aria_yaml` with the
//! same `RefRegistry` ref-minting recipe as the CDP `OXI.ariaSnapshot`
//! handler (session key + document generation + fingerprint + selector), so
//! refs handed out here validate identically under `OXI.*Ref`. On top of the
//! CDP surface it adds:
//!
//! - a **token budget** (bytes/4 approximation; corrected by corpus
//!   measurement in M5) — the YAML is truncated at a line boundary and the
//!   omission is reported, never silent;
//! - an **actionable-elements list** whose refs are the *same* `e{n}` tokens
//!   embedded in the YAML (the actionable pass mints refs first, the YAML
//!   render reuses them — both surfaces agree by construction).

use std::collections::HashMap;

use oxibrowser_cdp::refs::RefRegistry;
use oxibrowser_core::Tab;
use serde::Serialize;

/// Observation options.
#[derive(Debug, Clone, Default)]
pub struct ObsOpts {
    /// Soft cap on the aria YAML size in approximate tokens (4 bytes each).
    /// `None` = no cap.
    pub budget_tokens: Option<u64>,
    /// Cap on the actionable-elements list. Default 64 (ghost snapshot
    /// parity); elements beyond the cap are reported via `omitted_elements`.
    pub max_actionable: Option<usize>,
}

/// One actionable element with its stable ref.
#[derive(Debug, Clone, Serialize)]
pub struct Actionable {
    pub r#ref: String,
    pub node_id: u32,
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub tag: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_type: Option<String>,
    pub disabled: bool,
    pub selector: String,
}

/// Observation result — the agent-facing answer to "what is on this page".
#[derive(Debug, Clone, Serialize)]
pub struct ObsResult {
    pub url: String,
    pub title: String,
    /// Document generation at observation time. Refs minted against this
    /// generation go stale after the next navigation (re-observe).
    pub generation: u64,
    pub aria_yaml: String,
    pub truncated: bool,
    pub omitted_bytes: u64,
    pub actionable: Vec<Actionable>,
    pub omitted_elements: usize,
    /// Size of `aria_yaml` in approximate tokens (bytes/4).
    pub approximate_tokens: u64,
}

/// Default actionable cap (ghost snapshot parity).
const DEFAULT_MAX_ACTIONABLE: usize = 64;

/// Bytes per token for the approximation (calibratable from corpus/real
/// usage via `OXIBROWSER_BYTES_PER_TOKEN`; 4 ≈ English prose).
fn bytes_per_token() -> u64 {
    std::env::var("OXIBROWSER_BYTES_PER_TOKEN")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
        .unwrap_or(4)
}

/// Observe `tab`: aria YAML + actionable refs, within budget.
pub async fn observe(tab: &Tab, opts: &ObsOpts) -> Result<ObsResult, String> {
    let mut guard = tab.lock_session().await;
    observe_session(&mut guard, opts).await
}

/// Session-level observation — the REPL host owns `Arc<RwLock<Session>>`
/// sessions (not `Tab` handles), so the core logic lives on `&mut Session`.
pub async fn observe_session(
    session: &mut oxibrowser_core::session::Session,
    opts: &ObsOpts,
) -> Result<ObsResult, String> {
    let session_key = session.id().to_string();
    let generation = session.page().map(|p| p.generation()).unwrap_or(0);
    let url = session
        .current_url()
        .map(|u| u.to_string())
        .unwrap_or_default();
    let snapshot = session.dom_snapshot().await.map_err(|e| e.to_string())?;

    let Some(s) = snapshot else {
        return Ok(ObsResult {
            url,
            title: String::new(),
            generation,
            aria_yaml: String::new(),
            truncated: false,
            omitted_bytes: 0,
            actionable: Vec::new(),
            omitted_elements: 0,
            approximate_tokens: 0,
        });
    };

    // Actionable pass first: mint refs, then reuse them in the YAML so both
    // surfaces agree on e{n} (the CDP handler allocates independently per
    // command; here one observation = one consistent ref namespace).
    let interactive = s.interactive_elements();
    let cap = opts.max_actionable.unwrap_or(DEFAULT_MAX_ACTIONABLE);
    let mut ref_by_node: HashMap<u32, String> = HashMap::new();
    let mut actionable = Vec::with_capacity(interactive.len().min(cap));
    for el in interactive.iter().take(cap) {
        let r#ref = RefRegistry::allocate(
            &session_key,
            el.node_id,
            generation,
            s.fingerprint(el.node_id),
            el.selector.clone(),
        );
        ref_by_node.insert(el.node_id, r#ref.clone());
        actionable.push(Actionable {
            r#ref,
            node_id: el.node_id,
            role: el.role.clone(),
            name: el.name.clone(),
            tag: el.tag.clone(),
            input_type: el.input_type.clone(),
            disabled: el.disabled,
            selector: el.selector.clone(),
        });
    }
    let omitted_elements = interactive.len().saturating_sub(cap);

    let title = s.title.clone();
    let mut yaml = s.render_aria_yaml(&mut |node_id| ref_by_node.get(&node_id).cloned());

    // Token budget: 4 bytes ≈ 1 token (M0 approximation; M5 corrects the
    // coefficient from corpus measurements). Truncate at a line boundary and
    // report the omission — never silently.
    let mut truncated = false;
    let mut omitted_bytes = 0u64;
    if let Some(budget) = opts.budget_tokens {
        let byte_cap = budget.saturating_mul(bytes_per_token()) as usize;
        if yaml.len() > byte_cap {
            let mut cut = byte_cap.min(yaml.len());
            while cut > 0 && !yaml.is_char_boundary(cut) {
                cut -= 1;
            }
            let line_cut = yaml[..cut].rfind('\n').map(|i| i + 1).unwrap_or(cut);
            omitted_bytes = yaml.len() as u64 - line_cut as u64;
            yaml.truncate(line_cut);
            yaml.push_str(&format!(
                "# … truncated {omitted_bytes} bytes — re-observe with a larger --budget\n"
            ));
            truncated = true;
        }
    }

    Ok(ObsResult {
        url,
        title,
        generation,
        approximate_tokens: yaml.len() as u64 / bytes_per_token(),
        aria_yaml: yaml,
        truncated,
        omitted_bytes,
        actionable,
        omitted_elements,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opts_default_to_uncapped() {
        let opts = ObsOpts::default();
        assert!(opts.budget_tokens.is_none());
        assert!(opts.max_actionable.is_none());
    }
}
