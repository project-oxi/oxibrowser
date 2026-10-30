//! CDP event broadcasting system.
//!
//! Provides an `EventSender` that domain handlers use to queue CDP events,
//! and a background task that drains the queue and sends them over the
//! WebSocket connection.

use crate::domains::fetch::FetchPattern;
use crate::protocol::CdpEvent;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError, RwLock};
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Maximum number of lifecycle events buffered for `Page.getLifecycleEvents`.
/// Once full, the oldest entry is dropped to make room.
const LIFECYCLE_BUFFER_CAP: usize = 20;

type WorldScripts = HashMap<String, Vec<(String, String)>>;

/// Sender half of the event broadcaster.
#[derive(Clone)]
pub struct EventSender {
    tx: mpsc::UnboundedSender<CdpEvent>,
    page_enabled: Arc<AtomicBool>,
    runtime_enabled: Arc<AtomicBool>,
    network_enabled: Arc<AtomicBool>,
    log_enabled: Arc<AtomicBool>,
    fetch_enabled: Arc<AtomicBool>,
    fetch_patterns: Arc<RwLock<Vec<FetchPattern>>>,
    /// Buffered navigation lifecycle events `(method, params, timestamp)`,
    /// recorded while the Page/lifecycle flag is enabled and drained by
    /// `Page.getLifecycleEvents` (capped at `LIFECYCLE_BUFFER_CAP`).
    lifecycle_buffer: Arc<Mutex<Vec<(String, Value, f64)>>>,
    /// Session ID stamped onto every CDP event once a target is attached
    /// (flat / auto-attach protocol). `None` for root-level events.
    attached_session_id: Arc<RwLock<Option<String>>>,
    main_frame_id: Arc<String>,
    world_scripts: Arc<RwLock<WorldScripts>>,
    reported_worlds: Arc<RwLock<HashSet<(String, u32)>>>,
}

/// Receiver half of the event broadcaster.
pub struct EventReceiver {
    rx: mpsc::UnboundedReceiver<CdpEvent>,
}

/// Create a new event broadcaster pair.
pub fn event_channel() -> (EventSender, EventReceiver) {
    let (tx, rx) = mpsc::unbounded_channel();
    let sender = EventSender {
        tx,
        page_enabled: Arc::new(AtomicBool::new(false)),
        runtime_enabled: Arc::new(AtomicBool::new(false)),
        network_enabled: Arc::new(AtomicBool::new(false)),
        log_enabled: Arc::new(AtomicBool::new(false)),
        fetch_enabled: Arc::new(AtomicBool::new(false)),
        fetch_patterns: Arc::new(RwLock::new(Vec::new())),
        lifecycle_buffer: Arc::new(Mutex::new(Vec::new())),
        attached_session_id: Arc::new(RwLock::new(None)),
        main_frame_id: Arc::new("default".to_owned()),
        world_scripts: Arc::new(RwLock::new(HashMap::new())),
        reported_worlds: Arc::new(RwLock::new(HashSet::new())),
    };
    let receiver = EventReceiver { rx };
    (sender, receiver)
}

impl EventSender {
    pub(crate) fn add_world_script(&self, session: String, world: String) -> String {
        let id = format!("world-init-{}", uuid::Uuid::new_v4());
        self.world_scripts
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(session)
            .or_default()
            .push((id.clone(), world));
        id
    }

    pub(crate) fn world_scripts(&self, session: &str) -> Vec<(String, String)> {
        self.world_scripts
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(session)
            .cloned()
            .unwrap_or_default()
    }

    pub(crate) fn remove_world_script(&self, session: &str, id: &str) -> bool {
        let mut scripts = self
            .world_scripts
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(scripts) = scripts.get_mut(session) else {
            return false;
        };
        let before = scripts.len();
        scripts.retain(|(script, _)| script != id);
        before != scripts.len()
    }

    pub(crate) fn report_world_once(&self, session: &str, context: u32) -> bool {
        self.reported_worlds
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((session.to_owned(), context))
    }

