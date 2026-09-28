//! CDP Target domain handler.
//!
//! Handles Target.setDiscoverTargets, Target.setAutoAttach,
//! Target.attachToTarget, Target.createTarget, Target.closeTarget.
//!
//! After setDiscoverTargets(true), emits Target.targetCreated for the
//! current target. After setAutoAttach(true), emits Target.attachedToTarget.

use crate::domains::{DispatchContext, DomainResult};
use crate::protocol::CdpError;
use serde_json::{Value, json};

/// Dispatch Target domain methods.
pub async fn handle(method: &str, params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    match method {
        "setDiscoverTargets" => set_discover_targets(params, ctx),
        "setAutoAttach" => set_auto_attach(params, ctx),
        "attachToTarget" => attach_to_target(params, ctx),
        "detachFromTarget" => detach_from_target(params, ctx).await,
        "createTarget" => create_target(params, ctx).await,
        "closeTarget" => close_target(params, ctx).await,
        "createBrowserContext" => create_browser_context(params, ctx),
        "disposeBrowserContext" => dispose_browser_context(params, ctx),
        "getTargets" => get_targets(ctx).await,
        "getTargetInfo" => get_target_info(params, ctx).await,
        _ => Err(CdpError {
            code: -32601,
            message: format!("Target.{} not implemented", method),
        }),
    }
}

/// Target.setDiscoverTargets — enables target discovery.
///
/// When enabled, emits Target.targetCreated for the current target
/// so that Puppeteer/Playwright can discover it.
fn set_discover_targets(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_default();
    let discover = params
        .get("discover")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    if discover {
        // Emit targetCreated for the default page target
        let context_id = ctx.browser.default_context().id().to_string();
        ctx.events.send_event(
            "Target.targetCreated",
            json!({
                "targetInfo": {
                    "targetId": "default",
                    "type": "page",
                    "title": "OxiBrowser",
                    "url": "about:blank",
                    "attached": false,
                    "canAccessOpener": false,
                    "browserContextId": context_id
                }
            }),
        );
    }

    Ok(Some(json!({})))
}

/// Target.setAutoAttach — enables auto-attaching to new targets.
///
/// When enabled, emits Target.attachedToTarget for the current session
/// so that Puppeteer/Playwright can begin interacting.
fn set_auto_attach(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_default();
    let auto_attach = params
        .get("autoAttach")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let _flatten = params
        .get("flatten")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    if auto_attach {
        let session_id = format!("session-{}", uuid::Uuid::new_v4().as_simple());
        let attached_session_id = session_id.clone();
        let context_id = ctx.browser.default_context().id().to_string();

        // Emit attachedToTarget for the default target
        ctx.events.send_event(
            "Target.attachedToTarget",
            json!({
                "sessionId": session_id,
                "targetInfo": {
                    "targetId": "default",
                    "type": "page",
                    "title": "OxiBrowser",
                    "url": "about:blank",
                    "attached": true,
                    "canAccessOpener": false,
                    "browserContextId": context_id
                },
                "waitingForDebugger": false
            }),
        );
        // Stamp subsequent target events with this sessionId (flat protocol).
        ctx.events.set_session_id(attached_session_id);
    }

    Ok(Some(json!({})))
}

/// Target.attachToTarget — attaches to a target.
fn attach_to_target(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_default();
    let _target_id = params
        .get("targetId")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let session_id = format!("session-{}", uuid::Uuid::new_v4().as_simple());
    // Subsequent commands arrive with this sessionId; stamp events with it.
    ctx.events.set_session_id(session_id.clone());
    Ok(Some(json!({ "sessionId": session_id })))
}

