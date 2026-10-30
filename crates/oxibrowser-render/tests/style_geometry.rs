//! Style and geometry observations must describe the same state as capture.

use oxibrowser_render::{CaptureOpts, RenderDocument, Viewport};

fn document(html: &str) -> RenderDocument {
    RenderDocument::from_html(
        html,
        None,
        Viewport {
            width: 320,
            height: 240,
            scale: 1.0,
        },
    )
    .unwrap()
}

#[test]
fn resolved_styles_use_cascade_inheritance_and_used_box_sizes() {
    let mut doc = document(
        r#"<style>
        body { margin: 0; color: #123456; font: 20px/1.5 sans-serif; --accent: #123456; }
        #outer { width: 200px; }
        .box { width: 50%; height: 30px; padding: 5px; border: 2px solid; opacity: .4; }
        .changed { color: #ff0000; width: 80px; visibility: hidden; }
        #border { box-sizing: border-box; width: 100px; }
    </style><div id="outer"><div id="box" class="box"></div><div id="border" class="box"></div></div>"#,
    );
    let node = doc.query_selector("#box").unwrap();
    assert_eq!(doc.resolved_style(node, "color"), "rgb(18, 52, 86)");
    assert_eq!(
        doc.resolved_style(node, "border-top-color"),
        "rgb(18, 52, 86)"
    );
    assert_eq!(doc.resolved_style(node, "font-size"), "20px");
    assert_eq!(doc.resolved_style(node, "font-family"), "sans-serif");
    assert_eq!(doc.resolved_style(node, "--accent"), "#123456");
    assert_eq!(doc.resolved_style(node, "display"), "block");
    assert_eq!(doc.resolved_style(node, "visibility"), "visible");
    assert_eq!(doc.resolved_style(node, "opacity"), "0.4");
    assert_eq!(doc.resolved_style(node, "width"), "100px");
    assert_eq!(doc.resolved_style(node, "height"), "30px");
    assert_eq!(doc.resolved_style(node, "padding"), "5px");
    let outer = doc.query_selector("#outer").unwrap();
    assert_eq!(doc.resolved_style(outer, "height"), "74px");
    assert_eq!(doc.node_layout_rect(node).2, 114.0);
    let border = doc.query_selector("#border").unwrap();
    assert_eq!(doc.resolved_style(border, "width"), "100px");
    assert_eq!(doc.node_layout_rect(border).2, 100.0);
    assert_eq!(doc.resolved_style(node, "unknown-property"), "");
    assert_eq!(doc.resolved_style(usize::MAX, "color"), "");

    doc.set_attribute(node, "class", "box changed");
    doc.flush_layout();
    assert_eq!(doc.resolved_style(node, "color"), "rgb(255, 0, 0)");
    assert_eq!(doc.resolved_style(node, "width"), "80px");
    assert_eq!(doc.resolved_style(node, "visibility"), "hidden");
    doc.set_inline_style(node, "width", "90px");
    doc.flush_layout();
    assert_eq!(doc.resolved_style(node, "width"), "90px");
    assert_eq!(doc.node_layout_rect(node).2, 104.0);
}

#[test]
fn nested_boxes_scroll_and_capture_share_viewport_coordinates() {
    let mut doc = document(
        r#"<style>
        body { margin: 8px; }
        #outer { margin: 12px 0 0 20px; padding: 10px; border: 2px solid black;
                 width: 200px; height: 100px; overflow: auto; }
        #box { margin: 7px 0 0 9px; width: 40px; height: 30px; background: red; }
        #spacer { width: 400px; height: 500px; }
    </style><div id="outer"><div id="box"></div><div id="spacer"></div></div>"#,
    );
    let outer = doc.query_selector("#outer").unwrap();
    let node = doc.query_selector("#box").unwrap();
    let initial_outer = doc.node_layout_rect(outer);
    let initial = doc.node_layout_rect(node);
    assert_eq!(initial_outer, (28.0, 12.0, 224.0, 124.0));
    assert_eq!(initial, (49.0, 31.0, 40.0, 30.0));
    // The scroller's border stays put; only descendant boxes move.
    let inner = doc.document_mut().get_node_mut(outer).unwrap();
    inner.scroll_offset.x = 5.0;
    inner.scroll_offset.y = 8.0;
    doc.document_mut()
        .set_viewport_scroll(blitz_dom::Point { x: 3.0, y: 4.0 });
    doc.flush_layout();
    assert_eq!(doc.node_layout_rect(outer), (25.0, 8.0, 224.0, 124.0));
    assert_eq!(doc.node_layout_rect(node), (41.0, 19.0, 40.0, 30.0));
    assert_quad(
        doc.node_box_quads(node).unwrap().2,
        [41.0, 19.0, 81.0, 19.0, 81.0, 49.0, 41.0, 49.0],
    );
    let png = doc.capture_png(&CaptureOpts::default()).unwrap();
    let mut reader = png::Decoder::new(std::io::Cursor::new(png))
        .read_info()
        .unwrap();
    let mut rgba = vec![0; reader.output_buffer_size().unwrap()];
    reader.next_frame(&mut rgba).unwrap();
    let pixel = |x: usize, y: usize| &rgba[(y * 320 + x) * 4..(y * 320 + x + 1) * 4];
    assert_eq!(pixel(45, 25), &[255, 0, 0, 255]);
    assert_ne!(pixel(85, 25), &[255, 0, 0, 255]);

    doc.set_inline_style(node, "margin-left", "19px");
    doc.flush_layout();
    assert_eq!(doc.node_layout_rect(node), (51.0, 19.0, 40.0, 30.0));
}

#[test]
fn transformed_and_hidden_geometry_are_observable() {
    let mut doc = document(
        r#"<style>
        body { margin: 0; }
        #outer { margin: 10px; transform: translate(20px, 30px); }
        #box { width: 40px; height: 20px; transform-origin: 0 0; transform: scale(2); }
    </style><div id="outer"><div id="box"></div></div>"#,
    );
    let node = doc.query_selector("#box").unwrap();
    assert_eq!(doc.node_layout_rect(node), (30.0, 40.0, 80.0, 40.0));
    assert_eq!(doc.resolved_style(node, "width"), "40px");
    let outer = doc.query_selector("#outer").unwrap();
    doc.set_inline_style(outer, "display", "none");
    doc.flush_layout();
    assert_eq!(doc.node_layout_rect(node), (0.0, 0.0, 0.0, 0.0));
    assert_eq!(doc.resolved_style(node, "width"), "");
    assert_eq!(doc.node_layout_rect(usize::MAX), (0.0, 0.0, 0.0, 0.0));
    doc.set_inline_style(outer, "display", "block");
    doc.flush_layout();
    doc.remove_node(node);
    doc.flush_layout();
    assert_eq!(doc.node_layout_rect(node), (0.0, 0.0, 0.0, 0.0));
}

fn assert_quad(actual: [f64; 8], expected: [f64; 8]) {
    for (index, (actual, expected)) in actual.into_iter().zip(expected).enumerate() {
        assert!(
            (actual - expected).abs() < 0.0001,
            "coordinate {index}: {actual} != {expected}"
        );
    }
}

#[test]
fn box_model_quads_preserve_distinct_edges_and_rotated_corners() {
    let mut doc = document(
        r#"<style>
        body { margin: 0; }
        #outer { margin: 10px 0 0 20px; padding: 3px; border: 2px solid;
                 transform: translate(7px, 11px); }
        #box { width: 40px; height: 20px; margin: 5px 6px 7px 8px;
               padding: 2px 4px 6px 8px; border-style: solid;
               border-width: 1px 3px 5px 7px;
               transform-origin: 0 0; transform: rotate(90deg); }
    </style><div id="outer"><div id="box"></div></div>"#,
    );
    let node = doc.query_selector("#box").unwrap();
    let (content, padding, border, margin, width, height) = doc.node_box_quads(node).unwrap();
    assert_eq!((width, height), (62.0, 34.0));
    assert_quad(border, [40.0, 31.0, 40.0, 93.0, 6.0, 93.0, 6.0, 31.0]);
    assert_quad(padding, [39.0, 38.0, 39.0, 90.0, 11.0, 90.0, 11.0, 38.0]);
    assert_quad(content, [37.0, 46.0, 37.0, 86.0, 17.0, 86.0, 17.0, 46.0]);
    assert_quad(margin, [45.0, 23.0, 45.0, 99.0, -1.0, 99.0, -1.0, 23.0]);
    let rect = doc.node_layout_rect(node);
    assert_quad(
        [rect.0, rect.1, rect.2, rect.3, 0.0, 0.0, 0.0, 0.0],
        [6.0, 31.0, 34.0, 62.0, 0.0, 0.0, 0.0, 0.0],
    );

    doc.document_mut()
        .set_viewport_scroll(blitz_dom::Point { x: 3.0, y: 4.0 });
    doc.flush_layout();
    let shifted = doc.node_box_quads(node).unwrap().2;
    for index in 0..8 {
        let expected = border[index] - if index % 2 == 0 { 3.0 } else { 4.0 };
        assert!((shifted[index] - expected).abs() < 0.0001);
    }
}

