//! E2E integration tests for the CDP server.
//!
//! These tests verify the full CDP stack: HTTP endpoints, WebSocket upgrade,
//! command dispatch, event broadcasting, and DOM access.
//!
//! Pure Rust — uses tokio-tungstenite as the CDP client. No Node.js/Puppeteer.

use base64::Engine;
use futures::{SinkExt, StreamExt};
use oxibrowser_cdp::CdpServer;
use oxibrowser_cdp::credential::CredentialBroker;
use oxibrowser_core::Browser;
use oxibrowser_core::context::ContextId;
use oxibrowser_core::network::origin_policy::{Origin, OriginRule, RuleMode};
use oxibrowser_credentials::{
    ConsentRecord, ConsentSubject, CredentialId, CredentialKind, CredentialProvider,
    InMemoryProvider, NewCredential, PolicyEngine, SecretBox,
};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

// ---------------------------------------------------------------------------
// Test infrastructure
// ---------------------------------------------------------------------------

/// Find an available TCP port.
fn find_available_port() -> u16 {
    use std::net::TcpListener;
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A minimal HTTP server that serves static HTML for testing.
struct TestHttpServer {
    addr: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl TestHttpServer {
    /// Start serving the given HTML on a random port.
    fn start(html: &'static str) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Set non-blocking so tokio can use it
        listener.set_nonblocking(true).unwrap();
        let tokio_listener = tokio::net::TcpListener::from_std(listener).unwrap();

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            let body = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: text/html; charset=utf-8\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n\
                 {}",
                html.len(),
                html
            );

            loop {
                tokio::select! {
                    accept = tokio_listener.accept() => {
                        if let Ok((mut stream, _)) = accept {
                            use tokio::io::AsyncWriteExt;
                            let _ = stream.write_all(body.as_bytes()).await;
                            let _ = stream.shutdown().await;
                        }
                    }
                    _ = &mut shutdown_rx => {
                        break;
                    }
                }
            }
        });

        Self {
            addr,
            shutdown: Some(shutdown_tx),
        }
    }

    /// Get the address this server is listening on.
    fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for TestHttpServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

/// Start a CDP server on a random port.
async fn start_cdp_server() -> (Arc<CdpServer>, SocketAddr) {
    let port = find_available_port();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let mut config = oxibrowser_core::BrowserConfig::headless();
    // Disable SSRF filter for tests — need to connect to local test server
    config.enable_ssrf_filter = false;
    let browser = Arc::new(Browser::new(config).await.unwrap());
    let server = Arc::new(CdpServer::new(addr, browser));

    let server_clone = server.clone();
    tokio::spawn(async move {
        let _ = server_clone.start().await;
    });

    // Give the server a moment to bind
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    (server, addr)
}

// Events that arrive while a `send_command` is awaiting its response are
// buffered here (concurrent dispatch may deliver events before the response).
// `collect_events` drains the prefix-matching ones so tests observe them.
thread_local! {
    static SIDECAR_EVENTS: std::cell::RefCell<Vec<Value>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Connect to a CDP server via WebSocket.
async fn connect_ws(
    addr: SocketAddr,
) -> (
    futures::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        tungstenite::Message,
    >,
    futures::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
) {
    let url = format!("ws://{addr}/ws");
    let request = url.into_client_request().unwrap();
    let (ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let (sink, stream) = ws.split();
    SIDECAR_EVENTS.with(|b| b.borrow_mut().clear());
    (sink, stream)
}

/// Send a CDP command and return the response (skipping events).
async fn send_command(
    sink: &mut futures::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        tungstenite::Message,
    >,
    ws: &mut futures::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    id: u64,
    method: &str,
    params: Option<Value>,
) -> Value {
    let msg = match params {
        Some(p) => json!({ "id": id, "method": method, "params": p }),
        None => json!({ "id": id, "method": method }),
    };

    sink.send(tungstenite::Message::Text(msg.to_string().into()))
        .await
        .unwrap();

    // Read response (skip events)
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("timeout waiting for response to command {id} ({method})");
        }
        match tokio::time::timeout(remaining, ws.next()).await {
            Ok(Some(Ok(tungstenite::Message::Text(text)))) => {
                let response: Value = serde_json::from_str(&text).unwrap();
                if response.get("id").and_then(|v| v.as_u64()) == Some(id) {
                    return response;
                }
                // Buffer events for a later collect_events (concurrent
                // dispatch may deliver them before this command's response).
                SIDECAR_EVENTS.with(|b| b.borrow_mut().push(response));
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(e))) => panic!("WebSocket error: {e}"),
            Ok(None) => panic!("WebSocket stream ended before response"),
            Err(_) => panic!("timeout waiting for response to command {id}"),
        }
    }
}

/// Read the response for a previously-sent command by id (it may already be
/// buffered in the sidecar, or arrive on the stream). Used when a command was
/// sent without awaiting (e.g. a paused navigation that completes later).
async fn read_command_response(
    ws: &mut futures::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    id: u64,
    timeout_ms: u64,
) -> Value {
    // Check the sidecar buffer first (concurrent dispatch may have delivered it).
    let buffered = SIDECAR_EVENTS.with(|b| {
        let mut buf = b.borrow_mut();
        buf.iter()
            .position(|v| v.get("id").and_then(|x| x.as_u64()) == Some(id))
            .map(|pos| buf.remove(pos))
    });
    if let Some(v) = buffered {
        return v;
    }
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(timeout_ms);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("timeout waiting for response to command {id}");
        }
        match tokio::time::timeout(remaining, ws.next()).await {
            Ok(Some(Ok(tungstenite::Message::Text(text)))) => {
                let response: Value = serde_json::from_str(&text).unwrap();
                if response.get("id").and_then(|v| v.as_u64()) == Some(id) {
                    return response;
                }
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(e))) => panic!("WebSocket error: {e}"),
            Ok(None) => panic!("WebSocket stream ended"),
            Err(_) => panic!("timeout waiting for response to command {id}"),
        }
    }
}

/// Collect CDP events matching a method prefix within a time window.
async fn collect_events(
    ws: &mut futures::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    method_prefix: &str,
    max_wait_ms: u64,
) -> Vec<Value> {
    let mut events = Vec::new();
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(max_wait_ms);
    // Drain prefix-matching events buffered while prior send_command calls
    // awaited their responses (concurrent dispatch delivers them early).
    SIDECAR_EVENTS.with(|b| {
        let mut buf = b.borrow_mut();
        let mut kept = Vec::with_capacity(buf.len());
        for ev in buf.drain(..) {
            match ev.get("method").and_then(|v| v.as_str()) {
                Some(m) if m.starts_with(method_prefix) => events.push(ev),
                _ => kept.push(ev),
            }
        }
        *buf = kept;
    });

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        match tokio::time::timeout(remaining, ws.next()).await {
            Ok(Some(Ok(tungstenite::Message::Text(text)))) => {
                let msg: Value = serde_json::from_str(&text).unwrap();
                if let Some(method) = msg.get("method").and_then(|v| v.as_str())
                    && method.starts_with(method_prefix)
                {
                    events.push(msg);
                }
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(_))) => break,
            Ok(None) => break,
            Err(_) => break,
        }
    }

    events
}

// ============================================================
// HTTP endpoint tests
// ============================================================

#[tokio::test]
async fn test_http_json_version() {
    let (server, addr) = start_cdp_server().await;

    let resp = reqwest::get(format!("http://{addr}/json/version"))
        .await
        .unwrap();
    assert!(resp.status().is_success());

    let body: Value = resp.json().await.unwrap();
    assert!(body["browser"].as_str().unwrap().starts_with("OxiBrowser/"));
    assert_eq!(body["protocolVersion"], "1.3");
    assert!(body["webSocketDebuggerUrl"].is_string());

    server.shutdown();
}

#[tokio::test]
async fn test_http_json_list() {
    let (server, addr) = start_cdp_server().await;

    let resp = reqwest::get(format!("http://{addr}/json")).await.unwrap();
    assert!(resp.status().is_success());

    let body: Vec<Value> = resp.json().await.unwrap();
    assert!(!body.is_empty());
    assert_eq!(body[0]["type"], "page");

    server.shutdown();
}

// ============================================================
// WebSocket CDP command tests
// ============================================================

#[tokio::test]
async fn test_ws_connect_and_browser_get_version() {
    let (server, addr) = start_cdp_server().await;
    let (mut sink, mut ws) = connect_ws(addr).await;

    let resp = send_command(&mut sink, &mut ws, 1, "Browser.getVersion", None).await;
    assert_eq!(resp["id"], 1);
    assert_eq!(resp["result"]["protocolVersion"], "1.3");

    server.shutdown();
}

#[tokio::test]
async fn test_page_enable_events() {
    let (server, addr) = start_cdp_server().await;
    let (mut sink, mut ws) = connect_ws(addr).await;

    // Runtime.enable
    let resp = send_command(&mut sink, &mut ws, 1, "Runtime.enable", None).await;
    assert_eq!(resp["id"], 1);

    // Collect Runtime.executionContextCreated event
    let events = collect_events(&mut ws, "Runtime.", 500).await;
    assert!(
        !events.is_empty(),
        "should receive Runtime.executionContextCreated"
    );
    assert_eq!(
        events[0]["method"], "Runtime.executionContextCreated",
        "first event should be executionContextCreated"
    );
    assert!(events[0]["params"]["context"]["id"].is_number());

    server.shutdown();
}

#[tokio::test]
async fn test_page_get_frame_tree() {
    let (server, addr) = start_cdp_server().await;
    let (mut sink, mut ws) = connect_ws(addr).await;

    let resp = send_command(&mut sink, &mut ws, 1, "Page.getFrameTree", None).await;
    assert_eq!(resp["id"], 1);
    assert!(resp["result"]["frameTree"]["frame"].is_object());
    assert_eq!(
        resp["result"]["frameTree"]["frame"]["mimeType"],
        "text/html"
    );

    server.shutdown();
}

#[tokio::test]
async fn test_dom_get_document() {
    let (server, addr) = start_cdp_server().await;
    let (mut sink, mut ws) = connect_ws(addr).await;

    let resp = send_command(&mut sink, &mut ws, 1, "DOM.getDocument", None).await;
    assert_eq!(resp["id"], 1);
    assert!(resp["result"]["root"].is_object());
    assert_eq!(resp["result"]["root"]["nodeType"], 9);

    server.shutdown();
}

#[tokio::test]
async fn test_runtime_evaluate() {
    let (server, addr) = start_cdp_server().await;
    let (mut sink, mut ws) = connect_ws(addr).await;

    // Number literal
    let resp = send_command(
        &mut sink,
        &mut ws,
        1,
        "Runtime.evaluate",
        Some(json!({ "expression": "42" })),
    )
    .await;
    assert_eq!(resp["id"], 1);
    assert_eq!(resp["result"]["result"]["type"], "number");
    assert_eq!(resp["result"]["result"]["value"], 42);

    // String literal
    let resp = send_command(
        &mut sink,
        &mut ws,
        2,
        "Runtime.evaluate",
        Some(json!({ "expression": "'hello'" })),
    )
    .await;
    assert_eq!(resp["id"], 2);
    assert_eq!(resp["result"]["result"]["type"], "string");
    assert_eq!(resp["result"]["result"]["value"], "hello");

    server.shutdown();
}

#[tokio::test]
async fn test_unknown_domain_returns_error() {
    let (server, addr) = start_cdp_server().await;
    let (mut sink, mut ws) = connect_ws(addr).await;

    let resp = send_command(&mut sink, &mut ws, 1, "Foo.bar", None).await;
    assert_eq!(resp["id"], 1);
    assert!(resp["error"].is_object());
    assert_eq!(resp["error"]["code"], -32601);

    server.shutdown();
}

#[tokio::test]
async fn test_target_domain() {
    let (server, addr) = start_cdp_server().await;
    let (mut sink, mut ws) = connect_ws(addr).await;

    // Target.getTargets
    let resp = send_command(&mut sink, &mut ws, 1, "Target.getTargets", None).await;
    assert_eq!(resp["id"], 1);
    assert!(resp["result"]["targetInfos"].is_array());

    // Target.createTarget
    let resp = send_command(
        &mut sink,
        &mut ws,
        2,
        "Target.createTarget",
        Some(json!({ "url": "https://example.com" })),
    )
    .await;
    assert_eq!(resp["id"], 2);
    assert!(resp["result"]["targetId"].is_string());

    server.shutdown();
}

