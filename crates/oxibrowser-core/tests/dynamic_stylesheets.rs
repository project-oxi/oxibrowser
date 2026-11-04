//! Dynamic stylesheet requests use the fetch bridge and commit their real CSS
//! before delivering a load event. Idle queries/captures do not pump JavaScript.
use oxibrowser_core::js::runtime::{FetchRequestMsg, FetchResponseMsg};
use oxibrowser_core::js::{JsRuntime, JsRuntimeConfig};
use serde_json::{Value, json};
use std::sync::mpsc;
use std::time::Duration;

async fn runtime() -> JsRuntime {
    let mut runtime = JsRuntime::with_config(JsRuntimeConfig {
        background_event_loop: true,
        ..Default::default()
    });
    runtime.set_page_url("https://styles.example/app");
    runtime
        .set_document(
            "<html><head></head><body><div id='box'>initial</div><p id='events'></p></body></html>",
            Some("https://styles.example/app"),
            (320, 240),
        )
        .await
        .unwrap();
    runtime
}
async fn eval(runtime: &mut JsRuntime, source: &str) -> Value {
    let result = runtime.evaluate(source).await.unwrap();
    assert!(result.exception.is_none(), "{result:?}");
    result.value.unwrap_or(Value::Null)
}
fn bridge(
    runtime: &mut JsRuntime,
) -> (
    mpsc::Receiver<FetchRequestMsg>,
    mpsc::Sender<FetchResponseMsg>,
) {
    let (tx, requests) = mpsc::channel();
    let (responses, rx) = mpsc::channel();
    runtime.set_fetch_channel(tx, rx);
    (requests, responses)
}
fn response(request: FetchRequestMsg, status: u16, css: &str) -> FetchResponseMsg {
    FetchResponseMsg {
        id: request.id,
        status,
        status_text: String::new(),
        url: request.url,
        headers: vec![("content-type".into(), "text/css".into())],
        body: css.into(),
        body_bytes: Some(css.as_bytes().to_vec()),
        error: None,
    }
}
async fn event_text(runtime: &mut JsRuntime) -> String {
    runtime.query_selector_all("#events").await.unwrap()[0]
        .text
        .clone()
}
async fn wait_event(runtime: &mut JsRuntime, expected: &str) {
    for _ in 0..100 {
        if event_text(runtime).await == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "expected events {expected:?}, got {:?}",
        event_text(runtime).await
    );
}