#[test]
fn box_model_quads_reject_absent_boxes_and_unsupported_3d() {
    let mut doc =
        document("<div id='outer'><div id='box' style='width:40px;height:20px'></div></div>");
    let node = doc.query_selector("#box").unwrap();
    let outer = doc.query_selector("#outer").unwrap();
    assert!(doc.node_box_quads(node).is_some());
    for (property, value) in [
        ("transform", "rotateX(45deg)"),
        ("rotate", "x 45deg"),
        ("translate", "0 0 10px"),
        ("scale", "1 1 2"),
        ("perspective", "100px"),
    ] {
        doc.set_inline_style(outer, property, value);
        doc.flush_layout();
        assert!(
            doc.node_box_quads(node).is_none(),
            "unsupported {property}: {value}"
        );
        doc.remove_inline_style(outer, property);
        doc.flush_layout();
        assert!(doc.node_box_quads(node).is_some());
    }
    doc.set_inline_style(outer, "display", "none");
    doc.flush_layout();
    assert!(doc.node_box_quads(node).is_none());
    assert!(doc.node_box_quads(usize::MAX).is_none());
}

#[test]
fn attached_response_stylesheet_replaces_rules_without_rewriting_link() {
    let mut doc = RenderDocument::from_html(
        r#"<head><link id="sheet" rel="stylesheet" href="/theme.css"></head>
        <body><div id="box">Content</div></body>"#,
        Some("https://example.test/app"),
        Viewport::default(),
    )
    .unwrap();
    let sheet = doc.query_selector("#sheet").unwrap();
    let node = doc.query_selector("#box").unwrap();
    doc.attach_stylesheet(
        sheet,
        "https://cdn.example.test/theme.css",
        "#box { color: red; width: 80px; }",
    )
    .unwrap();
    doc.flush_layout();
    assert_eq!(doc.resolved_style(node, "color"), "rgb(255, 0, 0)");
    assert_eq!(doc.resolved_style(node, "width"), "80px");
    assert_eq!(doc.node_attr(sheet, "href").as_deref(), Some("/theme.css"));
    assert!(!doc.document().has_pending_critical_resources());
    assert_eq!(
        doc.preloaded_stylesheets()
            .iter()
            .next()
            .unwrap()
            .response_url,
        "https://cdn.example.test/theme.css"
    );
    doc.attach_stylesheet(
        sheet,
        "https://cdn.example.test/theme.css",
        "#box { color: blue; width: 100px; }",
    )
    .unwrap();
    doc.flush_layout();
    assert_eq!(doc.resolved_style(node, "color"), "rgb(0, 0, 255)");
    assert_eq!(doc.resolved_style(node, "width"), "100px");
    assert!(
        doc.attach_stylesheet(node, "https://example.test/x.css", "")
            .is_err()
    );
    assert!(doc.attach_stylesheet(sheet, "invalid url", "").is_err());
}

