//! Response bytes survive the real HTTP and interception bridges into JS.

use oxibrowser_core::js::CoreEvent;
use oxibrowser_core::network::intercept::{InterceptAction, shared_registry};
use oxibrowser_core::session::{Session, set_fetch_patterns};
use oxibrowser_core::{Browser, BrowserConfig};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn session() -> Session {
    let mut config = BrowserConfig::headless();
    config.enable_ssrf_filter = false;
    session_with_config(config).await
}

async fn session_with_config(config: BrowserConfig) -> Session {
    let browser = Browser::new(config.clone()).await.unwrap();
    Session::new(browser.id(), config, browser.default_context())
        .await
        .unwrap()
}

#[tokio::test]
async fn fetch_rejects_oversized_body_and_accepts_exact_limit() {
    let server = MockServer::start().await;
    for (route, body) in [("/exact", vec![0, 128, 255]), ("/oversized", vec![0; 4])] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;
    }
    let mut config = BrowserConfig::headless();
    config.enable_ssrf_filter = false;
    config.max_response_body_bytes = 3;
    let mut session = session_with_config(config).await;
    let base = serde_json::to_string(&server.uri()).unwrap();
    let result = evaluate(
        &mut session,
        &format!(
            r#"
        (async () => {{
            const bytes = await (await fetch({base} + '/exact')).arrayBuffer();
            const failure = await fetch({base} + '/oversized').then(
                () => 'unexpected success', error => String(error));
            return JSON.stringify([Array.from(new Uint8Array(bytes)), failure]);
        }})()
    "#
        ),
    )
    .await;
    assert_eq!(result[0], json!([0, 128, 255]));
    assert!(
        result[1]
            .as_str()
            .unwrap()
            .contains("exceeds limit of 3 bytes"),
        "{result}"
    );
    session.close().await.unwrap();
}

async fn evaluate(session: &mut Session, expression: &str) -> Value {
    let result = session
        .evaluate_js_with_await(expression, true)
        .await
        .unwrap();
    assert!(!result.timed_out, "{result:?}");
    assert!(result.exception.is_none(), "{result:?}");
    serde_json::from_str(result.value.as_ref().and_then(Value::as_str).unwrap()).unwrap()
}

#[tokio::test]
async fn fetch_http_preserves_binary_and_decodes_utf8() {
    let server = MockServer::start().await;
    let binary = vec![0, 0x80, 0xff, 0x41, 0xc3, 0xa9];
    let text = "청계천 · café 🌿";
    let json_text = format!("\u{feff}{}", json!({"text": text}));
    for (route, bytes, mime) in [
        ("/binary", binary.clone(), "application/octet-stream"),
        ("/text", text.as_bytes().to_vec(), "text/plain"),
        ("/json", json_text.as_bytes().to_vec(), "application/json"),
        ("/empty", Vec::new(), "application/octet-stream"),
    ] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_raw(bytes, mime))
            .expect(1)
            .mount(&server)
            .await;
    }
    let mut session = session().await;
    let base = serde_json::to_string(&server.uri()).unwrap();
    let result = evaluate(
        &mut session,
        &format!(
            r#"
        (async () => {{
            const base = {base};
            const binary = await (await fetch(base + '/binary')).arrayBuffer();
            const text = await (await fetch(base + '/text')).text();
            const parsed = await (await fetch(base + '/json')).json();
            const empty = await (await fetch(base + '/empty')).arrayBuffer();
            return JSON.stringify({{
                isBuffer: binary instanceof ArrayBuffer,
                isView: ArrayBuffer.isView(binary),
                bytes: Array.from(new Uint8Array(binary)),
                length: binary.byteLength,
                text, parsed,
                empty: [empty instanceof ArrayBuffer, empty.byteLength]
            }});
        }})()
    "#
        ),
    )
    .await;
    assert_eq!(
        result,
        json!({
            "isBuffer": true,
            "isView": false,
            "bytes": binary,
            "length": 6,
            "text": text,
            "parsed": {"text": text},
            "empty": [true, 0]
        })
    );
    session.close().await.unwrap();
    server.verify().await;
}

struct ClearFetchPatterns;

impl Drop for ClearFetchPatterns {
    fn drop(&mut self) {
        set_fetch_patterns(Vec::new());
    }
}

#[tokio::test]
async fn fetch_fulfilled_interception_preserves_binary() {
    let server = MockServer::start().await;
    let url = format!("{}/intercepted-binary", server.uri());
    let mut session = session().await;
    let (events_tx, events_rx) = std::sync::mpsc::channel();
    session.set_event_sink(events_tx);
    set_fetch_patterns(vec![url.clone()]);
    let _reset = ClearFetchPatterns;
    let responder = tokio::task::spawn_blocking(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let event = events_rx
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .expect("interception event");
            if let CoreEvent::RequestPaused { request_id, .. } = event {
                shared_registry()
                    .take(&request_id)
                    .unwrap()
                    .tx
                    .send(InterceptAction::Fulfill {
                        status_code: 200,
                        status_text: "OK".to_string(),
                        headers: Vec::new(),
                        body: vec![0, 0x80, 0xff],
                    })
                    .unwrap();
                break;
            }
        }
    });
    let url = serde_json::to_string(&url).unwrap();
    let result = evaluate(
        &mut session,
        &format!(
            r#"
        fetch({url}).then(r => r.arrayBuffer()).then(buffer => JSON.stringify([
            buffer instanceof ArrayBuffer, Array.from(new Uint8Array(buffer))
        ]))
    "#
        ),
    )
    .await;
    responder.await.unwrap();
    assert_eq!(result, json!([true, [0, 128, 255]]));
    assert!(server.received_requests().await.unwrap().is_empty());
    session.close().await.unwrap();
}