/// Target.createTarget — creates a new page target (a real Browser session).
///
/// The new session is registered under a fresh `sessionId` so flat-protocol
/// commands routed by that `sessionId` reach it. The optional
/// `browserContextId` parameter selects the browser context (cookie jar,
/// storage, egress) the target lives in; when absent the browser's default
/// context is used. Emits `Target.targetCreated` and
/// `Target.attachedToTarget` carrying the real context id.
///
/// Child-target lifecycle events (load, etc.) currently do not flow (each child
/// needs its own CoreEvent drainer); the command surface (navigate/evaluate/DOM)
/// works.
async fn create_target(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_default();
    let url = params
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("about:blank");

    // Resolve the target's browser context: explicit `browserContextId`,
    // else the browser's default (anonymous) context.
    let context = match params.get("browserContextId").and_then(|v| v.as_str()) {
        Some(id) => ctx
            .browser
            .context(&oxibrowser_core::ContextId::from_string(id))
            .ok_or_else(|| CdpError {
                code: -32000,
                message: "invalid browserContextId".to_string(),
            })?,
        None => ctx.browser.default_context(),
    };
    let context_id = context.id().to_string();

    let new_session = ctx
        .browser
        .new_session_in(&context)
        .await
        .map_err(|e| CdpError {
            code: -32000,
            message: format!("failed to create new session: {e}"),
        })?;

    let target_id = format!("TID-{}", uuid::Uuid::new_v4().as_simple());
    let session_id = format!("session-{}", uuid::Uuid::new_v4().as_simple());

    // Wire the child session's CoreEvent sink so its JS-thread events
    // (console, exceptions, fetch/WS lifecycle) flow to the client stamped
    // with this child's sessionId.
    let (core_tx, core_rx) = std::sync::mpsc::channel::<oxibrowser_core::js::CoreEvent>();
    {
        let mut s = new_session.write().await;
        s.set_event_sink(core_tx);
    }
    let child_events = ctx.events.clone();
    let child_sid = session_id.clone();
    let drain_handle = tokio::spawn(async move {
        loop {
            match core_rx.try_recv() {
                Ok(ev) => {
                    crate::core_event::emit_core_event_with_session(&child_events, ev, &child_sid)
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
            }
        }
    });

    // Register the child session so commands routed by sessionId reach it.
    ctx.child_targets
        .insert(
            session_id.clone(),
            crate::domains::TargetEntry {
                target_id: target_id.clone(),
                session: new_session.clone(),
                drain_abort: Some(drain_handle.abort_handle()),
                browser_context_id: context_id.clone(),
            },
        )
        .await;

    ctx.events.send_event(
        "Target.targetCreated",
        json!({
            "targetInfo": {
                "targetId": target_id,
                "type": "page",
                "title": "about:blank",
                "url": url,
                "attached": false,
                "canAccessOpener": false,
                "browserContextId": context_id
            }
        }),
    );
    ctx.events.send_event(
        "Target.attachedToTarget",
        json!({
            "sessionId": session_id,
            "targetInfo": {
                "targetId": target_id,
                "type": "page",
                "title": "about:blank",
                "url": url,
                "attached": true,
                "canAccessOpener": false,
                "browserContextId": context_id
            },
            "waitingForDebugger": false
        }),
    );

    Ok(Some(json!({
        "targetId": target_id
    })))
}

/// Target.closeTarget — closes a child target created via `Target.createTarget`.
///
/// Aborts the child's event drainer, closes the child session, removes it from
/// the registry, and emits `Target.detachedFromTarget` then
/// `Target.targetDestroyed`. Unknown `targetId` yields a `-32001` error.
async fn close_target(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_default();
    let target_id = params
        .get("targetId")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    let (session_id, entry) = ctx
        .child_targets
        .close_by_target(target_id)
        .await
        .ok_or_else(|| CdpError {
            code: -32001,
            message: "target not found".to_string(),
        })?;

    if let Some(handle) = entry.drain_abort.as_ref() {
        handle.abort();
    }
    entry
        .session
        .write()
        .await
        .close()
        .await
        .map_err(|e| CdpError {
            code: -32000,
            message: format!("failed to close target: {e}"),
        })?;

    ctx.events.send_event(
        "Target.detachedFromTarget",
        json!({ "sessionId": session_id }),
    );
    ctx.events
        .send_event("Target.targetDestroyed", json!({ "targetId": target_id }));

    Ok(Some(json!({ "targetId": target_id, "success": true })))
}

/// Target.createBrowserContext — creates a new isolated browser context.
///
/// Returns `{"browserContextId": "ctx-N"}`. Standard parameters:
/// `disposeOnDetach` and `proxyBypassList` are accepted but currently
/// ignored; `proxyServer` fixes the context's egress proxy. The extension
/// parameter `oxiAccount` is rejected with an honest error until account
/// binding lands (M-D).
fn create_browser_context(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_default();

    if params.get("oxiAccount").is_some() {
        return Err(CdpError {
            code: -32000,
            message: "account binding not yet implemented".to_string(),
        });
    }
    let _dispose_on_detach = params.get("disposeOnDetach").and_then(|v| v.as_bool());
    let proxy = params
        .get("proxyServer")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let context = ctx
        .browser
        .new_context(oxibrowser_core::ContextConfig { label: None, proxy })
        .map_err(|e| CdpError {
            code: -32000,
            message: format!("failed to create browser context: {e}"),
        })?;

    Ok(Some(json!({
        "browserContextId": context.id().to_string()
    })))
}

/// Target.disposeBrowserContext — disposes a previously created context.
///
/// The default context cannot be disposed; unknown ids yield `-32000`.
fn dispose_browser_context(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_default();
    let id = params
        .get("browserContextId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CdpError {
            code: -32602,
            message: "browserContextId required".to_string(),
        })?;

    ctx.browser
        .dispose_context(&oxibrowser_core::ContextId::from_string(id))
        .map_err(|e| CdpError {
            code: -32000,
            message: e.to_string(),
        })?;

    Ok(Some(json!({})))
}

