//! Irreversible-action pattern gate (upper design §8.2, FM-L8; roadmap
//! item 16).
//!
//! `interact`-grade grants stop at the pattern list: clicking/filling a
//! control whose descriptor (selector, page URL, enclosing form action,
//! aria-label, visible text) matches an irreversible pattern requires a
//! context carrying the `irreversible` action. The list is **best-effort
//! and knowingly incomplete** (§8.2) — deny-biased defaults plus
//! per-account injection (`account.json` `irreversible_patterns`) let
//! operators harden specific services ("확정 게이트").

/// Built-in deny-biased defaults. Matched as case-insensitive substrings
/// against the interaction descriptor.
pub const DEFAULT_IRREVERSIBLE_PATTERNS: &[&str] = &[
    "delete account",
    "delete my",
    "delete this",
    "delete permanently",
    "cancel account",
    "close account",
    "unsubscribe",
    "checkout",
    "place order",
    "confirm order",
    "complete purchase",
    "buy now",
    "pay now",
    "make payment",
    "withdraw",
    "send money",
    "transfer funds",
    "danger zone",
];

/// First pattern (defaults + `extra`, in that order) contained in the
/// lowercased `haystack`, if any.
pub fn matched_pattern<'a>(haystack: &str, extra: &'a [String]) -> Option<&'a str> {
    let hay = haystack.to_ascii_lowercase();
    DEFAULT_IRREVERSIBLE_PATTERNS
        .iter()
        .find(|p| hay.contains(*p))
        .map(|p| *p)
        .or_else(|| {
            extra
                .iter()
                .find(|p| hay.contains(p.as_str()))
                .map(|p| p.as_str())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extras(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn defaults_match_destructive_and_payment_controls() {
        assert!(matched_pattern("button#del → Delete Account", &[]).is_some());
        assert!(matched_pattern("https://shop.io/checkout form=…", &[]).is_some());
        assert!(matched_pattern("Place Order", &[]).is_some());
    }

    #[test]
    fn innocent_controls_pass() {
        assert!(matched_pattern("a.nav → Documentation", &[]).is_none());
        assert!(matched_pattern("input#search", &[]).is_none());
        assert!(matched_pattern("button → Load more", &[]).is_none());
    }

    #[test]
    fn per_account_patterns_extend_not_replace() {
        let extra = extras(&["purge workspace"]);
        assert!(matched_pattern("Purge Workspace", &extra).is_some());
        // defaults still apply alongside the injection
        assert!(matched_pattern("Unsubscribe", &extra).is_some());
    }

    #[test]
    fn matching_is_case_insensitive_substring() {
        assert!(matched_pattern("CLICK TO DELETE THIS POST", &[]).is_some());
    }
}
