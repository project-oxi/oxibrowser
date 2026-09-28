//! [`ValidationProbe`] — post-detection false-positive defense (upper design
//! §4.4).
//!
//! A detector success alone never seals an envelope: the probe GETs the
//! account's probe URL (default: scope root) through the live session's HTTP
//! client — so the account's cookies ride along — and requires the
//! configured marker to be present **and** no login form in the response.
//! Bot-management challenges ([`crate::challenge`]) are classified and
//! reported rather than treated as invalid sessions (the session may be
//! fine; the probe path is just blocked).
//!
//! With no probe configured the fallback runs: scope root + login-form
//! absence only.

use crate::challenge::{self, DetectedChallenge};
use crate::error::Result;
use crate::network::HttpClient;
use url::Url;

use super::record::ProbeConfig;

/// Probe verdict (§4.4).
#[derive(Debug, Clone, PartialEq)]
pub enum ProbeVerdict {
    /// Marker present / login form absent — session is usable.
    Valid,
    /// Probe proved the session is no longer authenticated (`stale`).
    Invalid { reason: String },
    /// A bot-management challenge blocked the probe (`challenge`).
    Challenge { challenge: DetectedChallenge },
    /// Network/transport failure — inconclusive, state must not flip.
    Unreachable { reason: String },
}

/// Full probe result for surfaces (status board, audits, tests).
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeOutcome {
    pub verdict: ProbeVerdict,
    pub http_status: Option<u16>,
    /// Marker presence when a marker was configured.
    pub marker_found: Option<bool>,
    /// Login-form detection in the probed document.
    pub login_form_present: Option<bool>,
}

/// Configured probe for one account.
#[derive(Debug, Clone)]
pub struct ValidationProbe {
    url: Url,
    marker: Option<String>,
}

impl ValidationProbe {
    /// Probe from the record's `probe` config; falls back to the scope root
    /// (§4.4). Errors only when the configured probe URL is unusable.
    pub fn from_record(probe: Option<&ProbeConfig>, scope: &str) -> Result<Self> {
        match probe {
            Some(cfg) => {
                let url = Url::parse(&cfg.url)
                    .map_err(|e| super::record::account_error(format!("probe url: {e}")))?;
                Ok(ValidationProbe {
                    url,
                    marker: cfg.marker.clone(),
                })
            }
            None => {
                let url = Url::parse(&format!("https://{scope}/"))
                    .map_err(|e| super::record::account_error(format!("scope url: {e}")))?;
                Ok(ValidationProbe { url, marker: None })
            }
        }
    }

    /// The GET performed by this probe.
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// Run the probe through `client` (the live session's client, so
    /// account cookies and fingerprint headers apply).
    pub async fn run(&self, client: &HttpClient) -> ProbeOutcome {
        let response = match client.fetch(&self.url).await {
            Ok(r) => r,
            Err(e) => {
                return ProbeOutcome {
                    verdict: ProbeVerdict::Unreachable {
                        reason: e.to_string(),
                    },
                    http_status: None,
                    marker_found: None,
                    login_form_present: None,
                };
            }
        };
        let status = response.status().as_u16();
        let headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let body = response.text().await.unwrap_or_default();

        // A bot-management challenge is not evidence of a dead session.
        if let Some(challenge) = challenge::detect(status, &headers, &body) {
            return ProbeOutcome {
                verdict: ProbeVerdict::Challenge { challenge },
                http_status: Some(status),
                marker_found: None,
                login_form_present: None,
            };
        }

        // Hard auth failures: the session is stale.
        if status == 401 || status == 403 {
            return ProbeOutcome {
                verdict: ProbeVerdict::Invalid {
                    reason: format!("probe_{status}"),
                },
                http_status: Some(status),
                marker_found: None,
                login_form_present: None,
            };
        }

        let login_form_present = login_form_present(&body);
        if login_form_present {
            return ProbeOutcome {
                verdict: ProbeVerdict::Invalid {
                    reason: "login_form".into(),
                },
                http_status: Some(status),
                marker_found: None,
                login_form_present: Some(true),
            };
        }

        match &self.marker {
            Some(marker) => {
                let found = marker_present(&body, marker);
                ProbeOutcome {
                    verdict: if found {
                        ProbeVerdict::Valid
                    } else {
                        ProbeVerdict::Invalid {
                            reason: "marker_missing".into(),
                        }
                    },
                    http_status: Some(status),
                    marker_found: Some(found),
                    login_form_present: Some(false),
                }
            }
            // Fallback (§4.4): scope root reachable + login form absent.
            None => ProbeOutcome {
                verdict: ProbeVerdict::Valid,
                http_status: Some(status),
                marker_found: None,
                login_form_present: Some(false),
            },
        }
    }
}

