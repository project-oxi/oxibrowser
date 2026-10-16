//! Playwright-subset locator resolution against a `DomSnapshot` (M1).
//!
//! Locators resolve over `interactive_elements()` — the same Rust-side
//! snapshot the observation mints refs from, never a page `querySelector`
//! (page scripts cannot forge what we match on).
//!
//! Matching is deliberately conservative v1: case-insensitive containment
//! for role/text/label/placeholder; `css` matches a full
//! `css_selector_path` or its last path segment (a real selector engine
//! arrives with the locator-engine follow-up). Errors follow the M1 error
//! contract: a stable code plus a Playwright-style message so the model can
//! self-correct.

use oxibrowser_core::js::dom_snapshot::{DomSnapshot, InteractiveElement};
use serde::{Deserialize, Serialize};

/// How a locator selects elements.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "value", rename_all = "snake_case")]
pub enum LocatorSpec {
    /// `getByRole(role)` / `getByRole(role, { name })`.
    Role { role: String, name: Option<String> },
    /// `getByText(text)`.
    Text { text: String },
    /// `getByLabel(text)` — labels feed the accessible name.
    Label { text: String },
    /// `getByPlaceholder(text)`.
    Placeholder { text: String },
    /// `locator(css)` — full snapshot selector or last path segment.
    Css { css: String },
}

/// A locator plus its narrowing chain (`.first()`, `.nth(i)`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocatorQuery {
    pub spec: LocatorSpec,
    pub nth: Option<usize>,
}

/// Error contract (M1): stable codes the loop classifies on; messages echo
/// Playwright phrasing where a direct equivalent exists.
#[derive(Debug, Clone, Serialize)]
pub struct LocError {
    pub code: &'static str,
    pub message: String,
}

impl LocError {
    pub fn not_found(query: &LocatorQuery) -> Self {
        Self {
            code: "LOCATOR_NOT_FOUND",
            message: format!(
                "locator resolved to no elements: {} — re-observe, then retry",
                describe(query)
            ),
        }
    }

    pub fn strict_mode_violation(query: &LocatorQuery, count: usize) -> Self {
        Self {
            code: "STRICT_MODE_VIOLATION",
            message: format!(
                "strict mode violation: {} resolved to {count} elements, expected 1 — \
                 narrow with .first() or .nth(i)",
                describe(query)
            ),
        }
    }

    pub fn bad_nth(query: &LocatorQuery, index: usize) -> Self {
        Self {
            code: "LOCATOR_OUT_OF_RANGE",
            message: format!(
                "locator.nth out of range: {} has no element at index {index}",
                describe(query)
            ),
        }
    }
}

fn describe(query: &LocatorQuery) -> String {
    let spec = match &query.spec {
        LocatorSpec::Role { role, name: None } => format!("getByRole({role:?})"),
        LocatorSpec::Role {
            role,
            name: Some(name),
        } => format!("getByRole({role:?}, {{ name: {name:?} }})"),
        LocatorSpec::Text { text } => format!("getByText({text:?})"),
        LocatorSpec::Label { text } => format!("getByLabel({text:?})"),
        LocatorSpec::Placeholder { text } => format!("getByPlaceholder({text:?})"),
        LocatorSpec::Css { css } => format!("locator({css:?})"),
    };
    match query.nth {
        None => spec,
        Some(0) => format!("{spec}.first()"),
        Some(i) => format!("{spec}.nth({i})"),
    }
}

fn contains(haystack: Option<&str>, needle: &str) -> bool {
    haystack.is_some_and(|h| {
        h.to_ascii_lowercase()
            .contains(&needle.to_ascii_lowercase())
    })
}

fn matches(el: &InteractiveElement, spec: &LocatorSpec) -> bool {
    match spec {
        LocatorSpec::Role { role, name } => {
            if !el.role.eq_ignore_ascii_case(role) {
                return false;
            }
            match name {
                None => true,
                Some(n) => contains(el.name.as_deref(), n) || contains(Some(&el.text), n),
            }
        }
        LocatorSpec::Text { text } => {
            contains(el.name.as_deref(), text) || contains(Some(&el.text), text)
        }
        LocatorSpec::Label { text } => contains(el.name.as_deref(), text),
        LocatorSpec::Placeholder { text } => contains(el.placeholder.as_deref(), text),
        LocatorSpec::Css { css } => {
            if el.selector == *css {
                return true;
            }
            // Any contiguous suffix of the snapshot path matches
            // (`p > button` for `div > p > button`).
            let segs: Vec<&str> = el.selector.split(" > ").collect();
            let css_segs: Vec<&str> = css.split(" > ").collect();
            css_segs.len() <= segs.len() && segs[segs.len() - css_segs.len()..] == css_segs[..]
        }
    }
}

