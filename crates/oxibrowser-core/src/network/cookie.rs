//! Cookie jar for session cookie management with RFC 6265 compliance.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use url::Url;

/// SameSite cookie attribute (RFC 6265bis).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SameSite {
    Strict,
    Lax,
    None,
}

impl std::fmt::Display for SameSite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SameSite::Strict => write!(f, "Strict"),
            SameSite::Lax => write!(f, "Lax"),
            SameSite::None => write!(f, "None"),
        }
    }
}

/// Context in which cookies are being sent, used for SameSite enforcement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SameSiteContext {
    /// Request originates from the same origin as the cookie.
    SameOrigin,
    /// Safe cross-site request (GET, HEAD navigation).
    CrossSiteSafe,
    /// Unsafe cross-site request (POST, PUT, DELETE from cross-site).
    CrossSiteUnsafe,
}

/// Maximum number of cookies per domain (RFC 6265 §6.1 recommends 50).
const MAX_COOKIES_PER_DOMAIN: usize = 50;

/// Maximum total number of cookies (RFC 6265 §6.1 recommends 3000).
const MAX_TOTAL_COOKIES: usize = 3000;

/// Maximum cookie value size in bytes (RFC 6265 §6.1 recommends 4096).
const MAX_COOKIE_VALUE_SIZE: usize = 4096;

/// Deserialize an epoch-seconds field that may arrive as integer (ours),
/// float (Playwright `storageState`), or absent/null. Floats truncate
/// toward zero — sub-second precision is meaningless for cookie expiry.
/// Negative values (Playwright's `-1` session-cookie convention) map to
/// `None` = session cookie — otherwise the jar's expiry fold treats them
/// as long-past and DROPS the cookie on import, silently losing the
/// session (the exact guide-capture failure this helper exists to fix).
fn de_epoch_seconds<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;
    let v = serde_json::Value::deserialize(deserializer)?;
    match v {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Number(n) => {
            let secs = n
                .as_i64()
                .or_else(|| n.as_f64().map(|f| f.trunc() as i64))
                .ok_or_else(|| D::Error::custom("epoch seconds must be a number"))?;
            Ok((secs > 0).then_some(secs))
        }
        other => Err(D::Error::custom(format!(
            "epoch seconds must be a number, got {other:?}"
        ))),
    }
}

/// A parsed cookie entry with its attributes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CookieEntry {
    pub name: String,
    pub value: String,
    pub path: Option<String>,
    pub domain: Option<String>,
    pub secure: bool,
    /// Playwright spells this `httpOnly`; the alias keeps both spellings
    /// deserializable (guide-capture imports, item 11).
    #[serde(default, rename = "httpOnly", alias = "http_only")]
    pub http_only: bool,
    /// Playwright `storageState` spells this `sameSite`; the alias keeps
    /// older core-serialized snapshots (`same_site`) deserializable. Variant
    /// values serialize as `"Strict"` / `"Lax"` / `"None"` (Playwright casing).
    #[serde(rename = "sameSite", alias = "same_site")]
    pub same_site: Option<SameSite>,
    /// Raw parsed `Expires` attribute as Unix-epoch seconds. Accepts
    /// Playwright `storageState` exports, which emit **floating-point**
    /// epochs (`expires: 1798761600.42`) — truncated to whole seconds.
    #[serde(default, deserialize_with = "de_epoch_seconds")]
    pub expires: Option<i64>,
    /// Raw parsed `Max-Age` attribute in seconds.
    #[serde(default)]
    pub max_age: Option<i64>,
    /// Authoritative absolute expiry as Unix-epoch seconds.
    ///
    /// `None` = session cookie (lives until the jar is dropped). Computed in
    /// [`CookieJar::store`] from `max_age`/`expires` + the current time. A past
    /// value means the cookie is already expired and is purged lazily on read.
    #[serde(default)]
    pub expiry: Option<i64>,
    /// `Partitioned` flag (CHIPS, RFC 6265bis). Stored cookies scoped to a
    /// partition key (top-level site) so they are not shared cross-site.
    #[serde(default)]
    pub partitioned: bool,
    /// CHIPS partition key (top-level registrable domain) for partitioned
    /// cookies. `None` for non-partitioned cookies. Until Phase 8 threads the
    /// real top-level site, this defaults to the cookie's own registrable
    /// domain (first-party partition).
    #[serde(default)]
    pub partition_key: Option<String>,
}

impl CookieEntry {
    /// Parse a Set-Cookie header value into a CookieEntry.
    ///
    /// Expected format: `name=value; Path=/; HttpOnly; Secure; Domain=.example.com; SameSite=Lax`
    pub fn parse(header: &str) -> Option<Self> {
        let mut parts = header.split(';');
        let nv = parts.next()?.trim();
        let eq_pos = nv.find('=')?;
        let name = nv[..eq_pos].trim().to_string();
        let value = nv[eq_pos + 1..].trim().to_string();

        let mut path = None;
        let mut domain = None;
        let mut secure = false;
        let mut http_only = false;
        let mut same_site = None;
        let mut expires = None;
        let mut max_age = None;
        let mut partitioned = false;

        for attr in parts {
            let attr = attr.trim();
            if attr.eq_ignore_ascii_case("secure") {
                secure = true;
            } else if attr.eq_ignore_ascii_case("httponly") {
                http_only = true;
            } else if attr.eq_ignore_ascii_case("partitioned") {
                partitioned = true;
            } else if let Some(val) = strip_prefix_case_insensitive(attr, "Path=") {
                path = Some(val.to_string());
            } else if let Some(val) = strip_prefix_case_insensitive(attr, "Domain=") {
                domain = Some(val.to_string());
            } else if let Some(val) = strip_prefix_case_insensitive(attr, "Max-Age=") {
                max_age = val.trim().parse::<i64>().ok();
            } else if let Some(val) = strip_prefix_case_insensitive(attr, "Expires=") {
                expires = parse_http_date(val.trim());
            } else if let Some(val) = strip_prefix_case_insensitive(attr, "SameSite=") {
                same_site = match val {
                    v if v.eq_ignore_ascii_case("strict") => Some(SameSite::Strict),
                    v if v.eq_ignore_ascii_case("lax") => Some(SameSite::Lax),
                    v if v.eq_ignore_ascii_case("none") => Some(SameSite::None),
                    _ => None, // Unknown SameSite value → treat as not set
                };
            }
        }

        Some(Self {
            name,
            value,
            path,
            domain,
            secure,
            http_only,
            same_site,
            expires,
            max_age,
            expiry: None,
            partitioned,
            partition_key: None,
        })
    }

