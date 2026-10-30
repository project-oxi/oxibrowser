use oxibrowser_render::{
    CaptureOpts, PreloadedStylesheet, PreloadedStylesheets, RenderDocument, Viewport,
};

fn stylesheets() -> PreloadedStylesheets {
    let mut sheets = PreloadedStylesheets::default();
    sheets.insert(PreloadedStylesheet {
        request_url: "https://example.com/css/app.css".into(),
        response_url: "https://example.com/css/app.css".into(),
        body: b"body {margin:0} #box {width:64px;height:64px;background:rgb(0,0,255)}".to_vec(),
    });
    sheets
}

fn pixel(doc: &mut RenderDocument) -> [u8; 4] {
    let png = doc.capture_png(&CaptureOpts::default()).unwrap();
    let mut reader = png::Decoder::new(std::io::Cursor::new(png))
        .read_info()
        .unwrap();
    let mut bytes = vec![0; reader.output_buffer_size().unwrap()];
    reader.next_frame(&mut bytes).unwrap();
    bytes[4 * (20 * 64 + 20)..4 * (20 * 64 + 20) + 4]
        .try_into()
        .unwrap()
}

const VIEWPORT: Viewport = Viewport {
    width: 64,
    height: 64,
    scale: 1.0,
};
const HTML: &str = "<html><head><link id='original' rel='stylesheet' href='/css/app.css' media='screen' nonce='proof' crossorigin='anonymous'></head><body><div id='box'></div></body></html>";

#[test]
fn real_stylesheet_bytes_paint_and_original_link_attributes_survive() {
    let mut doc = RenderDocument::from_html_with_resources(
        HTML,
        Some("https://example.com/page"),
        VIEWPORT,
        &[],
        &stylesheets(),
    )
    .unwrap();
    assert!(!doc.document().has_pending_critical_resources());
    let link = doc.query_selector("link[rel=stylesheet]").unwrap();
    for (name, value) in [
        ("id", "original"),
        ("href", "/css/app.css"),
        ("media", "screen"),
        ("nonce", "proof"),
        ("crossorigin", "anonymous"),
    ] {
        assert_eq!(doc.node_attr(link, name).as_deref(), Some(value));
    }
    assert_eq!(pixel(&mut doc), [0, 0, 255, 255]);
}

#[test]
fn link_and_inline_styles_follow_document_cascade_order() {
    let html = HTML.replace(
        "</head>",
        "<style>#box {background:rgb(255,0,0)}</style></head>",
    );
    let mut doc = RenderDocument::from_html_with_resources(
        &html,
        Some("https://example.com/page"),
        VIEWPORT,
        &[],
        &stylesheets(),
    )
    .unwrap();
    assert_eq!(pixel(&mut doc), [255, 0, 0, 255]);
}

#[test]
fn stylesheet_rel_tokens_match_ascii_case_insensitively_without_rewriting_dom() {
    let html = HTML.replace("rel='stylesheet'", "rel='StyleSheet'");
    let mut doc = RenderDocument::from_html_with_resources(
        &html,
        Some("https://example.com/page"),
        VIEWPORT,
        &[],
        &stylesheets(),
    )
    .unwrap();
    assert_eq!(
        doc.node_attr(doc.query_selector("#original").unwrap(), "rel")
            .as_deref(),
        Some("StyleSheet")
    );
    assert_eq!(pixel(&mut doc), [0, 0, 255, 255]);
}

#[test]
fn cyclic_import_does_not_reenter_the_memory_provider_forever() {
    let mut sheets = stylesheets();
    sheets.insert(PreloadedStylesheet {
        request_url: "https://example.com/css/app.css".into(),
        response_url: "https://example.com/css/app.css".into(),
        body: b"@import url('/css/app.css'); body{margin:0} #box{width:64px;height:64px;background:blue}".to_vec(),
    });
    let mut doc = RenderDocument::from_html_with_resources(
        HTML,
        Some("https://example.com/page"),
        VIEWPORT,
        &[],
        &sheets,
    )
    .unwrap();
    assert!(!doc.document().has_pending_critical_resources());
    assert_eq!(pixel(&mut doc), [0, 0, 255, 255]);
}

#[test]
fn unavailable_stylesheets_do_not_become_critical_or_bleed_between_documents() {
    let mut loaded = RenderDocument::from_html_with_resources(
        HTML,
        Some("https://example.com/page"),
        VIEWPORT,
        &[],
        &stylesheets(),
    )
    .unwrap();
    assert_eq!(pixel(&mut loaded), [0, 0, 255, 255]);
    let mut other =
        RenderDocument::from_html(HTML, Some("https://example.com/page"), VIEWPORT).unwrap();
    assert!(!other.document().has_pending_critical_resources());
    assert!(other.query_selector("#original").is_some());
    assert_eq!(pixel(&mut other), [255, 255, 255, 255]);
}

