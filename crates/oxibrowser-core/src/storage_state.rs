//! Playwright-compatible storage state: cookies + per-origin localStorage.
//!
//! [`StorageState`] is the serializable snapshot exchanged via
//! [`crate::session::Session::export_state`] / [`crate::session::Session::import_state`].
//! The JSON shape follows Playwright's `storageState` (`cookies` +
//! `origins[].localStorage`), so snapshots are hand-writable and diffable.
//!
//! # Example
//!
//! ```
//! use oxibrowser_core::storage_state::StorageState;
//! let json = r#"{"cookies":[],"origins":[{"origin":"https://example.com","localStorage":[{"name":"session","value":"abc"}]}]}"#;
//! let st: StorageState = serde_json::from_str(json).unwrap();
//! assert_eq!(st.origins[0].local_storage[0].name, "session");
//! ```

use crate::network::cookie::CookieEntry;
use serde::{Deserialize, Serialize};

/// Persisted browser storage state: cookies plus per-origin localStorage.
///
/// Both fields default to empty so a partial snapshot (cookies only, or
/// storage only) deserializes without ceremony.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct StorageState {
    #[serde(default)]
    pub cookies: Vec<CookieEntry>,
    #[serde(default)]
    pub origins: Vec<OriginState>,
}

/// localStorage entries for one origin.
///
/// Playwright spells each entry `{"name": ..., "value": ...}` — mirror that
/// exactly so snapshots are interchangeable.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct LocalStorageEntry {
    pub name: String,
    pub value: String,
}

/// localStorage entries for one origin.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OriginState {
    pub origin: String,
    #[serde(rename = "localStorage")]
    pub local_storage: Vec<LocalStorageEntry>,
}