    /// Render the name=value pair (without attributes) for use in a Cookie header.
    pub fn to_cookie_header(&self) -> String {
        format!("{}={}", self.name, self.value)
    }
}

/// Case-insensitive prefix stripper for attribute parsing.
fn strip_prefix_case_insensitive<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

/// Current time as Unix-epoch seconds.
fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Parse an HTTP-date (RFC 7231 IMF-fixdate, e.g. `Wed, 09 Jun 2021 10:18:14 GMT`)
/// into Unix-epoch seconds. Returns `None` for unparseable dates (the cookie
/// then falls back to no expiry, matching the "ignore invalid Expires" rule).
fn parse_http_date(s: &str) -> Option<i64> {
    let date = httpdate::parse_http_date(s).ok()?;
    let secs = date
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Some(secs)
}

/// True if the cookie's computed `expiry` is in the past.
fn is_expired(cookie: &CookieEntry) -> bool {
    cookie.expiry.is_some_and(|exp| exp <= now_epoch_secs())
}

/// True if `domain` is itself a public suffix (e.g. `co.uk`, `com`) per the
/// Mozilla Public Suffix List. Cookies must not be scoped to a bare public
/// suffix (RFC 6265bis §5.3 step 5 "public suffix" rejection).
fn is_public_suffix(domain: &str) -> bool {
    psl::suffix_str(domain).is_some_and(|s| s.eq_ignore_ascii_case(domain))
}

pub(crate) fn registrable_domain(host: &str) -> String {
    if is_ip_address(host) {
        return host.to_lowercase();
    }
    if let Some(d) = psl::domain_str(host) {
        return d.to_lowercase();
    }
    host.to_lowercase()
}

/// Trustworthy origin per the secure-contexts rules (RFC 6265bis §5.4 /
/// WHATWG): `https`, or a loopback host over plain http — browsers treat
/// `localhost`/`127.0.0.1`/`::1` as potentially trustworthy, so Secure
/// cookies may be *sent* there without TLS.
fn is_trustworthy_origin(url: &Url) -> bool {
    if url.scheme() == "https" {
        return true;
    }
    matches!(
        url.host_str(),
        Some("localhost") | Some("127.0.0.1") | Some("::1")
    )
}

/// Compute the default cookie path from a URL per RFC 6265 §5.1.4.
fn default_path(url_path: &str) -> String {
    if url_path.is_empty() || !url_path.starts_with('/') {
        return "/".to_string();
    }
    // Strip everything after the last '/'
    if let Some(last_slash) = url_path.rfind('/') {
        let path = &url_path[..=last_slash];
        if path == "/" {
            return "/".to_string();
        }
        return path.to_string();
    }
    "/".to_string()
}

/// Check if `cookie_domain` domain-matches `host` per RFC 6265 §5.1.3.
///
/// A string domain-matches a given host string if at least one of the following
/// conditions holds:
/// 1. The domain string and the host string are identical.
/// 2. All of the following conditions hold:
///    - The domain string is a suffix of the host string.
///    - The last character of the host string that is not included in the domain
///      string is a "." character.
///    - The host string is not an IP address.
fn domain_matches(host: &str, cookie_domain: &str) -> bool {
    let host = host.to_lowercase();
    let cookie_domain = cookie_domain.to_lowercase();

    // Exact match
    if host == cookie_domain {
        return true;
    }

    // Suffix match: host ends with "." + cookie_domain
    if host.ends_with(&format!(".{}", cookie_domain)) {
        // Check that host is not an IP address (basic heuristic)
        if !is_ip_address(&host) {
            return true;
        }
    }

    false
}

/// Check if the host looks like an IP address (v4 or v6).
fn is_ip_address(host: &str) -> bool {
    // IPv4
    if host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    // IPv6 (may have brackets like [::1])
    let trimmed = host.trim_start_matches('[').trim_end_matches(']');
    trimmed.parse::<std::net::IpAddr>().is_ok()
}

/// Check if a request path matches a cookie path per RFC 6265 §5.1.4.
///
/// The cookie-path matches the request-path if:
/// 1. The cookie-path is identical to the request-path, or
/// 2. The cookie-path is a prefix of the request-path, and the last character
///    of the cookie-path is "/", or
/// 3. The cookie-path is a prefix of the request-path, and the first character
///    of the request-path that is not included in the cookie-path is a "/".
fn path_matches(request_path: &str, cookie_path: &str) -> bool {
    if cookie_path.is_empty() || cookie_path == "/" {
        return true;
    }

    // Exact match
    if request_path == cookie_path {
        return true;
    }

    // Prefix match
    if request_path.starts_with(cookie_path) {
        // Check: either cookie_path ends with "/" or next char in request_path is "/"
        if cookie_path.ends_with('/') {
            return true;
        }
        if request_path.as_bytes().get(cookie_path.len()) == Some(&b'/') {
            return true;
        }
    }

    false
}

/// A cookie jar that stores cookies per domain with RFC 6265 compliance.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CookieJar {
    cookies: HashMap<String, Vec<CookieEntry>>,
}

impl CookieJar {
    /// Create an empty cookie jar.
    pub fn new() -> Self {
        Self::default()
    }

    /// Total number of cookies across all domains.
    fn total_count(&self) -> usize {
        self.cookies.values().map(|v| v.len()).sum()
    }