    pub(crate) fn clear_reported_worlds(&self, session: &str) {
        self.reported_worlds
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(owner, _)| owner != session);
    }

    /// Whether the event channel's receiver is gone (e.g. the client's
    /// WebSocket dispatch loop ended) — further `send`s will fail. Cheap
    /// disconnect probe for background emitters such as the screencast pump.
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    /// Send a CDP event to the client.
    pub fn send(&self, event: CdpEvent) {
        if let Err(e) = self.tx.send(event) {
            warn!(error = %e, "failed to send CDP event (channel closed)");
        }
    }

    /// Send a typed event with a method name and JSON params.
    pub fn send_event(&self, method: &str, params: Value) {
        self.send_event_opt(method, params, None);
    }

    /// Send an event stamped with an explicit `sessionId` (used for child-target
    /// events whose session differs from the currently attached one).
    pub fn send_event_with_session(&self, method: &str, params: Value, session_id: &str) {
        self.send_event_opt(method, params, Some(session_id.to_string()));
    }

    fn send_event_opt(&self, method: &str, params: Value, explicit_session: Option<String>) {
        debug!(method = %method, "queuing CDP event");
        let mut event = CdpEvent::new(method, params);
        let sid = explicit_session
            .or_else(|| self.attached_session_id.read().ok().and_then(|g| g.clone()));
        if let Some(sid) = sid {
            event.session_id = Some(sid);
        }
        self.send(event);
    }

    /// Set the session ID stamped onto subsequent events (called when a target
    /// is attached via `Target.setAutoAttach` / `Target.attachToTarget`).
    pub fn set_session_id(&self, session_id: String) {
        if let Ok(mut guard) = self.attached_session_id.write() {
            *guard = Some(session_id);
        }
    }

    /// Route a command's events to its target without changing other targets.
    pub(crate) fn for_session(&self, session: &str) -> Self {
        let mut sender = self.clone();
        sender.attached_session_id = Arc::new(RwLock::new(Some(session.to_owned())));
        sender
    }

    pub(crate) fn for_target_session(&self, session: &str, target: &str) -> Self {
        let mut sender = self.for_session(session);
        sender.main_frame_id = Arc::new(target.to_owned());
        sender
    }

    pub(crate) fn main_frame_id(&self) -> &str {
        self.main_frame_id.as_str()
    }

    /// The currently-attached session ID, if any.
    pub fn session_id(&self) -> Option<String> {
        self.attached_session_id
            .read()
            .ok()
            .and_then(|guard| guard.clone())
    }

    /// Send a Page domain event (only if Page domain is enabled).
    pub fn send_page_event(&self, method: &str, params: Value) {
        if self.page_enabled.load(Ordering::Relaxed) {
            self.record_lifecycle(method, &params);
            self.send_event(method, params);
        }
    }

    /// Page domain event stamped with an explicit `sessionId` (child target).
    pub fn send_page_event_with_session(&self, method: &str, params: Value, session_id: &str) {
        if self.page_enabled.load(Ordering::Relaxed) {
            self.record_lifecycle(method, &params);
            self.send_event_with_session(method, params, session_id);
        }
    }

    /// Buffer one navigation lifecycle event for `Page.getLifecycleEvents`.
    ///
    /// Only the three navigation lifecycle events are recorded; the buffer is
    /// capped at `LIFECYCLE_BUFFER_CAP`, dropping the oldest entry first.
    fn record_lifecycle(&self, method: &str, params: &Value) {
        const LIFECYCLE_METHODS: [&str; 3] = [
            "Page.domContentLoadedEventFired",
            "Page.loadEventFired",
            "Page.frameNavigated",
        ];
        if !LIFECYCLE_METHODS.contains(&method) {
            return;
        }
        let mut buffer = self
            .lifecycle_buffer
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if buffer.len() >= LIFECYCLE_BUFFER_CAP {
            buffer.remove(0);
        }
        buffer.push((
            method.to_string(),
            params.clone(),
            EventSender::timestamp_ms(),
        ));
    }

    /// Snapshot of the buffered lifecycle events, oldest first (drained by
    /// `Page.getLifecycleEvents`; the buffer itself is left intact).
    pub fn lifecycle_events(&self) -> Vec<(String, Value, f64)> {
        self.lifecycle_buffer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Send a Runtime domain event (only if Runtime domain is enabled).
    pub fn send_runtime_event(&self, method: &str, params: Value) {
        if self.runtime_enabled.load(Ordering::Relaxed) {
            self.send_event(method, params);
        }
    }

    /// Runtime domain event stamped with an explicit `sessionId` (child target).
    pub fn send_runtime_event_with_session(&self, method: &str, params: Value, session_id: &str) {
        if self.runtime_enabled.load(Ordering::Relaxed) {
            self.send_event_with_session(method, params, session_id);
        }
    }

    /// Send a Network domain event (only if Network domain is enabled).
    pub fn send_network_event(&self, method: &str, params: Value) {
        if self.network_enabled.load(Ordering::Relaxed) {
            self.send_event(method, params);
        }
    }

    /// Network domain event stamped with an explicit `sessionId` (child target).
    pub fn send_network_event_with_session(&self, method: &str, params: Value, session_id: &str) {
        if self.network_enabled.load(Ordering::Relaxed) {
            self.send_event_with_session(method, params, session_id);
        }
    }

    /// Send a Fetch domain event (only if Fetch domain is enabled).
    pub fn send_fetch_event(&self, method: &str, params: Value) {
        if self.fetch_enabled.load(Ordering::Relaxed) {
            self.send_event(method, params);
        }
    }

    /// Fetch domain event stamped with an explicit `sessionId` (child target).
    pub fn send_fetch_event_with_session(&self, method: &str, params: Value, session_id: &str) {
        if self.fetch_enabled.load(Ordering::Relaxed) {
            self.send_event_with_session(method, params, session_id);
        }
    }

    /// Send a Log domain event (only if Log domain is enabled).
    pub fn send_log_event(&self, method: &str, params: Value) {
        if self.log_enabled.load(Ordering::Relaxed) {
            self.send_event(method, params);
        }
    }

    /// Log domain event stamped with an explicit `sessionId` (child target).
    pub fn send_log_event_with_session(&self, method: &str, params: Value, session_id: &str) {
        if self.log_enabled.load(Ordering::Relaxed) {
            self.send_event_with_session(method, params, session_id);
        }
    }

    /// Send a Browser domain event — **ungated**.
    ///
    /// There is no `Browser.enable` flag in this server: Chrome emits
    /// `Browser.downloadWillBegin` / `Browser.downloadProgress` without a
    /// domain-enable handshake, so these events go out on the event channel
    /// as soon as they happen.
    ///
    /// Known limitation: events always target the root session (no
    /// `sessionId` stamping) — downloads initiated by an attached child
    /// target surface on the root session too.
    pub fn send_browser_event(&self, method: &str, params: Value) {
        self.send(CdpEvent::new(method, params));
    }

    /// Enable Log domain events.
    pub fn set_log_enabled(&self, enabled: bool) {
        self.log_enabled.store(enabled, Ordering::Relaxed);
    }

    /// Check if Log domain events are enabled.
    pub fn is_log_enabled(&self) -> bool {
        self.log_enabled.load(Ordering::Relaxed)
    }

    // -- Flag getters/setters --

    /// Enable Page domain events.
    pub fn set_page_enabled(&self, enabled: bool) {
        self.page_enabled.store(enabled, Ordering::Relaxed);
    }

    /// Enable Runtime domain events.
    pub fn set_runtime_enabled(&self, enabled: bool) {
        self.runtime_enabled.store(enabled, Ordering::Relaxed);
    }

    /// Enable Network domain events.
    pub fn set_network_enabled(&self, enabled: bool) {
        self.network_enabled.store(enabled, Ordering::Relaxed);
    }

    /// Enable Fetch domain events.
    pub fn set_fetch_enabled(&self, enabled: bool) {
        self.fetch_enabled.store(enabled, Ordering::Relaxed);
    }

    /// Set Fetch interception patterns.
    pub fn set_fetch_patterns(&self, patterns: Vec<FetchPattern>) {
        if let Ok(mut guard) = self.fetch_patterns.write() {
            *guard = patterns;
        }
    }

    /// Get Fetch interception patterns.
    pub fn get_fetch_patterns(&self) -> Vec<FetchPattern> {
        self.fetch_patterns
            .read()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    /// Check if Page domain events are enabled.
    pub fn is_page_enabled(&self) -> bool {
        self.page_enabled.load(Ordering::Relaxed)
    }

    /// Check if Fetch domain events are enabled.
    pub fn is_fetch_enabled(&self) -> bool {
        self.fetch_enabled.load(Ordering::Relaxed)
    }

    /// Helper: current timestamp as milliseconds since epoch.
    pub fn timestamp_ms() -> f64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64()
            * 1000.0
    }
}