#[test]
fn text_node_mutation_updates_data_without_replacing_identity() {
    let mut doc = document("<div id='host'>old text</div>");
    let host = doc.query_selector("#host").unwrap();
    let text = doc.first_child(host).unwrap();
    doc.set_text(text, "updated text");
    doc.flush_layout();
    assert_eq!(doc.first_child(host), Some(text));
    assert_eq!(doc.text_of(text).as_deref(), Some("updated text"));
    assert_eq!(doc.node_text(host), "updated text");
    assert!(doc.children_ids(text).is_empty());
}

#[test]
fn dynamic_head_links_defer_fetch_without_blocking_capture() {
    let mut doc = document(
        "<head></head><body><div style='width:40px;height:20px;background:red'></div></body>",
    );
    let head = doc.query_selector("head").unwrap();
    let link = doc.create_element("link");
    doc.set_attribute(link, "rel", "stylesheet");
    doc.set_attribute(link, "href", "https://example.test/pending.css");
    doc.append_child(head, link);
    doc.flush_layout();
    assert!(!doc.document().has_pending_critical_resources());
    assert_eq!(doc.node_attr(link, "rel").as_deref(), Some("stylesheet"));
    doc.set_attribute(link, "href", "https://example.test/replacement.css");
    doc.flush_layout();
    assert!(!doc.document().has_pending_critical_resources());
    assert_eq!(
        doc.node_attr(link, "href").as_deref(),
        Some("https://example.test/replacement.css")
    );
    let png = doc.capture_png(&CaptureOpts::default()).unwrap();
    let mut reader = png::Decoder::new(std::io::Cursor::new(png))
        .read_info()
        .unwrap();
    let mut rgba = vec![0; reader.output_buffer_size().unwrap()];
    reader.next_frame(&mut rgba).unwrap();
    assert_eq!(
        &rgba[(10 * 320 + 10) * 4..(10 * 320 + 11) * 4],
        &[255, 0, 0, 255]
    );
    doc.attach_stylesheet(
        link,
        "https://example.test/replacement.css",
        "div { width: 60px !important; }",
    )
    .unwrap();
    doc.flush_layout();
    assert_eq!(
        doc.resolved_style(doc.query_selector("div").unwrap(), "width"),
        "60px"
    );
}