    /// Store a cookie from a Set-Cookie header value.
    ///
    /// Parses the header into a CookieEntry and replaces any existing cookie
    /// with the same name, domain, and path.
    ///
    /// Per RFC 6265:
    /// - Uses the cookie's Domain attribute (validated against request host) as storage key
    /// - Validates that the cookie domain domain-matches the request URL's host
    /// - Enforces cookie count and size limits
    pub fn store(&mut self, url: &Url, cookie_header: &str) {
        let mut entry = match CookieEntry::parse(cookie_header) {
            Some(e) => e,
            None => {
                // Fallback: store as a raw name;value entry
                CookieEntry {
                    name: cookie_header.to_string(),
                    value: String::new(),
                    path: None,
                    domain: None,
                    secure: false,
                    http_only: false,
                    same_site: None,
                    expires: None,
                    max_age: None,
                    expiry: None,
                    partitioned: false,
                    partition_key: None,
                }
            }
        };

        // Enforce value size limit
        if entry.value.len() > MAX_COOKIE_VALUE_SIZE {
            entry.value.truncate(MAX_COOKIE_VALUE_SIZE);
        }

        // Cookie-name prefix validation (RFC 6265bis §4.1.3).
        //   __Secure-Name: requires the Secure attribute.
        //   __Host-Name:   requires Secure, Path=/, and no Domain attribute.
        if entry.name.starts_with("__Secure-") {
            if !entry.secure {
                return;
            }
        } else if entry.name.starts_with("__Host-")
            && (!entry.secure || entry.path.as_deref() != Some("/") || entry.domain.is_some())
        {
            return;
        }

        let request_host = url.host_str().unwrap_or("unknown");

        // Determine storage domain per RFC 6265 §5.3 step 6–8
        let storage_domain = if let Some(ref cookie_domain) = entry.domain {
            // Strip leading dot for comparison
            let canonical = cookie_domain.trim_start_matches('.').to_lowercase();

            // Domain validation: cookie domain must domain-match the request host
            if !domain_matches(request_host, &canonical) {
                // Reject: evil.com cannot set Domain=.bank.com
                return;
            }

            // Reject cookies scoped to a bare public suffix (e.g. Domain=co.uk).
            if is_public_suffix(&canonical) {
                return;
            }

            // Store under the canonical domain (without leading dot)
            entry.domain = Some(canonical.clone());
            canonical
        } else {
            // No Domain attribute: cookie is host-only, use request host
            let host = request_host.to_lowercase();
            entry.domain = Some(host.clone());
            host
        };

        // CHIPS: a Partitioned cookie is scoped to a partition key (the
        // top-level registrable domain). Until Phase 8 threads the real
        // top-level site, default to the cookie's own registrable domain
        // (first-party partition). Non-partitioned cookies have no key.
        if entry.partitioned {
            entry.partition_key = Some(registrable_domain(&storage_domain));
        }

        // Default path per RFC 6265 §5.1.4
        if entry.path.is_none() || entry.path.as_deref() == Some("") {
            entry.path = Some(default_path(url.path()));
        }
        // Compute absolute expiry (RFC 6265 §5.2.1–5.2.2 / §5.3 step 11).
        let now = now_epoch_secs();
        if let Some(max_age) = entry.max_age {
            if max_age <= 0 {
                // Max-Age <= 0: delete any existing matching cookie and don't store.
                self.remove_matching(&storage_domain, &entry.name, &entry.path);
                return;
            }
            entry.expiry = Some(now.saturating_add(max_age));
        } else if let Some(expires) = entry.expires {
            entry.expiry = Some(expires);
        } else {
            entry.expiry = None; // session cookie — lives until the jar is dropped
        }

        // An `Expires` already in the past means the cookie is expired: delete
        // any existing match and don't store the new one (RFC 6265 §5.4).
        if entry.expiry.is_some_and(|exp| exp <= now) {
            self.remove_matching(&storage_domain, &entry.name, &entry.path);
            return;
        }

        self.commit_entry(entry, storage_domain);
    }

    /// Commit a fully-normalized entry: replace the existing cookie with the
    /// same (name, path) under `storage_domain`, or append with the RFC 6265
    /// count limits enforced. Shared by [`CookieJar::store`] and
    /// [`CookieJar::insert_entry`].
    fn commit_entry(&mut self, entry: CookieEntry, storage_domain: String) {
        // Replace existing cookie with same name and path, or append
        if let Some(existing) = self.cookies.get_mut(&storage_domain).and_then(|entries| {
            entries
                .iter_mut()
                .find(|c| c.name == entry.name && c.path == entry.path)
        }) {
            *existing = entry;
            return;
        }

        // Need to add a new entry — enforce limits first
        // Enforce total cookie limit by evicting from another domain
        if self.total_count() >= MAX_TOTAL_COOKIES {
            let domain_to_evict = self
                .cookies
                .iter()
                .filter(|(_, v)| !v.is_empty())
                .find(|(k, _)| *k != &storage_domain)
                .map(|(k, _)| k.clone());
            if let Some(d) = domain_to_evict
                && let Some(v) = self.cookies.get_mut(&d)
            {
                v.remove(0);
            }
        }

        let entries = self.cookies.entry(storage_domain).or_default();
        // Enforce per-domain cookie limit
        if entries.len() >= MAX_COOKIES_PER_DOMAIN {
            entries.remove(0); // Evict oldest
        }
        entries.push(entry);
    }

