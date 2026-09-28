//! CDP DOM domain handler.
//!
//! Handles DOM.getDocument, DOM.querySelector, DOM.querySelectorAll,
//! DOM.getOuterHTML, DOM.describeNode, DOM.resolveNode.
//!
//! All reads go through the live (post-JS) [`DomSnapshot`] serialized from the
//! `RenderDocument` on the JS thread (`Session::dom_snapshot`), so DOM reads
//! reflect JS-driven mutations rather than a stale navigate-time copy.

use crate::domains::{DispatchContext, DomainResult};
use crate::protocol::CdpError;
use oxibrowser_core::css::{LayoutEngine, LayoutRect};
use oxibrowser_core::js::dom_snapshot::DomSnapshot;
use serde_json::{Value, json};

/// Dispatch DOM domain methods.
pub async fn handle(method: &str, params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    match method {
        "getDocument" => get_document(ctx).await,
        "querySelector" => query_selector(params, ctx).await,
        "querySelectorAll" => query_selector_all(params, ctx).await,
        "getOuterHTML" => get_outer_html(ctx).await,
        "describeNode" => describe_node(params, ctx).await,
        "resolveNode" => resolve_node(params),
        "requestNode" => request_node(params),
        "setAttributeValue" => set_attribute_value(params, ctx).await,
        "removeAttribute" => remove_attribute(params, ctx).await,
        "removeNode" => remove_node(params, ctx).await,
        "getProperty" => get_property(params, ctx).await,
        "setNodeValue" => set_node_value(params, ctx).await,
        "focus" => focus(params, ctx).await,
        "scrollIntoViewIfNeeded" => scroll_into_view_if_needed(params, ctx).await,
        "setFileInputFiles" => set_file_input_files(params, ctx).await,
        "getBoxModel" => get_box_model(params, ctx).await,
        "getContentQuads" => get_content_quads(params, ctx).await,
        "getNodeForLocation" => get_node_for_location(params, ctx).await,
        _ => Err(CdpError {
            code: -32601,
            message: format!("Method not found: DOM.{method}"),
        }),
    }
}

/// DOM.getDocument — returns the root DOM node from the live document.
async fn get_document(ctx: &DispatchContext) -> DomainResult {
    let mut guard = ctx.session.write().await;
    let snap = guard.dom_snapshot().await?;
    match snap {
        Some(s) => Ok(Some(json!({ "root": build_cdp_node(&s, s.root_id, 0) }))),
        None => Ok(Some(json!({
            "root": {
                "nodeId": 0,
                "backendNodeId": 0,
                "nodeType": 9,
                "nodeName": "#document",
                "localName": "",
                "nodeValue": "",
                "childNodeCount": 0
            }
        }))),
    }
}

/// DOM.querySelector — finds a single node matching a CSS selector.
async fn query_selector(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_default();
    let selector = params
        .get("selector")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let mut guard = ctx.session.write().await;
    let snap = guard.dom_snapshot().await?;
    let node_id = snap
        .as_ref()
        .and_then(|s| s.query_selector(selector))
        .unwrap_or(0);
    Ok(Some(json!({ "nodeId": node_id })))
}

/// DOM.querySelectorAll — finds all nodes matching a CSS selector.
async fn query_selector_all(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_default();
    let selector = params
        .get("selector")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let mut guard = ctx.session.write().await;
    let snap = guard.dom_snapshot().await?;
    let node_ids: Vec<u64> = snap
        .as_ref()
        .map(|s| {
            s.query_selector_all(selector)
                .iter()
                .map(|id| *id as u64)
                .collect()
        })
        .unwrap_or_default();
    Ok(Some(json!({ "nodeIds": node_ids })))
}

/// DOM.getOuterHTML — returns the page's HTML.
async fn get_outer_html(ctx: &DispatchContext) -> DomainResult {
    let guard = ctx.session.read().await;
    let html = match guard.page() {
        Some(page) => page.content().to_string(),
        None => "<html><head></head><body></body></html>".to_string(),
    };
    Ok(Some(json!({ "outerHTML": html })))
}

