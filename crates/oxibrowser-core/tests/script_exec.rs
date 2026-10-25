//! Phase 1 integration test: navigation fetches and executes an EXTERNAL
//! `<script src>` end-to-end through the public `Browser`/`Tab` API, proving
//! the keystone works against a real HTTP server (the path real SPA bundles take).

use oxibrowser_core::{Browser, BrowserConfig, Tab};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn make_tab() -> Tab {
    let mut config = BrowserConfig::headless();
    // Loopback mock server — must disable the SSRF filter to reach it.
    config.enable_ssrf_filter = false;
    let browser = Browser::new(config).await.expect("browser");
    browser.new_tab().await.expect("tab")
}

#[tokio::test]
async fn navigate_executes_external_script() {
    let server = MockServer::start().await;

    let html = concat!(
        "<!DOCTYPE html><html><head></head><body>",
        r#"<div id="app">placeholder</div>"#,
        r#"<script src="app.js"></script>"#,
        "</body></html>"
    );
    Mock::given(method("GET"))
        .and(path("/pages/index.html"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(html)
                .insert_header("content-type", "text/html"),
        )
        .mount(&server)
        .await;

    let js = "document.getElementById('app').textContent = 'from-external';";
    Mock::given(method("GET"))
        .and(path("/pages/app.js"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(js)
                .insert_header("content-type", "application/javascript"),
        )
        .mount(&server)
        .await;

    let tab = make_tab().await;
    let url = format!("{}/pages/index.html", server.uri());
    tab.goto(&url).await.expect("navigate");

    let val = tab
        .evaluate("document.getElementById('app').textContent")
        .await
        .expect("evaluate");
    assert_eq!(
        val,
        serde_json::json!("from-external"),
        "external <script src> fetched and executed during navigation"
    );
    let script_src = tab
        .evaluate("document.querySelector('script').src")
        .await
        .expect("evaluate script URL");
    assert_eq!(
        script_src,
        serde_json::json!(format!("{}/pages/app.js", server.uri())),
        "script.src resolves relative to the document URL"
    );
    let raw_src = tab
        .evaluate("document.querySelector('script').getAttribute('src')")
        .await
        .expect("evaluate raw script URL");
    assert_eq!(raw_src, serde_json::json!("app.js"));
}

#[tokio::test]
async fn navigate_executes_inline_and_external_in_order() {
    let server = MockServer::start().await;

    // Inline script runs first (document order), external second. The external
    // script appends to a value the inline script set, proving both execute
    // and that ordering is preserved across the inline/external boundary.
    let html = concat!(
        "<!DOCTYPE html><html><head></head><body>",
        r#"<script>window.__seq = (window.__seq || '') + 'inline-';</script>"#,
        r#"<script src="/b.js"></script>"#,
        "</body></html>"
    );
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(html)
                .insert_header("content-type", "text/html"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/b.js"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("window.__seq = (window.__seq || '') + 'external';")
                .insert_header("content-type", "application/javascript"),
        )
        .mount(&server)
        .await;

    let tab = make_tab().await;
    let url = format!("{}/", server.uri());
    tab.goto(&url).await.expect("navigate");

    let val = tab.evaluate("window.__seq").await.expect("evaluate");
    assert_eq!(
        val,
        serde_json::json!("inline-external"),
        "inline then external scripts execute in document order"
    );
}

#[tokio::test]
async fn navigate_renders_mini_spa_on_dom_content_loaded() {
    // Realistic framework pattern: an external bundle waits for
    // DOMContentLoaded, then renders a list into #app via createElement /
    // appendChild. Proves the keystone for SPA bootstraps end to end.
    let server = MockServer::start().await;
    let html = concat!(
        "<!DOCTYPE html><html><head></head><body>",
        r#"<ul id="app"></ul>"#,
        r#"<script src="/bundle.js"></script>"#,
        "</body></html>"
    );
    let bundle = concat!(
        "document.addEventListener('DOMContentLoaded', function () {",
        "  var app = document.getElementById('app');",
        "  ['alpha','beta','gamma'].forEach(function (t) {",
        "    var li = document.createElement('li');",
        "    li.textContent = t;",
        "    app.appendChild(li);",
        "  });",
        "});"
    );
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(html)
                .insert_header("content-type", "text/html"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/bundle.js"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(bundle)
                .insert_header("content-type", "application/javascript"),
        )
        .mount(&server)
        .await;

    let tab = make_tab().await;
    let url = format!("{}/", server.uri());
    tab.goto(&url).await.expect("navigate");

    let count = tab
        .evaluate("document.querySelectorAll('#app li').length")
        .await
        .expect("evaluate count");
    assert_eq!(
        count,
        serde_json::json!(3),
        "three <li> rendered by the bundle"
    );

    let text = tab
        .evaluate("document.getElementById('app').textContent")
        .await
        .expect("evaluate text");
    assert_eq!(
        text,
        serde_json::json!("alphabetagamma"),
        "rendered list text content"
    );
}

#[tokio::test]
async fn wait_for_finds_element_present_only_in_live_dom() {
    // A setTimeout creates #late ~80ms after load. The element exists only in
    // the LIVE (post-JS) DOM, never in the static navigate-time snapshot.
    // wait_for must observe the live DOM (Playwright-style) to find it; the
    // old static-snapshot poll would time out.
    let server = MockServer::start().await;
    let html = concat!(
        "<!DOCTYPE html><html><body>",
        "<script>setTimeout(function () {",
        "  var el = document.createElement('div');",
        "  el.id = 'late';",
        "  document.body.appendChild(el);",
        "}, 80);</script>",
        "</body></html>"
    );
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(html)
                .insert_header("content-type", "text/html"),
        )
        .mount(&server)
        .await;

    let tab = make_tab().await;
    tab.goto(&format!("{}/", server.uri()))
        .await
        .expect("navigate");

    tab.wait_for("#late", 2000)
        .await
        .expect("wait_for must find the element rendered into the live DOM");
    let count = tab
        .evaluate("document.querySelectorAll('#late').length")
        .await
        .expect("evaluate");
    assert_eq!(count, serde_json::json!(1));
}