#[tokio::test]
async fn test_create_target_creates_drivable_session() {
    // Multi-tab: Target.createTarget must mint a real session that commands
    // routed by the new sessionId can drive (navigate + evaluate).
    let (server, addr) = start_cdp_server().await;
    let (mut sink, mut ws) = connect_ws(addr).await;

    let resp = send_command(
        &mut sink,
        &mut ws,
        1,
        "Target.createTarget",
        Some(json!({ "url": "about:blank" })),
    )
    .await;
    assert_eq!(resp["id"], 1);
    let _target_id = resp["result"]["targetId"].as_str().unwrap().to_string();

    // The attachedToTarget event carries the child sessionId.
    let attached = collect_events(&mut ws, "Target.attachedToTarget", 3000)
        .await
        .pop()
        .expect("attachedToTarget event");
    let child_session = attached["params"]["sessionId"]
        .as_str()
        .expect("attachedToTarget should carry sessionId")
        .to_string();

    // Navigate the child tab (routed by sessionId).
    let nav_msg = json!({
        "id": 2, "method": "Page.navigate", "sessionId": child_session,
        "params": { "url": "data:text/html,<html><body><p id='x'>tab2</p></body></html>" }
    });
    sink.send(tungstenite::Message::Text(nav_msg.to_string().into()))
        .await
        .unwrap();
    let nav_resp = read_command_response(&mut ws, 2, 5000).await;
    assert_eq!(nav_resp["id"], 2);
    assert!(
        nav_resp.get("error").is_none(),
        "child navigate should route by sessionId: {:?}",
        nav_resp
    );

    // Evaluate in the child session.
    let eval_msg = json!({
        "id": 3, "method": "Runtime.evaluate", "sessionId": child_session,
        "params": { "expression": "document.getElementById('x').textContent" }
    });
    sink.send(tungstenite::Message::Text(eval_msg.to_string().into()))
        .await
        .unwrap();
    let eval_resp = read_command_response(&mut ws, 3, 5000).await;
    assert_eq!(eval_resp["id"], 3);
    assert!(
        eval_resp.get("error").is_none(),
        "child eval should route by sessionId: {:?}",
        eval_resp
    );
    assert_eq!(
        eval_resp["result"]["result"]["value"].as_str(),
        Some("tab2"),
        "child session should evaluate its own DOM"
    );

    // Child-originated events flow stamped with the child's sessionId:
    // enable Runtime (top-level sessionId), log from the child tab, and check
    // the console event.
    let enable_msg = json!({
        "id": 4, "method": "Runtime.enable", "sessionId": child_session
    });
    sink.send(tungstenite::Message::Text(enable_msg.to_string().into()))
        .await
        .unwrap();
    let _ = read_command_response(&mut ws, 4, 5000).await;
    let log_msg = json!({
        "id": 5, "method": "Runtime.evaluate", "sessionId": child_session,
        "params": { "expression": "console.log('child-tab-msg')" }
    });
    sink.send(tungstenite::Message::Text(log_msg.to_string().into()))
        .await
        .unwrap();
    let _log_resp = read_command_response(&mut ws, 5, 5000).await;

    let console_events = collect_events(&mut ws, "Runtime.consoleAPICalled", 3000).await;
    assert!(
        console_events.iter().any(|ev| {
            ev.get("sessionId").and_then(|v| v.as_str()) == Some(child_session.as_str())
        }),
        "child console event should carry the child sessionId: {:?}",
        console_events
    );

    server.shutdown();
}

#[tokio::test]
async fn test_fetch_domain_enable_disable() {
    let (server, addr) = start_cdp_server().await;
    let (mut sink, mut ws) = connect_ws(addr).await;

    let resp = send_command(&mut sink, &mut ws, 1, "Fetch.enable", None).await;
    assert_eq!(resp["id"], 1);
    assert!(resp["result"].is_object());

    let resp = send_command(&mut sink, &mut ws, 2, "Fetch.disable", None).await;
    assert_eq!(resp["id"], 2);
    assert!(resp["result"].is_object());

    server.shutdown();
}

// ============================================================
// Full navigation E2E tests
// ============================================================

#[tokio::test]
async fn test_navigate_to_local_server_and_inspect_dom() {
    // Start a local HTTP server with known HTML
    let html = r#"<html>
        <head><title>E2E Test Page</title></head>
        <body>
            <h1 id="heading">Hello OxiBrowser</h1>
            <p class="content">This is a test page.</p>
            <ul>
                <li class="item">Item 1</li>
                <li class="item">Item 2</li>
                <li class="item">Item 3</li>
            </ul>
            <a href="/link" id="mylink">Click me</a>
        </body>
    </html>"#;
    let http_server = TestHttpServer::start(html);

    // Start CDP server
    let (cdp_server, cdp_addr) = start_cdp_server().await;
    let (mut sink, mut ws) = connect_ws(cdp_addr).await;

    // Navigate to the local server
    let url = format!("http://{}/", http_server.addr());
    let resp = send_command(
        &mut sink,
        &mut ws,
        1,
        "Page.navigate",
        Some(json!({ "url": url })),
    )
    .await;

    assert_eq!(resp["id"], 1);
    assert!(resp["result"]["frameId"].is_string());
    assert!(resp["result"]["loaderId"].is_string());

    // Verify DOM.getDocument returns a real parsed tree
    let resp = send_command(&mut sink, &mut ws, 2, "DOM.getDocument", None).await;
    assert_eq!(resp["id"], 2);
    let root = &resp["result"]["root"];
    assert_eq!(root["nodeType"], 9, "root should be a document node");
    assert!(root["children"].is_array(), "root should have children");

    // Verify Page.getFrameTree has the navigated URL
    let resp = send_command(&mut sink, &mut ws, 3, "Page.getFrameTree", None).await;
    assert_eq!(resp["id"], 3);
    let frame_url = resp["result"]["frameTree"]["frame"]["url"]
        .as_str()
        .unwrap();
    assert!(
        frame_url.contains("127.0.0.1"),
        "frame URL should point to local server, got: {frame_url}"
    );

    // Verify DOM.querySelector finds #heading
    let resp = send_command(
        &mut sink,
        &mut ws,
        4,
        "DOM.querySelector",
        Some(json!({ "nodeId": 0, "selector": "#heading" })),
    )
    .await;
    assert_eq!(resp["id"], 4);
    let heading_id = resp["result"]["nodeId"].as_u64().unwrap();
    assert!(heading_id > 0, "should find #heading element");

    // Verify DOM.querySelectorAll finds .item
    let resp = send_command(
        &mut sink,
        &mut ws,
        5,
        "DOM.querySelectorAll",
        Some(json!({ "nodeId": 0, "selector": ".item" })),
    )
    .await;
    assert_eq!(resp["id"], 5);
    let items = resp["result"]["nodeIds"].as_array().unwrap();
    assert_eq!(items.len(), 3, "should find 3 .item elements");

    // Verify DOM.getOuterHTML returns the full HTML
    let resp = send_command(&mut sink, &mut ws, 6, "DOM.getOuterHTML", None).await;
    assert_eq!(resp["id"], 6);
    let outer_html = resp["result"]["outerHTML"].as_str().unwrap();
    assert!(
        outer_html.contains("Hello OxiBrowser"),
        "HTML should contain heading text"
    );
    assert!(
        outer_html.contains("E2E Test Page"),
        "HTML should contain title"
    );

    cdp_server.shutdown();
}

#[tokio::test]
async fn test_navigate_emits_page_events() {
    let html = r#"<html><head><title>Event Test</title></head><body><p>Content</p></body></html>"#;
    let http_server = TestHttpServer::start(html);

    let (cdp_server, cdp_addr) = start_cdp_server().await;
    let (mut sink, mut ws) = connect_ws(cdp_addr).await;

    // Enable Page events first
    let _ = send_command(&mut sink, &mut ws, 1, "Page.enable", None).await;

    // Navigate
    let url = format!("http://{}/", http_server.addr());
    let _ = send_command(
        &mut sink,
        &mut ws,
        2,
        "Page.navigate",
        Some(json!({ "url": url })),
    )
    .await;

    // Collect Page events
    let events = collect_events(&mut ws, "Page.", 1000).await;
    let methods: Vec<&str> = events.iter().filter_map(|e| e["method"].as_str()).collect();

    assert!(
        methods.contains(&"Page.frameNavigated"),
        "should emit Page.frameNavigated, got: {methods:?}"
    );
    assert!(
        methods.contains(&"Page.domContentLoadedEventFired"),
        "should emit Page.domContentLoadedEventFired, got: {methods:?}"
    );
    assert!(
        methods.contains(&"Page.loadEventFired"),
        "should emit Page.loadEventFired, got: {methods:?}"
    );

    cdp_server.shutdown();
}

#[tokio::test]
async fn test_navigate_emits_network_events() {
    let html = r#"<html><body><p>Network Test</p></body></html>"#;
    let http_server = TestHttpServer::start(html);

    let (cdp_server, cdp_addr) = start_cdp_server().await;
    let (mut sink, mut ws) = connect_ws(cdp_addr).await;

    // Enable Network events
    let _ = send_command(&mut sink, &mut ws, 1, "Network.enable", None).await;

    // Navigate
    let url = format!("http://{}/", http_server.addr());
    let _ = send_command(
        &mut sink,
        &mut ws,
        2,
        "Page.navigate",
        Some(json!({ "url": url })),
    )
    .await;

    // Collect Network events
    let events = collect_events(&mut ws, "Network.", 1000).await;
    let methods: Vec<&str> = events.iter().filter_map(|e| e["method"].as_str()).collect();

    assert!(
        methods.contains(&"Network.requestWillBeSent"),
        "should emit Network.requestWillBeSent, got: {methods:?}"
    );
    assert!(
        methods.contains(&"Network.responseReceived"),
        "should emit Network.responseReceived, got: {methods:?}"
    );
    assert!(
        methods.contains(&"Network.loadingFinished"),
        "should emit Network.loadingFinished, got: {methods:?}"
    );

    // Verify the request URL is correct
    let req_event = events
        .iter()
        .find(|e| e["method"] == "Network.requestWillBeSent")
        .unwrap();
    let req_url = req_event["params"]["request"]["url"].as_str().unwrap();
    assert!(
        req_url.contains("127.0.0.1"),
        "request URL should point to local server, got: {req_url}"
    );

    cdp_server.shutdown();
}

#[tokio::test]
async fn test_runtime_evaluate_after_navigation() {
    let html = r#"<html><body><p>JS Test</p></body></html>"#;
    let http_server = TestHttpServer::start(html);

    let (cdp_server, cdp_addr) = start_cdp_server().await;
    let (mut sink, mut ws) = connect_ws(cdp_addr).await;

    // Navigate first
    let url = format!("http://{}/", http_server.addr());
    let _ = send_command(
        &mut sink,
        &mut ws,
        1,
        "Page.navigate",
        Some(json!({ "url": url })),
    )
    .await;

    // Drain any events
    let _ = collect_events(&mut ws, "Page.", 200).await;

    // Now evaluate JS
    let resp = send_command(
        &mut sink,
        &mut ws,
        2,
        "Runtime.evaluate",
        Some(json!({ "expression": "'hello world'" })),
    )
    .await;
    assert_eq!(resp["id"], 2);
    assert_eq!(resp["result"]["result"]["type"], "string");
    assert_eq!(resp["result"]["result"]["value"], "hello world");

    // Boolean
    let resp = send_command(
        &mut sink,
        &mut ws,
        3,
        "Runtime.evaluate",
        Some(json!({ "expression": "true" })),
    )
    .await;
    assert_eq!(resp["result"]["result"]["type"], "boolean");
    assert_eq!(resp["result"]["result"]["value"], true);

    // Number
    let resp = send_command(
        &mut sink,
        &mut ws,
        4,
        "Runtime.evaluate",
        Some(json!({ "expression": "3.14" })),
    )
    .await;
    assert_eq!(resp["result"]["result"]["type"], "number");

    cdp_server.shutdown();
}