/// DOM.describeNode — describes a DOM node with real data from the live tree.
async fn describe_node(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_default();
    let node_id = params
        .get("nodeId")
        .and_then(|v| v.as_u64())
        .or_else(|| params.get("backendNodeId").and_then(|v| v.as_u64()))
        .unwrap_or(0) as u32;

    let mut guard = ctx.session.write().await;
    let snap = guard.dom_snapshot().await?;
    let Some(snap) = snap else {
        return Err(CdpError {
            code: -32000,
            message: "No active page".to_string(),
        });
    };
    let Some(node) = snap.nodes.get(&node_id) else {
        return Err(CdpError {
            code: -32000,
            message: format!("Node not found: {node_id}"),
        });
    };

    let (node_type_num, node_name, local_name, node_value) = match node.node_type {
        9 => (9, "#document".to_string(), String::new(), String::new()),
        1 => (1, node.tag.to_uppercase(), node.tag.clone(), String::new()),
        3 => (
            3,
            "#text".to_string(),
            String::new(),
            node.text_content.clone(),
        ),
        8 => (
            8,
            "#comment".to_string(),
            String::new(),
            node.text_content.clone(),
        ),
        _ => (1, node.tag.to_uppercase(), node.tag.clone(), String::new()),
    };

    let child_count = node.children.len();
    let attributes: Vec<Value> = if node.node_type == 1 {
        node.attributes
            .iter()
            .flat_map(|(k, v)| [json!(k), json!(v)])
            .collect()
    } else {
        Vec::new()
    };

    Ok(Some(json!({
        "node": {
            "nodeId": node_id,
            "backendNodeId": node_id,
            "nodeType": node_type_num,
            "nodeName": node_name,
            "localName": local_name,
            "nodeValue": node_value,
            "childNodeCount": child_count,
            "attributes": attributes,
        }
    })))
}

/// DOM.resolveNode — resolves a DOM node to a JS remote object.
///
/// Uses deterministic objectId format "oxi-node-{nodeId}" so that
/// Runtime.callFunctionOn can look up the node by its objectId.
fn resolve_node(params: Option<Value>) -> DomainResult {
    let params = params.unwrap_or_default();
    let node_id = params.get("nodeId").and_then(|v| v.as_u64()).unwrap_or(0);
    let object_id = format!("oxi-node-{node_id}");

    Ok(Some(json!({
        "object": {
            "type": "object",
            "subtype": "node",
            "className": "HTMLElement",
            "description": format!("node#{node_id}"),
            "objectId": object_id
        }
    })))
}

// ---------------------------------------------------------------------------
// DOM.* element mutators
//
// These methods all take `{nodeId, ...}` and route through a JS expression
// that finds the target element by walking `document.querySelectorAll('*')`
// and matching on the render document's `__nodeId` property (every JS element
// carries one — see `runtime.rs` `create_element_object` / friends). We can't
// use a CSS attribute selector like `[data-oxi-node-id="N"]` because the
// `data-oxi-node-id` attribute is only added to the JS-side `enriched_attrs`
// map on each element object, never to the snapshot's `attributes` HashMap —
// so the snapshot's `query_selector` (which powers every JS `querySelector`)
// never sees it.
//
// Each handler returns `Ok(Some(json!({})))` on success — CDP treats the
// empty object as an acknowledgement, since the actual effect is the JS
// mutation applied to the live render document. We surface JS exceptions
// as a CDP error so callers can distinguish "method not found" from
// "method failed at runtime".
// ---------------------------------------------------------------------------