#[test]
fn removing_pending_stylesheet_href_does_not_panic_or_block_capture() {
    let mut doc = document("<html><head></head><body>pending stylesheet</body></html>");
    let link = doc.create_element("link");
    doc.set_attribute(link, "rel", "stylesheet");
    doc.set_attribute(link, "href", "https://fixture.invalid/pending.css");
    let head = doc.query_selector("head").unwrap();
    doc.append_child(head, link);
    doc.remove_attribute(link, "href");
    assert!(doc.node_attr(link, "href").is_none());
    assert_eq!(doc.node_attr(link, "rel").as_deref(), Some("stylesheet"));
    doc.flush_layout();
    assert!(!doc.capture_png(&CaptureOpts::default()).unwrap().is_empty());
}

#[test]
fn canvas_pixel_updates_preserve_dom_incarnations_and_tree_revision() {
    let mut doc = document("<canvas id='canvas'></canvas><button id='go'>Go</button>");
    let canvas = doc.query_selector("#canvas").unwrap();
    let button = doc.query_selector("#go").unwrap();
    let incarnation = doc.node_incarnation(button);
    let revision = doc.tree_revision();
    assert!(doc.set_canvas_bitmap(canvas, 1, 1, std::sync::Arc::new(vec![255, 0, 0, 255])));
    assert_eq!(doc.node_incarnation(button), incarnation);
    assert_eq!(doc.tree_revision(), revision);
    doc.capture_png(&CaptureOpts::default()).unwrap();
    assert_eq!(doc.node_incarnation(button), incarnation);
}
