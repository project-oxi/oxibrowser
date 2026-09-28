//! BrowserContext — the per-context isolation unit (M-A, design §3).
//!
//! A [`Browser`](crate::browser::Browser) hosts one default (anonymous)
//! context plus any number of named contexts. Each context owns:
//!
//! - a dedicated [`CookieJar`] (replacing the former single browser-global jar),
//! - an origin-keyed localStorage map (replacing the former flat per-session map),
//! - an [`HttpClient`] (the browser's client when no per-context proxy is set).
//!
//! Sessions derive their cookie jar, HTTP client, and storage map from their
//! context, so two sessions in different contexts cannot observe each other's
//! cookies or web storage. The default context inherits the former global
//! jar's role, keeping anonymous (context-unspecified) behavior unchanged.
//!
//! Known M-A limitation: opaque origins (`about:`, `data:` URLs) all serialize
//! to the literal string `"null"` via
//! [`url::Origin::ascii_serialization`] and therefore share one storage bucket.
//! This mirrors what real browsers isolate further; per-opaque-origin
//! partitioning is out of scope.

use crate::network::HttpClient;
use crate::network::cookie::CookieJar;
use parking_lot::RwLock;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use url::Url;

/// Unique browser-context ID (`"ctx-<n>"`).
///
/// Surfaced as the CDP `browserContextId` string; serializes as its string
/// form.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct ContextId(pub(crate) String);