/// Build a JS expression that runs `body` against the element whose
/// `__nodeId` equals `id`. Returns `null` if the element isn't in the document.
fn node_js_expr(id: u32, body: &str) -> String {
    format!(
        "(function() {{ var __all = document.querySelectorAll('*'); for (var __i = 0; __i < __all.length; __i++) {{ if (__all[__i].__nodeId === {id}) {{ var __el = __all[__i]; return ({body}); }} }} return null; }})()",
    )
}

/// DOM.requestNode — reverse of `resolveNode`.
///
/// `{ objectId: "oxi-node-{N}" }` → `{ nodeId: N }`. We accept any
/// `oxi-node-` prefix and parse the trailing integer; non-matching
/// objectIds yield `nodeId: 0` (Playwright only ever calls this with the
/// canonical prefix).
fn request_node(params: Option<Value>) -> DomainResult {
    let params = params.unwrap_or_default();
    let object_id = params
        .get("objectId")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let node_id = object_id
        .strip_prefix("oxi-node-")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    Ok(Some(json!({ "nodeId": node_id })))
}

/// DOM.setAttributeValue — writes `name=value` on the element.
async fn set_attribute_value(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let (node_id, name, value) = match extract_attr_set(params) {
        Some(v) => v,
        None => return Ok(Some(json!({}))),
    };
    let expr = node_js_expr(
        node_id,
        &format!(
            "__el.setAttribute({}, {})",
            serde_json::to_string(&name).unwrap_or_else(|_| "\"\"".into()),
            serde_json::to_string(&value).unwrap_or_else(|_| "\"\"".into()),
        ),
    );
    run_js_void(&expr, ctx).await
}

/// DOM.removeAttribute — removes the named attribute from the element.
async fn remove_attribute(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let (node_id, name) = match extract_attr_name(params) {
        Some(v) => v,
        None => return Ok(Some(json!({}))),
    };
    let expr = node_js_expr(
        node_id,
        &format!(
            "(__el.removeAttribute({}), null)",
            serde_json::to_string(&name).unwrap_or_else(|_| "\"\"".into()),
        ),
    );
    run_js_void(&expr, ctx).await
}

/// DOM.removeNode — detaches the element from its parent.
async fn remove_node(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let node_id = match params
        .as_ref()
        .and_then(|p| p.get("nodeId"))
        .and_then(|v| v.as_u64())
    {
        Some(id) => id as u32,
        None => return Ok(Some(json!({}))),
    };
    let expr = node_js_expr(node_id, "(__el.remove(), null)");
    run_js_void(&expr, ctx).await
}

/// DOM.getProperty — reads a JS property (`el[prop]`) off the element and
/// returns it as a string-coerced `value`. Real DOM properties (id, value,
/// checked, …) reflect the live state; arbitrary attributes (e.g. `data-x`)
/// appear as `undefined` and we render them as `null` in the response.
async fn get_property(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let (node_id, name) = match extract_attr_name(params) {
        Some(v) => v,
        None => return Ok(Some(json!({ "value": Value::Null }))),
    };
    let expr = node_js_expr(
        node_id,
        &format!(
            "(function() {{ var v = __el[{key}]; if (v === undefined || v === null) return null; if (typeof v === 'string') return v; try {{ return String(v); }} catch (e) {{ return null; }} }})()",
            key = serde_json::to_string(&name).unwrap_or_else(|_| "\"\"".into()),
        ),
    );
    let value = run_js_value(&expr, ctx).await?;
    Ok(Some(json!({ "value": value })))
}

/// DOM.setNodeValue — sets the value of a text/comment node (via `nodeValue`)
/// or, for element nodes, the element's `textContent`. Both shapes converge
/// on what CDP drivers expect from a `<p>before</p>` → `<p>AFTER</p>` round-trip.
async fn set_node_value(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let (node_id, value) = match params
        .as_ref()
        .and_then(|p| p.get("nodeId").and_then(|v| v.as_u64()))
        .zip(
            params
                .as_ref()
                .and_then(|p| p.get("value").and_then(|v| v.as_str())),
        ) {
        Some((id, val)) => (id as u32, val.to_string()),
        None => return Ok(Some(json!({}))),
    };
    let expr = node_js_expr(
        node_id,
        &format!(
            "(function() {{ if (__el.nodeType === 3 || __el.nodeType === 8) {{ __el.nodeValue = {val}; }} else {{ __el.textContent = {val}; }} return null; }})()",
            val = serde_json::to_string(&value).unwrap_or_else(|_| "\"\"".into()),
        ),
    );
    run_js_void(&expr, ctx).await
}