#[test]
fn malformed_urls_and_opaque_bases_preserve_dom_without_panicking() {
    for base in [
        None,
        Some("data:text/html,hello"),
        Some("about:blank"),
        Some("not a URL"),
    ] {
        let html = "<html><head><link rel='stylesheet' href='http://['><link id='relative' rel='stylesheet' href='app.css'></head><body><img src='pic.png'><p>visible</p></body></html>";
        let mut doc = RenderDocument::from_html(html, base, VIEWPORT).unwrap();
        assert!(!doc.document().has_pending_critical_resources());
        assert_eq!(
            doc.node_attr(doc.query_selector("#relative").unwrap(), "href")
                .as_deref(),
            Some("app.css")
        );
        assert!(!doc.capture_png(&CaptureOpts::default()).unwrap().is_empty());
    }
}

#[test]
fn redirected_stylesheet_parser_uses_final_source_url_and_restores_original_href() {
    let mut sheets = PreloadedStylesheets::default();
    sheets.insert(PreloadedStylesheet {
        request_url: "https://example.com/css/app.css".into(),
        response_url: "https://example.com/redirected/theme.css".into(),
        body: b"#box{width:64px;height:64px;background:blue}".to_vec(),
    });
    let mut doc = RenderDocument::from_html_with_resources(
        HTML,
        Some("https://example.com/page"),
        VIEWPORT,
        &[],
        &sheets,
    )
    .unwrap();
    let link = doc.query_selector("#original").unwrap();
    assert_eq!(doc.node_attr(link, "href").as_deref(), Some("/css/app.css"));
    let element = doc
        .document()
        .get_node(link)
        .unwrap()
        .element_data()
        .unwrap();
    let blitz_dom::node::SpecialElementData::Stylesheet(sheet) = &element.special_data else {
        panic!("loaded stylesheet")
    };
    let guard = sheet.0.shared_lock.read();
    let contents = sheet.0.contents.read_with(&guard);
    assert_eq!(
        contents
            .url_data
            .0
            .join("parts/colors.css")
            .unwrap()
            .as_str(),
        "https://example.com/redirected/parts/colors.css"
    );
    drop(guard);
    assert_eq!(pixel(&mut doc), [0, 0, 255, 255]);
}

#[test]
fn distinct_requests_sharing_a_final_url_keep_their_own_css_response() {
    let html = "<html><head><link id='link-a' rel='stylesheet' href='/a.css'><link id='link-b' rel='stylesheet' href='/b.css'></head><body><div id='a'></div><div id='b'></div></body></html>";
    let mut sheets = PreloadedStylesheets::default();
    for (request, body) in [
        ("a", "#a{width:10px;height:10px}"),
        ("b", "#b{width:20px;height:10px}"),
    ] {
        sheets.insert(PreloadedStylesheet {
            request_url: format!("https://example.com/{request}.css"),
            response_url: "https://example.com/redirected/theme.css".into(),
            body: body.as_bytes().to_vec(),
        });
    }
    let doc = RenderDocument::from_html_with_resources(
        html,
        Some("https://example.com/page"),
        VIEWPORT,
        &[],
        &sheets,
    )
    .unwrap();
    assert_eq!(
        doc.node_layout_rect(doc.query_selector("#a").unwrap()).2,
        10.0
    );
    assert_eq!(
        doc.node_layout_rect(doc.query_selector("#b").unwrap()).2,
        20.0
    );
    assert!(!doc.document().has_pending_critical_resources());
    for name in ["a", "b"] {
        let link = doc.query_selector(&format!("#link-{name}")).unwrap();
        assert_eq!(doc.node_attr(link, "href"), Some(format!("/{name}.css")));
        let element = doc
            .document()
            .get_node(link)
            .unwrap()
            .element_data()
            .unwrap();
        let blitz_dom::node::SpecialElementData::Stylesheet(sheet) = &element.special_data else {
            panic!("loaded CSS")
        };
        let guard = sheet.0.shared_lock.read();
        let contents = sheet.0.contents.read_with(&guard);
        assert_eq!(
            contents.url_data.0.join("nested.css").unwrap().as_str(),
            "https://example.com/redirected/nested.css"
        );
    }
}
