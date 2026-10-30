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

fn pixels(doc: &mut RenderDocument) -> Vec<u8> {
    let png = doc.capture_png(&CaptureOpts::default()).unwrap();
    let mut reader = png::Decoder::new(std::io::Cursor::new(png))
        .read_info()
        .unwrap();
    let mut data = vec![0; reader.output_buffer_size().unwrap()];
    reader.next_frame(&mut data).unwrap();
    data
}

fn pixel(data: &[u8], x: usize, y: usize) -> &[u8] {
    &data[(y * 320 + x) * 4..(y * 320 + x + 1) * 4]
}

#[test]
fn absolute_initial_containing_block_matches_pixels_and_native_hit() {
    let mut doc = document(
        "<style>#marker{position:absolute;left:10px;top:120px;width:50px;height:20px;background:red}</style><h1>Heading</h1><div id='marker'></div>",
    );
    let marker = doc.query_selector("#marker").unwrap();
    assert_eq!(doc.node_layout_rect(marker), (10.0, 120.0, 50.0, 20.0));
    let data = pixels(&mut doc);
    assert_eq!(pixel(&data, 10, 120), &[255, 0, 0, 255]);
    assert_eq!(pixel(&data, 59, 139), &[255, 0, 0, 255]);
    assert_eq!(pixel(&data, 60, 139), &[255, 255, 255, 255]);
    assert_eq!(doc.document().hit(35.0, 130.0).unwrap().node_id, marker);
    // Repeated resolves and changed insets must not accumulate an offset.
    doc.set_inline_style(marker, "left", "160px");
    doc.flush_layout();
    assert_eq!(doc.node_layout_rect(marker), (160.0, 120.0, 50.0, 20.0));
    assert_eq!(doc.document().hit(180.0, 130.0).unwrap().node_id, marker);
}

#[test]
fn absolute_percentages_and_end_insets_use_the_initial_viewport() {
    let doc = document(
        "<div id='box' style='position:absolute;right:10px;bottom:5px;width:50%;height:20px'></div>",
    );
    assert_eq!(
        doc.node_layout_rect(doc.query_selector("#box").unwrap()),
        (150.0, 215.0, 160.0, 20.0)
    );
}

#[test]
fn absolute_descendant_skips_static_wrapper_for_positioned_padding_box() {
    let mut doc = document(
        "<style>#positioned{position:relative;margin:30px 0 0 20px;width:200px;height:100px;padding:10px;border:4px solid} #static{margin:7px;padding:3px;height:60px} #box{position:absolute;right:10px;bottom:5px;width:50%;height:20px;background:lime}</style><div id='positioned'><div id='static'><div id='box'></div></div></div>",
    );
    let host = doc.node_layout_rect(doc.query_selector("#positioned").unwrap());
    let node = doc.query_selector("#box").unwrap();
    assert_eq!(
        doc.node_layout_rect(node),
        (host.0 + 104.0, host.1 + 99.0, 110.0, 20.0)
    );
    let data = pixels(&mut doc);
    assert_eq!(
        pixel(&data, (host.0 + 110.0) as usize, (host.1 + 105.0) as usize),
        &[0, 255, 0, 255]
    );
    assert_eq!(
        doc.document()
            .hit((host.0 + 110.0) as f32, (host.1 + 105.0) as f32)
            .unwrap()
            .node_id,
        node
    );
}