/// DOM.focus — best-effort `.focus()`. The render document's element object
/// exposes a no-op `focus` binding (see runtime.rs `make_noop`).
async fn focus(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let node_id = match params
        .as_ref()
        .and_then(|p| p.get("nodeId"))
        .and_then(|v| v.as_u64())
    {
        Some(id) => id as u32,
        None => return Ok(Some(json!({}))),
    };
    let expr = node_js_expr(node_id, "(__el.focus(), null)");
    run_js_void(&expr, ctx).await
}

/// DOM.scrollIntoViewIfNeeded — best-effort `.scrollIntoView()`. The render
/// document's element object also exposes a no-op `scrollIntoView` binding.
async fn scroll_into_view_if_needed(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let node_id = match params
        .as_ref()
        .and_then(|p| p.get("nodeId"))
        .and_then(|v| v.as_u64())
    {
        Some(id) => id as u32,
        None => return Ok(Some(json!({}))),
    };
    let expr = node_js_expr(
        node_id,
        "(__el.scrollIntoView({block:'nearest', inline:'nearest'}), null)",
    );
    run_js_void(&expr, ctx).await
}

/// DOM.setFileInputFiles — best-effort stub.
///
/// Real Playwright `setInputFiles` requires a Chromium file-chooser round-trip
/// that this headless engine doesn't support; CDP clients expect the method
/// to exist (so they don't crash on the WS), but they don't read a meaningful
/// return. We accept `files: [String]`, set the input's `value` to the first
/// path, and return `{}`. The element's `files` property stays empty — real
/// file content would need browser-level chooser plumbing, which is out of
/// scope here. The note in the design doc called this out explicitly.
async fn set_file_input_files(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let (node_id, first) = match params
        .as_ref()
        .and_then(|p| p.get("nodeId").and_then(|v| v.as_u64()))
        .zip(
            params
                .as_ref()
                .and_then(|p| p.get("files"))
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str()),
        ) {
        Some((id, name)) => (id as u32, name.to_string()),
        None => return Ok(Some(json!({}))),
    };
    let expr = node_js_expr(
        node_id,
        &format!(
            "(function() {{ try {{ __el.value = {val}; }} catch (e) {{ }} return null; }})()",
            val = serde_json::to_string(&first).unwrap_or_else(|_| "\"\"".into()),
        ),
    );
    run_js_void(&expr, ctx).await
}

// --- internal helpers -------------------------------------------------------

fn extract_attr_name(params: Option<Value>) -> Option<(u32, String)> {
    let p = params?;
    let node_id = p.get("nodeId").and_then(|v| v.as_u64())? as u32;
    let name = p.get("name").and_then(|v| v.as_str())?.to_string();
    Some((node_id, name))
}

fn extract_attr_set(params: Option<Value>) -> Option<(u32, String, String)> {
    let p = params?;
    let node_id = p.get("nodeId").and_then(|v| v.as_u64())? as u32;
    let name = p.get("name").and_then(|v| v.as_str())?.to_string();
    let value = p
        .get("value")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Some((node_id, name, value))
}

/// Run a JS expression and return `Ok(Some(json!({})))` on success, or
/// surface a JS exception as a CDP error.
async fn run_js_void(expr: &str, ctx: &DispatchContext) -> DomainResult {
    let mut guard = ctx.session.write().await;
    let result = guard.evaluate_js(expr).await?;
    if let Some(exception) = result.exception {
        return Err(CdpError {
            code: -32000,
            message: format!("DOM.* JS error: {exception}"),
        });
    }
    Ok(Some(json!({})))
}

