//! [`LoginDetector`] — broker-computed login-success signals (upper design
//! §4.3).
//!
//! Page text is never trusted on its own (research 06 §5.4): every signal is
//! computed by the broker from browser state the page cannot forge into a
//! verdict by itself —
//!
//! | signal | evidence | weight |
//! |---|---|---|
//! | cookie | new HttpOnly+Secure cookie with a session-ish name in scope | high (4) |
//! | navigation | history shows an auth path, current URL has left it | medium (2) |
//! | DOM | `meta[name=user-login]`, logout form, avatar (from a snapshot) | medium (2) |
//! | storage | new token-ish localStorage key in the scope's buckets | medium (2) |
//! | explicit | user/host `done` (`explicit_success`) | decisive |
//!
//! Verdict: explicit → logged in; otherwise at least one **high** signal and
//! a combined score ≥ 6 (cookie + one corroborating medium). A cookie alone
//! or mediums alone never confirm. Stored-state baselines (cookies,
//! localStorage keys) come from [`LoginDetector::snapshot_session`] taken
//! before the login flow, so only *new* state counts.
//!
//! Everything is fed through public `Session` accessors only; the pure
//! [`LoginDetector::assess_input`] core is unit-testable without a browser.

use std::collections::BTreeSet;

use crate::network::cookie::CookieEntry;
use crate::session::Session;

use super::record::validate_scope;

/// Weight of a high-confidence signal (new session cookie).
pub const WEIGHT_HIGH: u32 = 4;
/// Weight of a medium-confidence signal (nav/DOM/storage).
pub const WEIGHT_MEDIUM: u32 = 2;
/// Score required (together with one high signal) for a non-explicit verdict.
pub const SCORE_THRESHOLD: u32 = 6;

/// Which broker signal fired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalKind {
    /// New in-scope HttpOnly+Secure session cookie.
    SessionCookie { name: String },
    /// Navigation left `/login`-family paths.
    NavigationAway { from: String, to: String },
    /// DOM marker found in the snapshot.
    DomMarker { marker: &'static str },
    /// New token-ish localStorage key.
    LocalStorageToken { key: String },
    /// User/host-declared success.
    Explicit,
}

impl SignalKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SignalKind::SessionCookie { .. } => "cookie",
            SignalKind::NavigationAway { .. } => "navigation",
            SignalKind::DomMarker { .. } => "dom",
            SignalKind::LocalStorageToken { .. } => "storage",
            SignalKind::Explicit => "explicit",
        }
    }
}

/// One fired signal with its weight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signal {
    pub kind: SignalKind,
    pub weight: u32,
}

/// Detection verdict with the evidence trail (surfaced to status/REPL/CDP).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    pub logged_in: bool,
    pub score: u32,
    pub signals: Vec<Signal>,
}

/// Pre-login state baseline: only state absent from this snapshot counts as
/// *new* evidence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreLoginSnapshot {
    /// `domain|name|path` of every cookie in the jar.
    pub cookie_keys: BTreeSet<String>,
    /// localStorage key names across the scope's origin buckets.
    pub storage_keys: BTreeSet<String>,
}

/// Everything [`LoginDetector::assess_input`] needs, gathered from public
/// accessors. Fields are plain data so tests can synthesize sessions.
#[derive(Debug, Clone, Default)]
pub struct DetectionInput {
    /// Current in-scope cookies (detector re-filters by domain).
    pub cookies: Vec<CookieEntry>,
    pub baseline_cookie_keys: BTreeSet<String>,
    /// Path of the current document, when any page is loaded.
    pub current_path: Option<String>,
    /// Paths seen by this session's history.
    pub history_paths: Vec<String>,
    /// Serialized DOM of the current document (`Session::dom_snapshot`).
    pub dom_html: Option<String>,
    /// localStorage key names across the scope's buckets.
    pub local_storage_keys: Vec<String>,
    pub baseline_storage_keys: BTreeSet<String>,
    pub explicit_success: bool,
}

/// Login-success detector for one account scope.
#[derive(Debug, Clone)]
pub struct LoginDetector {
    scope: String,
}

impl LoginDetector {
    /// Detector for a registrable-domain scope.
    pub fn new(scope: impl Into<String>) -> crate::error::Result<Self> {
        let scope = scope.into();
        validate_scope(&scope)?;
        Ok(LoginDetector { scope })
    }

    /// The scope this detector judges.
    pub fn scope(&self) -> &str {
        &self.scope
    }

    /// Baseline snapshot of a session taken **before** a login flow starts.
    pub fn snapshot_session(&self, session: &Session) -> PreLoginSnapshot {
        PreLoginSnapshot {
            cookie_keys: session
                .cookie_jar()
                .read()
                .get_all()
                .into_iter()
                .map(|c| cookie_key(&c))
                .collect(),
            storage_keys: scope_storage_keys(session, &self.scope),
        }
    }

