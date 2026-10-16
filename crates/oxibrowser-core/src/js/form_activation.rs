//! Form activation behavior: serialization + submit navigation (M1.5).
//!
//! Pure helpers shared by three trigger paths:
//! - `form.submit()` JS binding (`create_element_object` in `runtime.rs`),
//! - click default action on submit controls via the JS-eval dispatch path
//!   (`dispatchEvent` in `runtime.rs`),
//! - click default action on submit controls via the native path
//!   (`native_action::act`).
//!
//! Encoding follows `application/x-www-form-urlencoded` semantics: disabled
//! controls are excluded, radio/checkboxes only when checked, selects
//! contribute their selected option (or the first option), textareas their
//! text content.

use super::dom_snapshot::{DomMutation, DomNode, DomSnapshot};
use crate::account::agent_login::{form_body, form_post_url};
use boa_engine::{Context, js_string};
use url::Url;

/// Read the current document URL from the `__oxiPageUrl` global installed by
/// `register_window_globals`.
pub fn page_url_from_ctx(ctx: &mut Context) -> Option<Url> {
    let v = ctx
        .global_object()
        .get(js_string!("__oxiPageUrl"), ctx)
        .ok()?;
    let s = v.as_string()?.to_std_string_escaped();
    Url::parse(&s).ok()
}

/// Whether the given element is a control whose click activates its owner form.
pub fn is_submit_control(tag: &str, raw_type: &str) -> bool {
    let ty = raw_type.to_ascii_lowercase();
    match tag {
        "input" => ty == "submit",
        "button" => ty.is_empty() || ty == "submit",
        _ => false,
    }
}

/// Nearest `<form>` ancestor of `node_id`, including the node itself.
pub fn nearest_form(snapshot: &DomSnapshot, node_id: u32) -> Option<u32> {
    let mut current = Some(node_id);
    while let Some(id) = current {
        let node = snapshot.nodes.get(&id)?;
        if node.tag == "form" {
            return Some(id);
        }
        current = node.parent;
    }
    None
}

/// Serialize the form's successful controls into `(name, value)` pairs.
pub fn serialize_form(snapshot: &DomSnapshot, form_id: u32) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let Some(form) = snapshot.nodes.get(&form_id) else {
        return pairs;
    };
    // Document-order walk of the form subtree.
    let mut stack: Vec<u32> = form.children.iter().rev().copied().collect();
    while let Some(id) = stack.pop() {
        let Some(node) = snapshot.nodes.get(&id) else {
            continue;
        };
        stack.extend(node.children.iter().rev().copied());
        if node.attributes.contains_key("disabled") {
            continue;
        }
        let name = match node.attributes.get("name") {
            Some(n) if !n.is_empty() => n.clone(),
            _ => continue,
        };
        let ty = node
            .attributes
            .get("type")
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        match node.tag.as_str() {
            "input" => match ty.as_str() {
                "checkbox" | "radio" => {
                    if node.attributes.contains_key("checked") {
                        let value = node
                            .attributes
                            .get("value")
                            .cloned()
                            .unwrap_or_else(|| "on".to_string());
                        pairs.push((name, value));
                    }
                }
                "submit" | "reset" | "button" | "image" | "file" => {}
                _ => {
                    let value = node.attributes.get("value").cloned().unwrap_or_default();
                    pairs.push((name, value));
                }
            },
            "textarea" => pairs.push((name, node.text_content.clone())),
            "select" => {
                // Selected option wins; fall back to the first option.
                let mut options: Vec<&DomNode> = Vec::new();
                let mut inner: Vec<u32> = node.children.iter().rev().copied().collect();
                while let Some(cid) = inner.pop() {
                    let Some(child) = snapshot.nodes.get(&cid) else {
                        continue;
                    };
                    inner.extend(child.children.iter().rev().copied());
                    if child.tag == "option" {
                        options.push(child);
                    }
                }
                let chosen = options
                    .iter()
                    .find(|o| o.attributes.contains_key("selected"))
                    .or_else(|| options.first());
                if let Some(option) = chosen {
                    let value = option
                        .attributes
                        .get("value")
                        .cloned()
                        .unwrap_or_else(|| option.text_content.clone());
                    pairs.push((name, value));
                }
            }
            _ => {
                // option/optgroup and everything else contribute nothing
                // directly (select handled above).
            }
        }
    }
    pairs
}

/// The navigation a form submission performs: `Navigate` for GET (fields as
/// query parameters) or `NavigatePost` for POST (urlencoded body).
pub fn submit_mutation(
    snapshot: &DomSnapshot,
    form_id: u32,
    page_url: &Url,
) -> Option<DomMutation> {
    let form = snapshot.nodes.get(&form_id)?;
    let method = form
        .attributes
        .get("method")
        .map(|s| s.to_ascii_uppercase())
        .unwrap_or_else(|| "GET".to_string());
    let action = form.attributes.get("action").cloned();
    let target_url = form_post_url(page_url, &action).ok()?;
    let owned_pairs = serialize_form(snapshot, form_id);
    if method == "POST" {
        let borrowed: Vec<(&str, &str)> = owned_pairs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        Some(DomMutation::NavigatePost {
            url: target_url.to_string(),
            body: form_body(&borrowed),
            content_type: "application/x-www-form-urlencoded".to_string(),
        })
    } else {
        let mut u = target_url;
        u.set_query(None);
        {
            let mut q = u.query_pairs_mut();
            q.extend_pairs(owned_pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        }
        Some(DomMutation::Navigate { url: u.to_string() })
    }
}

/// Submit from a `<form>` node (the `form.submit()` binding): resolve the
/// owner form of `node_id` (the node itself included) and serialize it.
pub fn form_submit_mutation(
    snapshot: &DomSnapshot,
    node_id: u32,
    page_url: &Url,
) -> Option<DomMutation> {
    let form_id = nearest_form(snapshot, node_id)?;
    submit_mutation(snapshot, form_id, page_url)
}