/// Does `html` contain the marker? Markers are CSS-ish selectors or literal
/// substrings:
///
/// - `meta[name=user-login]` — tag with exact attribute value
/// - `form[action*=logout]` — tag with attribute substring
/// - `#settings-nav` — element id
/// - `.user-avatar` — class-list entry
/// - anything else — literal substring
pub fn marker_present(html: &str, marker: &str) -> bool {
    let marker = marker.trim();
    if marker.is_empty() {
        return false;
    }
    if let Some(id) = marker.strip_prefix('#')
        && !id.is_empty()
    {
        return attr_value_matches(html, "id", id, AttrMatch::Exact);
    }
    if let Some(class) = marker.strip_prefix('.')
        && !class.is_empty()
    {
        return attr_value_matches(html, "class", class, AttrMatch::Word);
    }
    if let Some((tag, attr, value, substring)) = parse_selector(marker) {
        return tag_attr_matches(html, &tag, &attr, &value, substring);
    }
    html.contains(marker)
}

#[derive(Clone, Copy)]
enum AttrMatch {
    Exact,
    Word,
}

/// `id="…"`/`class="…"` attribute check with quote and word-boundary
/// handling (regex is escaped — ids/classes are data, never patterns).
fn attr_value_matches(html: &str, attr: &str, value: &str, mode: AttrMatch) -> bool {
    let pattern = format!(
        r#"(?i)\b{}\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#,
        regex::escape(attr),
    );
    let re = regex::Regex::new(&pattern).unwrap();
    re.find_iter(html).any(|m| {
        let raw = m.as_str();
        let v = raw
            .split_once('=')
            .map(|(_, v)| v.trim().trim_matches(['"', '\'']))
            .unwrap_or("");
        match mode {
            AttrMatch::Exact => v == value,
            AttrMatch::Word => v.split_whitespace().any(|w| w == value),
        }
    })
}

/// `tag[attr=value]` / `tag[attr*=value]` selector.
fn parse_selector(marker: &str) -> Option<(String, String, String, bool)> {
    let (tag, rest) = marker.split_once('[')?;
    let close = rest.strip_suffix(']')?;
    if tag.is_empty() || !tag.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    let (attr, value, substring) = if let Some((attr, value)) = close.split_once("*=") {
        (attr.trim(), value.trim(), true)
    } else if let Some((attr, value)) = close.split_once('=') {
        (attr.trim(), value.trim(), false)
    } else {
        return None;
    };
    if attr.is_empty() || value.is_empty() {
        return None;
    }
    let value = value.trim_matches(['"', '\'']);
    Some((
        tag.to_ascii_lowercase(),
        attr.to_ascii_lowercase(),
        value.to_string(),
        substring,
    ))
}

/// `<tag … attr="value" …>` open-tag check. `substring` mirrors the `[attr*=v]`
/// CSS meaning (value occurs anywhere in the attribute); otherwise the whole
/// attribute equals `value`.
fn tag_attr_matches(html: &str, tag: &str, attr: &str, value: &str, substring: bool) -> bool {
    let open = format!("<{tag}");
    let mut rest = html;
    while let Some(pos) = rest.find(&open) {
        let after = &rest[pos + open.len()..];
        let end = after.find('>').unwrap_or(after.len());
        let tag_html = &after[..end];
        if attr_in_tag(tag_html, attr, value, substring) {
            return true;
        }
        rest = &rest[pos + open.len()..];
    }
    false
}

fn attr_in_tag(tag_html: &str, attr: &str, value: &str, substring: bool) -> bool {
    let pattern = format!(
        r#"(?i)\b{}\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>/]+))"#,
        regex::escape(attr),
    );
    let re = regex::Regex::new(&pattern).unwrap();
    re.captures(tag_html).is_some_and(|caps| {
        let v = caps
            .get(1)
            .or_else(|| caps.get(2))
            .or_else(|| caps.get(3))
            .map(|m| m.as_str())
            .unwrap_or("");
        if substring {
            // only case-insensitive here: value came from the selector, not
            // from page data, so escaping it would over-restrict markers.
            let v_lc = v.to_ascii_lowercase();
            let q_lc = value.to_ascii_lowercase();
            v_lc.contains(&q_lc)
        } else {
            v == value
        }
    })
}

/// Does the document look like an unauthenticated login page? A form whose
/// action targets the auth-path family (segment check, no regex lookaround —
/// the `regex` crate has none), or any password input, counts.
pub fn login_form_present(html: &str) -> bool {
    static PASSWORD_INPUT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"(?is)<input\b[^>]*\btype\s*=\s*["']?password"#).unwrap()
    });
    static FORM_OPEN: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#"(?is)<form\b[^>]*>"#).unwrap());
    static ACTION_ATTR: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"(?i)\baction\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#).unwrap()
    });
    PASSWORD_INPUT.is_match(html)
        || FORM_OPEN.find_iter(html).any(|form| {
            ACTION_ATTR
                .captures(form.as_str())
                .and_then(|caps| caps.get(1).or_else(|| caps.get(2)).or_else(|| caps.get(3)))
                .map(|m| has_auth_segment(m.as_str()))
                .unwrap_or(false)
        })
}

