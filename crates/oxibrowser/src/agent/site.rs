//! Site files (execution design M4): per-origin memory of what worked.
//!
//! `~/.local/share/oxibrowser/sites/<host>.json` accumulates verified
//! selectors (from successful locator actions), login markers, and
//! discovered API endpoints. The observe command surfaces them as
//! `site_hints`, so a second run of the same task starts from proven
//! selectors instead of re-probing the page.
//!
//! Local-only by design (privacy boundary: no sharing, no sync).

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One origin's site file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SiteFile {
    /// Selector → times it resolved and the action succeeded.
    #[serde(default)]
    pub selectors: HashMap<String, u32>,
    /// Login markers observed (probe or detected login forms).
    #[serde(default)]
    pub login_markers: Vec<String>,
    /// API endpoints seen in the network log (method + URL, capped).
    #[serde(default)]
    pub api_endpoints: Vec<String>,
}

fn dir() -> PathBuf {
    if let Some(base) = std::env::var_os("OXIBROWSER_STATE_DIR") {
        return PathBuf::from(base).join("sites");
    }
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join(".local/share/oxibrowser/sites")
}

fn path_for(origin: &str) -> Option<PathBuf> {
    let host = origin
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split([':', '/'])
        .next()?;
    if host.is_empty() {
        return None;
    }
    // One file per host; no path traversal from page-controlled data.
    let safe: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Some(dir().join(format!("{safe}.json")))
}

/// Load the site file for an origin (missing → empty).
pub fn load(origin: &str) -> SiteFile {
    let Some(path) = path_for(origin) else {
        return SiteFile::default();
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Persist the site file for an origin (best-effort).
pub fn save(origin: &str, file: &SiteFile) {
    let Some(path) = path_for(origin) else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(file) {
        let _ = std::fs::write(path, json);
    }
}

/// Record a selector that resolved and acted successfully.
pub fn record_selector(origin: &str, selector: &str) {
    let mut file = load(origin);
    *file.selectors.entry(selector.to_string()).or_insert(0) += 1;
    save(origin, &file);
}

/// Record a login marker.
pub fn record_login_marker(origin: &str, marker: &str) {
    let mut file = load(origin);
    if !file.login_markers.iter().any(|m| m == marker) {
        file.login_markers.push(marker.to_string());
    }
    save(origin, &file);
}

/// Record an API endpoint (deduped, capped at 32).
pub fn record_endpoint(origin: &str, endpoint: &str) {
    let mut file = load(origin);
    if !file.api_endpoints.iter().any(|e| e == endpoint) {
        file.api_endpoints.push(endpoint.to_string());
        if file.api_endpoints.len() > 32 {
            file.api_endpoints.remove(0);
        }
    }
    save(origin, &file);
}

/// Hints for the observe response: the strongest verified selectors.
pub fn hints(origin: &str) -> Vec<String> {
    let file = load(origin);
    let mut selectors: Vec<(String, u32)> = file.selectors.into_iter().collect();
    selectors.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    selectors
        .into_iter()
        .take(6)
        .map(|(s, n)| format!("{s} (verified {n}x)"))
        .chain(
            file.login_markers
                .into_iter()
                .take(2)
                .map(|m| format!("login: {m}")),
        )
        .chain(
            file.api_endpoints
                .into_iter()
                .rev()
                .take(4)
                .map(|e| format!("api: {e}")),
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_is_host_scoped_and_sanitized() {
        assert!(
            path_for("http://example.com/x")
                .unwrap()
                .ends_with("example.com.json")
        );
        assert!(path_for("https://a..b-..example").is_some());
        assert!(path_for("").is_none());
    }
}