    /// Gather current evidence from public accessors (cookies, localStorage,
    /// history, current URL). DOM HTML is supplied by the caller because
    /// `Session::dom_snapshot` needs `&mut` — see [`LoginDetector::assess`].
    pub fn input_from_session(
        &self,
        session: &Session,
        baseline: &PreLoginSnapshot,
        dom_html: Option<String>,
        explicit_success: bool,
    ) -> DetectionInput {
        DetectionInput {
            cookies: session.cookie_jar().read().get_all(),
            baseline_cookie_keys: baseline.cookie_keys.clone(),
            current_path: session.current_url().map(|u| u.path().to_string()),
            history_paths: session
                .history()
                .iter()
                .map(|u| u.path().to_string())
                .collect(),
            dom_html,
            local_storage_keys: scope_storage_keys(session, &self.scope)
                .into_iter()
                .collect(),
            baseline_storage_keys: baseline.storage_keys.clone(),
            explicit_success,
        }
    }

    /// Async convenience: snapshot DOM then judge (uses `&mut Session`).
    pub async fn assess(
        &self,
        session: &mut Session,
        baseline: &PreLoginSnapshot,
        explicit_success: bool,
    ) -> crate::error::Result<Detection> {
        let dom_html = match session.dom_snapshot().await {
            Ok(Some(snap)) => Some(snap.to_html()),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(error = %e, "dom snapshot failed for login detection");
                None
            }
        };
        let input = self.input_from_session(session, baseline, dom_html, explicit_success);
        Ok(self.assess_input(&input))
    }

    /// Judge one input (pure — no browser needed).
    pub fn assess_input(&self, input: &DetectionInput) -> Detection {
        let mut signals = Vec::new();

        if input.explicit_success {
            signals.push(Signal {
                kind: SignalKind::Explicit,
                weight: u32::MAX,
            });
        }

        // (a) new HttpOnly+Secure session-ish cookie in scope
        for cookie in in_scope_cookies(&input.cookies, &self.scope) {
            let key = cookie_key(cookie);
            if input.baseline_cookie_keys.contains(&key) {
                continue;
            }
            if cookie.http_only && cookie.secure && is_session_cookie_name(&cookie.name) {
                signals.push(Signal {
                    kind: SignalKind::SessionCookie {
                        name: cookie.name.clone(),
                    },
                    weight: WEIGHT_HIGH,
                });
            }
        }

        // (b) navigation away from auth paths
        if let Some(to) = &input.current_path
            && let Some(from) = input
                .history_paths
                .iter()
                .find(|p| is_auth_path(p) && p.as_str() != to.as_str())
            && !is_auth_path(to)
        {
            signals.push(Signal {
                kind: SignalKind::NavigationAway {
                    from: (*from).clone(),
                    to: to.clone(),
                },
                weight: WEIGHT_MEDIUM,
            });
        }

        // (c) DOM markers
        if let Some(html) = &input.dom_html {
            for marker in dom_login_markers(html) {
                signals.push(Signal {
                    kind: SignalKind::DomMarker { marker },
                    weight: WEIGHT_MEDIUM,
                });
            }
        }

        // (d) new token-ish localStorage keys
        for key in &input.local_storage_keys {
            if input.baseline_storage_keys.contains(key) {
                continue;
            }
            if is_tokenish_storage_key(key) {
                signals.push(Signal {
                    kind: SignalKind::LocalStorageToken { key: key.clone() },
                    weight: WEIGHT_MEDIUM,
                });
            }
        }

        let explicit = signals
            .iter()
            .any(|s| matches!(s.kind, SignalKind::Explicit));
        let has_high = signals.iter().any(|s| s.weight == WEIGHT_HIGH);
        let score: u32 = signals.iter().map(|s| s.weight).sum();

        Detection {
            logged_in: explicit || (has_high && score >= SCORE_THRESHOLD),
            score,
            signals,
        }
    }
}

/// `domain|name|path` identity of a cookie (jar replacement semantics).
fn cookie_key(cookie: &CookieEntry) -> String {
    format!(
        "{}|{}|{}",
        cookie.domain.as_deref().unwrap_or(""),
        cookie.name,
        cookie.path.as_deref().unwrap_or("")
    )
}

/// Cookies whose registrable domain equals `scope` (same rule as
/// `Session::export_state_for_scope`).
fn in_scope_cookies<'a>(cookies: &'a [CookieEntry], scope: &str) -> Vec<&'a CookieEntry> {
    cookies
        .iter()
        .filter(|c| {
            c.domain
                .as_deref()
                .map(|d| {
                    crate::network::cookie::registrable_domain(d.trim_start_matches('.')) == scope
                })
                .unwrap_or(false)
        })
        .collect()
}

