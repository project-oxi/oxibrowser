//! Browser — the top-level browser instance.
//!
//! Owns sessions, the HTTP client, and global browser state.

use crate::browse_result::BrowseResult;
use crate::config::BrowserConfig;
use crate::context::{BrowserContext, ContextConfig, ContextId};
use crate::error::{CoreError, Result};
use crate::event::BrowserEvent;
use crate::network::HttpClient;
use crate::network::cookie::CookieJar;
use crate::security::audit;
use crate::session::Session;
use crate::tab::Tab;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tokio::sync::broadcast;
use tracing::{info, warn};

/// Unique browser instance ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BrowserId(u64);

impl BrowserId {
    pub(crate) fn next() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        Self(COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

impl std::fmt::Display for BrowserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "browser-{}", self.0)
    }
}

/// The top-level browser instance.
///
/// A Browser can hold multiple Sessions (browsing contexts), each with its own
/// cookie jar, storage, and pages. All sessions hang off a
/// [`BrowserContext`] (M-A): the default (anonymous) context inherits the
/// former global jar's role; additional contexts isolate jar + storage + egress.
pub struct Browser {
    /// Unique ID.
    id: BrowserId,
    /// Configuration.
    config: BrowserConfig,
    /// Shared HTTP client (also the default context's client).
    http_client: Arc<HttpClient>,
    /// Active sessions.
    sessions: RwLock<Vec<Arc<tokio::sync::RwLock<Session>>>>,
    /// Default (anonymous) context — every context-less `new_session` /
    /// `new_tab` lands here. Owns the cookie-file-loaded jar.
    default_context: Arc<BrowserContext>,
    /// Named contexts created via [`Browser::new_context`].
    contexts: RwLock<HashMap<ContextId, Arc<BrowserContext>>>,
    /// Monotonic counter backing [`ContextId`]s (`ctx-<n>`); the default
    /// context consumes `ctx-1`.
    context_seq: AtomicU64,
    /// Whether the browser has been closed.
    closed: std::sync::atomic::AtomicBool,
    /// Number of active Tab sessions (not in the sessions vec).
    tab_count: Arc<AtomicUsize>,
    /// Shutdown signal — broadcast to all session holders.
    shutdown_tx: broadcast::Sender<()>,
    /// Lifecycle event stream — `subscribe_events()` for observers.
    ///
    /// 32-slot buffer is plenty: we emit ≤4 events per page load
    /// (NavigationStarted, optional WaitingForSelector, DocumentReady,
    /// optional ScreenshotCaptured). The agent drops oldest on overflow.
    event_tx: broadcast::Sender<BrowserEvent>,
}

impl Browser {
    /// Create a new Browser instance with the given config.
    #[tracing::instrument(skip(config), err)]
    pub async fn new(config: BrowserConfig) -> Result<Self> {
        let cookie_jar = if let Some(ref path) = config.cookie_file {
            match CookieJar::load_from_file(path) {
                Ok(jar) => {
                    info!(path = %path.display(), "loaded cookies from file");
                    jar
                }
                Err(e) => {
                    // File missing or invalid is not fatal — start with empty jar
                    info!(
                        path = %path.display(),
                        error = %e,
                        "could not load cookie file, starting with empty jar"
                    );
                    CookieJar::new()
                }
            }
        } else {
            CookieJar::new()
        };

        let cookie_jar = Arc::new(RwLock::new(cookie_jar));
        let http_client = Arc::new(HttpClient::new(&config, cookie_jar.clone())?);
        // Default (anonymous) context: inherits the former global jar role and
        // the browser's HTTP client. `ctx-1` is reserved for it; named
        // contexts continue the sequence.
        let default_context = Arc::new(BrowserContext::with_cookie_jar(
            ContextId("ctx-1".into()),
            None,
            http_client.clone(),
            cookie_jar,
        ));
        let (shutdown_tx, _) = broadcast::channel::<()>(1);
        // 32 slots = generous headroom; we emit ≤4 events per page load.
        let (event_tx, _) = broadcast::channel::<BrowserEvent>(32);

        let id = BrowserId::next();
        info!(id = %id, "browser created");

        Ok(Self {
            id,
            config,
            http_client,
            sessions: RwLock::new(Vec::new()),
            default_context,
            contexts: RwLock::new(HashMap::new()),
            context_seq: AtomicU64::new(2),
            closed: std::sync::atomic::AtomicBool::new(false),
            tab_count: Arc::new(AtomicUsize::new(0)),
            shutdown_tx,
            event_tx,
        })
    }