#[tokio::test]
async fn dynamic_sheet_fetches_once_applies_css_before_load_and_capture_finishes() {
    let mut runtime = runtime().await;
    let (requests, responses) = bridge(&mut runtime);
    eval(&mut runtime, "document.head.addEventListener('load',()=>document.querySelector('#events').textContent+='capture,',true);document.head.onload=()=>document.querySelector('#events').textContent+='bad-bubble,';globalThis.sheet=document.createElement('link');sheet.rel='stylesheet';sheet.href='/route.css';sheet.addEventListener('load',e=>{document.querySelector('#events').textContent += (e.target===sheet && e.currentTarget===sheet && !e.bubbles ? 'listener,' : 'bad,')},{once:true});sheet.onload=()=>{document.querySelector('#events').textContent += getComputedStyle(document.querySelector('#box')).width};document.head.appendChild(sheet)").await;
    let request = requests.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(request.url, "https://styles.example/route.css");
    assert_eq!(request.origin.as_deref(), Some("https://styles.example"));
    assert_eq!(request.method, "GET");
    assert_eq!(event_text(&mut runtime).await, "");
    responses
        .send(response(
            request,
            200,
            "#box { width: 137px; height: 41px; background: rgb(4, 19, 91) }",
        ))
        .unwrap();
    wait_event(&mut runtime, "capture,listener,137px").await;
    assert_eq!(eval(&mut runtime, "[document.querySelector('#box').getBoundingClientRect().width,getComputedStyle(document.querySelector('#box')).height]").await, json!([137, "41px"]));
    let png = tokio::time::timeout(
        Duration::from_secs(2),
        runtime.capture_png(Default::default()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(png.starts_with(b"\x89PNG"));
    assert!(
        requests.try_recv().is_err(),
        "same link should not be fetched again"
    );
}

#[tokio::test]
async fn stylesheet_failure_dispatches_error_without_fabricated_load_or_capture_hang() {
    let mut runtime = runtime().await;
    let (requests, responses) = bridge(&mut runtime);
    eval(&mut runtime, "globalThis.sheet=document.createElement('link');sheet.rel='stylesheet';sheet.href='/missing.css';sheet.onload=()=>document.querySelector('#events').textContent+='load';sheet.onerror=()=>document.querySelector('#events').textContent+='error';document.head.appendChild(sheet)").await;
    let request = requests.recv_timeout(Duration::from_secs(2)).unwrap();
    responses
        .send(response(request, 404, "#box { width: 211px }"))
        .unwrap();
    wait_event(&mut runtime, "error").await;
    assert_ne!(
        eval(
            &mut runtime,
            "getComputedStyle(document.querySelector('#box')).width"
        )
        .await,
        json!("211px")
    );
    tokio::time::timeout(
        Duration::from_secs(2),
        runtime.capture_png(Default::default()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(requests.try_recv().is_err());
}

#[tokio::test]
async fn changed_href_and_document_discard_late_stylesheet_results() {
    let mut runtime = runtime().await;
    let (requests, responses) = bridge(&mut runtime);
    eval(&mut runtime, "globalThis.sheet=document.createElement('link');sheet.rel='stylesheet';sheet.href='/old.css';sheet.onload=()=>document.querySelector('#events').textContent+='load';document.head.appendChild(sheet)").await;
    let old = requests.recv_timeout(Duration::from_secs(2)).unwrap();
    eval(&mut runtime, "sheet.href='/new.css'").await;
    let new = requests.recv_timeout(Duration::from_secs(2)).unwrap();
    responses
        .send(response(old, 200, "#box { width: 211px }"))
        .unwrap();
    responses
        .send(response(new, 200, "#box { width: 123px }"))
        .unwrap();
    wait_event(&mut runtime, "load").await;
    assert_eq!(
        eval(
            &mut runtime,
            "getComputedStyle(document.querySelector('#box')).width"
        )
        .await,
        json!("123px")
    );
    eval(&mut runtime, "sheet.href='/late.css'").await;
    let late = requests.recv_timeout(Duration::from_secs(2)).unwrap();
    runtime
        .set_document(
            "<html><body><div id='box'>new</div><p id='events'></p></body></html>",
            Some("https://styles.example/new"),
            (320, 240),
        )
        .await
        .unwrap();
    responses
        .send(response(late, 200, "#box { width: 211px }"))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(event_text(&mut runtime).await, "");
    assert_ne!(
        eval(
            &mut runtime,
            "getComputedStyle(document.querySelector('#box')).width"
        )
        .await,
        json!("211px")
    );
}

#[tokio::test]
async fn dynamic_route_stylesheet_uses_real_session_network_and_css_cascade() {
    use oxibrowser_core::session::Session;
    use oxibrowser_core::{Browser, BrowserConfig};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/page"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("<html><head><style>body{margin:0}#box{width:11px;height:10px;background:red}</style></head><body><div id='box'></div><script>globalThis.sheet=document.createElement('link');sheet.rel='stylesheet';sheet.href='/route.css';sheet.onload=()=>document.querySelector('#box').textContent='loaded';sheet.onerror=()=>document.querySelector('#box').textContent='failed';document.head.appendChild(sheet)</script></body></html>", "text/html"))
        .mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/route.css"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "#box{width:123px;height:61px;background:rgb(0,0,255)}",
            "text/css",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let mut config = BrowserConfig::headless();
    config.enable_ssrf_filter = false;
    let browser = Browser::new(config.clone()).await.unwrap();
    let mut session = Session::new(browser.id(), config, browser.default_context())
        .await
        .unwrap();
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    let result = session.evaluate_js("JSON.stringify([document.querySelector('#box').textContent,document.querySelector('#box').getBoundingClientRect().width,getComputedStyle(document.querySelector('#box')).height])").await.unwrap();
    assert!(result.exception.is_none(), "{result:?}");
    assert_eq!(
        serde_json::from_str::<Value>(result.value.unwrap().as_str().unwrap()).unwrap(),
        json!(["loaded", 123, "61px"])
    );
    let png = tokio::time::timeout(Duration::from_secs(2), session.capture_screenshot_png(320))
        .await
        .unwrap()
        .unwrap();
    let pixels = image::load_from_memory(&png).unwrap().into_rgba8();
    assert_eq!(pixels.get_pixel(80, 40).0, [0, 0, 255, 255]);
    server.verify().await;
    session.close().await.unwrap();
}

#[tokio::test]
async fn detached_and_reinserted_link_ignores_its_old_attachment_response() {
    let mut runtime = runtime().await;
    let (requests, responses) = bridge(&mut runtime);
    eval(&mut runtime, "globalThis.sheet=document.createElement('link');sheet.rel='stylesheet';sheet.href='/route.css';sheet.onload=()=>document.querySelector('#events').textContent+='load';document.head.appendChild(sheet)").await;
    let detached = requests.recv_timeout(Duration::from_secs(2)).unwrap();
    eval(
        &mut runtime,
        "sheet.remove();document.head.appendChild(sheet)",
    )
    .await;
    let current = requests.recv_timeout(Duration::from_secs(2)).unwrap();
    responses
        .send(response(detached, 200, "#box{width:211px}"))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(70)).await;
    assert_eq!(event_text(&mut runtime).await, "");
    responses
        .send(response(current, 200, "#box{width:119px}"))
        .unwrap();
    wait_event(&mut runtime, "load").await;
    assert_eq!(
        eval(
            &mut runtime,
            "getComputedStyle(document.querySelector('#box')).width"
        )
        .await,
        json!("119px")
    );
}

#[tokio::test]
async fn stylesheet_cascade_follows_dom_order_when_responses_arrive_out_of_order() {
    let mut runtime = runtime().await;
    let (requests, responses) = bridge(&mut runtime);
    eval(&mut runtime, "for(const name of ['first','second']){const link=document.createElement('link');link.rel='stylesheet';link.href='/'+name+'.css';link.onload=()=>document.querySelector('#events').textContent+='load';document.head.appendChild(link)}").await;
    let first = requests.recv_timeout(Duration::from_secs(2)).unwrap();
    let second = requests.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(first.url.ends_with("/first.css"));
    assert!(second.url.ends_with("/second.css"));
    responses
        .send(response(second, 200, "#box{width:141px}"))
        .unwrap();
    responses
        .send(response(first, 200, "#box{width:99px}"))
        .unwrap();
    wait_event(&mut runtime, "loadload").await;
    assert_eq!(
        eval(
            &mut runtime,
            "getComputedStyle(document.querySelector('#box')).width"
        )
        .await,
        json!("141px")
    );
}