/// Session-ish cookie names (§4.3): `user_session`, `__Host-…`,
/// `logged_in`, `remember_*`, auth/token families.
pub(crate) fn is_session_cookie_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with("__host-")
        || n.starts_with("__secure-")
        || n.contains("session")
        || n.contains("logged_in")
        || n.contains("logintoken")
        || n.contains("remember")
        || n.contains("auth")
        || n.contains("token")
        || n.contains("sid")
}

/// Auth-path family: any path segment `login` / `signin` / `sign-in` /
/// `sign_in` / `session` / `auth` / `authenticate`.
pub(crate) fn is_auth_path(path: &str) -> bool {
    path.split(['/', '?', '#']).any(|seg| {
        matches!(
            seg.to_ascii_lowercase().as_str(),
            "login" | "signin" | "sign-in" | "sign_in" | "session" | "auth" | "authenticate"
        )
    })
}

/// Token-ish localStorage key names (§4.3 storage signal).
pub(crate) fn is_tokenish_storage_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    k.contains("token")
        || k.contains("jwt")
        || k.contains("auth")
        || k.contains("session")
        || k.contains("oidc")
}

/// localStorage keys across every scope bucket, via
/// `Session::export_state_for_scope` (public API; no session.rs changes).
fn scope_storage_keys(session: &Session, scope: &str) -> BTreeSet<String> {
    session
        .export_state_for_scope(scope)
        .origins
        .into_iter()
        .flat_map(|o| o.local_storage.into_iter().map(|e| e.name))
        .collect()
}

/// DOM markers present in serialized HTML (§4.3): `meta[name=user-login]`
/// (GitHub) with non-empty content, a logout form, an avatar image.
pub(crate) fn dom_login_markers(html: &str) -> Vec<&'static str> {
    let mut markers = Vec::new();
    if meta_user_login_present(html) {
        markers.push("meta[name=user-login]");
    }
    if logout_form_present(html) {
        markers.push("form[action*=logout]");
    }
    if avatar_present(html) {
        markers.push("avatar");
    }
    markers
}

static META_USER_LOGIN: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r#"(?i)<meta\b[^>]*\bname\s*=\s*["']?user-login["']?[^>]*>"#).unwrap()
});
// `name` and `content` are checked independently of their order inside the
// tag: the snapshot serializer emits attributes in HashMap order, so
// `content` may precede `name`.
static META_TAG_NONEMPTY_CONTENT: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"(?i)<meta\b[^>]*\bcontent\s*=\s*["']?[^"'\s>]+["']?[^>]*>"#).unwrap()
    });
static META_TAG_USER_LOGIN_NAME: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"(?i)\bname\s*=\s*["']?user-login["']?"#).unwrap()
    });
static LOGOUT_FORM: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r#"(?i)<form\b[^>]*\baction\s*=\s*["']?[^"'>]*logout"#).unwrap()
});
static AVATAR: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(
        r#"(?i)<(img|span|div)\b[^>]*(\bclass|\balt|\baria-label)\s*=\s*["'][^"']*avatar"#,
    )
    .unwrap()
});

fn meta_user_login_present(html: &str) -> bool {
    // Every `<meta …>` tag carrying `name="user-login"` must also carry a
    // non-empty `content` (order-independent attribute inspection).
    for tag in META_USER_LOGIN.find_iter(html) {
        if META_TAG_USER_LOGIN_NAME.is_match(tag.as_str())
            && META_TAG_NONEMPTY_CONTENT.is_match(tag.as_str())
        {
            return true;
        }
    }
    false
}

fn logout_form_present(html: &str) -> bool {
    LOGOUT_FORM.is_match(html)
}