    /// Mint the next context id (`ctx-<n>`); `ctx-1` is the default context.
    fn next_context_id(&self) -> ContextId {
        let n = self.context_seq.fetch_add(1, Ordering::Relaxed);
        ContextId(format!("ctx-{n}"))
    }

    /// Create a new browsing session.
    ///
    /// A session represents a browsing context group (cookie jar, session
    /// storage, navigation history). Lands on the default (anonymous)
    /// context — see [`Browser::new_session_in`] for per-context sessions.
    #[tracing::instrument(skip(self), fields(id = %self.id), err)]
    pub async fn new_session(&self) -> Result<Arc<tokio::sync::RwLock<Session>>> {
        self.new_session_in(&self.default_context).await
    }

    /// Create a new browsing session bound to `ctx`.
    ///
    /// The session derives its cookie jar, HTTP client, and origin-keyed
    /// localStorage from `ctx` — full isolation from other contexts.
    #[tracing::instrument(skip(self, ctx), fields(id = %self.id), err)]
    pub async fn new_session_in(
        &self,
        ctx: &Arc<BrowserContext>,
    ) -> Result<Arc<tokio::sync::RwLock<Session>>> {
        self.ensure_open()?;

        // Check capacity: both CDP sessions and Tab sessions count.
        let total = self.sessions.read().len() + self.tab_count.load(Ordering::Relaxed);
        if total >= self.config.max_sessions {
            return Err(CoreError::SessionError(
                "maximum number of sessions reached".into(),
            ));
        }

        let session = Session::new(self.id, self.config.clone(), ctx.clone()).await?;

        let session = Arc::new(tokio::sync::RwLock::new(session));
        self.sessions.write().push(session.clone());

        info!(
            session_count = self.sessions.read().len(),
            context = %ctx.id(),
            "new session created"
        );
        Ok(session)
    }

    /// One-shot: URL → content.
    ///
    /// Creates a temporary session, navigates to the URL, extracts the
    /// `BrowseResult`, and cleans up. Cookies persist across calls via
    /// the browser's shared cookie jar.
    ///
    /// This covers the 90% agent use case: "read this URL".
    #[tracing::instrument(skip(self), fields(id = %self.id), err)]
    pub async fn browse(&self, url: &str) -> Result<BrowseResult> {
        self.ensure_open()?;
        let session = self.new_session().await?;
        let mut s = session.write().await;
        s.navigate(url).await?;
        let result = match s.page() {
            Some(page) => BrowseResult::from_page(page),
            None => BrowseResult::empty(),
        };
        // Close the temporary session to free the slot.
        // Non-fatal — we already have the result.
        let _ = s.close().await;
        drop(s);
        Ok(result)
    }

    /// Open an interactive tab for agent use.
    ///
    /// Returns a `Tab` that is `Clone` and takes `&self` only — no lock
    /// management needed by the consumer.
    ///
    /// The session counts toward `max_sessions` but is not tracked for
    /// CDP cleanup — use `Tab::close()` to release the slot.
    ///
    /// The returned `Tab` is wired to this `Browser`'s event stream —
    /// navigation/wait/screenshot operations emit `BrowserEvent`s to
    /// subscribers of `subscribe_events()`.
    #[tracing::instrument(skip(self), fields(id = %self.id), err)]
    pub async fn new_tab(&self) -> Result<Tab> {
        self.new_tab_in(&self.default_context).await
    }

    /// Open an interactive tab bound to `ctx` (per-context isolation —
    /// see [`Browser::new_tab`]).
    #[tracing::instrument(skip(self, ctx), fields(id = %self.id), err)]
    pub async fn new_tab_in(&self, ctx: &Arc<BrowserContext>) -> Result<Tab> {
        self.ensure_open()?;

        // Check capacity against tracked sessions
        let session_count = self.sessions.read().len() + self.tab_count.load(Ordering::Relaxed);
        if session_count >= self.config.max_sessions {
            return Err(CoreError::SessionError(
                "maximum number of sessions reached".into(),
            ));
        }

        self.tab_count.fetch_add(1, Ordering::Relaxed);

        let session = Session::new(self.id, self.config.clone(), ctx.clone()).await?;

        let tab_id = uuid::Uuid::new_v4();
        tracing::info!(
            session_count = self.sessions.read().len(),
            context = %ctx.id(),
            tab_id = %tab_id,
            "new tab created"
        );
        Ok(Tab::new_with_cleanup_and_events(
            session,
            self.tab_count.clone(),
            self.event_tx.clone(),
            tab_id,
        ))
    }