/// Target.detachFromTarget — detaches from a child target session.
///
/// Aborts the child's event drainer and removes it from the registry; the
/// child session itself stays alive.
async fn detach_from_target(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_default();
    let session_id = params
        .get("sessionId")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    if let Some(entry) = ctx.child_targets.detach(session_id).await
        && let Some(handle) = entry.drain_abort.as_ref()
    {
        handle.abort();
    }

    Ok(Some(json!({})))
}

/// Target.getTargets — returns list of available targets.
///
/// Lists the root `default` target plus every registered child target,
/// each carrying its real browser context id.
async fn get_targets(ctx: &DispatchContext) -> DomainResult {
    let default_context_id = ctx.browser.default_context().id().to_string();
    let mut infos = vec![json!({
        "targetId": "default",
        "type": "page",
        "title": "OxiBrowser",
        "url": "about:blank",
        "attached": false,
        "canAccessOpener": false,
        "browserContextId": default_context_id
    })];
    for (_, entry) in ctx.child_targets.entries().await {
        infos.push(json!({
            "targetId": entry.target_id,
            "type": "page",
            "title": "about:blank",
            "url": "about:blank",
            "attached": true,
            "canAccessOpener": false,
            "browserContextId": entry.browser_context_id
        }));
    }
    Ok(Some(json!({ "targetInfos": infos })))
}

/// Target.getTargetInfo — returns info about a specific target.
///
/// Registered child targets report their real browser context id; the root
/// `default` target (and unknown ids, for compatibility) report the default
/// context's id.
async fn get_target_info(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_default();
    let target_id = params
        .get("targetId")
        .and_then(|v| v.as_str())
        .unwrap_or("default");

    let context_id = match ctx.child_targets.get(target_id).await {
        Some(entry) => entry.browser_context_id,
        None => ctx.browser.default_context().id().to_string(),
    };

    Ok(Some(json!({
        "targetInfo": {
            "targetId": target_id,
            "type": "page",
            "title": "OxiBrowser",
            "url": "about:blank",
            "attached": false,
            "canAccessOpener": false,
            "browserContextId": context_id
        }
    })))
}

#[cfg(test)]
mod tests {

    use oxibrowser_core::{Browser, BrowserConfig};
    use std::sync::Arc;