impl EventReceiver {
    /// Receive the next event, waiting asynchronously.
    pub async fn recv(&mut self) -> Option<CdpEvent> {
        self.rx.recv().await
    }

    /// Try to receive all pending events without waiting.
    pub fn drain(&mut self) -> Vec<CdpEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.rx.try_recv() {
            events.push(event);
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domains::fetch::FetchPattern;
    use serde_json::json;

    #[test]
    fn test_event_channel_send_receive() {
        let (sender, mut receiver) = event_channel();
        sender.send_event("Page.loadEventFired", json!({ "timestamp": 1234.5 }));
        sender.send_event("Page.frameNavigated", json!({ "frameId": "main" }));

        let events = receiver.drain();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].method, "Page.loadEventFired");
        assert_eq!(events[1].method, "Page.frameNavigated");
    }

    #[test]
    fn test_page_event_gated() {
        let (sender, mut receiver) = event_channel();

        sender.send_page_event("Page.loadEventFired", json!({}));
        assert!(receiver.drain().is_empty());

        sender.set_page_enabled(true);
        sender.send_page_event("Page.loadEventFired", json!({ "timestamp": 1.0 }));
        let events = receiver.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].method, "Page.loadEventFired");
    }

    #[test]
    fn test_lifecycle_buffer_records_only_lifecycle_methods_and_caps() {
        let (sender, mut receiver) = event_channel();

        // Not recorded while the lifecycle/Page flag is disabled.
        sender.send_page_event("Page.frameNavigated", json!({ "frameId": "main" }));
        assert!(sender.lifecycle_events().is_empty());

        sender.set_page_enabled(true);
        // Only the three navigation lifecycle methods are buffered.
        sender.send_page_event("Page.screencastFrame", json!({}));
        assert!(sender.lifecycle_events().is_empty());

        // Cap at 20: the oldest entries are dropped first.
        for i in 0..25 {
            sender.send_page_event("Page.loadEventFired", json!({ "i": i }));
        }
        let buffered = sender.lifecycle_events();
        assert_eq!(buffered.len(), 20, "buffer never exceeds the cap");
        assert_eq!(buffered[0].0, "Page.loadEventFired");
        assert_eq!(buffered[0].1["i"], 5, "oldest dropped past the cap");
        assert_eq!(buffered[19].1["i"], 24);
        assert!(buffered[0].2 > 0.0, "timestamp recorded");

        // Buffered events still flow to the channel (25 lifecycle + 1 other).
        assert_eq!(receiver.drain().len(), 26);
    }

    #[test]
    fn test_network_event_gated() {
        let (sender, mut receiver) = event_channel();

        sender.send_network_event("Network.requestWillBeSent", json!({}));
        assert!(receiver.drain().is_empty());

        sender.set_network_enabled(true);
        sender.send_network_event("Network.requestWillBeSent", json!({ "requestId": "1" }));
        let events = receiver.drain();
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn test_browser_event_ungated() {
        // There is no Browser.enable flag: Browser.* events (download
        // progress) must flow with every domain flag disabled.
        let (sender, mut receiver) = event_channel();

        sender.set_session_id("page-session".to_owned());
        sender.send_browser_event(
            "Browser.downloadWillBegin",
            json!({
                "frameId": "",
                "guid": "g1",
                "url": "http://example.com/f.zip",
                "suggestedFilename": "f.zip",
            }),
        );
        let events = receiver.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].method, "Browser.downloadWillBegin");
        assert!(events[0].session_id.is_none());
        assert_eq!(events[0].params.as_ref().unwrap()["guid"], "g1");
    }

    #[tokio::test]
    async fn test_async_recv() {
        let (sender, mut receiver) = event_channel();
        sender.send_event("Test.event", json!({ "key": "value" }));

        let event = receiver.recv().await.unwrap();
        assert_eq!(event.method, "Test.event");
        assert_eq!(event.params.as_ref().unwrap()["key"], "value");
    }

    #[test]
    fn test_timestamp_ms() {
        let ts = EventSender::timestamp_ms();
        assert!(ts > 1_700_000_000_000.0);
    }

    #[test]
    fn test_atomic_flags() {
        let sender = EventSender {
            tx: mpsc::unbounded_channel().0,
            page_enabled: Arc::new(AtomicBool::new(false)),
            runtime_enabled: Arc::new(AtomicBool::new(false)),
            network_enabled: Arc::new(AtomicBool::new(false)),
            log_enabled: Arc::new(AtomicBool::new(false)),
            fetch_enabled: Arc::new(AtomicBool::new(false)),
            fetch_patterns: Arc::new(RwLock::new(Vec::new())),
            lifecycle_buffer: Arc::new(Mutex::new(Vec::new())),
            attached_session_id: Arc::new(RwLock::new(None)),
            main_frame_id: Arc::new("default".to_owned()),
            world_scripts: Arc::new(RwLock::new(HashMap::new())),
            reported_worlds: Arc::new(RwLock::new(HashSet::new())),
        };

        assert!(!sender.is_page_enabled());
        sender.set_page_enabled(true);
        assert!(sender.is_page_enabled());

        assert!(!sender.is_fetch_enabled());
        sender.set_fetch_enabled(true);
        assert!(sender.is_fetch_enabled());

        sender.set_fetch_patterns(vec![FetchPattern::default()]);
        assert!(!sender.get_fetch_patterns().is_empty());
    }

    #[test]
    fn test_send_event_stamps_session_id() {
        let (sender, mut receiver) = event_channel();
        // Before any attach: events carry no sessionId (root-level).
        sender.send_event("Page.loadEventFired", json!({}));
        let pre = receiver.drain();
        assert_eq!(pre.len(), 1);
        assert!(
            pre[0].session_id.is_none(),
            "root events must not carry a sessionId"
        );

        // After attach stamps the sessionId, all subsequent events carry it.
        sender.set_session_id("session-abc".to_string());
        sender.send_event("Page.frameNavigated", json!({}));
        let post = receiver.drain();
        assert_eq!(post.len(), 1);
        assert_eq!(post[0].session_id.as_deref(), Some("session-abc"));
    }
}