impl ContextId {
    /// Mint a process-unique test id (`ctx-test-<n>`); distinct from the
    /// browser-allocated `ctx-<n>` sequence.
    #[cfg(test)]
    pub(crate) fn test_next() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        Self(format!(
            "ctx-test-{}",
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// Rebuild a [`ContextId`] from its string form (e.g. a CDP-supplied
    /// `browserContextId`).
    ///
    /// Permissive by design: any string is accepted; unknown ids simply
    /// resolve to `None` in [`Browser::context`](crate::browser::Browser::context)
    /// and an error in
    /// [`Browser::dispose_context`](crate::browser::Browser::dispose_context).
    pub fn from_string(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// String form of the id (same as [`Display`](std::fmt::Display)).
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ContextId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Per-context creation knobs for [`Browser::new_context`]
/// (crate::browser::Browser).
#[derive(Debug, Clone, Default)]
pub struct ContextConfig {
    /// Debugging label; carried on the context, never used for lookup.
    pub label: Option<String>,
    /// Fixed egress proxy for this context. `Some` must parse as an
    /// http/https/socks4/socks5 URL (else `new_context` errors) and builds a
    /// dedicated client; `None` still builds a dedicated client — every
    /// context's client is bound to that context's own cookie jar.
    pub proxy: Option<String>,
}

/// An isolated execution context on a [`Browser`](crate::browser::Browser).
///
/// Chrome browser context / Playwright `BrowserContext` counterpart: each
/// context holds its own cookie jar, origin-keyed localStorage, and egress
/// (HTTP client). Sessions created "in" a context reference these shared
/// handles — closing a session never disposes context state.
pub struct BrowserContext {
    /// Unique ID (`ctx-<n>`).
    id: ContextId,
    /// Debugging label.
    label: Option<String>,
    /// Dedicated cookie jar — isolated from every other context.
    cookie_jar: Arc<RwLock<CookieJar>>,
    /// Origin-keyed localStorage (`origin → {key → value}`). Shared by all
    /// sessions of this context; writes from one session are visible to the
    /// others at the same origin.
    local_storage: Arc<RwLock<HashMap<String, HashMap<String, String>>>>,
    /// Egress client (dedicated when a proxy was set, else the browser's).
    http_client: Arc<HttpClient>,
    /// Credential mode (M4, design §6.2): when set, storage-exporting CDP
    /// surface (`Network.*Cookies`, `OXI.exportStorageState`) is denied for
    /// sessions of this context — credentials flow only through the broker.
    credential_mode: AtomicBool,
}

impl BrowserContext {
    /// Create a context around an existing (e.g. cookie-file-loaded) jar.
    pub(crate) fn with_cookie_jar(
        id: ContextId,
        label: Option<String>,
        http_client: Arc<HttpClient>,
        cookie_jar: Arc<RwLock<CookieJar>>,
    ) -> Self {
        Self {
            id,
            label,
            cookie_jar,
            local_storage: Arc::new(RwLock::new(HashMap::new())),
            http_client,
            credential_mode: AtomicBool::new(false),
        }
    }

    /// Get the context ID.
    pub fn id(&self) -> &ContextId {
        &self.id
    }

    /// Get the debugging label, if any.
    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }

    /// Get the context's cookie jar.
    pub fn cookie_jar(&self) -> &Arc<RwLock<CookieJar>> {
        &self.cookie_jar
    }

    /// Get a handle-cloned Arc to the context's HTTP client.
    pub fn http_client(&self) -> Arc<HttpClient> {
        self.http_client.clone()
    }

    /// Get a handle-cloned Arc to the context's origin-keyed storage map.
    pub fn storage_map(&self) -> Arc<RwLock<HashMap<String, HashMap<String, String>>>> {
        self.local_storage.clone()
    }

    /// Read-clone of one origin's storage bucket (empty when absent).
    pub fn storage_bucket(&self, origin: &str) -> HashMap<String, String> {
        self.local_storage
            .read()
            .get(origin)
            .cloned()
            .unwrap_or_default()
    }

    /// Clear every storage bucket in this context (all origins).
    pub fn clear_storage(&self) {
        self.local_storage.write().clear();
    }

    /// Enable or disable credential mode (M4, design §6.2). Flipping the flag
    /// affects every session of this context immediately — the CDP gate reads
    /// the flag live at command time.
    pub fn set_credential_mode(&self, on: bool) {
        self.credential_mode.store(on, Ordering::SeqCst);
    }

    /// Whether credential mode is active for this context.
    pub fn credential_mode(&self) -> bool {
        self.credential_mode.load(Ordering::SeqCst)
    }
}

/// Storage origin key for a URL: [`url::Origin::ascii_serialization`].
///
/// Tuple origins serialize as `scheme://host[:port]`; opaque origins
/// (`about:`, `data:`, …) all converge on `"null"` — the documented M-A
/// limitation (see module docs).
pub fn storage_origin_of(url: &Url) -> String {
    url.origin().ascii_serialization()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BrowserConfig;

    fn test_context() -> BrowserContext {
        let config = BrowserConfig::headless();
        let jar = Arc::new(RwLock::new(CookieJar::new()));
        let client = Arc::new(HttpClient::new(&config, jar.clone()).unwrap());
        BrowserContext::with_cookie_jar(ContextId::test_next(), Some("t".into()), client, jar)
    }

    #[test]
    fn context_id_serializes_as_string() {
        let id = ContextId("ctx-7".into());
        assert_eq!(id.to_string(), "ctx-7");
        assert_eq!(serde_json::to_string(&id).unwrap(), r#""ctx-7""#);
    }

    #[test]
    fn context_id_round_trips_through_string() {
        let id = ContextId::from_string("ctx-3");
        assert_eq!(id.as_str(), "ctx-3");
        assert_eq!(id.to_string(), "ctx-3");
        assert_eq!(id, ContextId::from_string(id.as_str()));
    }

    #[test]
    fn storage_origin_of_partitions_and_converges() {
        let https = Url::parse("https://github.com/x").unwrap();
        let other_port = Url::parse("https://github.com:8443/x").unwrap();
        assert_eq!(storage_origin_of(&https), "https://github.com");
        assert_eq!(storage_origin_of(&other_port), "https://github.com:8443");
        assert_eq!(storage_origin_of(&https), storage_origin_of(&https));
        // Opaque origins converge on "null" (documented M-A limitation).
        let about = Url::parse("about:blank").unwrap();
        let data = Url::parse("data:text/html,hi").unwrap();
        assert_eq!(storage_origin_of(&about), "null");
        assert_eq!(storage_origin_of(&data), "null");
    }

    #[test]
    fn storage_bucket_and_clear_storage() {
        let ctx = test_context();
        ctx.storage_map()
            .write()
            .entry("https://a.test".into())
            .or_default()
            .insert("k".into(), "v".into());
        assert_eq!(
            ctx.storage_bucket("https://a.test")
                .get("k")
                .map(String::as_str),
            Some("v")
        );
        assert!(ctx.storage_bucket("https://missing.test").is_empty());
        ctx.clear_storage();
        assert!(ctx.storage_bucket("https://a.test").is_empty());
    }

    #[test]
    fn credential_mode_defaults_off_and_toggles_live() {
        let ctx = test_context();
        assert!(!ctx.credential_mode(), "credential mode defaults to off");
        ctx.set_credential_mode(true);
        assert!(ctx.credential_mode());
        ctx.set_credential_mode(false);
        assert!(!ctx.credential_mode());
    }
}