/// Resolve `query` against `snapshot` under strict-mode semantics.
pub fn resolve(
    snapshot: &DomSnapshot,
    query: &LocatorQuery,
) -> Result<InteractiveElement, LocError> {
    let hits: Vec<InteractiveElement> = snapshot
        .interactive_elements()
        .into_iter()
        .filter(|el| matches(el, &query.spec))
        .collect();
    match query.nth {
        None => match hits.len() {
            0 => Err(LocError::not_found(query)),
            1 => Ok(hits.into_iter().next().unwrap()),
            n => Err(LocError::strict_mode_violation(query, n)),
        },
        Some(i) => hits
            .into_iter()
            .nth(i)
            .ok_or_else(|| LocError::bad_nth(query, i)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_of(html: &str) -> DomSnapshot {
        let viewport = oxibrowser_render::Viewport::default();
        let doc = oxibrowser_render::RenderDocument::from_html(
            html,
            Some("http://test.local/"),
            viewport,
        )
        .expect("render fixture");
        DomSnapshot::from_render_document(&doc, "http://test.local/", "test")
    }

    fn button_query(name: &str) -> LocatorQuery {
        LocatorQuery {
            spec: LocatorSpec::Role {
                role: "button".into(),
                name: Some(name.into()),
            },
            nth: None,
        }
    }

    #[test]
    fn role_name_resolves_single_match() {
        let s = snapshot_of(r#"<button>Go</button><a href="/x">Go away</a>"#);
        let el = resolve(&s, &button_query("go")).unwrap();
        assert_eq!(el.tag, "button");
    }

    #[test]
    fn strict_mode_violation_on_ambiguous() {
        let s = snapshot_of(r#"<button>Go</button><button>Go too</button>"#);
        let err = resolve(&s, &button_query("go")).unwrap_err();
        assert_eq!(err.code, "STRICT_MODE_VIOLATION");
        assert!(err.message.contains("narrow"));
    }

    #[test]
    fn first_and_nth_disambiguate() {
        let s = snapshot_of(r#"<button>Go</button><button>Go too</button>"#);
        let mut q = button_query("go");
        q.nth = Some(0);
        let el = resolve(&s, &q).unwrap();
        assert_eq!(el.text, "Go");
        q.nth = Some(1);
        let el = resolve(&s, &q).unwrap();
        assert_eq!(el.text, "Go too");
        q.nth = Some(5);
        assert_eq!(resolve(&s, &q).unwrap_err().code, "LOCATOR_OUT_OF_RANGE");
    }

    #[test]
    fn not_found_reports_reobserve() {
        let s = snapshot_of(r#"<button>Go</button>"#);
        let err = resolve(&s, &button_query("submit")).unwrap_err();
        assert_eq!(err.code, "LOCATOR_NOT_FOUND");
        assert!(err.message.contains("re-observe"));
    }

    #[test]
    fn css_matches_selector_or_last_segment() {
        // With an id the path short-circuits to `#id`.
        let s = snapshot_of(r#"<div><p><button id="go">Go</button></p></div>"#);
        let q = |css: &str| LocatorQuery {
            spec: LocatorSpec::Css { css: css.into() },
            nth: None,
        };
        assert!(resolve(&s, &q("#go")).is_ok());
        assert!(resolve(&s, &q("button")).is_err());

        // Without ids the full path is `div > p > button`; the last
        // segment matches.
        let s2 = snapshot_of(r#"<div><p><button>Go</button></p></div>"#);
        assert!(resolve(&s2, &q("button")).is_ok());
        assert!(resolve(&s2, &q("p > button")).is_ok());
        assert!(resolve(&s2, &q("#nope")).is_err());
    }
}