impl StorageState {
    /// Parse a Netscape `cookies.txt` file (the simple 7-field TAB format
    /// written by curl/wget/browser extensions) into a [`StorageState`] with
    /// cookies only.
    ///
    /// Format per line: `domain \t include_subdomains \t path \t secure \t
    /// expiry \t name \t value`. Comment (`#`) and blank lines are skipped;
    /// the `#HttpOnly_` prefix convention marks HttpOnly cookies. A `0`
    /// expiry means a session cookie. Malformed lines are skipped (import
    /// never fails on tail junk).
    pub fn from_netscape(text: &str) -> Self {
        let mut cookies = Vec::new();
        for line in text.lines() {
            let (http_only, line) = match line.strip_prefix("#HttpOnly_") {
                Some(rest) => (true, rest),
                None => (false, line),
            };
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = line.split('\t').collect();
            let [domain, _subdomains, path, secure, expiry, name, value] = match fields[..] {
                [d, s, p, sec, e, n, v] => [d, s, p, sec, e, n, v],
                _ => continue,
            };
            cookies.push(CookieEntry {
                name: name.to_string(),
                value: value.to_string(),
                path: Some(path.to_string()),
                domain: Some(domain.trim_start_matches('.').to_string()),
                secure: secure.eq_ignore_ascii_case("true"),
                http_only,
                same_site: None,
                expires: None,
                max_age: None,
                expiry: expiry.parse::<i64>().ok().filter(|t| *t > 0),
                ..CookieEntry::default()
            });
        }
        StorageState {
            cookies,
            origins: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `CookieEntry::same_site` must round-trip through the Playwright
    /// spelling (`"Lax"` / `"Strict"` / `"None"`): serde's derived unit-variant
    /// representation is the variant name, which already matches.
    #[test]
    fn same_site_serializes_playwright_compatible() {
        let json = serde_json::to_string(&Some(crate::network::cookie::SameSite::Lax)).unwrap();
        assert_eq!(json, r#""Lax""#);
        let json = serde_json::to_string(&Some(crate::network::cookie::SameSite::Strict)).unwrap();
        assert_eq!(json, r#""Strict""#);
        let json = serde_json::to_string(&Some(crate::network::cookie::SameSite::None)).unwrap();
        assert_eq!(json, r#""None""#);
        let back: Option<crate::network::cookie::SameSite> =
            serde_json::from_str(r#""Lax""#).unwrap();
        assert_eq!(back, Some(crate::network::cookie::SameSite::Lax));
    }

    #[test]
    fn storage_state_round_trips_through_json() {
        let st = StorageState {
            cookies: vec![CookieEntry {
                name: "sid".into(),
                value: "abc".into(),
                path: Some("/".into()),
                domain: Some("example.com".into()),
                secure: true,
                http_only: true,
                same_site: Some(crate::network::cookie::SameSite::Lax),
                ..Default::default()
            }],
            origins: vec![OriginState {
                origin: "https://example.com".into(),
                local_storage: vec![LocalStorageEntry {
                    name: "k".into(),
                    value: "v".into(),
                }],
            }],
        };
        let json = serde_json::to_string(&st).unwrap();
        assert!(
            json.contains(r#""sameSite":"Lax""#),
            "sameSite must keep the Playwright spelling: {json}"
        );
        assert!(
            json.contains(r#""localStorage""#),
            "localStorage key must be camelCase: {json}"
        );
        assert!(
            json.contains(r#"{"name":"k","value":"v"}"#),
            "entries must use the Playwright name/value object form: {json}"
        );
        let back: StorageState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cookies[0].name, "sid");
        assert_eq!(
            back.cookies[0].same_site,
            Some(crate::network::cookie::SameSite::Lax)
        );
        assert_eq!(
            back.origins[0].local_storage[0],
            LocalStorageEntry {
                name: "k".into(),
                value: "v".into()
            }
        );
    }

    #[test]
    fn partial_snapshots_deserialize_with_defaults() {
        let st: StorageState = serde_json::from_str(r#"{"cookies":[]}"#).unwrap();
        assert!(st.origins.is_empty());
        let st: StorageState = serde_json::from_str("{}").unwrap();
        assert!(st.cookies.is_empty() && st.origins.is_empty());
    }

    #[test]
    fn netscape_parse_maps_flags_domains_and_expiry() {
        let text = "# Netscape HTTP Cookie File\n\
                    # comment lines are skipped\n\
                    .example.com\tTRUE\t/\tTRUE\t1893456000\tsid\tabc123\n\
                    #HttpOnly_.example.com\tTRUE\t/\tTRUE\t1893456000\tuser_session\thunter2\n\
                    example.com\tFALSE\t/sub\tFALSE\t0\tguest\tv\n\
                    this line has too few fields\n\
                    \n";
        let st = StorageState::from_netscape(text);
        assert_eq!(st.cookies.len(), 3);
        assert_eq!(st.origins.len(), 0);

        let sid = &st.cookies[0];
        assert_eq!(sid.name, "sid");
        assert_eq!(sid.value, "abc123");
        assert_eq!(sid.domain.as_deref(), Some("example.com"));
        assert_eq!(sid.path.as_deref(), Some("/"));
        assert!(sid.secure);
        assert!(!sid.http_only);
        assert_eq!(sid.expiry, Some(1893456000));

        let sess = &st.cookies[1];
        assert_eq!(sess.name, "user_session");
        assert!(sess.http_only, "#HttpOnly_ prefix must mark HttpOnly");
        assert_eq!(sess.domain.as_deref(), Some("example.com"));

        let guest = &st.cookies[2];
        assert!(!guest.secure);
        assert!(!guest.http_only);
        assert_eq!(guest.expiry, None, "0 expiry means a session cookie");
        assert_eq!(guest.path.as_deref(), Some("/sub"));
    }

    /// Parsed cookies must round-trip through the Playwright JSON and come
    /// back through `import_state` (the CLI import path).
    #[test]
    fn netscape_output_serializes_playwright_compatible() {
        let st = StorageState::from_netscape(".example.com\tTRUE\t/\tTRUE\t1893456000\tsid\tabc\n");
        let json = serde_json::to_string(&st).unwrap();
        let back: StorageState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cookies.len(), 1);
        assert_eq!(back.cookies[0].name, "sid");
    }
}
