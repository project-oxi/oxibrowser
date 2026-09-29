//! Tab manager for the session REPL.
//!
//! Manages a collection of named tabs (t1, t2, ...) and maps them to
//! the underlying `Tab` handles from the browser.

use oxibrowser_core::{Browser, Tab};
use std::collections::HashMap;

/// Manages multiple named tabs within a session REPL.
pub struct TabManager {
    tabs: HashMap<String, Tab>,
    next_id: u32,
}

impl TabManager {
    /// Create a new empty tab manager.
    pub fn new() -> Self {
        Self {
            tabs: HashMap::new(),
            next_id: 1,
        }
    }

    /// Create a new tab and return its id.
    pub async fn create_tab(&mut self, browser: &Browser) -> Result<String, String> {
        let id = format!("t{}", self.next_id);
        self.next_id += 1;
        let tab = browser.new_tab().await.map_err(|e| format!("{e}"))?;
        self.tabs.insert(id.clone(), tab);
        Ok(id)
    }

    /// Create a tab inside a specific context (`session --account` primary).
    pub async fn create_tab_in(
        &mut self,
        browser: &Browser,
        ctx: &std::sync::Arc<oxibrowser_core::context::BrowserContext>,
    ) -> Result<String, String> {
        let id = format!("t{}", self.next_id);
        self.next_id += 1;
        let tab = browser.new_tab_in(ctx).await.map_err(|e| format!("{e}"))?;
        self.tabs.insert(id.clone(), tab);
        Ok(id)
    }

    /// Register an already-created tab (e.g. one created inside an account
    /// context) and return its id.
    pub fn insert_tab(&mut self, tab: Tab) -> String {
        let id = format!("t{}", self.next_id);
        self.next_id += 1;
        self.tabs.insert(id.clone(), tab);
        id
    }

    /// Get a reference to a tab by id.
    pub fn get(&self, tab_id: &str) -> Option<&Tab> {
        self.tabs.get(tab_id)
    }

    /// Close a tab and remove it from the manager.
    pub async fn close_tab(&mut self, tab_id: &str) -> Result<(), String> {
        if let Some(tab) = self.tabs.remove(tab_id) {
            tab.close().await.map_err(|e| format!("{e}"))?;
        }
        Ok(())
    }

    /// Close all tabs and clear the manager.
    pub async fn close_all(&mut self) {
        for (_, tab) in self.tabs.drain() {
            let _ = tab.close().await;
        }
    }

    /// List all tab ids.
    pub fn list(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.tabs.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Number of active tabs.
    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    /// Whether the manager has no tabs.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }
}

impl Default for TabManager {
    fn default() -> Self {
        Self::new()
    }
}