    /// Insert an already-parsed [`CookieEntry`], merging with any existing
    /// cookie that has the same (domain, name, path).
    ///
    /// Unlike [`CookieJar::store`] (which validates a live `Set-Cookie`
    /// header against the request host), this accepts the entry as authored:
    /// imported snapshots are trusted, so the Domain attribute is **not**
    /// validated against a request host and public-suffix checks don't apply.
    /// Normalization still happens: the domain is canonicalized (leading dot
    /// stripped, lowercased; a missing domain falls back to the `"unknown"`
    /// bucket, mirroring `cookies_for_url`), a missing/empty path defaults to
    /// `/`, and `max_age`/`expires` are folded into the absolute `expiry` —
    /// an already-expired entry deletes its stored match instead.
    pub fn insert_entry(&mut self, mut entry: CookieEntry) {
        // Struct-level serde defaults make hand-mangled imports parse with
        // empty fields — an anonymous cookie is garbage, not state.
        if entry.name.is_empty() {
            return;
        }
        // Enforce value size limit
        if entry.value.len() > MAX_COOKIE_VALUE_SIZE {
            entry.value.truncate(MAX_COOKIE_VALUE_SIZE);
        }

        // Storage domain: the cookie's own (canonicalized) Domain attribute,
        // or the host-only `"unknown"` bucket when absent.
        let storage_domain = match entry.domain.take() {
            Some(cookie_domain) => {
                let canonical = cookie_domain.trim_start_matches('.').to_lowercase();
                entry.domain = Some(canonical.clone());
                canonical
            }
            None => {
                entry.domain = Some("unknown".to_string());
                "unknown".to_string()
            }
        };

        // CHIPS partition key (mirrors `store`).
        if entry.partitioned {
            entry.partition_key = Some(registrable_domain(&storage_domain));
        }

        // Default path: no request URL to derive the RFC 6265 default-path
        // from, so imported cookies without a path scope to root.
        if entry.path.is_none() || entry.path.as_deref() == Some("") {
            entry.path = Some("/".to_string());
        }

        // Compute absolute expiry (same rules as `store`).
        let now = now_epoch_secs();
        if let Some(max_age) = entry.max_age {
            if max_age <= 0 {
                // Max-Age <= 0: delete any existing matching cookie and don't store.
                self.remove_matching(&storage_domain, &entry.name, &entry.path);
                return;
            }
            entry.expiry = Some(now.saturating_add(max_age));
        } else if let Some(expires) = entry.expires {
            entry.expiry = Some(expires);
        } else {
            entry.expiry = None; // session cookie — lives until the jar is dropped
        }

        // An already-expired entry deletes any existing match (RFC 6265 §5.4).
        if entry.expiry.is_some_and(|exp| exp <= now) {
            self.remove_matching(&storage_domain, &entry.name, &entry.path);
            return;
        }

        self.commit_entry(entry, storage_domain);
    }

    /// Remove a cookie matching `(domain, name, path)`, if present.
    fn remove_matching(&mut self, domain: &str, name: &str, path: &Option<String>) {
        if let Some(entries) = self.cookies.get_mut(domain) {
            entries.retain(|c| !(c.name == name && c.path == *path));
        }
        self.cookies.retain(|_, v| !v.is_empty());
    }

    /// Get all cookies applicable to a URL as a Cookie header value.
    ///
    /// Per RFC 6265 §5.4, filters by:
    /// - Domain matching (including subdomain/superdomain sharing)
    /// - Path matching
    /// - Secure flag enforcement (only send secure cookies over HTTPS)
    /// - SameSite enforcement via [`SameSiteContext`]
    ///
    /// Defaults to [`SameSiteContext::SameOrigin`], preserving the
    /// historical "same-origin request" assumption.
    pub fn cookies_for_url(&self, url: &Url) -> String {
        self.cookies_for_url_with_context(url, SameSiteContext::SameOrigin)
    }

