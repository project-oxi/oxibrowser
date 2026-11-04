use oxibrowser_render::{RenderDocument, Viewport};

fn document() -> RenderDocument {
    RenderDocument::from_html(
        "<head><style>#box { width: 10px }</style></head><body><div id='box'></div></body>",
        Some("https://example.test/app/"),
        Viewport {
            width: 320,
            height: 240,
            scale: 1.0,
        },
    )
    .unwrap()
}

fn link(doc: &mut RenderDocument, href: &str) -> usize {
    let node = doc.create_element("link");
    doc.set_attribute(node, "rel", "stylesheet");
    doc.set_attribute(node, "href", href);
    node
}

fn attach(doc: &mut RenderDocument, node: usize, width: u32) {
    doc.attach_stylesheet(
        node,
        "https://cdn.example.test/theme/sheet.css",
        &format!("#box {{ width: {width}px; background-image: url(images/box.png); }}"),
    )
    .unwrap();
}

fn width(doc: &mut RenderDocument) -> String {
    doc.flush_layout();
    doc.resolved_style(doc.query_selector("#box").unwrap(), "width")
}

#[test]
fn linked_sheet_order_follows_dom_instead_of_creation_or_response_order() {
    let mut doc = document();
    let head = doc.query_selector("head").unwrap();
    let a = link(&mut doc, "/a.css");
    let b = link(&mut doc, "/b.css");
    assert!(a < b);
    doc.append_child(head, a);
    doc.insert_before(a, b);
    attach(&mut doc, a, 111);
    attach(&mut doc, b, 222);
    assert_eq!(width(&mut doc), "111px");
    // Moving a loaded node changes precedence without a new response.
    doc.append_child(head, b);
    assert_eq!(width(&mut doc), "222px");
    doc.insert_before(a, b);
    assert_eq!(width(&mut doc), "111px");
    // Replacing an earlier response must not move its cascade position.
    attach(&mut doc, b, 333);
    assert_eq!(width(&mut doc), "111px");
}

#[test]
fn rel_disabled_and_href_changes_control_loaded_sheets() {
    let mut doc = document();
    let head = doc.query_selector("head").unwrap();
    let a = link(&mut doc, "/a.css");
    let b = link(&mut doc, "/b.css");
    doc.append_child(head, a);
    doc.append_child(head, b);
    attach(&mut doc, a, 111);
    attach(&mut doc, b, 222);
    assert_eq!(width(&mut doc), "222px");
    doc.set_attribute(b, "rel", "preload");
    assert_eq!(width(&mut doc), "111px");
    doc.set_attribute(b, "rel", "preload STYLESHEET");
    assert_eq!(width(&mut doc), "222px");
    doc.set_attribute(b, "disabled", "");
    assert_eq!(width(&mut doc), "111px");
    doc.remove_attribute(b, "disabled");
    assert_eq!(width(&mut doc), "222px");
    doc.remove_attribute(b, "rel");
    assert_eq!(width(&mut doc), "111px");
    doc.set_attribute(b, "rel", "stylesheet");
    assert_eq!(width(&mut doc), "222px");
    doc.set_attribute(b, "href", "/replacement.css");
    assert_eq!(width(&mut doc), "111px");
    assert!(!doc.document().has_pending_critical_resources());
    attach(&mut doc, b, 333);
    assert_eq!(width(&mut doc), "333px");
    doc.remove_attribute(b, "href");
    assert_eq!(width(&mut doc), "111px");
    // Safe even when a subsequent request has not produced a stylesheet.
    doc.set_attribute(b, "href", "/pending.css");
    doc.remove_attribute(b, "href");
    assert_eq!(width(&mut doc), "111px");
}

#[test]
fn detached_sheets_stop_cascading_and_reinsertion_preserves_response_base() {
    let mut doc = document();
    let head = doc.query_selector("head").unwrap();
    let node = link(&mut doc, "/redirected.css");
    doc.append_child(head, node);
    attach(&mut doc, node, 123);
    assert_eq!(width(&mut doc), "123px");
    let target = doc.query_selector("#box").unwrap();
    let background = doc.resolved_style(target, "background-image");
    assert!(
        background.contains("https://cdn.example.test/theme/images/box.png"),
        "{background}"
    );
    doc.remove_node(node);
    assert_eq!(width(&mut doc), "10px");
    doc.append_child(head, node);
    assert_eq!(width(&mut doc), "123px");
    assert_eq!(doc.resolved_style(target, "background-image"), background);
    // Removing an ancestor deactivates every stylesheet in its subtree.
    doc.remove_node(head);
    assert_eq!(width(&mut doc), "304px");
    doc.insert_before(doc.query_selector("body").unwrap(), head);
    assert_eq!(width(&mut doc), "123px");
}

#[test]
fn inline_and_linked_sheets_share_document_order_after_moves_and_edits() {
    let mut doc = document();
    let head = doc.query_selector("head").unwrap();
    let inline = doc.create_element("style");
    doc.set_text(inline, "#box { width: 300px }");
    doc.append_child(head, inline);
    let node = link(&mut doc, "/linked.css");
    doc.insert_before(inline, node);
    attach(&mut doc, node, 111);
    assert_eq!(width(&mut doc), "300px");
    doc.append_child(head, node);
    assert_eq!(width(&mut doc), "111px");
    doc.append_child(head, inline);
    assert_eq!(width(&mut doc), "300px");
    doc.set_text(inline, "#box { width: 444px }");
    assert_eq!(width(&mut doc), "444px");
    doc.remove_node(inline);
    assert_eq!(width(&mut doc), "111px");
    doc.insert_before(node, inline);
    assert_eq!(width(&mut doc), "111px");
    doc.set_attribute(node, "rel", "preload");
    assert_eq!(width(&mut doc), "444px");
}

#[test]
fn comments_are_real_detachable_anchors() {
    let mut doc = document();
    let body = doc.query_selector("body").unwrap();
    let box_node = doc.query_selector("#box").unwrap();
    let comment = doc.create_comment_node("anchor");
    assert_eq!(doc.node_kind(comment), Some("comment"));
    let incarnation = doc.node_incarnation(comment);
    doc.insert_before(box_node, comment);
    assert_eq!(doc.parent_id(comment), Some(body));
    assert_eq!(doc.next_sibling(comment), Some(box_node));
    doc.remove_node(comment);
    assert_eq!(doc.parent_id(comment), None);
    doc.append_child(body, comment);
    assert_eq!(doc.node_incarnation(comment), incarnation);
}

#[test]
fn dropping_retained_sheet_clears_rules_before_slab_id_reuse() {
    let mut doc = document();
    let head = doc.query_selector("head").unwrap();
    let node = link(&mut doc, "/removed.css");
    doc.append_child(head, node);
    attach(&mut doc, node, 456);
    assert_eq!(width(&mut doc), "456px");
    doc.remove_node(node);
    assert_eq!(width(&mut doc), "10px");
    doc.document_mut().mutate().remove_and_drop_node(node);
    let replacement = link(&mut doc, "/pending.css");
    assert_eq!(replacement, node, "fixture must exercise slab ID reuse");
    doc.append_child(head, replacement);
    assert_eq!(width(&mut doc), "10px");
    doc.document_mut().remove_stylesheet_for_node(replacement);
    doc.document_mut().remove_stylesheet_for_node(replacement);
    assert_eq!(width(&mut doc), "10px");
}
