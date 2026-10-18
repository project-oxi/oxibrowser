//! Steering events for the agent plane (execution design M2).
//!
//! The engine's official subscription path (`Session::set_event_sink`, a
//! std mpsc of `CoreEvent`) is bridged into an async queue:
//!
//! ```text
//! JS thread ──std mpsc──▶ forwarder thread ──tokio mpsc──▶ hub task ──▶ queue
//!                                                            │
//!                                             TabSpawned → sibling session
//!                                             (observer contract, see
//!                                              CoreEvent::TabSpawned docs)
//! ```
//!
//! The hub task owns popup-session creation: the engine cannot mint
//! sessions (a `Session` holds no `Browser` handle), so the observer —
//! this host — creates a sibling in the same browser/context and navigates
//! it to the requested URL. The source script still sees `null` from
//! `window.open` (v1).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use oxibrowser_core::js::{CoreEvent, DialogGate};
use oxibrowser_core::session::Session;
use oxibrowser_core::{Browser, BrowserContext};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::{Mutex as AsyncMutex, Notify, RwLock, mpsc};

/// Shared steering state: the event queue, the dialog-pending flag, and
/// the popup sessions this host created.
pub struct EventHub {
    queue: Mutex<VecDeque<Value>>,
    notify: Notify,
    dialog_pending: AtomicBool,
    popups: AsyncMutex<HashMap<String, Arc<RwLock<Session>>>>,
    next_popup: std::sync::atomic::AtomicU64,
}

impl EventHub {
    /// Attach to `session` and start the bridge. Returns the hub.
    ///
    /// The dialog gate is cloned separately by the caller *before* any
    /// action runs: resolution must never need the session lock (a
    /// blocking `alert()` holds the write lock).
    pub fn attach(
        browser: Arc<Browser>,
        context: Arc<BrowserContext>,
        session: &mut Session,
    ) -> Arc<Self> {
        let hub = Arc::new(Self {
            queue: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
            dialog_pending: AtomicBool::new(false),
            popups: AsyncMutex::new(HashMap::new()),
            next_popup: std::sync::atomic::AtomicU64::new(0),
        });

        let (tx, rx) = std::sync::mpsc::channel::<CoreEvent>();
        session.set_event_sink(tx);

        // std → tokio bridge thread.
        let (atk_tx, mut atk_rx) = mpsc::unbounded_channel::<CoreEvent>();
        std::thread::Builder::new()
            .name("oxibrowser-agent-events".into())
            .spawn(move || {
                for event in rx {
                    if atk_tx.send(event).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn event bridge thread");

        let hub_task = hub.clone();
        tokio::spawn(async move {
            while let Some(event) = atk_rx.recv().await {
                hub_task.handle(event, &browser, &context).await;
            }
        });

        hub
    }

    async fn handle(
        &self,
        event: CoreEvent,
        browser: &Arc<Browser>,
        context: &Arc<BrowserContext>,
    ) {
        match event {
            CoreEvent::TabSpawned {
                url,
                target,
                source_session_id,
            } => {
                // Observer contract: mint the sibling session here.
                let id = format!("p{}", self.next_popup.fetch_add(1, Ordering::SeqCst) + 1);
                let created = browser.new_session_in(context).await.ok();
                if let Some(session) = created {
                    let nav = session.clone();
                    let nav_url = url.clone();
                    tokio::spawn(async move {
                        let _ = nav.write().await.navigate(&nav_url).await;
                    });
                    self.popups.lock().await.insert(id.clone(), session);
                }
                self.push(json!({
                    "type": "popup", "id": id, "url": url, "target": target,
                    "source_session_id": source_session_id,
                }));
            }
            CoreEvent::Dialog {
                dialog_type,
                message,
                default_value,
            } => {
                self.dialog_pending.store(true, Ordering::SeqCst);
                self.push(json!({
                    "type": "dialog", "dialog_type": dialog_type.as_str(),
                    "message": message, "default_value": default_value,
                }));
            }
            CoreEvent::Download {
                guid,
                url,
                filename,
                save_path,
                total_bytes,
            } => {
                self.push(json!({
                    "type": "download", "guid": guid, "url": url,
                    "filename": filename, "save_path": save_path,
                    "total_bytes": total_bytes,
                }));
            }
            CoreEvent::DownloadFailed {
                guid, url, error, ..
            } => {
                self.push(json!({
                    "type": "download_failed", "guid": guid, "url": url, "error": error,
                }));
            }
            CoreEvent::Console {
                level,
                args,
                timestamp,
            } => {
                let text = args
                    .iter()
                    .map(|a| a.display())
                    .collect::<Vec<_>>()
                    .join(" ");
                self.push(json!({
                    "type": "console", "level": level.api_type(),
                    "text": text, "timestamp": timestamp,
                }));
            }
            CoreEvent::Exception { message, name, .. } => {
                self.push(json!({"type": "page_error", "name": name, "message": message}));
            }
            // Fetch/subresource/ws events are high-volume observability,
            // not steering — ignored here (M4's replay uses the network
            // log instead).
            _ => {}
        }
    }

    fn push(&self, event: Value) {
        self.queue.lock().push_back(event);
        self.notify.notify_waiters();
    }

    /// Is a dialog currently parked on the gate?
    pub fn dialog_pending(&self) -> bool {
        self.dialog_pending.load(Ordering::SeqCst)
    }

    /// Clear the pending flag (called after resolution).
    pub fn clear_dialog_pending(&self) {
        self.dialog_pending.store(false, Ordering::SeqCst);
    }

    /// Wait for the next event of `kind` (`"dialog"`, `"popup"`,
    /// `"download"`, `"download_failed"`, `"console"`, `"page_error"`,
    /// `"any"`), up to `timeout_ms` (clamped inside the 30s world bridge
    /// budget).
    pub async fn wait_for(&self, kind: &str, timeout_ms: u64) -> Option<Value> {
        let matches = |v: &Value| kind == "any" || v["type"].as_str() == Some(kind);
        let deadline = tokio::time::Instant::now()
            + tokio::time::Duration::from_millis(timeout_ms.clamp(1, 25_000));
        loop {
            {
                let mut queue = self.queue.lock();
                if let Some(pos) = queue.iter().position(matches) {
                    return Some(queue.remove(pos).unwrap());
                }
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return None;
            }
            let timeout = tokio::time::timeout_at(deadline, self.notify.notified()).await;
            if timeout.is_err() {
                return None;
            }
        }
    }
}

/// Resolve a parked dialog through the pre-cloned gate — never touches the
/// session lock (a blocking `alert()` holds the write lock until this
/// lands).
pub fn resolve_dialog(gate: &DialogGate, accept: bool, prompt_text: Option<String>) {
    *gate.lock() = Some(oxibrowser_core::js::DialogResult {
        accept,
        prompt_text,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wait_for_clamp_fits_bridge_budget() {
        // The clamp (25s) must stay inside the 30s world bridge budget —
        // crossing it would turn a busy wait into a bridge timeout.
        let hub = EventHub {
            queue: parking_lot::Mutex::new(VecDeque::new()),
            notify: tokio::sync::Notify::new(),
            dialog_pending: std::sync::atomic::AtomicBool::new(false),
            popups: tokio::sync::Mutex::new(HashMap::new()),
            next_popup: std::sync::atomic::AtomicU64::new(0),
        };
        let start = tokio::time::Instant::now();
        let none = hub.wait_for("popup", 1).await;
        assert!(none.is_none());
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
    }
}