/// Run a JS expression and return its evaluated value. `null` results are
/// preserved as `Value::Null` so callers can distinguish "JS returned null"
/// from "JS returned nothing".
async fn run_js_value(expr: &str, ctx: &DispatchContext) -> DomainResult {
    let mut guard = ctx.session.write().await;
    let result = guard.evaluate_js(expr).await?;
    if let Some(exception) = result.exception {
        return Err(CdpError {
            code: -32000,
            message: format!("DOM.* JS error: {exception}"),
        });
    }
    Ok(Some(result.value.unwrap_or(Value::Null)))
}

// ---------------------------------------------------------------------------
// CDP node tree builder
// ---------------------------------------------------------------------------

/// Maximum depth for the CDP node tree (avoids huge outputs).
const MAX_CDP_TREE_DEPTH: usize = 10;

/// Build a CDP-compatible JSON node from the live [`DomSnapshot`] tree.
fn build_cdp_node(snapshot: &DomSnapshot, node_id: u32, depth: usize) -> Value {
    let Some(node) = snapshot.nodes.get(&node_id) else {
        return json!({});
    };

    let (node_type_num, node_name, local_name, node_value) = match node.node_type {
        9 => (9, "#document".to_string(), String::new(), String::new()),
        1 => (1, node.tag.to_uppercase(), node.tag.clone(), String::new()),
        3 => (
            3,
            "#text".to_string(),
            String::new(),
            node.text_content.clone(),
        ),
        8 => (
            8,
            "#comment".to_string(),
            String::new(),
            node.text_content.clone(),
        ),
        _ => (1, node.tag.to_uppercase(), node.tag.clone(), String::new()),
    };

    // Attribute pairs [name1, value1, name2, value2, ...]
    let attributes: Vec<Value> = if node.node_type == 1 {
        node.attributes
            .iter()
            .flat_map(|(k, v)| [json!(k), json!(v)])
            .collect()
    } else {
        Vec::new()
    };

    let children: Vec<Value> = if depth < MAX_CDP_TREE_DEPTH {
        node.children
            .iter()
            .filter(|&&child_id| {
                if let Some(child_node) = snapshot.nodes.get(&child_id)
                    && child_node.node_type == 3
                {
                    return !child_node.text_content.trim().is_empty();
                }
                true
            })
            .map(|&child_id| build_cdp_node(snapshot, child_id, depth + 1))
            .collect()
    } else {
        Vec::new()
    };

    json!({
        "nodeId": node_id,
        "backendNodeId": node_id,
        "nodeType": node_type_num,
        "nodeName": node_name,
        "localName": local_name,
        "nodeValue": node_value,
        "childNodeCount": children.len(),
        "children": children,
        "attributes": attributes,
    })
}

// ---------------------------------------------------------------------------
// Layout geometry (LayoutEngine-backed)
// ---------------------------------------------------------------------------

/// Flatten a [`LayoutRect`] into a CDP quad: 4 (x, y) points clockwise from
/// the top-left. CDP represents a quad as `[x1,y1, x2,y2, x3,y3, x4,y4]`.
fn quad_from_rect(r: &LayoutRect) -> Vec<f64> {
    let (x, y, w, h) = (r.x, r.y, r.width, r.height);
    vec![x, y, x + w, y, x + w, y + h, x, y + h]
}