    /// Registry close/detach lifecycle against a real child session.
    #[tokio::test]
    async fn registry_close_and_detach_lifecycle() {
        let config = BrowserConfig::headless();
        let browser = Arc::new(Browser::new(config).await.unwrap());
        let session = browser.new_session().await.unwrap();

        let registry = crate::domains::TargetRegistry::new();
        registry
            .insert(
                "session-a".into(),
                crate::domains::TargetEntry {
                    target_id: "TID-a".into(),
                    session: session.clone(),
                    drain_abort: None,
                    browser_context_id: "ctx-1".into(),
                },
            )
            .await;
        registry
            .insert(
                "session-b".into(),
                crate::domains::TargetEntry {
                    target_id: "TID-b".into(),
                    session: session.clone(),
                    drain_abort: None,
                    browser_context_id: "ctx-1".into(),
                },
            )
            .await;

        // get() resolves by sessionId.
        let entry = registry.get("session-a").await.unwrap();
        assert_eq!(entry.target_id, "TID-a");
        assert!(registry.get("missing").await.is_none());

        // close_by_target resolves the sessionId and removes the entry.
        let (sid, entry) = registry.close_by_target("TID-b").await.unwrap();
        assert_eq!(sid, "session-b");
        assert_eq!(entry.target_id, "TID-b");
        assert!(registry.get("session-b").await.is_none());
        assert!(registry.close_by_target("TID-b").await.is_none());

        // Closing marks the child session closed.
        entry.session.write().await.close().await.unwrap();
        assert!(entry.session.read().await.is_closed());

        // detach removes only the requested session.
        let detached = registry.detach("session-a").await.unwrap();
        assert_eq!(detached.target_id, "TID-a");
        assert!(registry.detach("session-a").await.is_none());
        assert!(registry.entries().await.is_empty());
    }

    // ------------------------------------------------------------------
    // Browser contexts (M-A): Target.createBrowserContext / disposeBrowserContext
    // and createTarget(browserContextId).
    // ------------------------------------------------------------------

    use super::*;

    /// Build a DispatchContext backed by a real Browser session.
    async fn make_ctx() -> (DispatchContext, crate::event::EventReceiver) {
        let mut config = BrowserConfig::headless();
        config.enable_ssrf_filter = false;
        let browser = Arc::new(Browser::new(config).await.unwrap());
        let session = browser.new_session().await.unwrap();
        let (events, rx) = crate::event::event_channel();
        let ctx = DispatchContext {
            session,
            events,
            fetch_registry: oxibrowser_core::network::intercept::shared_registry(),
            dialog_gate: Arc::new(parking_lot::Mutex::new(None)),
            browser,
            child_targets: Arc::new(crate::domains::TargetRegistry::new()),
        };
        (ctx, rx)
    }

    /// DispatchContext variant whose `session` is a child target's session,
    /// so domain handlers (Network cookies, …) operate on that child.
    fn child_ctx(
        ctx: &DispatchContext,
        session: Arc<tokio::sync::RwLock<oxibrowser_core::session::Session>>,
    ) -> DispatchContext {
        DispatchContext {
            session,
            events: ctx.events.clone(),
            fetch_registry: ctx.fetch_registry.clone(),
            dialog_gate: ctx.dialog_gate.clone(),
            browser: ctx.browser.clone(),
            child_targets: ctx.child_targets.clone(),
        }
    }

    /// Look up the registry entry for a targetId.
    async fn entry_for(ctx: &DispatchContext, target_id: &str) -> crate::domains::TargetEntry {
        ctx.child_targets
            .entries()
            .await
            .into_iter()
            .find(|(_, e)| e.target_id == target_id)
            .map(|(_, e)| e)
            .unwrap_or_else(|| panic!("target {target_id} not registered"))
    }

    /// (a) createBrowserContext → createTarget(ctx id) → a cookie set in the
    /// child is invisible to a default-context target (real jar isolation),
    /// and both carry their real `browserContextId`.
    #[tokio::test]
    async fn create_browser_context_isolates_cookies() {
        let (ctx, mut rx) = make_ctx().await;
        let default_context_id = ctx.browser.default_context().id().to_string();

        // Create an isolated context.
        let resp = handle("createBrowserContext", Some(json!({})), &ctx)
            .await
            .unwrap()
            .unwrap();
        let context_id = resp["browserContextId"].as_str().unwrap().to_string();
        assert!(context_id.starts_with("ctx-"));
        assert_ne!(context_id, default_context_id);

        // Target inside the new context: real context id everywhere.
        let resp = handle(
            "createTarget",
            Some(json!({ "browserContextId": context_id })),
            &ctx,
        )
        .await
        .unwrap()
        .unwrap();
        let isolated_target = resp["targetId"].as_str().unwrap().to_string();
        let isolated = entry_for(&ctx, &isolated_target).await;
        assert_eq!(isolated.browser_context_id, context_id);

        // First event is the targetCreated for that target, carrying the id.
        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ev.method, "Target.targetCreated");
        assert_eq!(
            ev.params.as_ref().unwrap()["targetInfo"]["browserContextId"],
            context_id.as_str()
        );