#[test]
fn inline_text_has_real_glyph_geometry_content_quads_pixels_and_native_hit() {
    let mut doc = document(
        "<style>a{color:blue}</style><h1>Heading</h1><nav><a id='a'>Anchor</a> <a id='b'>Second</a></nav>",
    );
    let a = doc.query_selector("#a").unwrap();
    let b = doc.query_selector("#b").unwrap();
    let first = doc.node_layout_rect(a);
    let second = doc.node_layout_rect(b);
    assert!(first.2 > 40.0 && first.3 >= 16.0, "{first:?}");
    assert!(
        second.0 > first.0 + first.2 && second.2 > 40.0,
        "{second:?}"
    );
    let (content, _, _, _, width, height) = doc.node_box_quads(a).unwrap();
    assert_eq!((width, height), (first.2, first.3));
    assert_eq!(
        content,
        [
            first.0,
            first.1,
            first.0 + first.2,
            first.1,
            first.0 + first.2,
            first.1 + first.3,
            first.0,
            first.1 + first.3
        ]
    );
    assert_eq!(
        doc.document()
            .hit(
                (first.0 + first.2 / 2.0) as f32,
                (first.1 + first.3 / 2.0) as f32
            )
            .unwrap()
            .node_id,
        a
    );
    let data = pixels(&mut doc);
    let mut blue = 0;
    for y in first.1 as usize..(first.1 + first.3).ceil() as usize {
        for x in first.0 as usize..(first.0 + first.2).ceil() as usize {
            let color = pixel(&data, x, y);
            if color[2] > 180 && color[0] < 150 && color[1] < 150 {
                blue += 1;
            }
        }
    }
    assert!(blue > 20, "glyph box must contain actual painted blue text");
    doc.set_text(a, "Changed text");
    doc.flush_layout();
    assert!(doc.node_layout_rect(a).2 > first.2);
    doc.set_text(a, "");
    doc.flush_layout();
    assert_eq!(doc.node_layout_rect(a).2, 0.0);
}

#[test]
fn iframe_bitmap_paints_its_content_and_clear_preserves_dom_identity() {
    let mut doc = document("<iframe id='frame' width='60' height='40' style='border:0'></iframe>");
    let frame = doc.query_selector("#frame").unwrap();
    let incarnation = doc.node_incarnation(frame);
    let rgba = std::sync::Arc::new([0, 0, 255, 255].repeat(60 * 40));
    assert!(doc.set_canvas_bitmap(frame, 60, 40, rgba));
    let data = pixels(&mut doc);
    let rect = doc.node_layout_rect(frame);
    assert_eq!(
        pixel(&data, rect.0 as usize + 5, rect.1 as usize + 5),
        &[0, 0, 255, 255]
    );
    assert!(doc.clear_canvas_bitmap(frame));
    let cleared = pixels(&mut doc);
    assert_eq!(
        pixel(&cleared, rect.0 as usize + 5, rect.1 as usize + 5),
        &[255, 255, 255, 255]
    );
    assert_eq!(doc.node_incarnation(frame), incarnation);
    assert!(!doc.clear_canvas_bitmap(frame));
}

#[test]
fn checkbox_nested_in_inline_label_shares_paint_quad_and_native_hit() {
    let mut doc = document(include_str!("../../../tests/spa/fixtures/vanilla.html"));
    let checkbox = doc.query_selector("#agree").unwrap();
    let form = doc.query_selector("#form").unwrap();
    let label = doc.parent_id(checkbox).unwrap();
    assert_eq!(doc.tag_name(label).as_deref(), Some("label"));
    assert_eq!(
        doc.document()
            .get_node(checkbox)
            .unwrap()
            .layout_parent
            .get(),
        Some(form)
    );
    let (content, _, border, _, _, _) = doc.node_box_quads(checkbox).unwrap();
    let center = (
        (content[0] + content[4]) / 2.0,
        (content[1] + content[5]) / 2.0,
    );
    assert_eq!(
        doc.document()
            .hit(center.0 as f32, center.1 as f32)
            .unwrap()
            .node_id,
        checkbox
    );
    let rect = doc.node_layout_rect(checkbox);
    assert_eq!(
        border,
        [
            rect.0,
            rect.1,
            rect.0 + rect.2,
            rect.1,
            rect.0 + rect.2,
            rect.1 + rect.3,
            rect.0,
            rect.1 + rect.3
        ]
    );
    let data = pixels(&mut doc);
    assert!(
        pixel(&data, rect.0 as usize, (rect.1 + rect.3 / 2.0) as usize)[0] < 240,
        "reported border must cover the actually painted checkbox outline"
    );
    assert_eq!(doc.parent_id(checkbox), Some(label));
}