    /// Context-aware variant: SameSite filters honour the supplied context.
    ///
    /// - `Strict`: dropped for any non-same-origin context.
    /// - `Lax`:    dropped only on `CrossSiteUnsafe`.
    /// - `None` or unset: always sent.
    pub fn cookies_for_url_with_context(&self, url: &Url, ctx: SameSiteContext) -> String {
        let host = url.host_str().unwrap_or("unknown").to_lowercase();
        let url_path = url.path();
        let is_secure = is_trustworthy_origin(url);

        let mut matching: Vec<&CookieEntry> = Vec::new();

        for (domain, entries) in &self.cookies {
            // Check domain match: cookie's storage domain must match the URL host
            if !domain_matches(&host, domain) && domain != &host {
                continue;
            }

            for cookie in entries {
                // Skip expired cookies (lazy purge, RFC 6265 §5.3/§5.4)
                if is_expired(cookie) {
                    continue;
                }
                // Secure: only send over HTTPS
                if cookie.secure && !is_secure {
                    continue;
                }

                // Path matching
                let cookie_path = cookie.path.as_deref().unwrap_or("/");
                if !path_matches(url_path, cookie_path) {
                    continue;
                }

                // SameSite enforcement
                match cookie.same_site {
                    Some(SameSite::Strict) => {
                        if ctx != SameSiteContext::SameOrigin {
                            continue;
                        }
                    }
                    Some(SameSite::Lax) => {
                        if ctx == SameSiteContext::CrossSiteUnsafe {
                            continue;
                        }
                    }
                    Some(SameSite::None) | None => {
                        // Always sent.
                    }
                }

                matching.push(cookie);
            }
        }

        // Sort by path length (longest first per RFC 6265 §5.4)
        matching.sort_by(|a, b| {
            let pa = a.path.as_deref().unwrap_or("/").len();
            let pb = b.path.as_deref().unwrap_or("/").len();
            pb.cmp(&pa)
        });

        matching
            .iter()
            .map(|c| c.to_cookie_header())
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Per RFC 6265 §5.4, HttpOnly cookies must not be accessible via
    /// `document.cookie` or other script APIs.
    pub fn cookies_for_js(&self, url: &Url) -> String {
        let host = url.host_str().unwrap_or("unknown").to_lowercase();
        let url_path = url.path();
        let is_secure = is_trustworthy_origin(url);

        let mut matching: Vec<&CookieEntry> = Vec::new();

        for (domain, entries) in &self.cookies {
            if !domain_matches(&host, domain) && domain != &host {
                continue;
            }

            for cookie in entries {
                // Skip expired cookies (lazy purge)
                if is_expired(cookie) {
                    continue;
                }
                // HttpOnly cookies are never visible to JavaScript
                if cookie.http_only {
                    continue;
                }

                if cookie.secure && !is_secure {
                    continue;
                }

                let cookie_path = cookie.path.as_deref().unwrap_or("/");
                if !path_matches(url_path, cookie_path) {
                    continue;
                }

                match cookie.same_site {
                    Some(SameSite::Strict) => {}
                    Some(SameSite::Lax) => {}
                    Some(SameSite::None) | None => {}
                }

                matching.push(cookie);
            }
        }

        matching.sort_by(|a, b| {
            let pa = a.path.as_deref().unwrap_or("/").len();
            let pb = b.path.as_deref().unwrap_or("/").len();
            pb.cmp(&pa)
        });

        matching
            .iter()
            .map(|c| c.to_cookie_header())
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Clear all cookies.
    pub fn clear(&mut self) {
        self.cookies.clear();
    }

    /// Clear all cookies and return how many were disposed. Teardown paths
    /// use the count for the audit `session_teardown` event.
    pub fn clear_and_count(&mut self) -> usize {
        let n = self.cookies.len();
        self.cookies.clear();
        n
    }

    /// Number of stored cookie entries (by domain).
    pub fn len(&self) -> usize {
        self.cookies.len()
    }

    /// Check if the jar is empty.
    pub fn is_empty(&self) -> bool {
        self.cookies.is_empty()
    }

    /// Get all cookies as a flat Vec (all domains merged) with domain info populated.
    pub fn get_all(&self) -> Vec<CookieEntry> {
        self.cookies.values().flatten().cloned().collect()
    }

    /// Remove cookies matching name for a given URL.
    pub fn remove(&mut self, url: &Url, name: &str) {
        for cookies in self.cookies.values_mut() {
            cookies.retain(|c| {
                if c.name != name {
                    return true;
                }
                if let (Some(dom), Some(cdom)) = (&c.domain, url.domain())
                    && !dom.starts_with('.')
                    && dom != cdom
                {
                    return true;
                }
                false
            });
        }
        // Clean up empty domain entries
        self.cookies.retain(|_, v| !v.is_empty());
    }

    /// Save cookies to a JSON file.
    pub fn save_to_file(&self, path: &std::path::Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, json)
    }

    /// Load cookies from a JSON file.
    pub fn load_from_file(path: &std::path::Path) -> std::io::Result<Self> {
        let json = std::fs::read_to_string(path)?;
        serde_json::from_str(&json)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cookie_jar_store_and_retrieve() {
        let mut jar = CookieJar::new();
        let url = Url::parse("https://example.com/page").unwrap();
        jar.store(&url, "session=abc123; Path=/");

        let cookies = jar.cookies_for_url(&url);
        assert!(
            cookies.contains("session=abc123"),
            "stored cookie should be retrievable"
        );
    }

    #[test]
    fn test_cookie_jar_domain_isolation() {
        let mut jar = CookieJar::new();
        let url_a = Url::parse("https://site-a.com/").unwrap();
        let url_b = Url::parse("https://site-b.com/").unwrap();

        jar.store(&url_a, "token=aaa");
        jar.store(&url_b, "token=bbb");

        let cookies_a = jar.cookies_for_url(&url_a);
        let cookies_b = jar.cookies_for_url(&url_b);

        assert!(
            cookies_a.contains("token=aaa"),
            "site A should see its own cookie"
        );
        assert!(
            !cookies_a.contains("token=bbb"),
            "site A should NOT see site B's cookie"
        );
        assert!(
            cookies_b.contains("token=bbb"),
            "site B should see its own cookie"
        );
        assert!(
            !cookies_b.contains("token=aaa"),
            "site B should NOT see site A's cookie"
        );
    }

    #[test]
    fn test_cookie_jar_clear() {
        let mut jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        jar.store(&url, "key=val");
        assert!(!jar.is_empty());

        jar.clear();
        assert!(jar.is_empty(), "jar should be empty after clear");
        assert!(
            jar.cookies_for_url(&url).is_empty(),
            "no cookies after clear"
        );
    }

    #[test]
    fn test_cookie_jar_round_trip() {
        // Simulate: server sends Set-Cookie, then client sends Cookie on next request
        let mut jar = CookieJar::new();
        let url = Url::parse("https://example.com/page1").unwrap();

        // Server response includes Set-Cookie
        jar.store(&url, "session=abc123; Path=/; HttpOnly");
        jar.store(&url, "pref=dark; Path=/");

        // Client makes another request to the same domain
        let url2 = Url::parse("https://example.com/page2").unwrap();
        let cookies = jar.cookies_for_url(&url2);

        // Both cookies should be sent (stripped of attributes)
        assert!(
            cookies.contains("session=abc123"),
            "should send session cookie"
        );
        assert!(cookies.contains("pref=dark"), "should send pref cookie");
        assert!(!cookies.contains("Path="), "should strip Path attribute");
        assert!(!cookies.contains("HttpOnly"), "should strip HttpOnly flag");
    }

    #[test]
    fn test_cookie_jar_multiple_domains() {
        let mut jar = CookieJar::new();

        // Store cookies for 3 different domains
        for (domain, cookie) in [
            ("https://api.example.com", "token=api-token"),
            ("https://cdn.example.com", "cache=v1"),
            ("https://other.com", "session=xyz"),
        ] {
            let url = Url::parse(domain).unwrap();
            jar.store(&url, cookie);
        }

        // Each domain gets only its own cookies
        let api_url = Url::parse("https://api.example.com/data").unwrap();
        let api_cookies = jar.cookies_for_url(&api_url);
        assert!(api_cookies.contains("token=api-token"));
        assert!(!api_cookies.contains("cache=v1"));

        // Clear and verify all gone
        jar.clear();
        assert_eq!(jar.len(), 0);
    }

    #[test]
    fn test_cookie_entry_parsing() {
        let entry = CookieEntry::parse("session=abc123; Path=/; HttpOnly; Secure").unwrap();
        assert_eq!(entry.name, "session");
        assert_eq!(entry.value, "abc123");
        assert_eq!(entry.path.as_deref(), Some("/"));
        assert!(entry.http_only);
        assert!(entry.secure);
    }

    #[test]
    fn test_cookie_entry_parse_domain() {
        let entry = CookieEntry::parse("id=42; Domain=.example.com; Path=/").unwrap();
        assert_eq!(entry.name, "id");
        assert_eq!(entry.value, "42");
        assert_eq!(entry.domain.as_deref(), Some(".example.com"));
        assert_eq!(entry.path.as_deref(), Some("/"));
    }

    #[test]
    fn test_cookie_entry_parse_simple() {
        let entry = CookieEntry::parse("key=val").unwrap();
        assert_eq!(entry.name, "key");
        assert_eq!(entry.value, "val");
        assert!(entry.path.is_none());
        assert!(entry.domain.is_none());
        assert!(!entry.secure);
        assert!(!entry.http_only);
        assert!(entry.same_site.is_none());
    }

    #[test]
    fn test_cookie_entry_parse_no_equals() {
        // No '=' means it's not a valid cookie
        assert!(CookieEntry::parse("invalidcookie").is_none());
    }

    #[test]
    fn test_cookie_entry_parse_samesite() {
        let entry = CookieEntry::parse("session=x; SameSite=Strict").unwrap();
        assert_eq!(entry.same_site, Some(SameSite::Strict));

        let entry = CookieEntry::parse("session=x; SameSite=Lax").unwrap();
        assert_eq!(entry.same_site, Some(SameSite::Lax));

        let entry = CookieEntry::parse("session=x; SameSite=None").unwrap();
        assert_eq!(entry.same_site, Some(SameSite::None));

        let entry = CookieEntry::parse("session=x; SameSite=Invalid").unwrap();
        assert!(entry.same_site.is_none());
    }

    #[test]
    fn test_cookie_save_load_file() {
        let dir = std::env::temp_dir().join("oxibrowser_cookie_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cookies.json");

        // Clean up from previous runs
        let _ = std::fs::remove_file(&path);

        let mut jar = CookieJar::new();
        let url = Url::parse("https://example.com/page").unwrap();
        jar.store(&url, "session=abc123; Path=/");
        jar.store(&url, "theme=dark");

        // Save
        jar.save_to_file(&path).expect("save should succeed");

        // Load
        let loaded = CookieJar::load_from_file(&path).expect("load should succeed");

        // Verify loaded cookies match
        let cookies = loaded.cookies_for_url(&url);
        assert!(
            cookies.contains("session=abc123"),
            "loaded cookie should contain session"
        );
        assert!(
            cookies.contains("theme=dark"),
            "loaded cookie should contain theme"
        );

        // Clean up
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_cookie_save_load_preserves_attributes() {
        let dir = std::env::temp_dir().join("oxibrowser_cookie_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cookies_attrs.json");
        let _ = std::fs::remove_file(&path);

        let mut jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        jar.store(
            &url,
            "id=42; Path=/; Domain=.example.com; Secure; HttpOnly; SameSite=Lax",
        );

        jar.save_to_file(&path).unwrap();
        let loaded = CookieJar::load_from_file(&path).unwrap();

        // Check that the cookie entry has preserved attributes
        // After store, domain is canonicalized to "example.com"
        let entries = loaded.cookies.get("example.com").unwrap();
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.name, "id");
        assert_eq!(entry.value, "42");
        assert_eq!(entry.path.as_deref(), Some("/"));
        assert_eq!(entry.domain.as_deref(), Some("example.com"));
        assert!(entry.secure);
        assert!(entry.http_only);
        assert_eq!(entry.same_site, Some(SameSite::Lax));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_cookie_load_missing_file() {
        let path = std::path::PathBuf::from("/tmp/oxibrowser_nonexistent_cookie_file_12345.json");
        assert!(
            CookieJar::load_from_file(&path).is_err(),
            "loading missing file should fail"
        );
    }

    #[test]
    fn test_cookie_store_replaces_existing() {
        let mut jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();

        jar.store(&url, "token=old_value");
        jar.store(&url, "token=new_value");

        let entries = jar.cookies.get("example.com").unwrap();
        assert_eq!(entries.len(), 1, "should have only one cookie for the name");
        assert_eq!(entries[0].value, "new_value");

        let cookies = jar.cookies_for_url(&url);
        assert!(cookies.contains("token=new_value"));
        assert!(!cookies.contains("old_value"));
    }

    // --- New RFC 6265 compliance tests ---

    #[test]
    fn test_domain_validation_rejects_cross_domain() {
        // evil.com cannot set Domain=.bank.com
        let mut jar = CookieJar::new();
        let url = Url::parse("https://evil.com/").unwrap();
        jar.store(&url, "evil_cookie=hacked; Domain=.bank.com");

        let bank_url = Url::parse("https://bank.com/").unwrap();
        let cookies = jar.cookies_for_url(&bank_url);
        assert!(cookies.is_empty(), "cross-domain cookie should be rejected");
    }

    #[test]
    fn test_subdomain_sharing_with_domain_attribute() {
        let mut jar = CookieJar::new();
        // Set cookie from sub.example.com with Domain=.example.com
        let url = Url::parse("https://sub.example.com/").unwrap();
        jar.store(&url, "shared=yes; Domain=.example.com; Path=/");

        // Should be accessible from parent domain
        let parent_url = Url::parse("https://example.com/").unwrap();
        let cookies = jar.cookies_for_url(&parent_url);
        assert!(
            cookies.contains("shared=yes"),
            "cookie with Domain=.example.com should be visible to example.com"
        );

        // Should be accessible from another subdomain
        let other_sub_url = Url::parse("https://other.example.com/").unwrap();
        let cookies = jar.cookies_for_url(&other_sub_url);
        assert!(
            cookies.contains("shared=yes"),
            "cookie with Domain=.example.com should be visible to other.example.com"
        );
    }

    #[test]
    fn test_path_matching() {
        let mut jar = CookieJar::new();
        let url = Url::parse("https://example.com/app/login").unwrap();
        jar.store(&url, "auth=token; Path=/app");

        // Should match /app and sub-paths
        let app_url = Url::parse("https://example.com/app/dashboard").unwrap();
        assert!(
            jar.cookies_for_url(&app_url).contains("auth=token"),
            "should match sub-path of /app"
        );

        let app_root = Url::parse("https://example.com/app").unwrap();
        assert!(
            jar.cookies_for_url(&app_root).contains("auth=token"),
            "should match exact path /app"
        );

        // Should NOT match unrelated path
        let other_url = Url::parse("https://example.com/other").unwrap();
        assert!(
            !jar.cookies_for_url(&other_url).contains("auth=token"),
            "should not match unrelated path"
        );
    }

    #[test]
    fn test_secure_flag_enforcement() {
        let mut jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        jar.store(&url, "secret=val; Secure");

        // Should be sent over HTTPS
        let https_url = Url::parse("https://example.com/").unwrap();
        assert!(
            jar.cookies_for_url(&https_url).contains("secret=val"),
            "secure cookie should be sent over HTTPS"
        );

        // Should NOT be sent over HTTP
        let http_url = Url::parse("http://example.com/").unwrap();
        assert!(
            !jar.cookies_for_url(&http_url).contains("secret=val"),
            "secure cookie should NOT be sent over HTTP"
        );
    }

    #[test]
    fn test_cookie_value_size_limit() {
        let mut jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        let big_value = "x".repeat(5000);
        jar.store(&url, &format!("big={}", big_value));

        let _cookies = jar.cookies_for_url(&url);
        // Value should be truncated to 4096
        if let Some(entry) = jar.cookies.get("example.com").and_then(|v| v.first()) {
            assert!(
                entry.value.len() <= MAX_COOKIE_VALUE_SIZE,
                "cookie value should be truncated to max size"
            );
        }
    }

    #[test]
    fn test_httponly_excluded_from_js() {
        let mut jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();

        // Store a regular cookie and an HttpOnly cookie
        jar.store(&url, "session=abc123; Path=/");
        jar.store(&url, "hidden=secret456; Path=/; HttpOnly");

        // cookies_for_url (HTTP request) should include both
        let http_cookies = jar.cookies_for_url(&url);
        assert!(
            http_cookies.contains("session=abc123"),
            "regular cookie should be in HTTP request"
        );
        assert!(
            http_cookies.contains("hidden=secret456"),
            "HttpOnly cookie should be in HTTP request"
        );

        // cookies_for_js (document.cookie) should exclude HttpOnly
        let js_cookies = jar.cookies_for_js(&url);
        assert!(
            js_cookies.contains("session=abc123"),
            "regular cookie should be visible to JS"
        );
        assert!(
            !js_cookies.contains("hidden=secret456"),
            "HttpOnly cookie should NOT be visible to JS"
        );
    }

    #[test]
    fn test_all_httponly_cookies_hidden_from_js() {
        let mut jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();

        // Store only HttpOnly cookies
        jar.store(&url, "a=1; HttpOnly");
        jar.store(&url, "b=2; HttpOnly; Path=/");

        // JS should see nothing
        let js_cookies = jar.cookies_for_js(&url);
        assert!(
            js_cookies.is_empty(),
            "all HttpOnly cookies should be hidden from JS"
        );

        // HTTP request should still get them
        let http_cookies = jar.cookies_for_url(&url);
        assert!(
            http_cookies.contains("a=1") && http_cookies.contains("b=2"),
            "HttpOnly cookies should be sent in HTTP requests"
        );
    }

    // --- Phase 6: expiry / Max-Age / Partitioned ---

    #[test]
    fn test_parse_max_age_attribute() {
        let entry = CookieEntry::parse("token=abc; Max-Age=3600").unwrap();
        assert_eq!(entry.max_age, Some(3600));
        assert!(entry.expires.is_none());
    }

    #[test]
    fn test_parse_expires_attribute() {
        let entry = CookieEntry::parse("token=abc; Expires=Wed, 09 Jun 2021 10:18:14 GMT").unwrap();
        assert!(entry.expires.is_some());
        // 09 Jun 2021 10:18:14 UTC == 1623233894
        assert_eq!(entry.expires, Some(1623233894));
    }

    #[test]
    fn test_parse_expires_invalid_is_ignored() {
        // Invalid Expires → ignored, cookie is a session cookie.
        let entry = CookieEntry::parse("token=abc; Expires=not-a-date").unwrap();
        assert!(entry.expires.is_none());
    }

    #[test]
    fn test_max_age_zero_deletes_existing_cookie() {
        let mut jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        jar.store(&url, "token=abc; Max-Age=3600");
        assert!(jar.cookies_for_url(&url).contains("token=abc"));

        // Max-Age=0 → immediate deletion
        jar.store(&url, "token=abc; Max-Age=0");
        assert!(
            jar.cookies_for_url(&url).is_empty(),
            "Max-Age=0 should delete the cookie"
        );
    }

    #[test]
    fn test_max_age_negative_deletes_existing_cookie() {
        let mut jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        jar.store(&url, "pref=dark");
        assert!(jar.cookies_for_url(&url).contains("pref=dark"));

        jar.store(&url, "pref=dark; Max-Age=-1");
        assert!(
            jar.cookies_for_url(&url).is_empty(),
            "negative Max-Age should delete the cookie"
        );
    }

    #[test]
    fn test_expired_expires_not_stored() {
        let mut jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        // A date firmly in the past (09 Jun 2001 was a Saturday).
        jar.store(&url, "ghost=1; Expires=Sat, 09 Jun 2001 10:18:14 GMT");
        assert!(
            jar.cookies_for_url(&url).is_empty(),
            "cookie with past Expires must not be stored/sent"
        );
    }

    #[test]
    fn test_max_age_positive_stores_and_sends() {
        let mut jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        jar.store(&url, "token=abc; Max-Age=3600");
        assert!(
            jar.cookies_for_url(&url).contains("token=abc"),
            "cookie with future Max-Age should be sent"
        );
        // Verify expiry was computed (absolute, in the future).
        let entry = jar.cookies.get("example.com").unwrap().first().unwrap();
        assert!(entry.expiry.is_some());
        assert!(entry.expiry.unwrap() > now_epoch_secs());
    }

    #[test]
    fn test_session_cookie_has_no_expiry() {
        let mut jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        jar.store(&url, "sess=xyz");
        let entry = jar.cookies.get("example.com").unwrap().first().unwrap();
        assert!(entry.expiry.is_none(), "session cookie must have no expiry");
        assert!(jar.cookies_for_url(&url).contains("sess=xyz"));
    }

    #[test]
    fn test_lazy_purge_skips_expired_on_read() {
        // Inject a cookie that is already expired directly into the jar map,
        // then verify retrieval skips it.
        let mut jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        let past = now_epoch_secs() - 100;
        jar.cookies.insert(
            "example.com".to_string(),
            vec![CookieEntry {
                name: "dead".to_string(),
                value: "1".to_string(),
                path: Some("/".to_string()),
                domain: Some("example.com".to_string()),
                secure: false,
                http_only: false,
                same_site: None,
                expires: None,
                max_age: None,
                expiry: Some(past),
                partitioned: false,
                partition_key: None,
            }],
        );
        assert!(
            !jar.cookies_for_url(&url).contains("dead=1"),
            "expired cookie must not be sent"
        );
        assert!(
            !jar.cookies_for_js(&url).contains("dead=1"),
            "expired cookie must not be visible to JS"
        );
    }

    #[test]
    fn test_parse_partitioned_attribute() {
        let entry =
            CookieEntry::parse("chip=abc; Secure; Path=/; SameSite=None; Partitioned").unwrap();
        assert!(entry.partitioned);
    }

    #[test]
    fn test_partitioned_cookie_gets_partition_key() {
        let mut jar = CookieJar::new();
        let url = Url::parse("https://app.example.com/").unwrap();
        jar.store(&url, "chip=abc; Secure; Path=/; SameSite=None; Partitioned");
        let entry = jar.cookies.get("app.example.com").unwrap().first().unwrap();
        assert!(entry.partitioned);
        assert_eq!(
            entry.partition_key.as_deref(),
            Some("example.com"),
            "partition key defaults to the cookie's own registrable domain"
        );
        // First-party request still receives the cookie.
        assert!(jar.cookies_for_url(&url).contains("chip=abc"));
    }

    #[test]
    fn test_expiry_survives_serialization_roundtrip() {
        let mut jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        jar.store(&url, "token=abc; Max-Age=3600");

        let json = serde_json::to_string(&jar).unwrap();
        let loaded: CookieJar = serde_json::from_str(&json).unwrap();
        let entry = loaded.cookies.get("example.com").unwrap().first().unwrap();
        assert!(entry.expiry.is_some(), "expiry must survive serialization");
        assert!(loaded.cookies_for_url(&url).contains("token=abc"));
    }

    // --- Phase 6: Public Suffix List + cookie-name prefixes ---

    #[test]
    fn test_psl_rejects_public_suffix_domain() {
        let mut jar = CookieJar::new();
        // co.uk is a public suffix → Domain=co.uk must be rejected.
        let url = Url::parse("https://site.co.uk/").unwrap();
        jar.store(&url, "evil=1; Domain=co.uk");
        assert!(
            jar.cookies_for_url(&url).is_empty(),
            "cookie scoped to public suffix co.uk must be rejected"
        );
    }

    #[test]
    fn test_psl_rejects_bare_tld() {
        let mut jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        jar.store(&url, "evil=1; Domain=com");
        assert!(
            jar.cookies_for_url(&url).is_empty(),
            "cookie scoped to bare TLD com must be rejected"
        );
    }

    #[test]
    fn test_psl_allows_normal_domain() {
        let mut jar = CookieJar::new();
        let url = Url::parse("https://sub.example.com/").unwrap();
        jar.store(&url, "ok=1; Domain=example.com");
        assert!(
            jar.cookies_for_url(&Url::parse("https://example.com/").unwrap())
                .contains("ok=1"),
            "normal registrable domain must be allowed"
        );
    }

    #[test]
    fn test_secure_prefix_rejects_without_secure() {
        let mut jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        jar.store(&url, "__Secure-bad=1");
        assert!(
            jar.cookies_for_url(&url).is_empty(),
            "__Secure- without Secure must be rejected"
        );
    }

    #[test]
    fn test_secure_prefix_accepts_with_secure() {
        let mut jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        jar.store(&url, "__Secure-good=1; Secure");
        assert!(
            jar.cookies_for_url(&url).contains("__Secure-good=1"),
            "__Secure- with Secure must be stored"
        );
    }

    #[test]
    fn test_host_prefix_rejects_without_all_constraints() {
        let mut jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();

        // Missing Secure
        jar.store(&url, "__Host-a=1; Path=/");
        // Has Domain
        jar.store(&url, "__Host-b=1; Secure; Path=/; Domain=example.com");
        // Wrong path
        jar.store(&url, "__Host-c=1; Secure; Path=/app");
        assert!(
            jar.cookies_for_url(&url).is_empty(),
            "__Host- must require Secure + Path=/ + no Domain"
        );
    }

    #[test]
    fn test_host_prefix_accepts_valid() {
        let mut jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        jar.store(&url, "__Host-good=1; Secure; Path=/");
        assert!(
            jar.cookies_for_url(&url).contains("__Host-good=1"),
            "valid __Host- cookie must be stored"
        );
    }

    #[test]
    fn test_registrable_domain_helper() {
        assert_eq!(registrable_domain("a.b.example.com"), "example.com");
        assert_eq!(registrable_domain("example.co.uk"), "example.co.uk");
        assert_eq!(registrable_domain("localhost"), "localhost");
        assert_eq!(registrable_domain("127.0.0.1"), "127.0.0.1");
    }
}