fn avatar_present(html: &str) -> bool {
    AVATAR.is_match(html)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detector() -> LoginDetector {
        LoginDetector::new("github.com").unwrap()
    }

    fn cookie(name: &str, domain: &str, http_only: bool, secure: bool) -> CookieEntry {
        CookieEntry {
            name: name.into(),
            value: "v".into(),
            path: Some("/".into()),
            domain: Some(domain.into()),
            secure,
            http_only,
            same_site: None,
            expires: None,
            max_age: None,
            expiry: None,
            partitioned: false,
            partition_key: None,
        }
    }

    fn input() -> DetectionInput {
        DetectionInput::default()
    }

    #[test]
    fn cookie_alone_never_confirms() {
        let mut i = input();
        i.cookies
            .push(cookie("user_session", "github.com", true, true));
        let d = detector().assess_input(&i);
        assert!(!d.logged_in);
        assert_eq!(d.score, WEIGHT_HIGH);
    }

    #[test]
    fn cookie_plus_dom_marker_confirms() {
        let mut i = input();
        i.cookies
            .push(cookie("user_session", "github.com", true, true));
        i.dom_html =
            Some(r#"<html><head><meta name="user-login" content="garden"></head></html>"#.into());
        let d = detector().assess_input(&i);
        assert!(d.logged_in);
        assert!(d.signals.iter().any(|s| s.kind
            == SignalKind::DomMarker {
                marker: "meta[name=user-login]"
            }));
    }

    #[test]
    fn explicit_signal_is_decisive() {
        let mut i = input();
        i.explicit_success = true;
        let d = detector().assess_input(&i);
        assert!(d.logged_in);
    }

    #[test]
    fn mediums_without_cookie_never_confirm() {
        let mut i = input();
        i.history_paths = vec!["/login".into(), "/".into()];
        i.current_path = Some("/".into());
        i.dom_html = Some(r#"<form action="/logout" method="post"></form>"#.into());
        i.local_storage_keys = vec!["auth.token".into()];
        let d = detector().assess_input(&i);
        assert!(!d.logged_in, "medium-only evidence must not confirm");
        assert_eq!(d.signals.len(), 3);
    }

    #[test]
    fn non_httponly_or_insecure_cookie_ignored() {
        let mut i = input();
        i.cookies
            .push(cookie("user_session", "github.com", false, true));
        i.cookies
            .push(cookie("user_session", "github.com", true, false));
        i.dom_html = Some(r#"<img class="avatar" src="/a.png">"#.into());
        let d = detector().assess_input(&i);
        assert!(!d.logged_in, "cookie signal requires HttpOnly+Secure");
        assert!(
            !d.signals
                .iter()
                .any(|s| matches!(s.kind, SignalKind::SessionCookie { .. }))
        );
    }

    #[test]
    fn baseline_cookie_does_not_count() {
        let mut i = input();
        let c = cookie("user_session", "github.com", true, true);
        i.cookies.push(c.clone());
        i.baseline_cookie_keys.insert(cookie_key(&c));
        i.dom_html = Some(r#"<img class="avatar" src="/a.png">"#.into());
        let d = detector().assess_input(&i);
        assert!(!d.logged_in, "pre-existing cookie is not new evidence");
    }

    #[test]
    fn out_of_scope_cookie_ignored() {
        let mut i = input();
        i.cookies
            .push(cookie("user_session", "evil.org", true, true));
        let d = detector().assess_input(&i);
        assert!(
            !d.signals
                .iter()
                .any(|s| matches!(s.kind, SignalKind::SessionCookie { .. }))
        );
    }

    #[test]
    fn navigation_away_fires_only_when_leaving_auth_path() {
        let d1 = detector().assess_input(&DetectionInput {
            history_paths: vec!["/login".into(), "/dashboard".into()],
            current_path: Some("/dashboard".into()),
            ..input()
        });
        assert!(
            d1.signals
                .iter()
                .any(|s| matches!(s.kind, SignalKind::NavigationAway { .. }))
        );

        // still on a login path → no signal
        let d2 = detector().assess_input(&DetectionInput {
            history_paths: vec!["/login".into(), "/session".into()],
            current_path: Some("/session".into()),
            ..input()
        });
        assert!(
            !d2.signals
                .iter()
                .any(|s| matches!(s.kind, SignalKind::NavigationAway { .. }))
        );
    }

    #[test]
    fn auth_path_segments() {
        assert!(is_auth_path("/login"));
        assert!(is_auth_path("/users/sign_in"));
        assert!(is_auth_path("/accounts/authenticate?next=/"));
        assert!(is_auth_path("/session/new"));
        assert!(!is_auth_path("/dashboard"));
        assert!(!is_auth_path("/login-required-info")); // segments match exactly
    }

    #[test]
    fn tokenish_and_session_names() {
        assert!(is_session_cookie_name("user_session"));
        assert!(is_session_cookie_name("__Host-next-auth.csrf-token"));
        assert!(is_session_cookie_name("logged_in"));
        assert!(is_session_cookie_name("remember_web"));
        assert!(!is_session_cookie_name("theme"));
        assert!(is_tokenish_storage_key("access_token"));
        assert!(is_tokenish_storage_key("oidc.user"));
        assert!(!is_tokenish_storage_key("theme:dark"));
    }

    #[test]
    fn dom_marker_variants() {
        assert!(dom_login_markers("<p>hello</p>").is_empty());
        assert!(
            dom_login_markers(r#"<meta name='user-login' content='octocat'>"#)
                .contains(&"meta[name=user-login]")
        );
        // meta without content is not evidence
        assert!(
            !dom_login_markers(r#"<meta name="user-login">"#).contains(&"meta[name=user-login]")
        );
        assert!(dom_login_markers(
            r#"<form action="https://github.com/logout" method="post"><button>Sign out</button></form>"#
        )
        .contains(&"form[action*=logout]"));
        assert!(
            dom_login_markers(r#"<img class="avatar avatar-user" src="/u.png">"#)
                .contains(&"avatar")
        );
    }
}