/// DOM.getBoxModel — returns the box model for a node.
///
/// LayoutEngine produces a single bounding rect (no separate
/// content/padding/border/margin boxes), so all four quads share it.
async fn get_box_model(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_default();
    let node_id = params.get("nodeId").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let mut guard = ctx.session.write().await;
    let snap = guard.dom_snapshot().await?;
    let Some(s) = snap else {
        return Err(CdpError {
            code: -32000,
            message: "Could not find document root".into(),
        });
    };
    let rect = LayoutEngine::compute_rect(&s, node_id);
    let quad = quad_from_rect(&rect);
    Ok(Some(json!({
        "model": {
            "content": quad.clone(),
            "padding": quad.clone(),
            "border": quad.clone(),
            "margin": quad,
            "width": rect.width,
            "height": rect.height,
        }
    })))
}

/// DOM.getContentQuads — returns the content quads for a node.
async fn get_content_quads(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_default();
    let node_id = params.get("nodeId").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let mut guard = ctx.session.write().await;
    let snap = guard.dom_snapshot().await?;
    let Some(s) = snap else {
        return Ok(Some(json!({ "quads": [] })));
    };
    let rect = LayoutEngine::compute_rect(&s, node_id);
    Ok(Some(json!({ "quads": [quad_from_rect(&rect)] })))
}