#[tokio::test]
async fn test_full_workflow_connect_navigate_inspect_close() {
    let html = r#"<html>
        <head><title>Full Workflow</title></head>
        <body>
            <div id="main">
                <h2>Workflow Test</h2>
                <p class="desc">Description</p>
            </div>
        </body>
    </html>"#;
    let http_server = TestHttpServer::start(html);

    let (cdp_server, cdp_addr) = start_cdp_server().await;
    let (mut sink, mut ws) = connect_ws(cdp_addr).await;

    // 1. Browser.getVersion
    let resp = send_command(&mut sink, &mut ws, 1, "Browser.getVersion", None).await;
    assert_eq!(resp["result"]["protocolVersion"], "1.3");

    // 2. Enable Runtime
    let _ = send_command(&mut sink, &mut ws, 2, "Runtime.enable", None).await;
    let _ = collect_events(&mut ws, "Runtime.", 300).await;

    // 3. Enable Page
    let _ = send_command(&mut sink, &mut ws, 3, "Page.enable", None).await;

    // 4. Navigate
    let url = format!("http://{}/", http_server.addr());
    let resp = send_command(
        &mut sink,
        &mut ws,
        4,
        "Page.navigate",
        Some(json!({ "url": url })),
    )
    .await;
    assert!(resp["result"]["frameId"].is_string());

    // 5. Collect Page events
    let events = collect_events(&mut ws, "Page.", 500).await;
    assert!(!events.is_empty(), "should receive page events");

    // 6. Inspect DOM
    let resp = send_command(
        &mut sink,
        &mut ws,
        5,
        "DOM.querySelector",
        Some(json!({ "nodeId": 0, "selector": "#main" })),
    )
    .await;
    assert!(resp["result"]["nodeId"].as_u64().unwrap() > 0);

    // 7. Get all items
    let resp = send_command(
        &mut sink,
        &mut ws,
        6,
        "DOM.querySelectorAll",
        Some(json!({ "nodeId": 0, "selector": "p" })),
    )
    .await;
    assert_eq!(resp["result"]["nodeIds"].as_array().unwrap().len(), 1);

    // 8. Evaluate JS
    let resp = send_command(
        &mut sink,
        &mut ws,
        7,
        "Runtime.evaluate",
        Some(json!({ "expression": "1 + 1" })),
    )
    .await;
    // In stub mode, "1 + 1" returns as string "1 + 1"
    // (stub doesn't evaluate expressions, only literals)
    assert!(
        resp["result"]["result"]["value"].is_string()
            || resp["result"]["result"]["value"].is_number(),
        "should return some value"
    );

    cdp_server.shutdown();
}

// ---------------------------------------------------------------------------
// Input domain tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_input_dispatch_key_event() {
    let (server, addr) = start_cdp_server().await;
    let (mut sink, mut ws) = connect_ws(addr).await;

    let _ = send_command(&mut sink, &mut ws, 1, "Page.enable", None).await;

    let resp = send_command(
        &mut sink,
        &mut ws,
        2,
        "Input.dispatchKeyEvent",
        Some(json!({
            "type": "keyDown", "key": "a", "code": "KeyA", "modifiers": 0
        })),
    )
    .await;
    assert_eq!(resp["id"], 2);

    let resp = send_command(
        &mut sink,
        &mut ws,
        3,
        "Input.dispatchKeyEvent",
        Some(json!({
            "type": "keyUp", "key": "a", "code": "KeyA"
        })),
    )
    .await;
    assert_eq!(resp["id"], 3);

    let resp = send_command(
        &mut sink,
        &mut ws,
        4,
        "Input.insertText",
        Some(json!({"text": "hello"})),
    )
    .await;
    assert_eq!(resp["id"], 4);

    server.shutdown();
}

#[tokio::test]
async fn test_input_dispatch_mouse_event() {
    let (server, addr) = start_cdp_server().await;
    let (mut sink, mut ws) = connect_ws(addr).await;

    let resp = send_command(
        &mut sink,
        &mut ws,
        1,
        "Input.dispatchMouseEvent",
        Some(json!({
            "type": "mouseMoved", "x": 100.0, "y": 200.0, "button": "none", "clickCount": 0
        })),
    )
    .await;
    assert_eq!(resp["id"], 1);

    let resp = send_command(
        &mut sink,
        &mut ws,
        2,
        "Input.dispatchMouseEvent",
        Some(json!({
            "type": "mousePressed", "x": 100.0, "y": 200.0, "button": "left", "clickCount": 1
        })),
    )
    .await;
    assert_eq!(resp["id"], 2);

    let resp = send_command(
        &mut sink,
        &mut ws,
        3,
        "Input.dispatchMouseEvent",
        Some(json!({
            "type": "mouseReleased", "x": 100.0, "y": 200.0, "button": "left", "clickCount": 1
        })),
    )
    .await;
    assert_eq!(resp["id"], 3);

    server.shutdown();
}

// ---------------------------------------------------------------------------
// Network cookie tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_network_get_all_cookies() {
    let (server, addr) = start_cdp_server().await;
    let (mut sink, mut ws) = connect_ws(addr).await;

    let resp = send_command(&mut sink, &mut ws, 1, "Network.getAllCookies", None).await;
    assert_eq!(resp["id"], 1);
    assert!(resp["result"]["cookies"].is_array());

    server.shutdown();
}

#[tokio::test]
async fn test_network_set_cookie() {
    let (server, addr) = start_cdp_server().await;
    let (mut sink, mut ws) = connect_ws(addr).await;

    let resp = send_command(&mut sink, &mut ws, 1, "Network.setCookie", Some(json!({
        "name": "session_id", "value": "abc123", "url": "http://example.com/", "path": "/", "secure": false
    }))).await;
    assert_eq!(resp["id"], 1);
    assert_eq!(resp["result"]["success"], true);

    let resp = send_command(&mut sink, &mut ws, 2, "Network.getAllCookies", None).await;
    let cookies = resp["result"]["cookies"].as_array().unwrap();
    assert!(!cookies.is_empty());

    server.shutdown();
}

#[tokio::test]
async fn test_network_delete_cookies() {
    let (server, addr) = start_cdp_server().await;
    let (mut sink, mut ws) = connect_ws(addr).await;

    let _ = send_command(
        &mut sink,
        &mut ws,
        1,
        "Network.setCookie",
        Some(json!({
            "name": "temp_key", "value": "temp_val", "url": "http://example.com/"
        })),
    )
    .await;

    let resp = send_command(
        &mut sink,
        &mut ws,
        2,
        "Network.deleteCookies",
        Some(json!({
            "name": "temp_key", "url": "http://example.com/"
        })),
    )
    .await;
    assert_eq!(resp["id"], 2);
    assert_eq!(resp["result"]["success"], true);

    server.shutdown();
}

// ---------------------------------------------------------------------------
// Fetch domain tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_fetch_fulfill_request() {
    let (server, addr) = start_cdp_server().await;
    let (mut sink, mut ws) = connect_ws(addr).await;

    // Enable Fetch domain with a wildcard pattern (matches any http URL).
    let resp = send_command(
        &mut sink,
        &mut ws,
        1,
        "Fetch.enable",
        Some(json!({ "patterns": [{"urlPattern": "http://*"}] })),
    )
    .await;
    assert_eq!(resp["id"], 1);

    // Send Page.navigate WITHOUT awaiting — the server pauses the request and
    // emits Fetch.requestPaused; we must answer before it can complete.
    let nav_msg =
        json!({ "id": 2, "method": "Page.navigate", "params": { "url": "http://example.com/" } });
    sink.send(tungstenite::Message::Text(nav_msg.to_string().into()))
        .await
        .unwrap();

    // Collect the Fetch.requestPaused event and extract its requestId.
    let paused = collect_events(&mut ws, "Fetch.requestPaused", 3000)
        .await
        .pop()
        .expect("expected a Fetch.requestPaused event");
    let intercept_id = paused["params"]["requestId"]
        .as_str()
        .expect("requestPaused should carry a requestId")
        .to_string();

    // Fulfill the paused request with a mock HTML body.
    let resp = send_command(
        &mut sink,
        &mut ws,
        3,
        "Fetch.fulfillRequest",
        Some(json!({
            "requestId": intercept_id,
            "responseCode": 200,
            "responseHeaders": [{"name": "content-type", "value": "text/html"}],
            "body": base64::engine::general_purpose::STANDARD.encode("<html><body>mocked</body></html>")
        })),
    )
    .await;
    assert_eq!(resp["id"], 3);

    // The navigate (id 2) should now complete (fulfilled → data-URL navigation).
    let nav_resp = read_command_response(&mut ws, 2, 5000).await;
    assert_eq!(nav_resp["id"], 2);

    let _ = send_command(&mut sink, &mut ws, 4, "Fetch.disable", None).await;
    server.shutdown();
}

#[tokio::test]
async fn test_fetch_continue_request() {
    let (server, addr) = start_cdp_server().await;
    let (mut sink, mut ws) = connect_ws(addr).await;

    let _ = send_command(&mut sink, &mut ws, 1, "Fetch.enable", None).await;

    // Try to continue a non-existent request — should return error
    let resp = send_command(
        &mut sink,
        &mut ws,
        2,
        "Fetch.continueRequest",
        Some(json!({
            "requestId": "nonexistent", "url": "http://example.com/"
        })),
    )
    .await;
    assert_eq!(resp["id"], 2);
    // Should have an error because requestId not found
    assert!(
        resp.get("error").is_some(),
        "expected error for unknown requestId"
    );

    let _ = send_command(&mut sink, &mut ws, 3, "Fetch.disable", None).await;
    server.shutdown();
}

#[tokio::test]
async fn test_fetch_fulfill_unknown_request() {
    // Test that Fetch.fulfillRequest returns error for unknown requestId
    let (server, addr) = start_cdp_server().await;
    let (mut sink, mut ws) = connect_ws(addr).await;

    let _ = send_command(&mut sink, &mut ws, 1, "Fetch.enable", None).await;

    let resp = send_command(
        &mut sink,
        &mut ws,
        2,
        "Fetch.fulfillRequest",
        Some(json!({
            "requestId": "unknown-id", "statusCode": 200, "statusText": "OK",
            "body": "test", "responseHeaders": []
        })),
    )
    .await;
    assert_eq!(resp["id"], 2);
    // Should have an error because requestId not found
    assert!(
        resp.get("error").is_some(),
        "expected error for unknown requestId in fulfillRequest"
    );

    let _ = send_command(&mut sink, &mut ws, 3, "Fetch.disable", None).await;
    server.shutdown();
}

// ---------------------------------------------------------------------------
// Credential surface (M4): OXI.credentialList / fillCredential /
// resolveConfirmation + the credential-mode gate
// ---------------------------------------------------------------------------

/// CDP server wired with a credential broker (InMemoryProvider + real
/// PolicyEngine). Returns the browser handle too — the credential-mode flag
/// lives on its contexts — and the engine/provider handles for seeding.
async fn start_credential_server(
    engine: Arc<PolicyEngine>,
    ttl: Option<Duration>,
) -> (
    Arc<CdpServer>,
    SocketAddr,
    Arc<Browser>,
    Arc<InMemoryProvider>,
    Arc<PolicyEngine>,
) {
    let port = find_available_port();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let mut config = oxibrowser_core::BrowserConfig::headless();
    config.enable_ssrf_filter = false;
    let browser = Arc::new(Browser::new(config).await.unwrap());
    let provider = Arc::new(InMemoryProvider::new());
    let mut broker = CredentialBroker::new(provider.clone(), engine.clone());
    if let Some(ttl) = ttl {
        broker = broker.with_confirmation_ttl(ttl);
    }
    let server =
        Arc::new(CdpServer::new(addr, browser.clone()).with_credentials_broker(Arc::new(broker)));
    let server_clone = server.clone();
    tokio::spawn(async move {
        let _ = server_clone.start().await;
    });
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    (server, addr, browser, provider, engine)
}