        // Target in the default context (no browserContextId parameter).
        let resp = handle("createTarget", Some(json!({})), &ctx)
            .await
            .unwrap()
            .unwrap();
        let default_target = resp["targetId"].as_str().unwrap().to_string();
        let default_entry = entry_for(&ctx, &default_target).await;
        assert_eq!(default_entry.browser_context_id, default_context_id);

        // Set a cookie inside the isolated context via the child session.
        let isolated_ctx = child_ctx(&ctx, isolated.session.clone());
        crate::domains::network::handle(
            "setCookie",
            Some(json!({ "name": "iso", "value": "1", "url": "https://example.com/" })),
            &isolated_ctx,
        )
        .await
        .unwrap();

        // The isolated context sees it…
        let resp = crate::domains::network::handle("getAllCookies", None, &isolated_ctx)
            .await
            .unwrap()
            .unwrap();
        let names: Vec<&str> = resp["cookies"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["name"].as_str())
            .collect();
        assert!(names.contains(&"iso"), "cookies: {names:?}");

        // …but a default-context child does not (jar isolation).
        let default_child_ctx = child_ctx(&ctx, default_entry.session.clone());
        let resp = crate::domains::network::handle("getAllCookies", None, &default_child_ctx)
            .await
            .unwrap()
            .unwrap();
        let names: Vec<&str> = resp["cookies"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert!(!names.contains(&"iso"), "cookies: {names:?}");
    }

    /// (b) createTarget with an unknown browserContextId fails with -32000.
    #[tokio::test]
    async fn create_target_unknown_context_errors() {
        let (ctx, _rx) = make_ctx().await;

        let err = handle(
            "createTarget",
            Some(json!({ "browserContextId": "ctx-999" })),
            &ctx,
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, -32000);
        assert!(err.message.contains("invalid browserContextId"));

        // Nothing was registered.
        assert!(ctx.child_targets.entries().await.is_empty());
    }

    /// (c) createBrowserContext with the oxiAccount extension fails honestly
    /// until account binding lands (M-D).
    #[tokio::test]
    async fn create_browser_context_oxi_account_rejected() {
        let (ctx, _rx) = make_ctx().await;

        let err = handle(
            "createBrowserContext",
            Some(json!({ "oxiAccount": { "id": "acc-1" } })),
            &ctx,
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, -32000);
        assert_eq!(err.message, "account binding not yet implemented");

        // No context was created.
        assert!(ctx.browser.context(&oxibrowser_core::ContextId::from_string("ctx-2")).is_none());
    }

    /// (d) disposeBrowserContext removes the context: subsequent
    /// createTarget for it fails. The default context cannot be disposed.
    #[tokio::test]
    async fn dispose_context_then_create_target_errors() {
        let (ctx, _rx) = make_ctx().await;

        let resp = handle("createBrowserContext", Some(json!({})), &ctx)
            .await
            .unwrap()
            .unwrap();
        let context_id = resp["browserContextId"].as_str().unwrap().to_string();

        handle(
            "disposeBrowserContext",
            Some(json!({ "browserContextId": context_id })),
            &ctx,
        )
        .await
        .unwrap()
        .unwrap();

        let err = handle(
            "createTarget",
            Some(json!({ "browserContextId": context_id })),
            &ctx,
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, -32000);
        assert!(err.message.contains("invalid browserContextId"));

        // Disposing the default context is refused by core.
        let err = handle(
            "disposeBrowserContext",
            Some(json!({ "browserContextId": ctx.browser.default_context().id().to_string() })),
            &ctx,
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, -32000);
    }
}