/// DOM.getNodeForLocation — returns the node at the given viewport coordinate.
///
/// Picks the smallest-area element whose rect contains the point (the most
/// specific / topmost-painted element under the cursor).
async fn get_node_for_location(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let params = params.unwrap_or_default();
    let x = params.get("x").and_then(|v| v.as_f64()).unwrap_or(-1.0);
    let y = params.get("y").and_then(|v| v.as_f64()).unwrap_or(-1.0);
    let mut guard = ctx.session.write().await;
    let snap = guard.dom_snapshot().await?;
    let Some(s) = snap else {
        return Err(CdpError {
            code: -32000,
            message: "Could not find document root".into(),
        });
    };
    let mut best: Option<(f64, u32, String)> = None;
    for (&id, node) in &s.nodes {
        // Only elements are hit-testable.
        if node.node_type != 1 {
            continue;
        }
        let rect = LayoutEngine::compute_rect(&s, id);
        if rect.width <= 0.0 || rect.height <= 0.0 {
            continue;
        }
        if x >= rect.x && x <= rect.x + rect.width && y >= rect.y && y <= rect.y + rect.height {
            let area = rect.width * rect.height;
            if best.as_ref().is_none_or(|(a, _, _)| area < *a) {
                best = Some((area, id, node.tag.clone()));
            }
        }
    }
    match best {
        Some((_, id, tag)) => Ok(Some(json!({
            "nodeId": id,
            "backendNodeId": id,
            "frameId": "main",
            "nodeName": tag.to_uppercase(),
        }))),
        None => Err(CdpError {
            code: -32000,
            message: format!("No node found at given location ({x}, {y})"),
        }),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domains::DispatchContext;
    use crate::event::event_channel;
    use oxibrowser_core::network::intercept::shared_registry;
    use oxibrowser_core::session::Session;
    use oxibrowser_core::{Browser, BrowserConfig};
    use serde_json::json;

    use std::sync::Arc;
    use tokio::sync::RwLock;

    /// Spin up a headless browser + session, navigate to a known HTML page,
    /// and return `(DispatchContext, session_lock, server)`. The caller is
    /// responsible for shutting the server down with `server.shutdown().await`.
    async fn make_ctx() -> (DispatchContext, Arc<RwLock<Session>>) {
        use std::net::TcpListener;
        // Disable network — tests use data: URLs, no outbound HTTP.
        let mut config = BrowserConfig::headless();
        config.enable_ssrf_filter = false;
        let browser = Arc::new(Browser::new(config).await.unwrap());
        let session = browser.new_session().await.unwrap();
        // Bind a sink port so SSRF checks don't trip; not actually used.
        let _ = TcpListener::bind("127.0.0.1:0").unwrap();
        session
            .write()
            .await
            .navigate(
                "data:text/html;charset=utf-8,<html><body><p id=\"t\">before</p>\
                 <a id=\"l\" href=\"/x\">link</a>\
                 <input id=\"f\" type=\"file\"/>\
                 </body></html>",
            )
            .await
            .unwrap();
        // Wait for the DOM to settle.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let (events, _rx) = event_channel();
        let ctx = DispatchContext {
            session: session.clone(),
            events,
            fetch_registry: shared_registry(),
            dialog_gate: Arc::new(parking_lot::Mutex::new(None)),
            browser: browser.clone(),
            child_targets: Arc::new(crate::domains::TargetRegistry::new()),
            browser_context: browser.default_context(),
            credentials: None,
            role: crate::session::RoleKind::Agent,
            logins: None,
        };
        (ctx, session)
    }

    // No shutdown helper: dropping the session is sufficient.
    /// Look up the nodeId of a selector in the current document.
    async fn node_id_of(ctx: &DispatchContext, selector: &str) -> u32 {
        let resp = handle(
            "querySelector",
            Some(json!({ "nodeId": 0, "selector": selector })),
            ctx,
        )
        .await
        .unwrap()
        .unwrap();
        resp.get("nodeId").and_then(|v| v.as_u64()).unwrap_or(0) as u32
    }

    #[tokio::test]
    async fn request_node_parses_object_id_into_node_id() {
        let (ctx, _sess) = make_ctx().await;
        let resp = handle(
            "requestNode",
            Some(json!({ "objectId": "oxi-node-42" })),
            &ctx,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(resp.get("nodeId").and_then(|v| v.as_u64()), Some(42));
        let _ = _sess;
    }

    // no debug tests retained in committed code

    #[tokio::test]
    async fn set_attribute_value_writes_attribute() {
        let (ctx, _sess) = make_ctx().await;
        let p_id = node_id_of(&ctx, "#t").await;
        assert!(p_id > 0);

        let resp = handle(
            "setAttributeValue",
            Some(json!({ "nodeId": p_id, "name": "data-x", "value": "123" })),
            &ctx,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(resp.is_object());
        assert_eq!(resp.as_object().unwrap().len(), 0);

        // Verify the attribute is set: read it back via JS eval.
        let mut guard = ctx.session.write().await;
        let r = guard
            .evaluate_js(&format!(
                "(function() {{ var __all = document.querySelectorAll('*'); for (var __i = 0; __i < __all.length; __i++) {{ if (__all[__i].__nodeId === {p_id}) return __all[__i].getAttribute('data-x'); }} return null; }})()",
            ))
            .await
            .unwrap();
        assert_eq!(r.value.as_ref().and_then(|v| v.as_str()), Some("123"));
        let _ = _sess;
    }

    #[tokio::test]
    async fn remove_attribute_deletes_attribute() {
        let (ctx, _sess) = make_ctx().await;
        let p_id = node_id_of(&ctx, "#t").await;
        assert!(p_id > 0);

        {
            let mut guard = ctx.session.write().await;
            guard
                .evaluate_js(&format!(
                    "(function() {{ var __all = document.querySelectorAll('*'); for (var __i = 0; __i < __all.length; __i++) {{ if (__all[__i].__nodeId === {p_id}) {{ __all[__i].setAttribute('data-del', 'x'); return; }} }} }})()",
                ))
                .await
                .unwrap();
        }
        let resp = handle(
            "removeAttribute",
            Some(json!({ "nodeId": p_id, "name": "data-del" })),
            &ctx,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(resp.is_object());

        let mut guard = ctx.session.write().await;
        let r = guard
            .evaluate_js(&format!(
                "(function() {{ var __all = document.querySelectorAll('*'); for (var __i = 0; __i < __all.length; __i++) {{ if (__all[__i].__nodeId === {p_id}) return __all[__i].getAttribute('data-del'); }} return 'NO_ELEMENT'; }})()",
            ))
            .await
            .unwrap();
        // After removeAttribute, the attribute is gone → getAttribute returns null.
        assert_eq!(r.value, Some(serde_json::Value::Null));
    }

    #[tokio::test]
    async fn remove_node_detaches_from_parent() {
        let (ctx, _sess) = make_ctx().await;
        let p_id = node_id_of(&ctx, "#t").await;
        assert!(p_id > 0);

        handle("removeNode", Some(json!({ "nodeId": p_id })), &ctx)
            .await
            .unwrap()
            .unwrap();

        let resp = handle(
            "querySelector",
            Some(json!({ "nodeId": 0, "selector": "#t" })),
            &ctx,
        )
        .await
        .unwrap()
        .unwrap();
        let id = resp.get("nodeId").and_then(|v| v.as_u64()).unwrap_or(0);
        assert_eq!(id, 0, "node should have been removed");
        let _ = _sess;
    }

    #[tokio::test]
    async fn get_property_reads_dom_property() {
        let (ctx, _sess) = make_ctx().await;
        let p_id = node_id_of(&ctx, "#t").await;
        assert!(p_id > 0);

        let resp = handle(
            "getProperty",
            Some(json!({ "nodeId": p_id, "name": "id" })),
            &ctx,
        )
        .await
        .unwrap()
        .unwrap();
        let v = resp.get("value").unwrap();
        assert_eq!(v.as_str(), Some("t"));
        let _ = _sess;
    }

    #[tokio::test]
    async fn set_node_value_updates_text_content() {
        let (ctx, _sess) = make_ctx().await;
        let p_id = node_id_of(&ctx, "#t").await;
        assert!(p_id > 0);

        handle(
            "setNodeValue",
            Some(json!({ "nodeId": p_id, "value": "AFTER" })),
            &ctx,
        )
        .await
        .unwrap()
        .unwrap();

        let mut guard = ctx.session.write().await;
        let r = guard
            .evaluate_js(&format!(
                "(function() {{ var __all = document.querySelectorAll('*'); for (var __i = 0; __i < __all.length; __i++) {{ if (__all[__i].__nodeId === {p_id}) return __all[__i].textContent; }} return null; }})()",
            ))
            .await
            .unwrap();
        assert_eq!(r.value.as_ref().and_then(|v| v.as_str()), Some("AFTER"));
        let _ = _sess;
    }

    #[tokio::test]
    async fn focus_does_not_error() {
        let (ctx, _sess) = make_ctx().await;
        let p_id = node_id_of(&ctx, "#t").await;
        assert!(p_id > 0);

        let resp = handle("focus", Some(json!({ "nodeId": p_id })), &ctx)
            .await
            .unwrap()
            .unwrap();
        assert!(resp.is_object());
        let _ = _sess;
    }

    #[tokio::test]
    async fn scroll_into_view_if_needed_does_not_error() {
        let (ctx, _sess) = make_ctx().await;
        let p_id = node_id_of(&ctx, "#t").await;
        assert!(p_id > 0);

        let resp = handle(
            "scrollIntoViewIfNeeded",
            Some(json!({ "nodeId": p_id })),
            &ctx,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(resp.is_object());
        let _ = _sess;
    }

    #[tokio::test]
    async fn set_file_input_files_does_not_error() {
        let (ctx, _sess) = make_ctx().await;
        let f_id = node_id_of(&ctx, "#f").await;
        assert!(f_id > 0);

        let resp = handle(
            "setFileInputFiles",
            Some(json!({ "nodeId": f_id, "files": ["/tmp/example.txt"] })),
            &ctx,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(resp.is_object());
        let _ = _sess;
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_implemented() {
        let (ctx, _sess) = make_ctx().await;
        let err = handle(
            "setNodeName",
            Some(json!({ "nodeId": 1, "name": "x" })),
            &ctx,
        )
        .await
        .expect_err("expected error");
        assert_eq!(err.code, -32601);
        assert!(
            err.message.contains("DOM.setNodeName"),
            "message should name the unknown method, got: {}",
            err.message
        );
        let _ = _sess;
    }
}
