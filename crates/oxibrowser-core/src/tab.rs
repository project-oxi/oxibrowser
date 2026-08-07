//! Tab — agent-friendly interactive browsing session.
//!
//! A `Tab` wraps an inner `Session` behind an `Arc<Mutex<Session>>` so that:
//!
//! - All methods take `&self` (no `&mut self`) — callers never manage locks.
//! - `Tab` is `Clone` — multiple agents can share the same tab.
//! - Navigation methods return `BrowseResult` — no chaining needed.
//! - `click`/`type` are built-in — no JS assembly by the consumer.
//!
//! Created via `Browser::new_tab()`.
//!
use core::fmt;

/// Wait until this condition is satisfied.
///
/// Each variant has its own resolution semantics — see the variant doc.
/// Used with [`Tab::wait_for_condition`] and [`Tab::click_and_stabilize`].
///
/// `Visible(selector)` is the most common case: wait for a CSS selector
/// to match at least one element in the current page's DOM. The legacy
/// [`Tab::wait_for`] is a thin wrapper around this variant that emits the
/// same `BrowserEvent::WaitingForSelector` telemetry.
///
/// `NetworkIdle` waits for the Session's in-flight HTTP request counter
/// to reach zero and stay at zero for [`IdleOptions::quiet_window_ms`].
/// The counter tracks navigates (`goto`, `back`, `forward`, `reload`,
/// `post`), sub-resource loads, AND JS-issued fetches via the background
/// `handle_fetch_requests` thread — so a click that triggers an XHR
/// round-trip genuinely blocks until the XHR returns.
///
/// `DomContentLoaded` and `Load` resolve immediately for the current
/// `goto` cycle because the synchronous HTTP fetch + DOM parse in
/// `Session::navigate` already represents the document-parsed state by
/// the time `Tab::goto` returns. They're useful in scripts that want to
/// express intent (`wait_for_condition(DomContentLoaded, ...)`) without
/// a behavioral difference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitCondition {
    /// A CSS selector matches at least one element in the current DOM.
    Visible(String),
    /// In-flight HTTP request counter has been zero for the quiet window.
    NetworkIdle,
    /// `DOMContentLoaded` boundary has been crossed for the current page.
    DomContentLoaded,
    /// `load` boundary has been crossed for the current page.
    Load,
}

impl WaitCondition {
    /// Human-readable description for telemetry / error messages.
    fn describe(&self) -> String {
        match self {
            Self::Visible(s) => format!("visible:{s}"),
            Self::NetworkIdle => "networkidle".into(),
            Self::DomContentLoaded => "domcontentloaded".into(),
            Self::Load => "load".into(),
        }
    }
}

impl fmt::Display for WaitCondition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}

/// Tunable parameters for [`Tab::wait_for_condition`].
///
/// Defaults: 50ms poll interval, 500ms quiet window for `NetworkIdle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitOptions {
    /// How long to wait between condition checks. Default 50ms.
    pub poll_interval_ms: u64,
    /// For `NetworkIdle`: how long the in-flight counter must remain at
    /// zero before the condition resolves. Default 500ms. Ignored by
    /// other variants.
    pub quiet_window_ms: u64,
}

impl Default for WaitOptions {
    fn default() -> Self {
        Self {
            poll_interval_ms: 50,
            quiet_window_ms: 500,
        }
    }
}

use crate::browse_result::BrowseResult;
use crate::error::{CoreError, Result};
use crate::event::BrowserEvent;
use crate::js;
use crate::session::Session;
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{Mutex, broadcast};
use uuid::Uuid;

/// Clone-able, `&self`-only interactive tab for agent use.
///
/// Internally owns an `Arc<Mutex<Session>>`, hiding lock management
/// from the consumer. Created by `Browser::new_tab()`.
pub struct Tab {
    inner: Arc<Mutex<Session>>,
    /// Optional browser tab counter to decrement on close.
    tab_count: Option<Arc<AtomicUsize>>,
    /// Optional event sink to the parent `Browser`'s observer stream.
    ///
    /// When `Some`, navigation/wait/screenshot methods emit `BrowserEvent`s
    /// that observers (e.g. oxi-agent) can subscribe to. When `None` — in
    /// tests or in `Session`-only construction paths — events are silently
    /// dropped.
    event_tx: Option<broadcast::Sender<BrowserEvent>>,
    /// Unique ID of this tab, propagated to every `BrowserEvent` it emits.
    ///
    /// Stable for the lifetime of the tab and shared across `Tab::clone`
    /// (it's `Copy` + `Clone`). `Uuid::nil()` for tabs built via `Tab::new`
    /// (tests) so that any misrouted event is obvious in logs.
    tab_id: Uuid,
}

impl Clone for Tab {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            tab_count: self.tab_count.clone(),
            event_tx: self.event_tx.clone(),
            tab_id: self.tab_id,
        }
    }
}

impl Tab {
    /// Create a new Tab wrapping an existing Session.
    /// Used in tests where no browser tab_count tracking or event streaming is needed.
    #[allow(dead_code)]
    pub(crate) fn new(session: Session) -> Self {
        Self {
            inner: Arc::new(Mutex::new(session)),
            tab_count: None,
            event_tx: None,
            tab_id: Uuid::nil(),
        }
    }