    /// Convenience: create a session and navigate to a URL.
    #[tracing::instrument(skip(self), fields(id = %self.id), err)]
    pub async fn new_page(&self, url: &str) -> Result<Arc<tokio::sync::RwLock<Session>>> {
        let session = self.new_session().await?;
        session.write().await.navigate(url).await?;
        Ok(session)
    }

    /// Close all sessions and shut down.
    #[tracing::instrument(skip(self), fields(id = %self.id), err)]
    pub async fn close(&self) -> Result<()> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(()); // Already closed
        }

        // Save cookies to disk if a cookie_file path is configured. The file
        // has always backed the default (anonymous) jar — named contexts are
        // in-memory only (M-A).
        if let Some(path) = &self.config.cookie_file {
            let jar = self.default_context.cookie_jar().read();
            let save_result = jar.save_to_file(path);
            drop(jar);
            match save_result {
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "failed to save cookies to file");
                    audit::record(audit::event(
                        audit::AuditEventKind::SensitiveAction,
                        audit::AuditDecision::Deny,
                        format!("cookie_file_save_failed path={}", path.display()),
                    ));
                }
                Ok(()) => {
                    info!(path = %path.display(), "saved cookies to file");
                    audit::record(audit::event(
                        audit::AuditEventKind::SensitiveAction,
                        audit::AuditDecision::Allow,
                        format!("cookie_file_save path={}", path.display()),
                    ));
                }
            }
        }

        // Dispose of the in-memory jars so session cookies never outlive the
        // browser (design §7 P0-3) — the default jar plus every named
        // context's jar. `cookie_file` users already persisted above.
        if self.config.clear_cookies_on_close {
            let mut cleared = self.default_context.cookie_jar().write().clear_and_count();
            for ctx in self.contexts.read().values() {
                cleared += ctx.cookie_jar().write().clear_and_count();
            }
            if cleared > 0 {
                audit::record(audit::event(
                    audit::AuditEventKind::SessionTeardown,
                    audit::AuditDecision::Allow,
                    "browser_close",
                ));
                info!(cleared, "cookie jars cleared on close");
            }
        }

        // Broadcast shutdown signal to all session holders
        let _ = self.shutdown_tx.send(());

        // Drain sessions while holding the lock, then drop the lock
        // before awaiting session.close() to avoid holding a sync lock across await.
        let sessions: Vec<_> = self.sessions.write().drain(..).collect();
        // Lock is released here (sessions Vec goes out of scope implicitly)
        for session in sessions {
            let mut s = session.write().await;
            if let Err(e) = s.close().await {
                warn!("error closing session: {e}");
            }
        }

        info!("browser closed");
        Ok(())
    }

    /// Get a receiver for the shutdown signal.
    ///
    /// This can be used to detect when `close()` is called on the browser,
    /// e.g., for graceful shutdown in long-running tasks.
    pub fn shutdown_rx(&self) -> broadcast::Receiver<()> {
        self.shutdown_tx.subscribe()
    }

    /// Subscribe to browser lifecycle events.
    ///
    /// Observers (e.g. oxi-agent's `OxiBrowserEngine`) use this to forward
    /// events to the agent loop's `ToolExecutionUpdate` callback. The
    /// returned receiver can be safely dropped; new subscribers get their
    /// own queue. On overflow, the **oldest** undelivered event is dropped
    /// (broadcast semantics) — observers should treat `RecvError::Lagged`
    /// as a non-fatal signal that they fell behind, not as a hard error.
    pub fn subscribe_events(&self) -> broadcast::Receiver<BrowserEvent> {
        self.event_tx.subscribe()
    }

    /// Get the browser ID.
    pub fn id(&self) -> BrowserId {
        self.id
    }

    /// Get the browser config.
    pub fn config(&self) -> &BrowserConfig {
        &self.config
    }

    /// Get the HTTP client.
    pub fn http_client(&self) -> &Arc<HttpClient> {
        &self.http_client
    }

    /// Get the default (anonymous) context.
    pub fn default_context(&self) -> Arc<BrowserContext> {
        self.default_context.clone()
    }

    /// Look up a context by ID (default or named).
    pub fn context(&self, id: &ContextId) -> Option<Arc<BrowserContext>> {
        if id == self.default_context.id() {
            return Some(self.default_context.clone());
        }
        self.contexts.read().get(id).cloned()
    }

    /// Create a new isolated browser context.
    ///
    /// The context gets a dedicated cookie jar and its own origin-keyed
    /// storage map, and a dedicated [`HttpClient`] **bound to that same
    /// jar** — sharing the browser client would route all session HTTP
    /// traffic (navigate, subresources, POSTs) into the default context's
    /// jar, defeating isolation. `cfg.proxy` fixes the context's egress and
    /// must parse as an http/https/socks4/socks5 URL; anything else is
    /// rejected instead of silently degrading to direct egress.
    #[tracing::instrument(skip(self, cfg), fields(id = %self.id), err)]
    pub fn new_context(&self, cfg: ContextConfig) -> Result<Arc<BrowserContext>> {
        self.ensure_open()?;
        if let Some(proxy) = &cfg.proxy {
            let url = url::Url::parse(proxy).map_err(|e| {
                CoreError::SessionError(format!("invalid proxyServer {proxy:?}: {e}"))
            })?;
            if !matches!(url.scheme(), "http" | "https" | "socks4" | "socks5") {
                return Err(CoreError::SessionError(format!(
                    "unsupported proxyServer scheme {:?} in {proxy:?}",
                    url.scheme()
                )));
            }
        }
        let id = self.next_context_id();
        let jar = Arc::new(RwLock::new(CookieJar::new()));
        let mut ctx_config = self.config.clone();
        ctx_config.proxy = cfg.proxy.clone();
        let http_client = Arc::new(HttpClient::new(&ctx_config, jar.clone())?);
        let ctx = Arc::new(BrowserContext::with_cookie_jar(
            id,
            cfg.label,
            http_client,
            jar,
        ));
        self.contexts.write().insert(ctx.id().clone(), ctx.clone());
        info!(context = %ctx.id(), "browser context created");
        Ok(ctx)
    }

    /// Dispose of a named context: it is removed from the registry and its
    /// cookie jar is cleared immediately.
    ///
    /// Live sessions keep the context's `Arc` alive and continue to work,
    /// but the jar's cookies must never outlive the context (design §7
    /// P0-3) — sessions holding an `Arc` past `Browser::close` would
    /// otherwise still be able to read them.
    pub fn dispose_context(&self, id: &ContextId) -> Result<()> {
        if id == self.default_context.id() {
            return Err(CoreError::SessionError(
                "cannot dispose the default context".into(),
            ));
        }
        match self.contexts.write().remove(id) {
            Some(ctx) => {
                let cleared = ctx.cookie_jar().write().clear_and_count();
                if cleared > 0 {
                    audit::record(audit::event(
                        audit::AuditEventKind::SessionTeardown,
                        audit::AuditDecision::Allow,
                        format!("context_disposed context={} cookies={cleared}", ctx.id()),
                    ));
                }
                info!(context = %ctx.id(), "browser context disposed");
                Ok(())
            }
            None => Err(CoreError::SessionError(format!("unknown context {id}"))),
        }
    }

    /// Get the default context's cookie jar (the former global jar).
    pub fn cookie_jar(&self) -> &Arc<RwLock<CookieJar>> {
        self.default_context.cookie_jar()
    }

    /// Get active sessions.
    pub fn sessions(&self) -> &RwLock<Vec<Arc<tokio::sync::RwLock<Session>>>> {
        &self.sessions
    }

    /// Remove closed sessions from the active session list.
    ///
    /// Called by CDP session handlers after a WebSocket disconnects
    /// so that the session slot is freed for new connections.
    pub fn cleanup_closed_sessions(&self) {
        let mut sessions = self.sessions.write();
        let removed = sessions
            .extract_if(.., |s| match s.try_read() {
                Ok(guard) => guard.is_closed(),
                Err(_) => false, // locked — keep it for now
            })
            .count();
        if removed > 0 {
            info!(
                removed,
                session_count = sessions.len(),
                "cleaned up closed sessions"
            );
        }
    }

    /// Whether the browser is still open.
    pub fn is_open(&self) -> bool {
        !self.closed.load(Ordering::SeqCst)
    }

    fn ensure_open(&self) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            Err(CoreError::BrowserClosed)
        } else {
            Ok(())
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        if !self.closed.load(Ordering::SeqCst) {
            warn!("browser dropped without explicit close");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_browser_new_default_config() {
        let config = BrowserConfig::headless();
        let browser = Browser::new(config).await;
        assert!(
            browser.is_ok(),
            "Browser::new() with headless config should succeed"
        );
        let browser = browser.unwrap();
        assert!(browser.is_open());
    }

    #[tokio::test]
    async fn test_browser_new_session_creates_session() {
        let config = BrowserConfig::headless();
        let browser = Browser::new(config).await.unwrap();
        let session = browser.new_session().await;
        assert!(session.is_ok(), "new_session() should create a session");
        assert_eq!(browser.sessions().read().len(), 1);
    }

    #[tokio::test]
    async fn test_browser_new_session_respects_max_sessions() {
        let mut config = BrowserConfig::headless();
        config.max_sessions = 2;
        let browser = Browser::new(config).await.unwrap();

        let _s1 = browser.new_session().await.unwrap();
        let _s2 = browser.new_session().await.unwrap();
        let s3 = browser.new_session().await;

        assert!(s3.is_err(), "exceeding max_sessions should return error");
        match s3 {
            Err(CoreError::SessionError(msg)) => {
                assert!(
                    msg.contains("maximum number of sessions"),
                    "error should mention max sessions, got: {msg}"
                );
            }
            Err(e) => panic!("wrong error type: {e:?}"),
            Ok(_) => panic!("should have failed"),
        }
    }

    #[tokio::test]
    async fn test_browser_close_marks_closed() {
        let config = BrowserConfig::headless();
        let browser = Browser::new(config).await.unwrap();
        assert!(browser.is_open());

        browser.close().await.unwrap();
        assert!(!browser.is_open(), "browser should be closed after close()");
    }

    #[tokio::test]
    async fn test_browser_close_twice_no_panic() {
        let config = BrowserConfig::headless();
        let browser = Browser::new(config).await.unwrap();

        browser.close().await.unwrap();
        // Second close should succeed without panicking
        browser.close().await.unwrap();
        assert!(!browser.is_open());
    }

    #[tokio::test]
    async fn test_browser_close_clears_in_memory_cookie_jar() {
        let config = BrowserConfig::headless();
        let browser = Browser::new(config).await.unwrap();
        {
            let mut jar = browser.cookie_jar().write();
            let url = url::Url::parse("https://example.com/").unwrap();
            jar.store(&url, "sid=abc; Path=/");
        }
        assert!(!browser.cookie_jar().read().get_all().is_empty());
        browser.close().await.unwrap();
        assert!(
            browser.cookie_jar().read().get_all().is_empty(),
            "in-memory jar must be disposed on close"
        );
    }

    #[tokio::test]
    async fn test_browser_close_keeps_jar_when_disposal_disabled() {
        let mut config = BrowserConfig::headless();
        config.clear_cookies_on_close = false;
        let browser = Browser::new(config).await.unwrap();
        {
            let mut jar = browser.cookie_jar().write();
            let url = url::Url::parse("https://example.com/").unwrap();
            jar.store(&url, "sid=abc; Path=/");
        }
        browser.close().await.unwrap();
        assert_eq!(browser.cookie_jar().read().get_all().len(), 1);
    }

    #[tokio::test]
    async fn test_browser_new_session_after_close_returns_error() {
        let config = BrowserConfig::headless();
        let browser = Browser::new(config).await.unwrap();
        browser.close().await.unwrap();

        let result = browser.new_session().await;
        assert!(result.is_err(), "new_session() after close should fail");
        assert!(
            matches!(result, Err(CoreError::BrowserClosed)),
            "error should be BrowserClosed"
        );
    }

    #[tokio::test]
    async fn test_browser_browse_after_close_returns_error() {
        let config = BrowserConfig::headless();
        let browser = Browser::new(config).await.unwrap();
        browser.close().await.unwrap();

        let result = browser.browse("https://example.com").await;
        assert!(result.is_err(), "browse() after close should fail");
        assert!(
            matches!(result, Err(CoreError::BrowserClosed)),
            "error should be BrowserClosed"
        );
    }

    #[tokio::test]
    async fn test_browser_new_tab_creates_tab() {
        let config = BrowserConfig::headless();
        let browser = Browser::new(config).await.unwrap();
        let tab = browser.new_tab().await;
        assert!(tab.is_ok(), "new_tab() should create a tab");
        let tab = tab.unwrap();
        assert!(!tab.is_closed(), "new tab should not be closed");
    }

    #[tokio::test]
    async fn test_browser_new_tab_clonable() {
        let config = BrowserConfig::headless();
        let browser = Browser::new(config).await.unwrap();
        let tab = browser.new_tab().await.unwrap();
        let tab2 = tab.clone();
        assert!(!tab2.is_closed());
    }

    #[tokio::test]
    async fn test_browser_new_tab_after_close_returns_error() {
        let config = BrowserConfig::headless();
        let browser = Browser::new(config).await.unwrap();
        browser.close().await.unwrap();

        let result = browser.new_tab().await;
        assert!(result.is_err(), "new_tab() after close should fail");
        assert!(
            matches!(result, Err(CoreError::BrowserClosed)),
            "error should be BrowserClosed"
        );
    }

    #[tokio::test]
    async fn test_browser_new_tab_respects_max_sessions() {
        let mut config = BrowserConfig::headless();
        config.max_sessions = 2;
        let browser = Browser::new(config).await.unwrap();

        let _s1 = browser.new_session().await.unwrap();
        let _t1 = browser.new_tab().await.unwrap();
        let t2 = browser.new_tab().await;

        assert!(
            t2.is_err(),
            "exceeding max_sessions via new_tab should fail"
        );
    }

    #[tokio::test]
    async fn test_subscribe_events_returns_receiver() {
        let config = BrowserConfig::headless();
        let browser = Browser::new(config).await.unwrap();
        // Should not panic; multiple subscribers should be supported.
        let _rx1 = browser.subscribe_events();
        let _rx2 = browser.subscribe_events();
    }

    #[tokio::test]
    async fn test_emit_event_does_not_block_on_no_subscribers() {
        use crate::event::BrowserEvent;
        let config = BrowserConfig::headless();
        let browser = Browser::new(config).await.unwrap();
        // No subscribers — emit should silently succeed.
        // (Direct channel access; subscribers only added by tests below.)
        for i in 0..100 {
            let _ = browser.event_tx.send(BrowserEvent::NavigationStarted {
                tab_id: uuid::Uuid::nil(),
                url: format!("https://example.com/{i}"),
            });
        }
    }

    #[tokio::test]
    async fn test_emit_event_reaches_subscriber() {
        use crate::event::BrowserEvent;
        let config = BrowserConfig::headless();
        let browser = Browser::new(config).await.unwrap();
        let mut rx = browser.subscribe_events();

        // The Tab is what emits events; simulate that path here.
        let _ = browser.event_tx.send(BrowserEvent::NavigationStarted {
            tab_id: uuid::Uuid::nil(),
            url: "https://example.com".into(),
        });

        let event = rx.try_recv().expect("subscriber should receive event");
        match event {
            BrowserEvent::NavigationStarted { url, .. } => {
                assert_eq!(url, "https://example.com");
            }
            other => panic!("expected NavigationStarted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_context_ids_sequence_and_lookup() {
        let browser = Browser::new(BrowserConfig::headless()).await.unwrap();
        // ctx-1 is reserved for the default context.
        assert_eq!(
            browser.default_context().id().to_string(),
            "ctx-1",
            "default context must be ctx-1"
        );
        let c1 = browser
            .new_context(ContextConfig {
                label: Some("acct".into()),
                proxy: None,
            })
            .unwrap();
        let c2 = browser.new_context(ContextConfig::default()).unwrap();
        assert_eq!(c1.id().to_string(), "ctx-2");
        assert_eq!(c2.id().to_string(), "ctx-3");
        assert_eq!(c1.label(), Some("acct"));
        assert_eq!(c2.label(), None);
        assert!(browser.context(c1.id()).is_some());
        assert!(Arc::ptr_eq(&browser.context(c1.id()).unwrap(), &c1));
        assert!(browser.context(&ContextId("ctx-99".into())).is_none());
    }

    #[tokio::test]
    async fn test_contexts_isolate_cookie_jars() {
        let browser = Browser::new(BrowserConfig::headless()).await.unwrap();
        let c1 = browser.new_context(ContextConfig::default()).unwrap();
        let c2 = browser.new_context(ContextConfig::default()).unwrap();
        let url = url::Url::parse("https://example.com/").unwrap();

        {
            let mut jar = c1.cookie_jar().write();
            jar.store(&url, "sid=c1; Path=/");
        }
        assert_eq!(c1.cookie_jar().read().get_all().len(), 1);
        assert!(
            c2.cookie_jar().read().get_all().is_empty(),
            "sibling context must not see c1's cookies"
        );
        assert!(
            browser
                .default_context()
                .cookie_jar()
                .read()
                .get_all()
                .is_empty(),
            "default context must not see c1's cookies"
        );

        // Sessions inherit exactly their context's jar (same Arc).
        let s1 = browser.new_session_in(&c1).await.unwrap();
        let s2 = browser.new_session_in(&c2).await.unwrap();
        assert!(Arc::ptr_eq(s1.read().await.cookie_jar(), c1.cookie_jar()));
        assert!(Arc::ptr_eq(s2.read().await.cookie_jar(), c2.cookie_jar()));
    }

    #[tokio::test]
    async fn test_context_proxy_gets_dedicated_client() {
        let browser = Browser::new(BrowserConfig::headless()).await.unwrap();
        let proxied = browser
            .new_context(ContextConfig {
                label: None,
                proxy: Some("http://127.0.0.1:1".into()),
            })
            .unwrap();
        assert!(!Arc::ptr_eq(&proxied.http_client(), browser.http_client()));
        // No proxy → still a dedicated client (bound to the context's own
        // jar — sharing the browser client would route session HTTP traffic
        // into the default jar).
        let plain = browser.new_context(ContextConfig::default()).unwrap();
        assert!(!Arc::ptr_eq(&plain.http_client(), browser.http_client()));
    }

    #[tokio::test]
    async fn test_dispose_context_rules() {
        let browser = Browser::new(BrowserConfig::headless()).await.unwrap();
        let ctx = browser.new_context(ContextConfig::default()).unwrap();

        // The default context can never be disposed.
        let result = browser.dispose_context(browser.default_context().id());
        assert!(
            matches!(&result, Err(CoreError::SessionError(msg)) if msg.contains("default")),
            "default dispose must be refused, got {result:?}"
        );

        // Disposal removes the registry entry only — the session keeps its
        // Arc and stays fully functional.
        let session = browser.new_session_in(&ctx).await.unwrap();
        session.write().await.set_local_storage("k", "v");
        assert!(browser.dispose_context(ctx.id()).is_ok());
        assert!(browser.context(ctx.id()).is_none());
        assert_eq!(
            session.read().await.get_local_storage("k").as_deref(),
            Some("v")
        );

        // Unknown ids error.
        assert!(
            browser
                .dispose_context(&ContextId("ctx-999".into()))
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_close_clears_all_context_jars() {
        let browser = Browser::new(BrowserConfig::headless()).await.unwrap();
        let c1 = browser.new_context(ContextConfig::default()).unwrap();
        let url = url::Url::parse("https://example.com/").unwrap();
        browser.cookie_jar().write().store(&url, "sid=d; Path=/");
        c1.cookie_jar().write().store(&url, "sid=c1; Path=/");

        browser.close().await.unwrap();
        assert!(browser.cookie_jar().read().get_all().is_empty());
        assert!(
            c1.cookie_jar().read().get_all().is_empty(),
            "close must clear every context's jar"
        );
    }

    #[tokio::test]
    async fn test_session_close_keeps_context_storage() {
        let browser = Browser::new(BrowserConfig::headless()).await.unwrap();
        let ctx = browser.new_context(ContextConfig::default()).unwrap();

        let session = browser.new_session_in(&ctx).await.unwrap();
        session.write().await.set_local_storage("tok", "abc");
        session.write().await.close().await.unwrap();
        assert_eq!(
            ctx.storage_bucket("null").get("tok").map(String::as_str),
            Some("abc"),
            "context storage must survive session close"
        );

        // A sibling session of the same context observes the storage.
        let sibling = browser.new_session_in(&ctx).await.unwrap();
        assert_eq!(
            sibling.read().await.get_local_storage("tok").as_deref(),
            Some("abc")
        );
    }

    /// F1: a named context's HTTP traffic (navigate fetch carries Set-Cookie)
    /// must land in the CONTEXT's jar — never the default jar, and never a
    /// throwaway jar the JS `document.cookie` bridge can't see.
    #[tokio::test]
    async fn test_context_http_cookies_land_in_context_jar() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ctx"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("<html><body>hi</body></html>")
                    .insert_header("content-type", "text/html")
                    .insert_header("set-cookie", "ctxc=1; Path=/"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("<html><body>hi</body></html>")
                    .insert_header("content-type", "text/html"),
            )
            .mount(&server)
            .await;

        let mut config = BrowserConfig::headless();
        config.enable_ssrf_filter = false; // reach the loopback mock
        let browser = Browser::new(config).await.unwrap();
        let ctx = browser
            .new_context(ContextConfig {
                label: Some("iso".into()),
                proxy: None,
            })
            .unwrap();
        let tab = browser.new_tab_in(&ctx).await.unwrap();
        tab.goto(&format!("{}/ctx", server.uri())).await.unwrap();

        let url = url::Url::parse(&server.uri()).unwrap();
        let ctx_cookie = ctx.cookie_jar().read().cookies_for_js(&url);
        assert!(
            ctx_cookie.contains("ctxc=1"),
            "Set-Cookie from context navigation must land in the context jar, got: {ctx_cookie:?}"
        );
        let default_cookie = browser.cookie_jar().read().cookies_for_js(&url);
        assert!(
            !default_cookie.contains("ctxc"),
            "context cookies must not bleed into the default jar, got: {default_cookie:?}"
        );
        // The JS bridge reads the context jar too, so document.cookie sees it.
        let doc = tab.evaluate("document.cookie").await.unwrap();
        let doc = doc.as_str().unwrap_or_default();
        assert!(
            doc.contains("ctxc=1"),
            "document.cookie must observe the context jar, got: {doc:?}"
        );

        // A default-context tab must NOT see the context cookie.
        let default_tab = browser.new_tab().await.unwrap();
        default_tab
            .goto(&format!("{}/", server.uri()))
            .await
            .unwrap();
        let doc = default_tab.evaluate("document.cookie").await.unwrap();
        let doc = doc.as_str().unwrap_or_default();
        assert!(
            !doc.contains("ctxc"),
            "default-context sessions must not observe context cookies, got: {doc:?}"
        );
    }

    /// F3: dispose_context must clear the jar immediately — a session that
    /// keeps the context Arc alive past dispose (or past Browser::close) must
    /// not be able to read the disposed context's cookies.
    #[tokio::test]
    async fn test_dispose_context_clears_jar() {
        let browser = Browser::new(BrowserConfig::headless()).await.unwrap();
        let ctx = browser
            .new_context(ContextConfig {
                label: None,
                proxy: None,
            })
            .unwrap();
        let url = url::Url::parse("https://keep.test/").unwrap();
        ctx.cookie_jar().write().store(&url, "sid=leak-me; Path=/");
        assert!(!ctx.cookie_jar().read().is_empty());
        let _session = browser.new_session_in(&ctx).await.unwrap();

        browser.dispose_context(ctx.id()).unwrap();
        assert!(
            ctx.cookie_jar().read().is_empty(),
            "disposed context jar must be cleared even though the session keeps the Arc alive"
        );
    }

    /// F4: an invalid proxyServer must fail new_context instead of silently
    /// degrading the context to direct egress.
    #[tokio::test]
    async fn test_new_context_rejects_invalid_proxy() {
        let browser = Browser::new(BrowserConfig::headless()).await.unwrap();
        for bad in ["not a url", "ftp://proxy.test:1080", "://oops", ""] {
            let Err(err) = browser.new_context(ContextConfig {
                label: None,
                proxy: Some(bad.to_string()),
            }) else {
                panic!("proxy {bad:?} must be rejected");
            };
            assert!(
                matches!(&err, CoreError::SessionError(m) if m.contains("proxyServer")),
                "error should mention proxyServer, got: {err:?}"
            );
        }
        // Valid schemes are accepted.
        for ok in ["http://proxy.test:8080", "socks5://proxy.test:1080"] {
            browser
                .new_context(ContextConfig {
                    label: None,
                    proxy: Some(ok.to_string()),
                })
                .unwrap_or_else(|e| panic!("proxy {ok:?} must be accepted: {e}"));
        }
    }
}