/// Fresh PolicyEngine with its own temp-dir consent store + audit log.
fn test_engine(deny_rules: Vec<OriginRule>) -> Arc<PolicyEngine> {
    let dir = std::env::temp_dir().join(format!("oxi-cdp-e2e-{}", uuid::Uuid::new_v4()));
    let audit = Arc::new(
        oxibrowser_core::security::audit::AuditLog::open(dir.join("audit.jsonl")).unwrap(),
    );
    Arc::new(PolicyEngine::new(
        deny_rules,
        oxibrowser_credentials::ConsentStore::open(dir.join("consents.jsonl")).unwrap(),
        audit,
    ))
}

/// Seed a password credential into the provider; returns the handle.
fn seed_password(provider: &InMemoryProvider, origins: &[String]) -> String {
    provider
        .put(NewCredential {
            agent_id: "main".into(),
            scope: "example.test".into(),
            kind: CredentialKind::Password,
            slug: "dashboard".into(),
            allowed_origins: origins.to_vec(),
            login_hint: Some("user@example.com".into()),
            password: Some(SecretBox::from_string("hunter2".into())),
            otpauth_uri: None,
        })
        .unwrap()
        .0
}

/// Grant a consent record for the credential at the origin.
fn grant(engine: &PolicyEngine, credential: &str, origin: &str, action: &str) {
    engine
        .consents
        .grant(ConsentRecord::new(
            ConsentSubject::Credential {
                credential: CredentialId(credential.to_string()),
            },
            origin,
            &[action],
            chrono::Duration::hours(1),
            10,
        ))
        .unwrap();
}

const LOGIN_HTML: &str = r#"<html><body>
<form>
  <input id="user" type="text" name="username">
  <input id="pw" type="password" name="password">
  <button type="submit">Sign in</button>
</form>
</body></html>"#;

type WsStream = futures::stream::SplitStream<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
>;
type WsSink = futures::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    tungstenite::Message,
>;

/// Navigate the session to the local login page and return the page origin.
async fn navigate_to_login(sink: &mut WsSink, ws: &mut WsStream, http_addr: SocketAddr) -> String {
    let origin = format!("http://{http_addr}");
    let resp = send_command(sink, ws, 1, "Page.navigate", Some(json!({ "url": origin }))).await;
    assert!(resp.get("error").is_none(), "navigate failed: {resp}");
    origin
}