    /// Create a Tab wired to a parent `Browser`'s tab counter and event stream.
    pub(crate) fn new_with_cleanup_and_events(
        session: Session,
        tab_count: Arc<AtomicUsize>,
        event_tx: broadcast::Sender<BrowserEvent>,
        tab_id: Uuid,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(session)),
            tab_count: Some(tab_count),
            event_tx: Some(event_tx),
            tab_id,
        }
    }

    /// Return this tab's unique ID.
    pub fn tab_id(&self) -> Uuid {
        self.tab_id
    }

    /// Emit a `BrowserEvent` if the parent `Browser` wired us up.
    ///
    /// Silently does nothing when the event sink is `None` (e.g. in tests
    /// that build a Tab directly from a Session). On a full observer queue
    /// the event is dropped — observability must never block the hot path.
    fn emit(&self, event: BrowserEvent) {
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(event);
        }
    }

    /// Count `<script>` blocks referenced by the current page, if loaded.
    ///
    /// Returns 0 when no page is loaded or the DOM has no script resources.
    /// Used for the `js_script_count` field of `BrowserEvent::DocumentReady`.
    fn count_scripts(session: &Session) -> usize {
        match session.page() {
            Some(page) => page
                .root_frame()
                .extract_resource_urls()
                .into_iter()
                .filter(|r| matches!(r.kind, oxibrowser_webapi::dom::ResourceKind::Script))
                .count(),
            None => 0,
        }
    }

    // -----------------------------------------------------------------------
    // Navigation — all return BrowseResult
    // -----------------------------------------------------------------------

    /// Navigate to a URL.
    pub async fn goto(&self, url: &str) -> Result<BrowseResult> {
        let started = std::time::Instant::now();
        self.emit(BrowserEvent::NavigationStarted {
            tab_id: self.tab_id,
            url: url.to_string(),
        });

        let mut session = self.inner.lock().await;
        session.navigate(url).await?;
        let result = Self::extract_result(&session);

        self.emit(BrowserEvent::DocumentReady {
            tab_id: self.tab_id,
            final_url: result.url.clone(),
            title: result.title.clone(),
            status: result.status,
            total_bytes: result.html.len() as u64,
            js_script_count: Self::count_scripts(&session),
            total_duration: started.elapsed(),
        });

        Ok(result)
    }

    /// Go back in history.
    pub async fn back(&self) -> Result<BrowseResult> {
        let mut session = self.inner.lock().await;
        session.go_back().await?;
        Ok(Self::extract_result(&session))
    }

    /// Go forward in history.
    pub async fn forward(&self) -> Result<BrowseResult> {
        let mut session = self.inner.lock().await;
        session.go_forward().await?;
        Ok(Self::extract_result(&session))
    }

    /// Reload the current page.
    pub async fn reload(&self) -> Result<BrowseResult> {
        let mut session = self.inner.lock().await;
        session.reload().await?;
        Ok(Self::extract_result(&session))
    }

    /// POST to a URL and load the response as a page.
    pub async fn post(&self, url: &str, body: &str, content_type: &str) -> Result<BrowseResult> {
        let mut session = self.inner.lock().await;
        session.post(url, body, content_type).await?;
        Ok(Self::extract_result(&session))
    }

    // -----------------------------------------------------------------------
    // Interaction — built on js/input.rs generators
    // -----------------------------------------------------------------------

    /// Click an element matching a CSS selector.
    ///
    /// Dispatches a `click` MouseEvent on the first matching element.
    /// The element's bounding rect is used for coordinates.
    pub async fn click(&self, selector: &str) -> Result<()> {
        let mut session = self.inner.lock().await;

        let sel_json = serde_json::to_string(selector).unwrap_or_default();
        let click_js = format!(
            r#"(function() {{
                var el = document.querySelector({sel_json});
                if (!el) return null;
                var rect = el.getBoundingClientRect
                    ? el.getBoundingClientRect()
                    : {{ left: 0, top: 0, width: 0, height: 0 }};
                var x = rect.left + rect.width / 2;
                var y = rect.top + rect.height / 2;
                el.dispatchEvent(new MouseEvent('click', {{
                    bubbles: true,
                    cancelable: true,
                    clientX: x,
                    clientY: y,
                    button: 0
                }}));
                return el.tagName;
            }})()"#,
        );

        let result = session.evaluate_js(&click_js).await?;
        if result.value.as_ref().is_none_or(|v| v.is_null()) {
            return Err(CoreError::DomError(format!(
                "click: no element matching '{}'",
                selector
            )));
        }
        Ok(())
    }

    /// Type text into an element matching a CSS selector.
    ///
    /// Focuses the element first, then inserts text via `Input.insertText`.
    pub async fn r#type(&self, selector: &str, text: &str) -> Result<()> {
        let mut session = self.inner.lock().await;

        // Focus the target element
        let sel_json = serde_json::to_string(selector).unwrap_or_default();
        let focus_js = format!(
            r#"(function() {{
                var el = document.querySelector({sel_json});
                if (el) {{ el.focus(); return el.tagName; }}
                return null;
            }})()"#,
        );
        let result = session.evaluate_js(&focus_js).await?;
        if result.value.as_ref().is_none_or(|v| v.is_null()) {
            return Err(CoreError::DomError(format!(
                "type: no element matching '{}'",
                selector
            )));
        }

        // Insert text using the input generator
        let insert_js = js::input::js_insert_text(text);
        session.evaluate_js(&insert_js).await?;
        Ok(())
    }

    /// Press a key (dispatches keyDown + keyUp events).
    ///
    /// `key` is a key name like "Enter", "Tab", "Escape", "ArrowDown", etc.
    pub async fn press_key(&self, key: &str) -> Result<()> {
        let mut session = self.inner.lock().await;

        let code = key_to_code(key);
        let down_js =
            js::input::js_dispatch_key_event(key, &code, "keyDown", 0, timestamp_millis());
        session.evaluate_js(&down_js).await?;

        let up_js = js::input::js_dispatch_key_event(key, &code, "keyUp", 0, timestamp_millis());
        session.evaluate_js(&up_js).await?;
        Ok(())
    }

    /// Press a key combo (e.g., "Ctrl+C", "Shift+Tab").
    pub async fn press(&self, combo: &str) -> Result<()> {
        let (key, code, modifiers) = js::mouse::parse_key_combo(combo);
        if key.is_empty() {
            return Err(CoreError::DomError("press: empty key".to_string()));
        }
        let down_js =
            js::input::js_dispatch_key_event(&key, &code, "keyDown", modifiers, timestamp_millis());
        self.eval_js_checked(down_js).await?;
        let up_js =
            js::input::js_dispatch_key_event(&key, &code, "keyUp", modifiers, timestamp_millis());
        self.eval_js_checked(up_js).await?;
        Ok(())
    }

    /// Dispatch a keyDown event (supports modifiers).
    pub async fn key_down(&self, combo: &str) -> Result<()> {
        let (key, code, modifiers) = js::mouse::parse_key_combo(combo);
        if key.is_empty() {
            return Err(CoreError::DomError("key_down: empty key".to_string()));
        }
        let down_js =
            js::input::js_dispatch_key_event(&key, &code, "keyDown", modifiers, timestamp_millis());
        self.eval_js_checked(down_js).await?;
        Ok(())
    }

    /// Dispatch a keyUp event (supports modifiers).
    pub async fn key_up(&self, combo: &str) -> Result<()> {
        let (key, code, modifiers) = js::mouse::parse_key_combo(combo);
        if key.is_empty() {
            return Err(CoreError::DomError("key_up: empty key".to_string()));
        }
        let up_js =
            js::input::js_dispatch_key_event(&key, &code, "keyUp", modifiers, timestamp_millis());
        self.eval_js_checked(up_js).await?;
        Ok(())
    }

    /// Click at viewport coordinates.
    pub async fn click_at(&self, x: f64, y: f64) -> Result<()> {
        let down_js = js::input::js_dispatch_mouse_event(x, y, "mousedown", "left", 1);
        self.eval_js_checked(down_js).await?;
        let up_js = js::input::js_dispatch_mouse_event(x, y, "mouseup", "left", 1);
        self.eval_js_checked(up_js).await?;
        let click_js = js::input::js_dispatch_mouse_event(x, y, "click", "left", 1);
        self.eval_dom_action(click_js, format!("click_at: no element at ({x}, {y})"))
            .await?;
        Ok(())
    }

    /// Double-click an element matching a CSS selector.
    pub async fn double_click(&self, selector: &str) -> Result<()> {
        let js = js::mouse::js_double_click(selector);
        self.eval_dom_action(
            js,
            format!("double_click: no element matching '{selector}'"),
        )
        .await?;
        Ok(())
    }

    /// Right-click an element matching a CSS selector.
    pub async fn right_click(&self, selector: &str) -> Result<()> {
        let js = js::mouse::js_right_click(selector);
        self.eval_dom_action(js, format!("right_click: no element matching '{selector}'"))
            .await?;
        Ok(())
    }

    /// Hover over an element matching a CSS selector.
    pub async fn hover(&self, selector: &str) -> Result<()> {
        let js = js::mouse::js_hover(selector);
        self.eval_dom_action(js, format!("hover: no element matching '{selector}'"))
            .await?;
        Ok(())
    }

    /// Move mouse to viewport coordinates.
    pub async fn move_mouse(&self, x: f64, y: f64) -> Result<()> {
        let js = js::mouse::js_move_mouse(x, y);
        self.eval_dom_action(js, format!("move_mouse: no element at ({x}, {y})"))
            .await?;
        Ok(())
    }

    /// Scroll by (delta_x, delta_y) pixels.
    pub async fn scroll(&self, delta_x: f64, delta_y: f64) -> Result<()> {
        let js = js::mouse::js_scroll(delta_x, delta_y);
        self.eval_dom_action(js, "scroll failed".to_string())
            .await?;
        Ok(())
    }

    /// Scroll the first matching element into view.
    pub async fn scroll_into_view(&self, selector: &str, center: bool) -> Result<()> {
        let js = js::mouse::js_scroll_into_view(selector, center);
        self.eval_dom_action(
            js,
            format!("scroll_into_view: no element matching '{selector}'"),
        )
        .await?;
        Ok(())
    }

    /// Drag from one selector to another.
    pub async fn drag(&self, from_selector: &str, to_selector: &str) -> Result<()> {
        let js = js::mouse::js_drag(from_selector, to_selector);
        self.eval_dom_action(
            js,
            format!(
                "drag: no element matching '{}' or '{}'",
                from_selector, to_selector
            ),
        )
        .await?;
        Ok(())
    }

    /// Fill an input/textarea or contentEditable with a value.
    pub async fn fill(&self, selector: &str, value: &str) -> Result<()> {
        let js = js::form::js_fill(selector, value);
        self.eval_dom_action(js, format!("fill: no element matching '{selector}'"))
            .await?;
        Ok(())
    }

    /// Select an option by value or text.
    pub async fn select_option(&self, selector: &str, value: &str) -> Result<()> {
        let js = js::form::js_select_option(selector, value);
        self.eval_dom_action(
            js,
            format!("select_option: no element matching '{selector}'"),
        )
        .await?;
        Ok(())
    }

    /// Check a checkbox or radio input.
    pub async fn check(&self, selector: &str) -> Result<()> {
        let js = js::form::js_check(selector, true);
        self.eval_dom_action(js, format!("check: no element matching '{selector}'"))
            .await?;
        Ok(())
    }

    /// Uncheck a checkbox or radio input.
    pub async fn uncheck(&self, selector: &str) -> Result<()> {
        let js = js::form::js_check(selector, false);
        self.eval_dom_action(js, format!("uncheck: no element matching '{selector}'"))
            .await?;
        Ok(())
    }

    /// Upload a file (synthetic) to an <input type="file"> element.
    pub async fn upload_file(&self, selector: &str, file_path: &str) -> Result<()> {
        let js = js::form::js_upload_file(selector, file_path);
        self.eval_dom_action(js, format!("upload_file: no element matching '{selector}'"))
            .await?;
        Ok(())
    }

    /// Clear an input or textarea value.
    pub async fn clear_input(&self, selector: &str) -> Result<()> {
        let js = js::form::js_clear(selector);
        self.eval_dom_action(js, format!("clear_input: no element matching '{selector}'"))
            .await?;
        Ok(())
    }

    /// Get the current value/textContent for the first matching element.
    pub async fn get_value(&self, selector: &str) -> Result<String> {
        let js = js::form::js_get_value(selector);
        let value = self
            .eval_dom_action(js, format!("get_value: no element matching '{selector}'"))
            .await?;
        match value {
            Value::String(s) => Ok(s),
            Value::Null => Ok(String::new()),
            other => Ok(other.to_string()),
        }
    }

    /// Get an attribute value from the first matching element.
    pub async fn query_attr(&self, selector: &str, attr: &str) -> Result<Option<String>> {
        let sel_json = serde_json::to_string(selector).unwrap_or_default();
        let attr_json = serde_json::to_string(attr).unwrap_or_default();
        let js = format!(
            r#"(function() {{
                var el = document.querySelector({sel_json});
                if (!el) return {{ found: false }};
                return {{ found: true, value: el.getAttribute({attr_json}) }};
            }})()"#,
        );
        let value = self.eval_js_checked(js).await?;
        if let Value::Object(map) = value
            && map.get("found").and_then(|v| v.as_bool()) == Some(true)
        {
            let attr_val = map
                .get("value")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            return Ok(attr_val);
        }
        Err(CoreError::DomError(format!(
            "query_attr: no element matching '{selector}'"
        )))
    }

    // -----------------------------------------------------------------------
    // Content extraction
    // -----------------------------------------------------------------------

    /// Get the current page content as a `BrowseResult`.
    ///
    /// Does not navigate — just extracts from the currently loaded page.
    pub async fn content(&self) -> Result<BrowseResult> {
        let session = self.inner.lock().await;
        Ok(Self::extract_result(&session))
    }

    /// Get text content of all elements matching a CSS selector.
    pub async fn query_all(&self, selector: &str) -> Result<Vec<String>> {
        let mut session = self.inner.lock().await;

        let sel_json = serde_json::to_string(selector).unwrap_or_default();
        let js = format!(
            r#"(function() {{
                var els = document.querySelectorAll({sel_json});
                return Array.from(els).map(function(el) {{ return el.textContent; }});
            }})()"#,
        );

        let result = session.evaluate_js(&js).await?;
        Ok(parse_js_string_array(result.value.as_ref()))
    }

    /// Evaluate JavaScript (does not await Promises).
    pub async fn evaluate(&self, expression: &str) -> Result<Value> {
        let mut session = self.inner.lock().await;
        let result = session.evaluate_js(expression).await?;
        match result.exception {
            Some(e) => Err(CoreError::JsError(e)),
            None => Ok(result.value.unwrap_or(Value::Null)),
        }
    }

    /// Evaluate JavaScript, awaiting Promise resolution.
    pub async fn evaluate_await(&self, expression: &str) -> Result<Value> {
        let mut session = self.inner.lock().await;
        let result = session.evaluate_js_with_await(expression, true).await?;
        match result.exception {
            Some(e) => Err(CoreError::JsError(e)),
            None => Ok(result.value.unwrap_or(Value::Null)),
        }
    }

    // -----------------------------------------------------------------------
    // Waiting
    // -----------------------------------------------------------------------

    /// Wait until a CSS selector matches at least one element.
    ///
    /// Polls every 50ms. Returns error on timeout.
    pub async fn wait_for(&self, selector: &str, timeout_ms: u64) -> Result<()> {
        self.emit(BrowserEvent::WaitingForSelector {
            tab_id: self.tab_id,
            selector: selector.to_string(),
            timeout_ms,
        });

        let start = std::time::Instant::now();
        let deadline = start + std::time::Duration::from_millis(timeout_ms);

        loop {
            {
                let session = self.inner.lock().await;
                if let Some(page) = session.page()
                    && page.root_frame().query_selector(selector).is_some()
                {
                    return Ok(());
                }
            }
            // release the lock before sleeping
            if std::time::Instant::now() >= deadline {
                return Err(CoreError::Timeout(format!(
                    "wait_for('{}') timed out after {}ms",
                    selector, timeout_ms
                )));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
    /// Wait until `cond` is satisfied.
    ///
    /// Polls every `options.poll_interval_ms` (default 50ms). Returns
    /// `Err(CoreError::Timeout)` if `timeout_ms` elapses first. For
    /// `NetworkIdle`, the in-flight counter must be zero AND stay at zero
    /// for `options.quiet_window_ms` (default 500ms).
    ///
    /// Note: `Visible(...)` does NOT emit `BrowserEvent::WaitingForSelector`
    /// — that's the legacy `wait_for` telemetry. Use [`Tab::wait_for`] when
    /// you specifically want the existing event-stream contract; reach for
    /// `wait_for_condition(Visible(...), ...)` when you want the richer
    /// condition API and don't depend on event observers.
    pub async fn wait_for_condition(&self, cond: WaitCondition, timeout_ms: u64) -> Result<()> {
        self.wait_for_condition_with(cond, timeout_ms, WaitOptions::default())
            .await
    }

    /// Same as [`Tab::wait_for_condition`] with explicit [`WaitOptions`].
    ///
    /// Lets callers tune the poll interval and (for `NetworkIdle`) the
    /// quiet window without re-spelling the condition every call.
    pub async fn wait_for_condition_with(
        &self,
        cond: WaitCondition,
        timeout_ms: u64,
        options: WaitOptions,
    ) -> Result<()> {
        let poll = std::time::Duration::from_millis(options.poll_interval_ms.max(1));
        let quiet = std::time::Duration::from_millis(options.quiet_window_ms);
        let start = std::time::Instant::now();
        let deadline = start + std::time::Duration::from_millis(timeout_ms);

        // For NetworkIdle, track when the counter first observed zero so we
        // can require it to STAY at zero for `quiet_window_ms`. This matches
        // the Playwright/Puppeteer semantic: "no requests for N ms".
        let mut idle_since: Option<std::time::Instant> = None;

        tracing::debug!(condition = %cond, timeout_ms, "wait_for_condition started");

        loop {
            // Check the condition under the session lock. We release the lock
            // before sleeping so other tabs/observers don't see the tab as
            // stalled while a wait is in flight.
            let (satisfied, snapshot_in_flight) = {
                let session = self.inner.lock().await;
                match &cond {
                    WaitCondition::Visible(selector) => {
                        let ok = session
                            .page()
                            .and_then(|p| p.root_frame().query_selector(selector))
                            .is_some();
                        (ok, 0)
                    }
                    WaitCondition::NetworkIdle => {
                        // Snapshot the counter under the lock to get a
                        // consistent read; we don't hold the lock across
                        // `tokio::time::sleep`.
                        let n = session.in_flight_requests();
                        (n == 0, n)
                    }
                    WaitCondition::DomContentLoaded | WaitCondition::Load => {
                        // Document parse completes before Tab::goto returns,
                        // so by the time anyone calls wait_for_condition the
                        // page is already past these boundaries. If a page
                        // isn't loaded yet (no active page), treat as not
                        // satisfied and wait — the caller is racing goto.
                        (session.page().is_some(), 0)
                    }
                }
            };

            if satisfied {
                if let WaitCondition::NetworkIdle = cond {
                    match idle_since {
                        None => {
                            idle_since = Some(std::time::Instant::now());
                            // Don't return yet — require the quiet window.
                        }
                        Some(t) if t.elapsed() >= quiet => {
                            tracing::debug!(
                                condition = %cond,
                                quiet_window_ms = options.quiet_window_ms,
                                "wait_for_condition resolved"
                            );
                            return Ok(());
                        }
                        Some(_) => {
                            // In the quiet window but not yet expired. Fall
                            // through to the sleep + re-check below.
                        }
                    }
                } else {
                    tracing::debug!(condition = %cond, "wait_for_condition resolved");
                    return Ok(());
                }
            } else if matches!(cond, WaitCondition::NetworkIdle) {
                // A new request came in while we were waiting — reset the
                // quiet window. Otherwise we could "remember" an idle streak
                // from before a fetch started.
                idle_since = None;
            }

            if std::time::Instant::now() >= deadline {
                return Err(CoreError::Timeout(format!(
                    "wait_for_condition({cond}) timed out after {timeout_ms}ms \
                     (in_flight={snapshot_in_flight})"
                )));
            }

            tokio::time::sleep(poll).await;
        }
    }

    /// Click an element matching `selector`, then wait for the page to
    /// settle — i.e. wait until [`WaitCondition::NetworkIdle`] (with a
    /// default 5s settle timeout) so XHRs triggered by the click have
    /// completed before the next automation step runs.
    ///
    /// This is the fix for "clicked before the element rendered": instead
    /// of issuing a click and immediately querying the DOM, the caller
    /// waits for the resulting network activity to drain.
    ///
    /// The default settle timeout is 5000ms; pass `settle_timeout_ms`
    /// to override. NetworkIdle uses a 500ms quiet window (the default
    /// in [`WaitOptions`]) — a post-click XHR that finishes within that
    /// window genuinely blocks; a slow request gets the full settle.
    pub async fn click_and_stabilize(&self, selector: &str) -> Result<()> {
        self.click_and_stabilize_with(selector, 5_000, WaitOptions::default())
            .await
    }

    /// Same as [`Tab::click_and_stabilize`] with explicit settle timeout
    /// and [`WaitOptions`] (e.g. custom quiet window).
    pub async fn click_and_stabilize_with(
        &self,
        selector: &str,
        settle_timeout_ms: u64,
        options: WaitOptions,
    ) -> Result<()> {
        // Issue the click first — this enqueues any JS handler to run on
        // the JS runtime thread (which then enqueues fetches via the mpsc
        // channel into handle_fetch_requests).
        self.click(selector).await?;

        // Post-click settle: the click dispatched a MouseEvent that returns
        // synchronously to us, but the JS click handler that issues fetch/XHR
        // may not have started yet — there's a race between click() returning
        // here and the JS runtime thread actually queuing the fetch message.
        // Sleeping one poll tick before requiring NetworkIdle gives the JS
        // handler time to run and the mpsc channel time to deliver the fetch
        // request to the background thread (which increments the counter).
        tokio::time::sleep(std::time::Duration::from_millis(
            options.poll_interval_ms.max(1),
        ))
        .await;

        // Now wait for NetworkIdle. We pass `settle_timeout_ms` as the upper
        // bound; the quiet window is controlled by `options.quiet_window_ms`.
        self.wait_for_condition_with(WaitCondition::NetworkIdle, settle_timeout_ms, options)
            .await
    }

    // -----------------------------------------------------------------------
    // Sub-resources
    // -----------------------------------------------------------------------

    /// Load sub-resources (JS, CSS, images) referenced by the current page.
    ///
    /// Returns the number of resources successfully loaded.
    pub async fn load_resources(&self) -> Result<usize> {
        let mut session = self.inner.lock().await;
        Ok(session.load_sub_resources().await)
    }

    // -----------------------------------------------------------------------
    // Screenshot
    // -----------------------------------------------------------------------

    /// Render the current page as a PNG screenshot (text-based bitmap font).
    pub async fn screenshot(&self, width: u32) -> Result<Vec<u8>> {
        let started = std::time::Instant::now();
        let session = self.inner.lock().await;
        let png = match session.page() {
            Some(page) => page.to_screenshot_png_live(width)?,
            None => return Err(CoreError::PageNotLoaded),
        };

        self.emit(BrowserEvent::ScreenshotCaptured {
            tab_id: self.tab_id,
            bytes: png.len(),
            viewport_width: width,
            duration: started.elapsed(),
        });
        Ok(png)
    }

    // -----------------------------------------------------------------------
    // Lifecycle
    // -----------------------------------------------------------------------

    /// Close this tab.
    pub async fn close(&self) -> Result<()> {
        let mut session = self.inner.lock().await;
        let result = session.close().await;
        if result.is_ok()
            && let Some(ref counter) = self.tab_count
        {
            counter.fetch_sub(1, Ordering::Relaxed);
        }
        result
    }

    /// Whether this tab has been closed.
    pub fn is_closed(&self) -> bool {
        // Non-blocking check: try_lock succeeds ⇒ check is_closed
        match self.inner.try_lock() {
            Ok(session) => session.is_closed(),
            Err(_) => false, // locked ⇒ still alive
        }
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Evaluate JS and surface exceptions as CoreError::JsError.
    async fn eval_js_checked(&self, js_code: String) -> Result<Value> {
        let mut session = self.inner.lock().await;
        let result = session.evaluate_js(&js_code).await?;
        if let Some(exception) = result.exception {
            return Err(CoreError::JsError(exception));
        }
        Ok(result.value.unwrap_or(Value::Null))
    }

    /// Evaluate JS and ensure the result is non-null (DOM element found).
    async fn eval_dom_action(&self, js_code: String, error: String) -> Result<Value> {
        let value = self.eval_js_checked(js_code).await?;
        if value.is_null() {
            return Err(CoreError::DomError(error));
        }
        Ok(value)
    }

    /// Extract BrowseResult from a Session's current page.
    fn extract_result(session: &Session) -> BrowseResult {
        match session.page() {
            Some(page) => BrowseResult::from_page(page),
            None => BrowseResult::empty(),
        }
    }

    /// Test-only: clone the in-flight request counter handle so tests
    /// can simulate request starts/completions without a real HTTP
    /// round-trip. Wrapped in `cfg(test)` so it's invisible to consumers.
    #[cfg(test)]
    async fn in_flight_counter_for_test(&self) -> Arc<std::sync::atomic::AtomicU64> {
        let session = self.inner.lock().await;
        session.in_flight_counter_handle_for_test()
    }
}

// -----------------------------------------------------------------------
// Key name → code mapping (for press_key)

/// Map a human-readable key name to a DOM `KeyboardEvent.code` string.
fn key_to_code(key: &str) -> String {
    // Common special keys — single-char keys use "KeyX" pattern
    match key {
        "Enter" => "Enter".to_string(),
        "Tab" => "Tab".to_string(),
        "Escape" => "Escape".to_string(),
        "Backspace" => "Backspace".to_string(),
        "Delete" => "Delete".to_string(),
        "ArrowUp" => "ArrowUp".to_string(),
        "ArrowDown" => "ArrowDown".to_string(),
        "ArrowLeft" => "ArrowLeft".to_string(),
        "ArrowRight" => "ArrowRight".to_string(),
        "Home" => "Home".to_string(),
        "End" => "End".to_string(),
        "PageUp" => "PageUp".to_string(),
        "PageDown" => "PageDown".to_string(),
        "Space" => "Space".to_string(),
        "Control" | "ControlLeft" => "ControlLeft".to_string(),
        "ControlRight" => "ControlRight".to_string(),
        "Shift" | "ShiftLeft" => "ShiftLeft".to_string(),
        "ShiftRight" => "ShiftRight".to_string(),
        "Alt" | "AltLeft" => "AltLeft".to_string(),
        "AltRight" => "AltRight".to_string(),
        "Meta" | "MetaLeft" => "MetaLeft".to_string(),
        "MetaRight" => "MetaRight".to_string(),
        "CapsLock" => "CapsLock".to_string(),
        "F1" => "F1".to_string(),
        "F2" => "F2".to_string(),
        "F3" => "F3".to_string(),
        "F4" => "F4".to_string(),
        "F5" => "F5".to_string(),
        "F6" => "F6".to_string(),
        "F7" => "F7".to_string(),
        "F8" => "F8".to_string(),
        "F9" => "F9".to_string(),
        "F10" => "F10".to_string(),
        "F11" => "F11".to_string(),
        "F12" => "F12".to_string(),
        c if c.len() == 1 && c.chars().next().is_some_and(|ch| ch.is_ascii_lowercase()) => {
            format!("Key{}", c.to_ascii_uppercase())
        }
        c if c.len() == 1 && c.chars().next().is_some_and(|ch| ch.is_ascii_digit()) => {
            format!("Digit{}", c)
        }
        _ => key.to_string(),
    }
}

/// Current timestamp in fractional milliseconds (for KeyboardEvent).
fn timestamp_millis() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1000.0
}

/// Parse a `serde_json::Value` (expected array of strings) into `Vec<String>`.
fn parse_js_string_array(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::BrowserId;
    use crate::config::BrowserConfig;
    use crate::network::HttpClient;
    use crate::network::cookie::CookieJar;
    use crate::page::Page;
    use parking_lot::RwLock;

    /// Helper: create a Tab with a session loaded with an HTML page.
    async fn tab_with_html(html: &str) -> Tab {
        let config = BrowserConfig::headless();
        let cookie_jar = Arc::new(RwLock::new(CookieJar::new()));
        let http_client = Arc::new(HttpClient::new(&config, cookie_jar.clone()).unwrap());
        let mut session = Session::new(BrowserId::next(), config, http_client, cookie_jar)
            .await
            .unwrap();

        // Load HTML directly via navigate-style logic
        let url = url::Url::parse("https://test.local/page").unwrap();
        let page = Page::from_html(url, html, 200, "text/html".to_string())
            .await
            .unwrap();
        session.inject_dom_snapshot_for_test(page);
        Tab::new(session)
    }

    #[tokio::test]
    async fn test_tab_content_extracts_browse_result() {
        let html = "<!DOCTYPE html><html><head><title>Test Title</title></head>\
                     <body><p>Hello World</p></body></html>";
        let tab = tab_with_html(html).await;

        let result = tab.content().await.unwrap();
        assert_eq!(result.url, "https://test.local/page");
        assert_eq!(result.title, "Test Title");
        assert_eq!(result.status, 200);
        assert!(result.markdown.contains("Hello World"));
        assert!(result.html.contains("<p>Hello World</p>"));
    }

    #[tokio::test]
    async fn test_tab_clone_shared_state() {
        let html = "<!DOCTYPE html><html><head><title>Shared</title></head>\
                     <body><p>Content</p></body></html>";
        let tab = tab_with_html(html).await;
        let tab2 = tab.clone();

        let r1 = tab.content().await.unwrap();
        let r2 = tab2.content().await.unwrap();
        assert_eq!(r1.title, r2.title);
        assert_eq!(r1.url, r2.url);
    }

    #[tokio::test]
    async fn test_tab_query_all() {
        let html = "<!DOCTYPE html><html><body>\
                     <ul>\
                       <li class=\"item\">First</li>\
                       <li class=\"item\">Second</li>\
                       <li class=\"item\">Third</li>\
                     </ul>\
                     </body></html>";
        let tab = tab_with_html(html).await;

        let items = tab.query_all(".item").await.unwrap();
        assert_eq!(
            items.len(),
            3,
            "should find 3 .item elements, got: {items:?}"
        );
        // textContent includes all child text
        assert!(
            items.iter().any(|t| t.contains("First")),
            "should contain First: {items:?}"
        );
        assert!(
            items.iter().any(|t| t.contains("Second")),
            "should contain Second: {items:?}"
        );
        assert!(
            items.iter().any(|t| t.contains("Third")),
            "should contain Third: {items:?}"
        );
    }

    #[tokio::test]
    async fn test_tab_query_all_no_match() {
        let html = "<!DOCTYPE html><html><body><p>Hello</p></body></html>";
        let tab = tab_with_html(html).await;

        let items = tab.query_all(".nonexistent").await.unwrap();
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn test_tab_evaluate_js() {
        let html = "<!DOCTYPE html><html><body><p>JS Test</p></body></html>";
        let tab = tab_with_html(html).await;

        let result = tab.evaluate("1 + 2").await.unwrap();
        assert_eq!(result, serde_json::json!(3));
    }

    #[tokio::test]
    async fn test_tab_evaluate_json_roundtrip() {
        let html = "<!DOCTYPE html><html><body></body></html>";
        let tab = tab_with_html(html).await;

        let result = tab
            .evaluate("JSON.stringify({key: 'value', num: 42})")
            .await
            .unwrap();
        // Result is a JSON string
        let parsed: serde_json::Value = serde_json::from_str(result.as_str().unwrap()).unwrap();
        assert_eq!(parsed["key"], "value");
        assert_eq!(parsed["num"], 42);
    }

    #[tokio::test]
    async fn test_tab_evaluate_js_error() {
        let html = "<!DOCTYPE html><html><body></body></html>";
        let tab = tab_with_html(html).await;

        let result = tab.evaluate("throw new Error('boom')").await;
        assert!(result.is_err());
        match result {
            Err(CoreError::JsError(msg)) => {
                assert!(msg.contains("boom"), "error should mention 'boom': {msg}");
            }
            Err(e) => panic!("wrong error type: {e:?}"),
            Ok(_) => panic!("should have failed"),
        }
    }

    #[tokio::test]
    async fn test_tab_screenshot() {
        let html = "<!DOCTYPE html><html><head><title>Shot</title></head>\
                     <body><p>Screenshot test</p></body></html>";
        let tab = tab_with_html(html).await;

        let png = tab.screenshot(800).await.unwrap();
        // PNG magic header
        assert!(png.len() > 8);
        assert_eq!(&png[0..4], &[0x89, 0x50, 0x4E, 0x47]);
    }

    #[tokio::test]
    async fn test_tab_close() {
        let html = "<!DOCTYPE html><html><body><p>Close me</p></body></html>";
        let tab = tab_with_html(html).await;
        assert!(!tab.is_closed());

        tab.close().await.unwrap();
        assert!(tab.is_closed());
    }

    #[tokio::test]
    async fn test_tab_close_twice_no_panic() {
        let html = "<!DOCTYPE html><html><body></body></html>";
        let tab = tab_with_html(html).await;

        tab.close().await.unwrap();
        tab.close().await.unwrap(); // Should not panic
        assert!(tab.is_closed());
    }

    #[tokio::test]
    async fn test_tab_without_event_sink_silently_drops() {
        // Tabs built via Tab::new() (test path) have no event_tx;
        // emit() should silently no-op.
        use crate::event::BrowserEvent;
        let html = "<!DOCTYPE html><html><body><p>Hi</p></body></html>";
        let tab = tab_with_html(html).await;
        // Should not panic.
        tab.emit(BrowserEvent::NavigationStarted {
            tab_id: Uuid::nil(),
            url: "https://test".into(),
        });
    }

    #[tokio::test]
    async fn test_tab_with_event_sink_emits_on_screenshot() {
        use crate::browser::Browser;
        use crate::config::BrowserConfig;
        use crate::event::BrowserEvent;

        let browser = Browser::new(BrowserConfig::headless()).await.unwrap();
        let mut rx = browser.subscribe_events();
        let tab = browser.new_tab().await.unwrap();

        // The Tab holds a clone of browser's event_tx. Emit through the Tab
        // should reach this subscriber.
        tab.emit(BrowserEvent::ScreenshotCaptured {
            tab_id: Uuid::nil(),
            bytes: 1024,
            viewport_width: 800,
            duration: std::time::Duration::from_millis(10),
        });

        let event = rx.try_recv().expect("subscriber should receive event");
        match event {
            BrowserEvent::ScreenshotCaptured {
                bytes,
                viewport_width,
                ..
            } => {
                assert_eq!(bytes, 1024);
                assert_eq!(viewport_width, 800);
            }
            other => panic!("expected ScreenshotCaptured, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_tab_id_is_stable_across_clones() {
        use crate::browser::Browser;
        use crate::config::BrowserConfig;

        let browser = Browser::new(BrowserConfig::headless()).await.unwrap();
        let tab = browser.new_tab().await.unwrap();
        let original_id = tab.tab_id();
        assert_ne!(
            original_id,
            Uuid::nil(),
            "tabs created via Browser::new_tab should have a real Uuid"
        );

        let clone_a = tab.clone();
        let clone_b = tab.clone();
        assert_eq!(clone_a.tab_id(), original_id, "clone a should share tab_id");
        assert_eq!(clone_b.tab_id(), original_id, "clone b should share tab_id");
        assert_eq!(clone_a.tab_id(), clone_b.tab_id());
    }

    #[test]
    fn test_key_to_code_special_keys() {
        assert_eq!(key_to_code("Enter"), "Enter");
        assert_eq!(key_to_code("Tab"), "Tab");
        assert_eq!(key_to_code("Escape"), "Escape");
        assert_eq!(key_to_code("ArrowDown"), "ArrowDown");
        assert_eq!(key_to_code("Space"), "Space");
        assert_eq!(key_to_code("F5"), "F5");
    }

    #[test]
    fn test_key_to_code_letters() {
        assert_eq!(key_to_code("a"), "KeyA");
        assert_eq!(key_to_code("z"), "KeyZ");
    }

    #[test]
    fn test_key_to_code_digits() {
        assert_eq!(key_to_code("0"), "Digit0");
        assert_eq!(key_to_code("9"), "Digit9");
    }

    #[test]
    fn test_key_to_code_modifiers() {
        assert_eq!(key_to_code("Control"), "ControlLeft");
        assert_eq!(key_to_code("Shift"), "ShiftLeft");
        assert_eq!(key_to_code("Alt"), "AltLeft");
        assert_eq!(key_to_code("Meta"), "MetaLeft");
        assert_eq!(key_to_code("ControlRight"), "ControlRight");
        assert_eq!(key_to_code("ShiftRight"), "ShiftRight");
    }

    #[test]
    fn test_parse_js_string_array() {
        let v = serde_json::json!(["hello", "world"]);
        let result = parse_js_string_array(Some(&v));
        assert_eq!(result, vec!["hello", "world"]);
    }

    #[test]
    fn test_parse_js_string_array_empty() {
        assert_eq!(parse_js_string_array(None), Vec::<String>::new());
        assert_eq!(
            parse_js_string_array(Some(&serde_json::json!(null))),
            Vec::<String>::new()
        );
        assert_eq!(
            parse_js_string_array(Some(&serde_json::json!([]))),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_parse_js_string_array_skips_non_strings() {
        let v = serde_json::json!(["ok", 42, true, "also ok"]);
        let result = parse_js_string_array(Some(&v));
        assert_eq!(result, vec!["ok", "also ok"]);
    }

    // -----------------------------------------------------------------------
    // WaitCondition / wait_for_condition / click_and_stabilize
    // -----------------------------------------------------------------------

    /// `Visible(...)` resolves immediately when the selector matches an
    /// element in the current DOM.
    #[tokio::test]
    async fn test_wait_for_condition_visible_resolves() {
        let html = "<!DOCTYPE html><html><body>\
                    <button id=\"go\">Go</button>\
                    </body></html>";
        let tab = tab_with_html(html).await;

        let started = std::time::Instant::now();
        tab.wait_for_condition(WaitCondition::Visible("#go".into()), 1_000)
            .await
            .expect("should resolve immediately");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "visible condition should resolve quickly, took {:?}",
            started.elapsed()
        );
    }

    /// `Visible(...)` returns `Err(CoreError::Timeout)` when the selector
    /// never matches.
    #[tokio::test]
    async fn test_wait_for_condition_visible_times_out() {
        let html = "<!DOCTYPE html><html><body><p>nothing here</p></body></html>";
        let tab = tab_with_html(html).await;

        let err = tab
            .wait_for_condition(WaitCondition::Visible(".missing".into()), 120)
            .await
            .expect_err("must time out");
        match err {
            CoreError::Timeout(msg) => {
                assert!(msg.contains("wait_for_condition"), "msg={msg}");
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    /// `DomContentLoaded` resolves as soon as a page is loaded — the
    /// document parse completes inside `Session::navigate`, so by the
    /// time a Tab exists with an active page the boundary has been crossed.
    #[tokio::test]
    async fn test_wait_for_condition_dom_content_loaded_resolves() {
        let html = "<!DOCTYPE html><html><body><p>hi</p></body></html>";
        let tab = tab_with_html(html).await;

        let started = std::time::Instant::now();
        tab.wait_for_condition(WaitCondition::DomContentLoaded, 500)
            .await
            .expect("DCL should resolve immediately for a loaded page");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(200),
            "DCL should resolve quickly, took {:?}",
            started.elapsed()
        );
    }

    /// `NetworkIdle` resolves after the in-flight counter is observed at
    /// zero for the configured quiet window. The test starts with the
    /// counter at 0 and asserts the wait honored the quiet window.
    #[tokio::test]
    async fn test_wait_for_condition_network_idle_resolves_when_quiet() {
        let html = "<!DOCTYPE html><html><body><p>quiet</p></body></html>";
        let tab = tab_with_html(html).await;

        let counter = tab.in_flight_counter_for_test().await;
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 0);

        let options = WaitOptions {
            poll_interval_ms: 20,
            quiet_window_ms: 100,
        };
        let started = std::time::Instant::now();
        tab.wait_for_condition_with(WaitCondition::NetworkIdle, 2_000, options)
            .await
            .expect("NetworkIdle should resolve when counter is already zero");
        let elapsed = started.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(100),
            "should wait at least the quiet window, took {elapsed:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(1_500),
            "should not wait much longer than the quiet window, took {elapsed:?}"
        );
    }

    /// `NetworkIdle` resets its quiet-window clock whenever the counter
    /// goes back up after being zero — i.e. a request started during the
    /// idle streak must restart the wait. This is the core correctness
    /// property that prevents the "clicked before element rendered" bug.
    #[tokio::test]
    async fn test_wait_for_condition_network_idle_resets_on_new_request() {
        let html = "<!DOCTYPE html><html><body><p>reset</p></body></html>";
        let tab = tab_with_html(html).await;
        let counter = tab.in_flight_counter_for_test().await;

        let options = WaitOptions {
            poll_interval_ms: 10,
            quiet_window_ms: 80,
        };

        // Mimic a click-triggered fetch: keep counter > 0 for ~120ms
        // (longer than the quiet window), then release. NetworkIdle
        // must NOT resolve during the burst.
        let burst = counter.clone();
        let burst_handle = tokio::spawn(async move {
            burst.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            burst.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        });

        let started = std::time::Instant::now();
        tab.wait_for_condition_with(WaitCondition::NetworkIdle, 2_000, options)
            .await
            .expect("NetworkIdle should resolve after the burst drains");
        let elapsed = started.elapsed();

        burst_handle.await.expect("burst task should complete");

        assert!(
            elapsed >= std::time::Duration::from_millis(120),
            "should not resolve during in-flight burst, took {elapsed:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "should resolve shortly after burst drains, took {elapsed:?}"
        );
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    /// `click_and_stabilize` clicks the element, then waits for
    /// NetworkIdle. With no JS handler issuing fetches, the counter
    /// stays at zero and the call returns after settle + quiet window.
    #[tokio::test]
    async fn test_click_and_stabilize_resolves() {
        let html = "<!DOCTYPE html><html><body>\
                    <a id=\"link\" href=\"#\">Click me</a>\
                    </body></html>";
        let tab = tab_with_html(html).await;

        tab.click_and_stabilize_with(
            "#link",
            2_000,
            WaitOptions {
                poll_interval_ms: 20,
                quiet_window_ms: 50,
            },
        )
        .await
        .expect("click_and_stabilize should resolve with no in-flight fetches");
    }

    /// `click_and_stabilize` errors with `DomError` when the selector
    /// doesn't match — same contract as `click()`, surfaced before the
    /// network-idle wait runs.
    #[tokio::test]
    async fn test_click_and_stabilize_selector_miss_errors() {
        let html = "<!DOCTYPE html><html><body><p>nothing</p></body></html>";
        let tab = tab_with_html(html).await;

        let err = tab
            .click_and_stabilize("#missing")
            .await
            .expect_err("must fail when selector misses");
        match err {
            CoreError::DomError(msg) => {
                assert!(msg.contains("click"), "msg={msg}");
            }
            other => panic!("expected DomError, got {other:?}"),
        }
    }

    /// `WaitCondition` round-trips through `Display` for telemetry.
    #[test]
    fn test_wait_condition_display() {
        assert_eq!(
            WaitCondition::Visible("#x".into()).to_string(),
            "visible:#x"
        );
        assert_eq!(WaitCondition::NetworkIdle.to_string(), "networkidle");
        assert_eq!(
            WaitCondition::DomContentLoaded.to_string(),
            "domcontentloaded"
        );
        assert_eq!(WaitCondition::Load.to_string(), "load");
    }

    /// `WaitOptions::default()` has the documented values.
    #[test]
    fn test_wait_options_default_values() {
        let opts = WaitOptions::default();
        assert_eq!(opts.poll_interval_ms, 50);
        assert_eq!(opts.quiet_window_ms, 500);
    }
}
