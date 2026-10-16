//! Form activation behavior (M1.5): submit-button click default action,
//! `form.submit()`, and `preventDefault` suppression — against a loopback
//! HTTP fixture so the actual POST body/Content-Type is verified server-side.

use oxibrowser_core::js::runtime::NativeInput;
use oxibrowser_core::session::Session;
use oxibrowser_core::{Browser, BrowserConfig, Tab};
use wiremock::matchers::{body_string, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const FORM_HTML: &str = concat!(
    "<!DOCTYPE html><html><head><title>form-page</title></head><body>",
    r#"<form id="f" method="POST" action="/login">"#,
    r#"<input name="user" value="alice">"#,
    r#"<input type="password" name="pass" value="s3cret">"#,
    r#"<input type="checkbox" name="remember" value="yes" checked>"#,
    r#"<input type="checkbox" name="skip" value="no">"#,
    r#"<textarea name="msg">hi</textarea>"#,
    r#"<select name="opt"><option value="1">one</option><option value="2" selected>two</option></select>"#,
    r#"<button type="submit" id="go">Go</button>"#,
    "</form>",
    r#"<div id="marker">home</div>"#,
    "</body></html>"
);

const POSTED_BODY: &str = "user=alice&pass=s3cret&remember=yes&msg=hi&opt=2";

fn form_page() -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_string(FORM_HTML)
        .insert_header("content-type", "text/html")
}

fn post_response_page() -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_string("<html><body><div id=\"marker\">posted</div></body></html>")
        .insert_header("content-type", "text/html")
}

fn expect_post_mock() -> Mock {
    Mock::given(method("POST"))
        .and(path("/login"))
        .and(body_string(POSTED_BODY))
        .and(header("content-type", "application/x-www-form-urlencoded"))
        .respond_with(post_response_page())
}

async fn make_tab() -> (MockServer, Tab) {
    let server = MockServer::start().await;
    let mut config = BrowserConfig::headless();
    config.enable_ssrf_filter = false;
    let browser = Browser::new(config).await.expect("browser");
    (server, browser.new_tab().await.expect("tab"))
}

async fn make_session() -> (MockServer, Session) {
    let server = MockServer::start().await;
    let mut config = BrowserConfig::headless();
    config.enable_ssrf_filter = false;
    let browser = Browser::new(config.clone()).await.expect("browser");
    let ctx = browser.default_context();
    let session = Session::new(browser.id(), config, ctx)
        .await
        .expect("session");
    (server, session)
}

/// (a) JS-eval click path: clicking a submit control POSTs the serialized form.
#[tokio::test]
async fn submit_button_click_posts_form() {
    let (server, tab) = make_tab().await;
    let get_scope = Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(form_page())
        .expect(1)
        .mount_as_scoped(&server)
        .await;
    let post_scope = expect_post_mock().expect(1).mount_as_scoped(&server).await;

    tab.goto(&format!("{}/", server.uri())).await.expect("goto");
    tab.click("#go").await.expect("click");

    // POST response became the page.
    let marker = tab
        .evaluate("document.getElementById('marker').textContent")
        .await
        .expect("marker");
    assert_eq!(marker, serde_json::json!("posted"));

    wiremock::MockServer::verify(&server).await;
    drop(get_scope);
    drop(post_scope);
}

/// (b) `form.submit()` JS binding POSTs the serialized form.
#[tokio::test]
async fn form_submit_js_posts_form() {
    let (server, tab) = make_tab().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(form_page())
        .mount(&server)
        .await;
    let post_scope = expect_post_mock().expect(1).mount_as_scoped(&server).await;

    tab.goto(&format!("{}/", server.uri())).await.expect("goto");
    tab.evaluate("document.getElementById('f').submit()")
        .await
        .expect("submit");

    let marker = tab
        .evaluate("document.getElementById('marker').textContent")
        .await
        .expect("marker");
    assert_eq!(marker, serde_json::json!("posted"));

    wiremock::MockServer::verify(&server).await;
    drop(post_scope);
}

/// (c) A `submit` listener calling `preventDefault` suppresses navigation.
#[tokio::test]
async fn prevent_default_blocks_submission() {
    let html = FORM_HTML.replace(
        "</form>",
        "</form><script>document.getElementById('f').addEventListener('submit', function(e){ e.preventDefault(); });</script>",
    );
    let (server, tab) = make_tab().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(html)
                .insert_header("content-type", "text/html"),
        )
        .mount(&server)
        .await;
    let post_scope = expect_post_mock().expect(0).mount_as_scoped(&server).await;

    tab.goto(&format!("{}/", server.uri())).await.expect("goto");
    tab.click("#go").await.expect("click");

    // Still on the original page — no navigation happened.
    let marker = tab
        .evaluate("document.getElementById('marker').textContent")
        .await
        .expect("marker");
    assert_eq!(marker, serde_json::json!("home"));

    wiremock::MockServer::verify(&server).await;
    drop(post_scope);
}

/// (a, native path) `native_act` click on a submit control also triggers the
/// form POST through the session.
#[tokio::test]
async fn native_click_posts_form() {
    let (server, mut session) = make_session().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(form_page())
        .mount(&server)
        .await;
    let post_scope = expect_post_mock().expect(1).mount_as_scoped(&server).await;

    let url = format!("{}/", server.uri());
    session.navigate(&url).await.expect("navigate");

    let observation = session.native_observe().await.expect("observe");
    let node_id = observation
        .snapshot
        .query_selector("#go")
        .expect("submit control in snapshot");
    let target = observation.target(node_id).expect("target");

    let receipt = session
        .native_act(target, NativeInput::Click)
        .await
        .expect("native click");
    assert!(
        receipt.navigation_requested,
        "submit click must request navigation"
    );

    wiremock::MockServer::verify(&server).await;
    drop(post_scope);
}
