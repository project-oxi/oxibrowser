//! Curated validation-probe markers per service (roadmap item 12).
//!
//! A probe config per well-known scope: URL + a marker that exists **only
//! when authenticated**. Seeded from verified facts only — entries must be
//! either confirmed in this repo's design/testing or trivially checkable;
//! unverified guesses stay out (community contributions welcome via PR
//! with evidence). Absent scope → the §4.4 scope-root fallback applies.
//!
//! These also anchor the storage-compatibility matrix: a service whose
//! auth token lives in IndexedDB passes the probe after login yet drops
//! the session on envelope-only restore — the FM-L5 pattern.

use super::record::ProbeConfig;

/// Curated probes, keyed by registrable domain (exact match).
///
/// - `github.com` — `meta[name=user-login]` on the settings page: emitted
///   only for the signed-in user (verified in this repo's design + probe
///   tests, `2026-09-28` design §4.1/§4.3).
pub fn curated_for(scope: &str) -> Option<ProbeConfig> {
    let (site, url, marker) = match scope {
        "github.com" => (
            "github.com",
            "https://github.com/settings/profile",
            "meta[name=user-login]",
        ),
        _ => return None,
    };
    Some(ProbeConfig {
        url: url.replace("{site}", site).to_string(),
        marker: Some(marker.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_marker_curated() {
        let p = curated_for("github.com").expect("github curated");
        assert_eq!(p.url, "https://github.com/settings/profile");
        assert_eq!(p.marker.as_deref(), Some("meta[name=user-login]"));
    }

    #[test]
    fn unknown_scope_falls_back_to_none() {
        assert!(curated_for("example.com").is_none());
        assert!(curated_for("notgithub.com").is_none());
    }
}