/// Path-segment check on a form action value: any `/`-separated segment that
/// *is* an auth keyword, optionally with a file extension (`login.php`).
/// Exact stems only — `/authors` and `/settings` don't match; `/authenticate`,
/// `/oauth`, `/login.php` do.
pub(crate) fn has_auth_segment(action: &str) -> bool {
    const STEMS: &[&str] = &[
        "login",
        "log-in",
        "signin",
        "sign-in",
        "sign_in",
        "session",
        "sessions",
        "auth",
        "authenticate",
        "authentication",
        "authorize",
        "authorization",
        "oauth",
    ];
    action.split(['/', '?', '#', ':']).any(|seg| {
        let stem = seg.split('.').next().unwrap_or(seg);
        STEMS.contains(&stem.to_ascii_lowercase().as_str())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_literal_and_selectors() {
        let html = r#"<html><head><meta name="user-login" content="octocat"></head>
            <body><form action="/logout"></form><div id="settings-nav" class="nav user-avatar"></div></body></html>"#;
        assert!(marker_present(html, "meta[name=user-login]"));
        assert!(marker_present(html, "form[action*=logout]"));
        assert!(marker_present(html, "#settings-nav"));
        assert!(marker_present(html, ".user-avatar"));
        assert!(marker_present(html, "user-login"));
        assert!(!marker_present(
            html,
            "meta[name=user-login]"
                .replace("user-login", "user-logout")
                .as_str()
        ));
        assert!(!marker_present(html, ""));
    }

    #[test]
    fn marker_matches_unquoted_attributes() {
        let html = r#"<meta name=user-login content=octocat>"#;
        assert!(marker_present(html, "meta[name=user-login]"));
    }

    #[test]
    fn marker_substring_only_with_star() {
        let html = r#"<form action="/users/sign_out"></form>"#;
        assert!(
            !marker_present(html, "form[action=logout]"),
            "exact must not substring"
        );
        assert!(marker_present(html, "form[action*=sign_out]"));
    }

    #[test]
    fn login_form_detection() {
        assert!(login_form_present(
            r#"<form action="/session" method="post"><input type="password" name="pw"></form>"#
        ));
        assert!(login_form_present(r#"<input type='password' name='pass'>"#));
        assert!(login_form_present(
            r#"<form action="https://x.io/authenticate">"#
        ));
        assert!(login_form_present(r#"<form action="/login.php"></form>"#));
        // authenticated pages may contain forms that merely mention auth
        assert!(!login_form_present(
            r#"<form action="/settings/notifications"><input type="text"></form>"#
        ));
        assert!(!login_form_present(r#"<form action="/authors"></form>"#));
        assert!(!login_form_present("<p>no form here</p>"));
    }

    #[test]
    fn auth_segment_stems() {
        assert!(has_auth_segment("/login"));
        assert!(has_auth_segment("https://x.io/users/sign_in?next=%2F"));
        assert!(has_auth_segment("/oauth/authorize"));
        assert!(has_auth_segment("/login.php"));
        assert!(!has_auth_segment("/settings/profile"));
        assert!(!has_auth_segment("/authors"));
        assert!(!has_auth_segment("/logged-out-info"));
    }

    async fn client_for(_url: &str) -> crate::network::HttpClient {
        let mut config = crate::BrowserConfig::headless();
        config.enable_ssrf_filter = false; // loopback wiremock
        let jar = std::sync::Arc::new(parking_lot::RwLock::new(crate::network::CookieJar::new()));
        crate::network::HttpClient::new(&config, jar).unwrap()
    }

    #[tokio::test]
    async fn probe_fallback_and_marker_over_http() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Logged-in looking page (no login form).
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/settings/profile"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<html><head><meta name="user-login" content="octocat"></head></html>"#,
            ))
            .mount(&server)
            .await;
        // Login page for the fallback check.
        let login = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"<form action="/session"><input type="password"></form>"#),
            )
            .mount(&login)
            .await;

        // Marker probe: configured URL + marker → valid.
        let client = client_for("").await;
        let url: url::Url = format!("{}/settings/profile", server.uri())
            .parse()
            .unwrap();
        let probe = ValidationProbe {
            url: url.clone(),
            marker: Some("meta[name=user-login]".into()),
        };
        let outcome = probe.run(&client).await;
        assert_eq!(outcome.verdict, ProbeVerdict::Valid);
        assert_eq!(outcome.marker_found, Some(true));
        assert_eq!(outcome.http_status, Some(200));

        // Marker missing → invalid.
        let probe = ValidationProbe {
            url,
            marker: Some("meta[name=whoami]".into()),
        };
        let outcome = probe.run(&client).await;
        assert_eq!(
            outcome.verdict,
            ProbeVerdict::Invalid {
                reason: "marker_missing".into()
            }
        );

        // Fallback probe against a login page → invalid (login form).
        let url: url::Url = format!("{}/login", login.uri()).parse().unwrap();
        let probe = ValidationProbe { url, marker: None };
        let outcome = probe.run(&client).await;
        assert_eq!(
            outcome.verdict,
            ProbeVerdict::Invalid {
                reason: "login_form".into()
            }
        );
        assert_eq!(outcome.login_form_present, Some(true));
    }
}