/// Wait until the interactive-elements snapshot exposes the password input
/// and return its ref.
async fn password_ref(sink: &mut WsSink, ws: &mut WsStream) -> String {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
    let mut next_id = 100u64;
    loop {
        let resp = send_command(sink, ws, next_id, "OXI.getInteractiveElements", None).await;
        if let Some(elements) = resp["result"]["elements"].as_array() {
            for el in elements {
                if el["tag"] == "input" && el["inputType"] == "password" {
                    return el["ref"].as_str().expect("ref on element").to_string();
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("password input never appeared in the snapshot");
        }
        next_id += 1;
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }
}

/// Read the password field's value through Runtime.evaluate.
async fn password_input_value(sink: &mut WsSink, ws: &mut WsStream, id: u64) -> String {
    let resp = send_command(
        sink,
        ws,
        id,
        "Runtime.evaluate",
        Some(json!({ "expression": "document.getElementById('pw').value" })),
    )
    .await;
    resp["result"]["result"]["value"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn test_credential_mode_gate_denies_storage_surface() {
    let port = find_available_port();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let mut config = oxibrowser_core::BrowserConfig::headless();
    config.enable_ssrf_filter = false;
    let browser = Arc::new(Browser::new(config).await.unwrap());
    let server = Arc::new(CdpServer::new(addr, browser.clone()));
    let server_clone = server.clone();
    tokio::spawn(async move {
        let _ = server_clone.start().await;
    });
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    let (mut sink, mut ws) = connect_ws(addr).await;

    // Flip credential mode on the default context (the CDP session's context).
    browser.default_context().set_credential_mode(true);

    let denied: &[(&str, Option<Value>)] = &[
        ("Network.getAllCookies", None),
        ("Network.getCookies", Some(json!({}))),
        (
            "Network.setCookie",
            Some(json!({"name": "a", "value": "b", "url": "http://example.com/"})),
        ),
        (
            "Network.deleteCookies",
            Some(json!({"name": "a", "url": "http://example.com/"})),
        ),
        ("OXI.exportStorageState", None),
    ];
    for (i, (method, params)) in denied.iter().enumerate() {
        let resp = send_command(&mut sink, &mut ws, i as u64 + 1, method, params.clone()).await;
        assert_eq!(
            resp["error"]["message"].as_str(),
            Some("deniedInCredentialMode"),
            "{method} must be denied in credential mode: {resp}"
        );
    }

    // Non-storage surface is untouched by the flag.
    let resp = send_command(&mut sink, &mut ws, 50, "OXI.getPageInfo", None).await;
    assert!(
        resp.get("error").is_none(),
        "getPageInfo unaffected: {resp}"
    );

    // Flip off — the same surface passes again (gate reads the flag live).
    browser.default_context().set_credential_mode(false);
    let resp = send_command(&mut sink, &mut ws, 51, "Network.getAllCookies", None).await;
    assert!(
        resp.get("error").is_none(),
        "getAllCookies allowed after flip: {resp}"
    );
    assert!(resp["result"]["cookies"].is_array());
    let resp = send_command(&mut sink, &mut ws, 52, "OXI.exportStorageState", None).await;
    assert!(
        resp.get("error").is_none(),
        "exportStorageState allowed after flip: {resp}"
    );
    assert!(resp["result"]["state"].is_object());

    server.shutdown();
}

#[tokio::test]
async fn test_credential_gate_follows_child_target_context() {
    let port = find_available_port();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let mut config = oxibrowser_core::BrowserConfig::headless();
    config.enable_ssrf_filter = false;
    let browser = Arc::new(Browser::new(config).await.unwrap());
    let server = Arc::new(CdpServer::new(addr, browser.clone()));
    let server_clone = server.clone();
    tokio::spawn(async move {
        let _ = server_clone.start().await;
    });
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    let (mut sink, mut ws) = connect_ws(addr).await;

    // A fresh context + child target created in it.
    let resp = send_command(&mut sink, &mut ws, 1, "Target.createBrowserContext", None).await;
    let ctx_id = resp["result"]["browserContextId"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = send_command(
        &mut sink,
        &mut ws,
        2,
        "Target.createTarget",
        Some(json!({ "url": "about:blank", "browserContextId": ctx_id })),
    )
    .await;
    assert!(resp.get("error").is_none(), "createTarget failed: {resp}");
    let attached = collect_events(&mut ws, "Target.attachedToTarget", 3000)
        .await
        .pop()
        .expect("attachedToTarget event");
    let child_session = attached["params"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    // Flip credential mode on the CHILD's context only.
    browser
        .context(&ContextId::from_string(ctx_id.clone()))
        .expect("child context resolvable")
        .set_credential_mode(true);

    // Routed to the child: denied.
    let msg = json!({
        "id": 3, "method": "Network.getAllCookies", "sessionId": child_session
    });
    sink.send(tungstenite::Message::Text(msg.to_string().into()))
        .await
        .unwrap();
    let resp = read_command_response(&mut ws, 3, 5000).await;
    assert_eq!(resp["error"]["message"], "deniedInCredentialMode");

    // Same command on the main session (default context, flag off): allowed.
    let resp = send_command(&mut sink, &mut ws, 4, "Network.getAllCookies", None).await;
    assert!(
        resp.get("error").is_none(),
        "main session unaffected: {resp}"
    );

    server.shutdown();
}

#[tokio::test]
async fn test_oxi_credential_methods_unavailable_without_provider() {
    let (server, addr) = start_cdp_server().await;
    let (mut sink, mut ws) = connect_ws(addr).await;

    let resp = send_command(&mut sink, &mut ws, 1, "OXI.credentialList", None).await;
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap_or("")
            .starts_with("credentialsUnavailable"),
        "credentialList without a provider: {resp}"
    );

    let resp = send_command(
        &mut sink,
        &mut ws,
        2,
        "OXI.fillCredential",
        Some(json!({
            "ref": "e1",
            "credentialId": "kch:main/example.test/password/dashboard",
            "fieldKind": "password"
        })),
    )
    .await;
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap_or("")
            .starts_with("credentialsUnavailable"),
        "fillCredential without a provider: {resp}"
    );

    server.shutdown();
}

/// F5 (CREDMODE-FILLREF-LITERAL-GATE): in credential mode a literal
/// `OXI.fillRef` into an `input[type=password]` is rejected (the audited
/// `OXI.fillCredential` path is the only route), and `OXI.importStorageState`
/// is denied like its export twin. Outside credential mode both pass.
#[tokio::test]
async fn test_credential_mode_denies_password_literal_fill_and_import() {
    let http = TestHttpServer::start(LOGIN_HTML);
    let engine = test_engine(Vec::new());
    let (server, addr, browser, _provider, _engine) = start_credential_server(engine, None).await;
    browser.default_context().set_credential_mode(true);

    let (mut sink, mut ws) = connect_ws(addr).await;
    navigate_to_login(&mut sink, &mut ws, http.addr()).await;
    let r#ref = password_ref(&mut sink, &mut ws).await;

    // Literal fill targeting the password field → rejected.
    let resp = send_command(
        &mut sink,
        &mut ws,
        10,
        "OXI.fillRef",
        Some(json!({ "ref": r#ref, "value": "hunter2" })),
    )
    .await;
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("passwordFillRequiresCredential"),
        "password literal fill must be gated: {resp}"
    );
    assert_eq!(password_input_value(&mut sink, &mut ws, 11).await, "");

    // Non-password literals keep flowing.
    let resp = send_command(
        &mut sink,
        &mut ws,
        12,
        "OXI.fillRef",
        Some(json!({ "ref": "e1", "value": "user@example.com" })),
    )
    .await;
    // (e1 may or may not be the username field — only the gate verdict
    // matters; stale refs answer refStale, not the password gate.)
    let gated = resp["error"]["message"]
        .as_str()
        .unwrap_or("")
        .contains("passwordFillRequiresCredential");
    assert!(
        !gated,
        "non-password literal must not hit the password gate: {resp}"
    );

    // importStorageState: denied in credential mode (session-fixation vector).
    let resp = send_command(
        &mut sink,
        &mut ws,
        13,
        "OXI.importStorageState",
        Some(json!({ "state": { "cookies": [], "origins": [] } })),
    )
    .await;
    assert_eq!(resp["error"]["message"], "deniedInCredentialMode", "{resp}");

    // Flip credential mode off — the same surface passes again.
    browser.default_context().set_credential_mode(false);
    let r#ref = password_ref(&mut sink, &mut ws).await;
    let resp = send_command(
        &mut sink,
        &mut ws,
        14,
        "OXI.fillRef",
        Some(json!({ "ref": r#ref, "value": "hunter2" })),
    )
    .await;
    assert!(
        resp.get("error").is_none(),
        "literal fill must pass: {resp}"
    );
    assert_eq!(resp["result"]["filled"], true);
    assert_eq!(
        password_input_value(&mut sink, &mut ws, 15).await,
        "hunter2"
    );
    let resp = send_command(
        &mut sink,
        &mut ws,
        16,
        "OXI.importStorageState",
        Some(json!({ "state": { "cookies": [], "origins": [] } })),
    )
    .await;
    assert!(resp.get("error").is_none(), "import must pass: {resp}");

    server.shutdown();
}

#[tokio::test]
async fn test_credential_list_returns_metadata_only() {
    let engine = test_engine(Vec::new());
    let (server, addr, _browser, provider, _engine) = start_credential_server(engine, None).await;
    let cred_id = seed_password(&provider, &["https://dash.example.test".to_string()]);
    let (mut sink, mut ws) = connect_ws(addr).await;

    let resp = send_command(&mut sink, &mut ws, 1, "OXI.credentialList", None).await;
    assert!(resp.get("error").is_none(), "credentialList failed: {resp}");
    let creds = resp["result"]["credentials"].as_array().unwrap();
    assert_eq!(creds.len(), 1);
    let entry = &creds[0];
    assert_eq!(entry["id"], cred_id);
    assert_eq!(entry["kind"], "password");
    assert_eq!(entry["slug"], "dashboard");
    // Metadata only: no value slots, no secret anywhere in the payload.
    assert!(entry.get("password").is_none());
    assert!(entry.get("otpauth").is_none());
    assert!(!resp.to_string().contains("hunter2"));

    // Agent filter sees nothing for an unknown agent.
    let resp = send_command(
        &mut sink,
        &mut ws,
        2,
        "OXI.credentialList",
        Some(json!({ "agent": "other-agent" })),
    )
    .await;
    assert_eq!(resp["result"]["credentials"].as_array().unwrap().len(), 0);

    server.shutdown();
}

#[tokio::test]
async fn test_fill_credential_with_consent_fills_masked() {
    let http = TestHttpServer::start(LOGIN_HTML);
    let origin = format!("http://{}", http.addr());
    let engine = test_engine(Vec::new());
    let (server, addr, _browser, provider, engine) = start_credential_server(engine, None).await;
    let cred_id = seed_password(&provider, std::slice::from_ref(&origin));
    grant(&engine, &cred_id, &origin, "login");

    let (mut sink, mut ws) = connect_ws(addr).await;
    navigate_to_login(&mut sink, &mut ws, http.addr()).await;
    let r#ref = password_ref(&mut sink, &mut ws).await;

    let resp = send_command(
        &mut sink,
        &mut ws,
        10,
        "OXI.fillCredential",
        Some(json!({
            "ref": r#ref,
            "credentialId": cred_id,
            "fieldKind": "password"
        })),
    )
    .await;
    assert!(resp.get("error").is_none(), "fillCredential failed: {resp}");
    assert_eq!(resp["result"]["filled"], true);
    assert_eq!(resp["result"]["masked"], true);
    // The value never crosses the wire.
    assert!(!resp.to_string().contains("hunter2"));

    // …but it landed in the field.
    assert_eq!(
        password_input_value(&mut sink, &mut ws, 11).await,
        "hunter2"
    );

    server.shutdown();
}

#[tokio::test]
async fn test_fill_credential_confirmation_reject_then_approve() {
    let http = TestHttpServer::start(LOGIN_HTML);
    let origin = format!("http://{}", http.addr());
    // No consent grant → RequireConfirmation on the first use. Full stack:
    // login surface (viewer channel) + credential broker.
    let engine = test_engine(Vec::new());
    let provider = Arc::new(InMemoryProvider::new());
    let broker = Arc::new(CredentialBroker::new(provider.clone(), engine.clone()));
    let (server, addr, surface, _base) = start_account_server("confirm", Some(broker)).await;
    let cred_id = seed_password(&provider, std::slice::from_ref(&origin));

    // Agent connection (primary context = account context).
    let (mut sink, mut ws) = connect_ws_role(addr, None).await;
    navigate_to_login(&mut sink, &mut ws, http.addr()).await;
    let r#ref = password_ref(&mut sink, &mut ws).await;

    // 1st request → confirmationRequired event + consentRequired error.
    let resp = send_command(
        &mut sink,
        &mut ws,
        10,
        "OXI.fillCredential",
        Some(json!({
            "ref": r#ref,
            "credentialId": cred_id,
            "fieldKind": "password"
        })),
    )
    .await;
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap_or("")
            .starts_with("consentRequired"),
        "unconsented fill must ask for confirmation: {resp}"
    );
    let events = collect_events(&mut ws, "OXI.confirmationRequired", 3000).await;
    assert_eq!(events.len(), 1, "exactly one confirmation card: {events:?}");
    let card = &events[0]["params"];
    let request_id = card["requestId"].as_str().expect("requestId").to_string();
    assert!(request_id.starts_with("conf-"));
    assert_eq!(card["action"], "login");
    assert_eq!(card["origin"], origin);
    assert_eq!(card["summary"]["values"], json!([cred_id]));
    assert_eq!(card["summary"]["irreversible"], false);
    assert_eq!(card["timeoutMs"], 60000);

    // The requesting agent can never approve its own card
    // (OXI-CONFIRM-SELF-APPROVE) — the gate rejects it outright.
    let resp = send_command(
        &mut sink,
        &mut ws,
        12,
        "OXI.resolveConfirmation",
        Some(json!({ "requestId": request_id, "approved": true })),
    )
    .await;
    assert_eq!(
        resp["error"]["message"], "confirmationRequiresViewerRole",
        "{resp}"
    );

    // Human approval path: the one-time token arrives out-of-band (host
    // channel = orchestrator handle here); the viewer mirrors the account
    // context and navigates to the same page before resolving.
    let resp = send_command(
        &mut sink,
        &mut ws,
        13,
        "OXI.beginLogin",
        Some(json!({ "accountId": "acc", "mode": "user" })),
    )
    .await;
    assert!(resp.get("error").is_none(), "beginLogin failed: {resp}");
    assert!(
        resp["result"].get("viewerToken").is_none(),
        "no in-band token: {resp}"
    );
    let login_id = resp["result"]["loginId"].as_str().unwrap().to_string();
    let token = surface
        .orchestrator()
        .take_viewer_token(&login_id)
        .expect("out-of-band viewer token");
    let (mut viewer_sink, mut viewer_ws) = connect_ws_headers(
        addr,
        &[("x-oxi-role", "viewer"), ("x-oxi-viewer-token", &token)],
    )
    .await;

    // Viewer is on its own mirror session: navigate to the login page first.
    let resp = send_command(
        &mut viewer_sink,
        &mut viewer_ws,
        1,
        "Page.navigate",
        Some(json!({ "url": origin })),
    )
    .await;
    assert!(
        resp.get("error").is_none(),
        "viewer navigate failed: {resp}"
    );

    // Explicit rejection denies.
    let resp = send_command(
        &mut viewer_sink,
        &mut viewer_ws,
        2,
        "OXI.resolveConfirmation",
        Some(json!({ "requestId": request_id, "approved": false })),
    )
    .await;
    assert!(resp.get("error").is_none(), "resolve failed: {resp}");
    assert_eq!(resp["result"]["resolved"], true);
    assert_eq!(resp["result"]["outcome"], "denied");

    // Re-request (the page is unchanged, so the ref is still live), then the
    // viewer approves: verify_confirmation passes and the fill completes on
    // the viewer's mirror session.
    let r#ref = password_ref(&mut sink, &mut ws).await;
    let resp = send_command(
        &mut sink,
        &mut ws,
        14,
        "OXI.fillCredential",
        Some(json!({
            "ref": r#ref,
            "credentialId": cred_id,
            "fieldKind": "password"
        })),
    )
    .await;
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap_or("")
            .starts_with("consentRequired"),
        "second request still needs a card: {resp}"
    );
    let events = collect_events(&mut ws, "OXI.confirmationRequired", 3000).await;
    assert_eq!(events.len(), 1);
    let request_id = events[0]["params"]["requestId"]
        .as_str()
        .unwrap()
        .to_string();

    let resp = send_command(
        &mut viewer_sink,
        &mut viewer_ws,
        3,
        "OXI.resolveConfirmation",
        Some(json!({ "requestId": request_id, "approved": true })),
    )
    .await;
    assert!(resp.get("error").is_none(), "resolve failed: {resp}");
    assert_eq!(resp["result"]["resolved"], true);
    assert_eq!(resp["result"]["outcome"], "approved");
    assert_eq!(resp["result"]["filled"], true);
    assert_eq!(resp["result"]["masked"], true);
    assert!(!resp.to_string().contains("hunter2"));

    server.shutdown();
}

#[tokio::test]
async fn test_fill_credential_confirmation_timeout_denies() {
    let http = TestHttpServer::start(LOGIN_HTML);
    let origin = format!("http://{}", http.addr());
    // No consent grant → the engine Requires a confirmation card.
    let engine = test_engine(Vec::new());
    let provider = Arc::new(InMemoryProvider::new());
    // 500 ms card TTL — timeout must deny, never approve.
    let broker = Arc::new(
        CredentialBroker::new(provider.clone(), engine.clone())
            .with_confirmation_ttl(Duration::from_millis(500)),
    );
    let (server, addr, surface, _base) =
        start_account_server("confirm-timeout", Some(broker)).await;
    let cred_id = seed_password(&provider, std::slice::from_ref(&origin));

    let (mut sink, mut ws) = connect_ws_role(addr, None).await;
    navigate_to_login(&mut sink, &mut ws, http.addr()).await;
    let r#ref = password_ref(&mut sink, &mut ws).await;

    let resp = send_command(
        &mut sink,
        &mut ws,
        10,
        "OXI.fillCredential",
        Some(json!({
            "ref": r#ref,
            "credentialId": cred_id,
            "fieldKind": "password"
        })),
    )
    .await;
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap_or("")
            .starts_with("consentRequired"),
        "expected confirmation card path: {resp}"
    );
    let events = collect_events(&mut ws, "OXI.confirmationRequired", 3000).await;
    assert_eq!(events.len(), 1);
    let request_id = events[0]["params"]["requestId"]
        .as_str()
        .unwrap()
        .to_string();

    // Viewer channel for the (too-late) human approval.
    let resp = send_command(
        &mut sink,
        &mut ws,
        11,
        "OXI.beginLogin",
        Some(json!({ "accountId": "acc", "mode": "user" })),
    )
    .await;
    assert!(resp.get("error").is_none(), "beginLogin failed: {resp}");
    let login_id = resp["result"]["loginId"].as_str().unwrap().to_string();
    let token = surface
        .orchestrator()
        .take_viewer_token(&login_id)
        .expect("out-of-band viewer token");
    let (mut viewer_sink, mut viewer_ws) = connect_ws_headers(
        addr,
        &[("x-oxi-role", "viewer"), ("x-oxi-viewer-token", &token)],
    )
    .await;
    let resp = send_command(
        &mut viewer_sink,
        &mut viewer_ws,
        1,
        "Page.navigate",
        Some(json!({ "url": origin })),
    )
    .await;
    assert!(
        resp.get("error").is_none(),
        "viewer navigate failed: {resp}"
    );

    // Let the card expire, then approve — too late.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let resp = send_command(
        &mut viewer_sink,
        &mut viewer_ws,
        2,
        "OXI.resolveConfirmation",
        Some(json!({ "requestId": request_id, "approved": true })),
    )
    .await;
    let rejected =
        resp.get("error").is_some() || resp["result"]["outcome"].as_str() == Some("denied");
    assert!(rejected, "expired card must deny approval: {resp}");

    server.shutdown();
}

#[tokio::test]
async fn test_resolve_confirmation_requires_viewer_role() {
    let (server, addr, _surface, _base) = start_account_server("confirm-role", None).await;
    let (mut sink, mut ws) = connect_ws_role(addr, None).await;

    // Agent connections can never touch the confirmation surface — even a
    // well-formed approve is rejected at the role gate before the broker is
    // consulted (unknown ids included).
    for params in [
        json!({ "requestId": "conf-999", "approved": true }),
        json!({ "requestId": "conf-999" }),
    ] {
        let resp = send_command(
            &mut sink,
            &mut ws,
            1,
            "OXI.resolveConfirmation",
            Some(params),
        )
        .await;
        assert_eq!(
            resp["error"]["message"], "confirmationRequiresViewerRole",
            "agent resolve must be gate-denied: {resp}"
        );
    }

    server.shutdown();
}

#[tokio::test]
async fn test_fill_credential_deny_rule_wins() {
    let http = TestHttpServer::start(LOGIN_HTML);
    let origin = format!("http://{}", http.addr());
    let deny_origin = Origin::parse(&origin).unwrap();
    let engine = test_engine(vec![OriginRule {
        origin: deny_origin,
        mode: RuleMode::Deny,
    }]);
    let (server, addr, _browser, provider, engine) = start_credential_server(engine, None).await;
    let cred_id = seed_password(&provider, std::slice::from_ref(&origin));
    // A consent grant exists, but the deny rule wins.
    grant(&engine, &cred_id, &origin, "login");

    let (mut sink, mut ws) = connect_ws(addr).await;
    navigate_to_login(&mut sink, &mut ws, http.addr()).await;
    let r#ref = password_ref(&mut sink, &mut ws).await;

    let resp = send_command(
        &mut sink,
        &mut ws,
        10,
        "OXI.fillCredential",
        Some(json!({
            "ref": r#ref,
            "credentialId": cred_id,
            "fieldKind": "password"
        })),
    )
    .await;
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap_or("")
            .starts_with("consentRequired"),
        "deny rule must surface consentRequired: {resp}"
    );
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("deny rule"),
        "reason should name the deny rule: {resp}"
    );
    // Denied, no card, nothing filled.
    let events = collect_events(&mut ws, "OXI.confirmationRequired", 500).await;
    assert!(events.is_empty(), "deny must not open a card: {events:?}");
    assert_eq!(password_input_value(&mut sink, &mut ws, 11).await, "");

    server.shutdown();
}

#[tokio::test]
async fn test_fill_credential_origin_mismatch_and_not_found() {
    let http = TestHttpServer::start(LOGIN_HTML);
    let engine = test_engine(Vec::new());
    let (server, addr, _browser, provider, _engine) = start_credential_server(engine, None).await;
    // Credential for a DIFFERENT origin.
    let cred_id = seed_password(&provider, &["https://other.example".to_string()]);

    let (mut sink, mut ws) = connect_ws(addr).await;
    navigate_to_login(&mut sink, &mut ws, http.addr()).await;
    let r#ref = password_ref(&mut sink, &mut ws).await;

    // Allowlist miss → originMismatch.
    let resp = send_command(
        &mut sink,
        &mut ws,
        10,
        "OXI.fillCredential",
        Some(json!({
            "ref": r#ref,
            "credentialId": cred_id,
            "fieldKind": "password"
        })),
    )
    .await;
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap_or("")
            .starts_with("originMismatch"),
        "allowlist miss must surface originMismatch: {resp}"
    );

    // Unknown handle → credentialNotFound.
    let resp = send_command(
        &mut sink,
        &mut ws,
        11,
        "OXI.fillCredential",
        Some(json!({
            "ref": r#ref,
            "credentialId": "kch:main/example.test/password/ghost",
            "fieldKind": "password"
        })),
    )
    .await;
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap_or("")
            .starts_with("credentialNotFound"),
        "unknown handle must surface credentialNotFound: {resp}"
    );

    // Unknown ref → re-observe (refStale family).
    let resp = send_command(
        &mut sink,
        &mut ws,
        12,
        "OXI.fillCredential",
        Some(json!({
            "ref": "e999",
            "credentialId": cred_id,
            "fieldKind": "password"
        })),
    )
    .await;
    assert!(resp.get("error").is_some(), "unknown ref must fail: {resp}");

    assert_eq!(password_input_value(&mut sink, &mut ws, 13).await, "");
    server.shutdown();
}

// ---------------------------------------------------------------------------
// Account login surface (§5.1/§5.2, M-C): viewer role, takeover gating,
// reportLoginSuccess capture.
// ---------------------------------------------------------------------------

/// Server + login surface with one account bound to (and primary for) the
/// account context, so agent connections are routed into the takeover gate.
/// `credentials` optionally wires the credential broker (full-stack
/// confirmation-card tests).
async fn start_account_server(
    tag: &str,
    credentials: Option<Arc<CredentialBroker>>,
) -> (
    Arc<CdpServer>,
    SocketAddr,
    Arc<oxibrowser_cdp::LoginSurface>,
    String,
) {
    let port = find_available_port();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let mut config = oxibrowser_core::BrowserConfig::headless();
    config.enable_ssrf_filter = false;
    let browser = Arc::new(Browser::new(config).await.unwrap());

    let base = std::env::temp_dir().join(format!("oxi-cdp-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let audit = Arc::new(
        oxibrowser_core::security::audit::AuditLog::open(base.join("audit.jsonl")).unwrap(),
    );
    let registry = oxibrowser_core::account::AccountRegistry::open(base.join("accounts")).unwrap();
    registry
        .add(oxibrowser_core::account::AccountRecord::new("acc", "example.com").unwrap())
        .unwrap();
    let manager = Arc::new(oxibrowser_core::account::AccountManager::with_audit(
        registry, audit,
    ));
    let orch = Arc::new(oxibrowser_core::account::LoginOrchestrator::new(
        manager,
        browser.clone(),
        Arc::new(oxibrowser_core::storage::session_store::StaticKeyProvider::new([5u8; 32])),
    ));
    let surface = oxibrowser_cdp::LoginSurface::new(orch, browser.clone());

    // Bind the account to a context and make it primary (serve --account shape).
    let (ctx, _session) = surface
        .orchestrator()
        .open_account_context("acc")
        .await
        .unwrap();
    surface.bind_direct("acc", ctx.clone());

    let mut server_builder = CdpServer::new(addr, browser)
        .with_primary_context(ctx)
        .with_login(surface.clone());
    if let Some(broker) = credentials {
        server_builder = server_builder.with_credentials_broker(broker);
    }
    let server = Arc::new(server_builder);
    let s = server.clone();
    tokio::spawn(async move {
        let _ = s.start().await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    (
        server,
        addr,
        surface.clone(),
        base.to_string_lossy().to_string(),
    )
}

/// Connect with extra upgrade headers (role claim, §5.2).
async fn connect_ws_role(
    addr: SocketAddr,
    role: Option<(&str, &str)>,
) -> (
    futures::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        tungstenite::Message,
    >,
    futures::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
) {
    let headers: &[(&str, &str)] = match role {
        Some((name, value)) => &[(name, value)],
        None => &[],
    };
    connect_ws_headers(addr, headers).await
}

/// Connect with arbitrary upgrade headers (role + viewer token, §5.2).
async fn connect_ws_headers(
    addr: SocketAddr,
    headers: &[(&str, &str)],
) -> (
    futures::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        tungstenite::Message,
    >,
    futures::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
) {
    let url = format!("ws://{addr}/ws");
    let mut request = url.into_client_request().unwrap();
    for (name, value) in headers {
        request.headers_mut().insert(
            name.parse::<tungstenite::http::HeaderName>().unwrap(),
            value.parse::<tungstenite::http::HeaderValue>().unwrap(),
        );
    }
    let (ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    ws.split()
}

/// (b)+(c) beginLogin user mode issues a one-time viewer token; the viewer
/// may drive input/navigation but nothing else; the agent loses input and
/// capture inside the takeover window; the token is single-use.
#[tokio::test]
async fn test_begin_login_viewer_role_and_takeover_gating() {
    let (server, addr, surface, _base) = start_account_server("gating", None).await;

    // Agent connection (primary context = account context).
    let (mut agent_sink, mut agent_ws) = connect_ws_role(addr, None).await;

    // Open the login window.
    let resp = send_command(
        &mut agent_sink,
        &mut agent_ws,
        1,
        "OXI.beginLogin",
        Some(json!({ "accountId": "acc", "mode": "user" })),
    )
    .await;
    let result = resp
        .get("result")
        .unwrap_or_else(|| panic!("beginLogin failed: {resp}"));
    let login_id = result["loginId"].as_str().unwrap().to_string();
    assert!(result["timeoutMs"].as_u64().unwrap() >= 1000);
    assert!(!login_id.is_empty());
    // VIEWER-TOKEN-INBAND: the token never crosses the CDP response — it is
    // delivered out-of-band (here: the orchestrator handle the host owns).
    assert!(
        result.get("viewerToken").is_none(),
        "no in-band token: {resp}"
    );
    let token = surface
        .orchestrator()
        .take_viewer_token(&login_id)
        .expect("viewer token via out-of-band orchestrator handle");

    // Agent can never resolve confirmation cards — approval is the human's
    // viewer-channel act (OXI-CONFIRM-SELF-APPROVE).
    let resp = send_command(
        &mut agent_sink,
        &mut agent_ws,
        90,
        "OXI.resolveConfirmation",
        Some(json!({ "requestId": "conf-1", "approved": true })),
    )
    .await;
    assert_eq!(
        resp["error"]["message"], "confirmationRequiresViewerRole",
        "{resp}"
    );

    // Agent inside the takeover: Input denied, capture denied, DOM-event /
    // evaluate-based input and capture denied too…
    let resp = send_command(
        &mut agent_sink,
        &mut agent_ws,
        2,
        "Input.dispatchKeyEvent",
        Some(json!({ "type": "keyDown", "key": "a" })),
    )
    .await;
    assert_eq!(
        resp["error"]["message"], "input_denied_during_takeover",
        "{resp}"
    );
    let resp = send_command(
        &mut agent_sink,
        &mut agent_ws,
        3,
        "Page.captureScreenshot",
        Some(json!({})),
    )
    .await;
    assert_eq!(
        resp["error"]["message"], "capture_denied_during_takeover",
        "{resp}"
    );
    let resp = send_command(
        &mut agent_sink,
        &mut agent_ws,
        4,
        "Runtime.evaluate",
        Some(json!({ "expression": "1+1" })),
    )
    .await;
    assert_eq!(
        resp["error"]["message"], "input_denied_during_takeover",
        "evaluate is an input path: {resp}"
    );
    let resp = send_command(
        &mut agent_sink,
        &mut agent_ws,
        5,
        "OXI.fillRef",
        Some(json!({ "ref": "e1", "value": "x" })),
    )
    .await;
    assert_eq!(
        resp["error"]["message"], "input_denied_during_takeover",
        "fillRef is an input path: {resp}"
    );
    // …while read-only automation keeps working.
    let resp = send_command(
        &mut agent_sink,
        &mut agent_ws,
        6,
        "OXI.getInteractiveElements",
        None,
    )
    .await;
    assert!(
        resp.get("error").is_none(),
        "agent read-only observe must pass: {resp}"
    );

    // Viewer connects with the one-time token (role + token headers).
    let (mut viewer_sink2, mut viewer_ws2) = {
        let url = format!("ws://{addr}/ws");
        let mut request = url.into_client_request().unwrap();
        request
            .headers_mut()
            .insert("x-oxi-role", "viewer".parse().unwrap());
        request
            .headers_mut()
            .insert("x-oxi-viewer-token", token.parse().unwrap());
        let (ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        ws.split()
    };

    // Viewer allowlist: input + navigate pass…
    let resp = send_command(
        &mut viewer_sink2,
        &mut viewer_ws2,
        1,
        "Input.dispatchKeyEvent",
        Some(json!({ "type": "keyDown", "key": "a" })),
    )
    .await;
    assert!(
        resp.get("error").is_none(),
        "viewer input must pass: {resp}"
    );
    let resp = send_command(
        &mut viewer_sink2,
        &mut viewer_ws2,
        2,
        "Page.navigate",
        Some(json!({ "url": "about:blank" })),
    )
    .await;
    assert!(
        resp.get("error").is_none(),
        "viewer navigate must pass: {resp}"
    );
    // …cookies/storage/evaluation are denied.
    let resp = send_command(
        &mut viewer_sink2,
        &mut viewer_ws2,
        3,
        "Runtime.evaluate",
        Some(json!({ "expression": "document.cookie" })),
    )
    .await;
    assert_eq!(resp["error"]["message"], "deniedForViewerRole", "{resp}");
    let resp = send_command(
        &mut viewer_sink2,
        &mut viewer_ws2,
        4,
        "Network.getCookies",
        Some(json!({})),
    )
    .await;
    assert_eq!(resp["error"]["message"], "deniedForViewerRole", "{resp}");

    // One-time: a second viewer with the same token is rejected at upgrade.
    let url = format!("ws://{addr}/ws");
    let mut request = url.into_client_request().unwrap();
    request
        .headers_mut()
        .insert("x-oxi-role", "viewer".parse().unwrap());
    request
        .headers_mut()
        .insert("x-oxi-viewer-token", token.parse().unwrap());
    assert!(
        tokio_tungstenite::connect_async(request).await.is_err(),
        "reused viewer token must be rejected"
    );

    server.shutdown();
}

/// (d) reportLoginSuccess captures the envelope: response says captured,
/// accountList flips to valid, and loginStateChanged/accountStateChanged
/// events reach the client.
#[tokio::test]
async fn test_report_login_success_captures_and_emits_events() {
    let (server, addr, _surface, _base) = start_account_server("capture", None).await;
    let (mut sink, mut ws) = connect_ws_role(addr, None).await;

    let resp = send_command(
        &mut sink,
        &mut ws,
        1,
        "OXI.beginLogin",
        Some(json!({ "accountId": "acc", "mode": "user" })),
    )
    .await;
    let login_id = resp["result"]["loginId"].as_str().unwrap().to_string();

    let resp = send_command(
        &mut sink,
        &mut ws,
        2,
        "OXI.reportLoginSuccess",
        Some(json!({ "loginId": login_id })),
    )
    .await;
    assert_eq!(resp["result"]["state"], "captured", "{resp}");
    assert_eq!(resp["result"]["accountState"], "valid", "{resp}");

    // accountList reflects the capture.
    let resp = send_command(&mut sink, &mut ws, 3, "OXI.accountList", Some(json!({}))).await;
    let accounts = resp["result"]["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0]["state"], "valid", "{resp}");

    // Events flowed to this connection (forwarder): buffered alongside the
    // responses above.
    let events = collect_events(&mut ws, "OXI.loginStateChanged", 1000).await;
    assert!(
        events.iter().any(|e| {
            e["params"]["loginId"] == json!(login_id) && e["params"]["state"] == json!("captured")
        }),
        "expected loginStateChanged captured, got {events:?}"
    );
    let events = collect_events(&mut ws, "OXI.accountStateChanged", 1000).await;
    assert!(
        events
            .iter()
            .any(|e| e["params"]["state"] == json!("valid")),
        "expected accountStateChanged valid, got {events:?}"
    );

    // A captured account cannot open a second window.
    let resp = send_command(
        &mut sink,
        &mut ws,
        4,
        "OXI.beginLogin",
        Some(json!({ "accountId": "acc", "mode": "user" })),
    )
    .await;
    assert!(
        resp.get("error").is_some(),
        "second window must fail: {resp}"
    );

    server.shutdown();
}

/// OXI.captureSession — explicit "stop the work" capture of a bound
/// account context (roadmap item 3): no login window, no detector — the
/// context's live state is sealed directly and the account flips to valid.
#[tokio::test]
async fn test_capture_session_seals_bound_context() {
    let (server, addr, _surface, _base) = start_account_server("capturesess", None).await;

    // `start_account_server` binds "acc" to a context (serve --account shape).
    let (mut sink, mut ws) = connect_ws_role(addr, None).await;

    let resp = send_command(
        &mut sink,
        &mut ws,
        1,
        "OXI.captureSession",
        Some(json!({ "accountId": "acc" })),
    )
    .await;
    assert!(resp.get("error").is_none(), "{resp}");
    assert_eq!(resp["result"]["accountId"], "acc", "{resp}");
    assert_eq!(resp["result"]["state"], "valid", "{resp}");
    assert!(
        resp["result"]["sessionSummary"].get("updated_at").is_some(),
        "{resp}"
    );

    // The account board reflects the capture.
    let resp = send_command(&mut sink, &mut ws, 2, "OXI.accountList", Some(json!({}))).await;
    assert_eq!(resp["result"]["accounts"][0]["state"], "valid", "{resp}");

    // Unknown account → structured error, no state change.
    let resp = send_command(
        &mut sink,
        &mut ws,
        3,
        "OXI.captureSession",
        Some(json!({ "accountId": "nobody" })),
    )
    .await;
    assert!(resp.get("error").is_some(), "{resp}");

    server.shutdown();
}

/// endLogin abort reverts the account to needs_login.
#[tokio::test]
async fn test_end_login_abort_reverts_to_needs_login() {
    let (server, addr, _surface, _base) = start_account_server("abort", None).await;
    let (mut sink, mut ws) = connect_ws_role(addr, None).await;

    let resp = send_command(
        &mut sink,
        &mut ws,
        1,
        "OXI.beginLogin",
        Some(json!({ "accountId": "acc", "mode": "user" })),
    )
    .await;
    let login_id = resp["result"]["loginId"].as_str().unwrap().to_string();

    let resp = send_command(
        &mut sink,
        &mut ws,
        2,
        "OXI.endLogin",
        Some(json!({ "loginId": login_id, "outcome": "abort" })),
    )
    .await;
    assert_eq!(resp["result"]["state"], "aborted", "{resp}");
    assert_eq!(resp["result"]["accountState"], "needs_login", "{resp}");

    let resp = send_command(&mut sink, &mut ws, 3, "OXI.accountList", Some(json!({}))).await;
    assert_eq!(
        resp["result"]["accounts"][0]["state"], "needs_login",
        "{resp}"
    );

    // After the window is gone, the account can log in again.
    let resp = send_command(
        &mut sink,
        &mut ws,
        4,
        "OXI.beginLogin",
        Some(json!({ "accountId": "acc", "mode": "agent" })),
    )
    .await;
    assert!(
        resp.get("result").is_some(),
        "re-begin must succeed: {resp}"
    );
    let login_id2 = resp["result"]["loginId"].as_str().unwrap().to_string();
    let _ = send_command(
        &mut sink,
        &mut ws,
        5,
        "OXI.endLogin",
        Some(json!({ "loginId": login_id2, "outcome": "abort" })),
    )
    .await;

    server.shutdown();
}

// ---------------------------------------------------------------------------
// M-D: OXI.loginWithAccount — unattended agent login over the broker.
// ---------------------------------------------------------------------------

/// End-to-end unattended login: a form-POST login server (GET / → form,
/// POST /do-login → 303 + Set-Cookie, GET / with the session cookie →
/// signed-in marker), a broker-seeded password credential + login grant,
/// and `OXI.loginWithAccount` driving the whole flow over CDP.
#[tokio::test]
async fn test_login_with_account_unattended_captures() {
    use oxibrowser_core::account::AccountRecord;
    use oxibrowser_core::security::audit::AuditLog;
    use oxibrowser_core::storage::session_store::StaticKeyProvider;
    use oxibrowser_credentials::{ConsentRecord, ConsentSubject};

    const FORM: &str = r#"<html><body>
<form action="/do-login">
  <input type="text" name="user" id="user">
  <input type="password" name="pass" id="pass">
  <button type="submit">Sign in</button>
</form>
</body></html>"#;
    const SIGNED_IN: &str = r#"<html><head><meta name="user-login" content="garden"></head>
<body><h1>Signed in</h1></body></html>"#;

    // wiremock matches mounts in insertion order (first match wins).
    let http = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(
            wiremock::ResponseTemplate::new(303)
                .insert_header("Location", "/")
                .insert_header(
                    "Set-Cookie",
                    "user_session=oxyE2E; Path=/; HttpOnly; Secure",
                ),
        )
        .mount(&http)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(move |req: &wiremock::Request| {
            let has = req
                .headers
                .get("cookie")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|c| c.contains("user_session="));
            wiremock::ResponseTemplate::new(200).set_body_string(if has { SIGNED_IN } else { FORM })
        })
        .mount(&http)
        .await;

    // Account with the credential handle + probe wired in.
    let base = std::env::temp_dir().join(format!("oxi-cdp-md-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let audit = Arc::new(AuditLog::open(base.join("audit.jsonl")).unwrap());
    let registry = oxibrowser_core::account::AccountRegistry::open(base.join("accounts")).unwrap();
    let mut record = AccountRecord::new("acc", "127.0.0.1").unwrap();
    record.probe = Some(oxibrowser_core::account::ProbeConfig {
        url: http.uri(),
        marker: Some("Signed in".into()),
    });
    registry.add(record).unwrap();

    let mut config = oxibrowser_core::BrowserConfig::headless();
    config.enable_ssrf_filter = false;
    let browser = Arc::new(Browser::new(config).await.unwrap());
    let manager = Arc::new(oxibrowser_core::account::AccountManager::with_audit(
        registry, audit,
    ));
    let orch = Arc::new(oxibrowser_core::account::LoginOrchestrator::new(
        Arc::clone(&manager),
        Arc::clone(&browser),
        Arc::new(StaticKeyProvider::new([5u8; 32])),
    ));
    let surface = oxibrowser_cdp::LoginSurface::new(Arc::clone(&orch), Arc::clone(&browser));

    // Broker: password credential allowed exactly at the login-server origin,
    // plus a `login` grant for it.
    let engine = test_engine(Vec::new());
    let provider = Arc::new(InMemoryProvider::new());
    // The credential's scope must equal the account scope (127.0.0.1 — the
    // loopback host is its own registrable domain).
    let handle = provider
        .put(NewCredential {
            agent_id: "main".into(),
            scope: "127.0.0.1".into(),
            kind: CredentialKind::Password,
            slug: "login".into(),
            allowed_origins: vec![http.uri()],
            login_hint: Some("garden".into()),
            password: Some(SecretBox::from_string("hunter2".into())),
            otpauth_uri: None,
        })
        .unwrap()
        .0;
    engine
        .consents
        .grant(ConsentRecord::new(
            ConsentSubject::Credential {
                credential: oxibrowser_credentials::CredentialId(handle.clone()),
            },
            &http.uri(),
            &["login"],
            chrono::Duration::hours(1),
            10,
        ))
        .unwrap();
    // The account record must carry the handle (the engine resolves through it).
    let mut rec = manager.registry().get("acc").unwrap();
    rec.credentials = vec![handle.clone()];
    manager.registry().save(&rec).unwrap();

    let provider_dyn: Arc<dyn oxibrowser_credentials::CredentialProvider> = provider.clone();
    let broker = Arc::new(CredentialBroker::new(provider_dyn, Arc::clone(&engine)));

    let port = find_available_port();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let server = Arc::new(
        CdpServer::new(addr, Arc::clone(&browser))
            .with_login(Arc::clone(&surface))
            .with_credentials_broker(broker),
    );
    let s = Arc::clone(&server);
    tokio::spawn(async move {
        let _ = s.start().await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (mut sink, mut ws) = connect_ws_role(addr, None).await;
    let resp = send_command(
        &mut sink,
        &mut ws,
        1,
        "OXI.loginWithAccount",
        Some(json!({ "accountId": "acc", "agentId": "omp" })),
    )
    .await;
    let result = resp
        .get("result")
        .unwrap_or_else(|| panic!("loginWithAccount failed: {resp}"));
    assert_eq!(result["state"], "started", "{resp}");
    assert!(result["loginId"].as_str().unwrap().starts_with("alogin-"));

    // Grant-less variant would deny synchronously — verify the error code shape
    // on a fresh account below via consentRequired message prefix.
    // Drain every event for a bounded window and assert on both the
    // account flip and the terminal agent-login event.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut all = Vec::new();
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), ws.next()).await {
            Ok(Some(Ok(tungstenite::Message::Text(text)))) => {
                if let Ok(msg) = serde_json::from_str::<Value>(&text)
                    && msg
                        .get("method")
                        .and_then(|v| v.as_str())
                        .is_some_and(|m| m.starts_with("OXI."))
                {
                    let terminal = msg["params"]["state"] == json!("captured");
                    all.push(msg);
                    if terminal {
                        break;
                    }
                }
            }
            _ => continue,
        }
    }
    assert!(
        all.iter()
            .any(|e| e["method"] == json!("OXI.loginStateChanged")
                && e["params"]["state"] == json!("captured")
                && e["params"]["agentId"] == json!("omp")),
        "expected captured loginStateChanged, got {all:?}"
    );
    assert!(
        all.iter()
            .any(|e| e["method"] == json!("OXI.accountStateChanged")
                && e["params"]["state"] == json!("valid")),
        "expected accountStateChanged valid, got {all:?}"
    );

    let resp = send_command(&mut sink, &mut ws, 2, "OXI.accountList", Some(json!({}))).await;
    assert_eq!(resp["result"]["accounts"][0]["state"], "valid", "{resp}");

    server.shutdown();
    let _ = std::fs::remove_dir_all(&base);
}

/// Irreversible-action pattern gate (roadmap item 16): in an account-bound
/// context without the `irreversible` capability, clickRef on a destructive
/// control is denied (`irreversibleActionRequiresGrant`) while innocent
/// controls pass; per-account injected patterns ("purge workspace") gate
/// too; marking the context capable lets the same click through.
#[tokio::test]
async fn test_irreversible_gate_blocks_and_allows() {
    let port = find_available_port();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let mut config = oxibrowser_core::BrowserConfig::headless();
    config.enable_ssrf_filter = false;
    let browser = Arc::new(Browser::new(config).await.unwrap());

    let base = std::env::temp_dir().join(format!("oxi-cdp-e2e-irrev-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let audit = Arc::new(
        oxibrowser_core::security::audit::AuditLog::open(base.join("audit.jsonl")).unwrap(),
    );
    let registry = oxibrowser_core::account::AccountRegistry::open(base.join("accounts")).unwrap();
    registry
        .add(oxibrowser_core::account::AccountRecord::new("acc", "example.com").unwrap())
        .unwrap();
    // Per-account injection (knock's "확정 게이트"): an extra pattern on top
    // of the built-in defaults.
    let mut rec = registry.get("acc").unwrap();
    rec.irreversible_patterns = Some(vec!["purge workspace".to_string()]);
    registry.save(&rec).unwrap();
    let manager = Arc::new(oxibrowser_core::account::AccountManager::with_audit(
        registry,
        audit.clone(),
    ));
    let orch = Arc::new(oxibrowser_core::account::LoginOrchestrator::new(
        manager,
        browser.clone(),
        Arc::new(oxibrowser_core::storage::session_store::StaticKeyProvider::new([5u8; 32])),
    ));
    let surface = oxibrowser_cdp::LoginSurface::new(orch, browser.clone());
    let (ctx, _session) = surface
        .orchestrator()
        .open_account_context("acc")
        .await
        .unwrap();
    // Plain bind: bound but WITHOUT irreversible capability (the agent-path
    // shape — capability comes only from a grant probe).
    surface.bind("acc", ctx.clone());

    let server = Arc::new(
        CdpServer::new(addr, browser.clone())
            .with_primary_context(ctx.clone())
            .with_login(surface.clone()),
    );
    let s = server.clone();
    tokio::spawn(async move {
        let _ = s.start().await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (mut sink, mut ws) = connect_ws_role(addr, None).await;
    let page = "data:text/html,<html><body>                <button id='del'>Delete Account</button>                <button id='pz'>Purge Workspace</button>                <button id='ok'>Save Draft</button>                </body></html>";
    let mut next_id = 1u64;
    let resp = send_command(
        &mut sink,
        &mut ws,
        next_id,
        "Page.navigate",
        Some(json!({ "url": page })),
    )
    .await;
    assert!(resp.get("error").is_none(), "{resp}");

    // Poll the snapshot until the buttons expose refs.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let (del_ref, pz_ref, ok_ref) = loop {
        next_id += 1;
        let resp = send_command(
            &mut sink,
            &mut ws,
            next_id,
            "OXI.getInteractiveElements",
            None,
        )
        .await;
        let mut del = None;
        let mut pz = None;
        let mut ok = None;
        if let Some(elements) = resp["result"]["elements"].as_array() {
            for el in elements {
                let text = el["text"].as_str().unwrap_or_default();
                let r#ref = el["ref"].as_str().map(str::to_string);
                if text.contains("Delete Account") {
                    del = r#ref;
                } else if text.contains("Purge Workspace") {
                    pz = r#ref;
                } else if text.contains("Save Draft") {
                    ok = r#ref;
                }
            }
        }
        if let (Some(d), Some(p), Some(o)) = (del, pz, ok) {
            break (d, p, o);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "buttons never appeared"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    // Destructive (built-in pattern): denied without the capability.
    next_id += 1;
    let resp = send_command(
        &mut sink,
        &mut ws,
        next_id,
        "OXI.clickRef",
        Some(json!({ "ref": del_ref })),
    )
    .await;
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("irreversibleActionRequiresGrant"),
        "expected irreversible denial, got {resp}"
    );

    // Injected per-account pattern: denied the same way.
    next_id += 1;
    let resp = send_command(
        &mut sink,
        &mut ws,
        next_id,
        "OXI.clickRef",
        Some(json!({ "ref": pz_ref })),
    )
    .await;
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("irreversibleActionRequiresGrant"),
        "expected injected-pattern denial, got {resp}"
    );

    // Innocent control: passes.
    next_id += 1;
    let resp = send_command(
        &mut sink,
        &mut ws,
        next_id,
        "OXI.clickRef",
        Some(json!({ "ref": ok_ref })),
    )
    .await;
    assert!(resp.get("error").is_none(), "{resp}");

    // The gate decisions are audited.
    let audit_text = std::fs::read_to_string(base.join("audit.jsonl")).unwrap();
    assert!(
        audit_text.contains("irreversible_gate"),
        "gate decisions must be audited: {audit_text}"
    );

    // Grant the capability → the same destructive click passes.
    surface.mark_irreversible(ctx.id().as_str());
    next_id += 1;
    let resp = send_command(
        &mut sink,
        &mut ws,
        next_id,
        "OXI.clickRef",
        Some(json!({ "ref": del_ref })),
    )
    .await;
    assert!(resp.get("error").is_none(), "{resp}");

    server.shutdown();
    let _ = std::fs::remove_dir_all(&base);
}

/// Connect with an explicit URL (query-parameter role claims — the
/// reference viewer page's path; browsers cannot set WS headers).
async fn connect_ws_url(
    url: &str,
) -> (
    futures::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        tungstenite::Message,
    >,
    futures::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
) {
    let request = url.into_client_request().unwrap();
    let (ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    ws.split()
}

/// Reference viewer (roadmap item 10): `/viewer` serves the page, and the
/// viewer role can be claimed via **query parameters** — same one-time token
/// semantics as the header path, enabling the browser-hosted mirror.
#[tokio::test]
async fn test_viewer_page_and_query_param_role() {
    let (server, addr, surface, _base) = start_account_server("viewerq", None).await;

    // The reference page is served, self-contained.
    let resp = reqwest::get(format!("http://{addr}/viewer")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let html = resp.text().await.unwrap();
    assert!(html.contains("viewer token"), "viewer page: {:.120}", html);
    assert!(html.contains("resolveConfirmation"), "approval card wired");

    // Mint a one-time viewer token (out-of-band: orchestrator take).
    let (mut sink, mut ws) = connect_ws_role(addr, None).await;
    let resp = send_command(
        &mut sink,
        &mut ws,
        1,
        "OXI.beginLogin",
        Some(json!({ "accountId": "acc", "mode": "user" })),
    )
    .await;
    assert!(resp.get("error").is_none(), "{resp}");
    let login_id = resp["result"]["loginId"].as_str().unwrap().to_string();
    let token = surface
        .orchestrator()
        .take_viewer_token(&login_id)
        .expect("out-of-band viewer token");

    // Viewer role via the query string — the reference page's connect URL.
    let url = format!("ws://{addr}/ws?role=viewer&viewer_token={token}");
    let (mut vsink, mut vws) = connect_ws_url(&url).await;

    // Viewer allowlist: navigate passes…
    let resp = send_command(
        &mut vsink,
        &mut vws,
        1,
        "Page.navigate",
        Some(json!({ "url": "about:blank" })),
    )
    .await;
    assert!(resp.get("error").is_none(), "viewer navigate: {resp}");
    // …cookies are denied.
    let resp = send_command(
        &mut vsink,
        &mut vws,
        2,
        "Network.getCookies",
        Some(json!({})),
    )
    .await;
    assert_eq!(
        resp["error"]["message"], "deniedForViewerRole",
        "query-param viewer must be gated like the header path: {resp}"
    );

    // A bad token via query is rejected at upgrade time.
    let bad = format!("ws://{addr}/ws?role=viewer&viewer_token=oxi-viewer-bogus");
    assert!(
        tokio_tungstenite::connect_async(bad.as_str()).await.is_err(),
        "bogus query token must be rejected"
    );

    server.shutdown();
}

/// IndexedDB (roadmap item 12 / FM-L5): a token written through the JS
/// `indexedDB` API survives navigation — the JS-side map is wiped on every
/// document injection and re-seeded from the context bucket, which the IDB
/// sync thread maintains. This is the storage plane IDB-auth sites need.
#[tokio::test]
async fn test_indexeddb_persists_across_navigations() {
    let port = find_available_port();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let mut config = oxibrowser_core::BrowserConfig::headless();
    config.enable_ssrf_filter = false;
    let browser = Arc::new(Browser::new(config).await.unwrap());
    let server = Arc::new(CdpServer::new(addr, browser.clone()));
    let s = server.clone();
    tokio::spawn(async move {
        let _ = s.start().await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (mut sink, mut ws) = connect_ws_role(addr, None).await;
    let page = "data:text/html,<html><body>idb test</body></html>";
    let resp = send_command(
        &mut sink,
        &mut ws,
        1,
        "Page.navigate",
        Some(json!({ "url": page })),
    )
    .await;
    assert!(resp.get("error").is_none(), "{resp}");

    // Open the database, create the store, put an auth token (the canonical
    // IDB-auth shape), awaiting the deferred events via promises.
    let put_js = r#"(async () => {
        const req = indexedDB.open("auth", 1);
        req.onupgradeneeded = () => {
            req.result.createObjectStore("tokens", { keyPath: "id" });
        };
        await new Promise((res, rej) => { req.onsuccess = res; req.onerror = rej; });
        const tx = req.result.transaction("tokens", "readwrite");
        tx.objectStore("tokens").put({ id: "gcp", token: "tok-123" });
        await new Promise((res) => { tx.oncomplete = res; });
        return "put-ok";
    })()"#;
    let resp = send_command(
        &mut sink,
        &mut ws,
        2,
        "Runtime.evaluate",
        Some(json!({ "expression": put_js, "awaitPromise": true })),
    )
    .await;
    assert!(
        resp["result"]["result"]["value"] == json!("put-ok"),
        "put failed: {resp}"
    );

    // Navigate away — the JS-side IDB map is wiped by re-registration.
    let resp = send_command(
        &mut sink,
        &mut ws,
        3,
        "Page.navigate",
        Some(json!({ "url": "about:blank" })),
    )
    .await;
    assert!(resp.get("error").is_none(), "{resp}");

    // Navigate back to the same opaque URL — its bucket re-seeds the map.
    let resp = send_command(
        &mut sink,
        &mut ws,
        4,
        "Page.navigate",
        Some(json!({ "url": page })),
    )
    .await;
    assert!(resp.get("error").is_none(), "{resp}");

    let get_js = r#"(async () => {
        const req = indexedDB.open("auth");
        await new Promise((res, rej) => { req.onsuccess = res; req.onerror = rej; });
        return await new Promise((res) => {
            const tx = req.result.transaction("tokens", "readonly");
            const get = tx.objectStore("tokens").get("gcp");
            get.onsuccess = () => res(get.result ? get.result.token : null);
        });
    })()"#;
    let resp = send_command(
        &mut sink,
        &mut ws,
        5,
        "Runtime.evaluate",
        Some(json!({ "expression": get_js, "awaitPromise": true })),
    )
    .await;
    assert!(
        resp["result"]["result"]["value"] == json!("tok-123"),
        "token must survive navigation: {resp}"
    );

    // Finding-1 pin: after restore, a KEYLESS put against the keyPath store
    // must work (the seeded key_paths must survive the navigation re-seed).
    let put2_js = r#"(async () => {
        const req = indexedDB.open("auth");
        await new Promise((res, rej) => { req.onsuccess = res; req.onerror = rej; });
        const tx = req.result.transaction("tokens", "readwrite");
        tx.objectStore("tokens").put({ id: "gcp2", token: "tok-456" });
        await new Promise((res) => { tx.oncomplete = res; });
        const get2 = tx.objectStore("tokens").get("gcp2");
        get2.onsuccess = () => { __idb_put2 = get2.result ? get2.result.token : "MISSING"; };
        return "queued";
    })()"#;
    let resp = send_command(
        &mut sink,
        &mut ws,
        6,
        "Runtime.evaluate",
        Some(json!({ "expression": put2_js, "awaitPromise": true })),
    )
    .await;
    assert!(resp.get("error").is_none(), "{resp}");
    let resp = send_command(
        &mut sink,
        &mut ws,
        7,
        "Runtime.evaluate",
        Some(json!({ "expression": "__idb_put2" })),
    )
    .await;
    assert!(
        resp["result"]["result"]["value"] == json!("tok-456"),
        "keyless keyPath put after restore: {resp}"
    );

    server.shutdown();
}
