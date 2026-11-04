//! Page-driven popup / new-tab support (M2 engine part): `window.open(...)`
//! and the default action of a `target="_blank"` link click both journal a
//! tab-spawn request that `Session` translates into a `CoreEvent::TabSpawned`
//! on the attached event sink. The observer (upper layer) owns session
//! creation — a `Session` holds no `Browser` handle — so the "new session"
//! half of each test is the upper-layer contract: on `TabSpawned` the test
//! creates a sibling session on the same browser and navigates it, exactly
//! what the bin layer is required to do.

use oxibrowser_core::js::runtime::{CoreEvent, NativeInput};
use oxibrowser_core::session::Session;
use oxibrowser_core::{Browser, BrowserConfig};
use std::sync::mpsc;
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LINK_HTML: &str = concat!(
    "<!DOCTYPE html><html><head><title>link-page</title></head><body>",
    r#"<a id="pop" href="/popup.html" target="_blank">open</a>"#,
    r#"<a id="same" href="/same.html">same tab</a>"#,
    r#"<a id="noref" href="/popup.html">no target</a>"#,
    "</body></html>"
);

fn link_page() -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_string(LINK_HTML)
        .insert_header("content-type", "text/html")
}

async fn make_session() -> (MockServer, Browser, Session, mpsc::Receiver<CoreEvent>) {
    let server = MockServer::start().await;
    let mut config = BrowserConfig::headless();
    config.enable_ssrf_filter = false;
    let browser = Browser::new(config.clone()).await.expect("browser");
    let ctx = browser.default_context();
    let mut session = Session::new(browser.id(), config, ctx)
        .await
        .expect("session");
    let (tx, rx) = mpsc::channel::<CoreEvent>();
    session.set_event_sink(tx);
    (server, browser, session, rx)
}

fn recv_tab_spawned(rx: &mpsc::Receiver<CoreEvent>) -> (String, String, String) {
    let deadline = Duration::from_secs(5);
    loop {
        let ev = rx
            .recv_timeout(deadline)
            .expect("TabSpawned event within deadline");
        if let CoreEvent::TabSpawned {
            url,
            target,
            source_session_id,
        } = ev
        {
            return (url, target, source_session_id);
        }
    }
}

/// (a) Scripted `window.open` journals a tab spawn; the session emits
/// `CoreEvent::TabSpawned` with the resolved URL, and the upper-layer
/// contract (create session + navigate) succeeds against the loopback.
#[tokio::test]
async fn window_open_emits_tab_spawned_and_upper_layer_creates_session() {
    let (server, browser, mut session, rx) = make_session().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(link_page())
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/popup.html"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<html><body><div id=\"m\">popup</div></body></html>")
                .insert_header("content-type", "text/html"),
        )
        .mount(&server)
        .await;

    session
        .navigate(&format!("{}/", server.uri()))
        .await
        .expect("navigate");

    let result = session
        .evaluate_js("window.open('/popup.html', '_blank')")
        .await
        .expect("evaluate");
    assert!(
        result.exception.is_none(),
        "window.open must not throw: {result:?}"
    );

    let (url, target, source_session_id) = recv_tab_spawned(&rx);
    assert_eq!(url, format!("{}/popup.html", server.uri()));
    assert_eq!(target, "_blank");
    assert_eq!(source_session_id, session.id().to_string());

    // Upper-layer contract: create a sibling session on the same browser and
    // navigate it to the spawned URL.
    let mut spawned = {
        let ctx = browser.default_context();
        Session::new(browser.id(), BrowserConfig::headless(), ctx)
            .await
            .expect("spawned session")
    };
    spawned.navigate(&url).await.expect("spawned navigate");
    let marker = spawned
        .evaluate_js("document.getElementById('m').textContent")
        .await
        .expect("evaluate");
    assert_eq!(marker.value, Some(serde_json::json!("popup")));
}

/// `window.open` with an explicit named target and default `_blank` both
/// report the requested target verbatim.
#[tokio::test]
async fn window_open_reports_named_target() {
    let (server, _browser, mut session, rx) = make_session().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(link_page())
        .mount(&server)
        .await;
    session
        .navigate(&format!("{}/", server.uri()))
        .await
        .expect("navigate");

    session
        .evaluate_js("window.open('/popup.html', 'aux')")
        .await
        .expect("evaluate");
    let (_, target, _) = recv_tab_spawned(&rx);
    assert_eq!(target, "aux");

    session
        .evaluate_js("open('/popup.html')")
        .await
        .expect("eval");
    let (_, target, _) = recv_tab_spawned(&rx);
    assert_eq!(target, "_blank", "window.open defaults to _blank");
}

/// (b) Native click on a `target="_blank"` anchor triggers the tab-spawn
/// default action through the same event.
#[tokio::test]
async fn native_click_on_blank_anchor_emits_tab_spawned() {
    let (server, _browser, mut session, rx) = make_session().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(link_page())
        .mount(&server)
        .await;
    session
        .navigate(&format!("{}/", server.uri()))
        .await
        .expect("navigate");

    let observation = session.native_observe().await.expect("observe");
    let node_id = observation
        .snapshot
        .query_selector("#pop")
        .expect("anchor in snapshot");
    let target = observation.target(node_id).expect("target");
    session
        .native_act(target, NativeInput::Click)
        .await
        .expect("native click");

    let (url, event_target, source) = recv_tab_spawned(&rx);
    assert_eq!(url, format!("{}/popup.html", server.uri()));
    assert_eq!(event_target, "_blank");
    assert_eq!(source, session.id().to_string());
}

/// A plain (no-target) anchor click does NOT spawn a tab — only `target="_blank"`.
#[tokio::test]
async fn plain_anchor_click_does_not_spawn() {
    let (server, _browser, mut session, rx) = make_session().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(link_page())
        .mount(&server)
        .await;
    for route in ["/same.html", "/popup.html"] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(link_page())
            .mount(&server)
            .await;
    }
    session
        .navigate(&format!("{}/", server.uri()))
        .await
        .expect("navigate");

    for (sel, destination) in [("#same", "/same.html"), ("#noref", "/popup.html")] {
        let observation = session.native_observe().await.expect("observe");
        let node_id = observation.snapshot.query_selector(sel).expect("anchor");
        let target = observation.target(node_id).expect("target");
        session
            .native_act(target, NativeInput::Click)
            .await
            .expect("native click");
        assert_eq!(session.current_url().unwrap().path(), destination);
    }

    let events: Vec<_> = rx.try_iter().collect();
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, CoreEvent::TabSpawned { .. })),
        "no TabSpawned expected, got {events:?}"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}
