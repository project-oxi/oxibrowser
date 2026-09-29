//! Puppeteer/Playwright smoke test — verifies CDP compatibility with external clients.
//!

//! 1. Launch the CDP server (spawn OxiBrowser binary as child process)
//! 2. Connect via WebSocket
//! 3. Enable Page + Runtime domains
//! 4. Navigate to a local HTTP server
//! 5. Evaluate JavaScript
//! 6. Inspect DOM via CSS selector
//! 7. Close
//!
//! Run with: `cargo test -p oxibrowser --test smoke`

use futures::{SinkExt, StreamExt};
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Spawn the OxiBrowser binary and wait for the CDP server to be ready.
struct OxiBrowserProcess {
    child: std::process::Child,
    port: u16,
}

impl OxiBrowserProcess {
    /// Spawn `cargo run -p oxibrowser -- serve --port <port>`.
    fn start(port: u16) -> Self {
        let child = Command::new("cargo")
            .args([
                "run",
                "-p",
                "oxibrowser",
                "--features",
                "browser",
                "--",
                "serve",
                "--allow-private-ips",
                "--port",
                &port.to_string(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn OxiBrowser");

        Self { child, port }
    }

    /// Wait for the CDP HTTP endpoint to respond, returning the socket address.
    async fn wait_ready(&self) -> SocketAddr {
        let addr: SocketAddr = format!("127.0.0.1:{}", self.port).parse().unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                panic!("OxiBrowser CDP server did not become ready within 30s");
            }

            match reqwest::get(format!("http://{addr}/json/version")).await {
                Ok(resp) if resp.status().is_success() => return addr,
                _ => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }
}

impl Drop for OxiBrowserProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A minimal HTTP server that serves static HTML on a random port.
struct TestHttpServer {
    addr: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl TestHttpServer {
    fn start(html: &'static str) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
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

/// Connect to a CDP server WebSocket and return the split sink + stream.
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
    ws.split()
}

/// Send a CDP command and return the JSON response (skipping events).
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
    params: Option<serde_json::Value>,
) -> serde_json::Value {
    let msg = match params {
        Some(p) => serde_json::json!({ "id": id, "method": method, "params": p }),
        None => serde_json::json!({ "id": id, "method": method }),
    };

    sink.send(tungstenite::Message::Text(msg.to_string().into()))
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("timeout waiting for response to command {id} ({method})");
        }
        match timeout(remaining, ws.next()).await {
            Ok(Some(Ok(tungstenite::Message::Text(text)))) => {
                let response: serde_json::Value = serde_json::from_str(&text).unwrap();
                if response.get("id").and_then(|v| v.as_u64()) == Some(id) {
                    return response;
                }
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(e))) => panic!("WebSocket error: {e}"),
            Ok(None) => panic!("WebSocket stream ended before response"),
            Err(_) => panic!("timeout waiting for response to command {id}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Smoke tests — mirror what Puppeteer does
// ---------------------------------------------------------------------------

/// Simulates: `const browser = await puppeteer.connect({ browserWSEndpoint })`
#[tokio::test]
#[ignore] // Requires `cargo run` build; run with `cargo test -p oxibrowser --test smoke -- --ignored`
async fn test_puppeteer_connect_and_get_version() {
    let port = {
        use std::net::TcpListener;
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    };
    let _oxi = OxiBrowserProcess::start(port);
    let addr = _oxi.wait_ready().await;

    let (mut sink, mut ws) = connect_ws(addr).await;

    let resp = send_command(&mut sink, &mut ws, 1, "Browser.getVersion", None).await;

    assert_eq!(resp["id"], 1);
    assert!(resp["result"].is_object());
    assert_eq!(resp["result"]["protocolVersion"], "1.3");
    // Note: Browser.getVersion does NOT return webSocketDebuggerUrl in OxiBrowser.
    // The URL is only in /json/version endpoint response.
    assert!(resp["result"]["product"].is_string());

    let _ = send_command(&mut sink, &mut ws, 2, "Browser.close", None).await;
}

/// Simulates: `const page = await browser.newPage()` → Target.createTarget
#[tokio::test]
#[ignore] // Requires `cargo run` build; run with `cargo test -p oxibrowser --test smoke -- --ignored`
async fn test_puppeteer_new_page_equivalent() {
    let port = {
        use std::net::TcpListener;
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    };
    let _oxi = OxiBrowserProcess::start(port);
    let addr = _oxi.wait_ready().await;

    let (mut sink, mut ws) = connect_ws(addr).await;

    let resp = send_command(
        &mut sink,
        &mut ws,
        1,
        "Target.createTarget",
        Some(serde_json::json!({ "url": "about:blank" })),
    )
    .await;

    assert_eq!(resp["id"], 1);
    assert!(resp["result"]["targetId"].is_string());

    let target_id = resp["result"]["targetId"].as_str().unwrap();

    let resp = send_command(
        &mut sink,
        &mut ws,
        2,
        "Target.attachToTarget",
        Some(serde_json::json!({ "targetId": target_id })),
    )
    .await;
    assert_eq!(resp["id"], 2);
    assert!(resp["result"]["sessionId"].is_string());

    let _ = send_command(&mut sink, &mut ws, 3, "Browser.close", None).await;
}

/// Simulates the full Puppeteer workflow:
/// ```js
/// const browser = await puppeteer.connect({ browserWSEndpoint });
/// const page = await browser.newPage();
/// await page.goto('http://example.com');
/// const title = await page.title();
/// await page.evaluate(() => document.querySelector('h1').textContent);
/// await browser.close();
/// ```
#[tokio::test]
#[ignore] // Requires `cargo run` build; run with `cargo test -p oxibrowser --test smoke -- --ignored`
async fn test_puppeteer_full_workflow() {
    let html = r#"<!DOCTYPE html>
<html lang="en">
<head><title>Smoke Test Page</title></head>
<body>
    <h1 id="title">Hello from OxiBrowser</h1>
    <p class="content">This page verifies Puppeteer compatibility.</p>
    <span class="item">Item A</span>
    <span class="item">Item B</span>
</body>
</html>"#;

    let http_server = TestHttpServer::start(html);

    let port = {
        use std::net::TcpListener;
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    };
    let _oxi = OxiBrowserProcess::start(port);
    let addr = _oxi.wait_ready().await;
    let (mut sink, mut ws) = connect_ws(addr).await;

    // 1. Browser.getVersion
    let resp = send_command(&mut sink, &mut ws, 1, "Browser.getVersion", None).await;
    assert_eq!(resp["id"], 1);
    assert_eq!(resp["result"]["protocolVersion"], "1.3");

    // 2. Runtime.enable
    let resp = send_command(&mut sink, &mut ws, 2, "Runtime.enable", None).await;
    assert_eq!(resp["id"], 2);

    // 3. Page.enable
    let resp = send_command(&mut sink, &mut ws, 3, "Page.enable", None).await;
    assert_eq!(resp["id"], 3);

    // 4. Page.setLifecycleEventsEnabled(true)
    let resp = send_command(
        &mut sink,
        &mut ws,
        4,
        "Page.setLifecycleEventsEnabled",
        Some(serde_json::json!({ "enabled": true })),
    )
    .await;
    assert_eq!(resp["id"], 4);

    // 5. Page.navigate
    let url = format!("http://{}/", http_server.addr());
    let resp = send_command(
        &mut sink,
        &mut ws,
        5,
        "Page.navigate",
        Some(serde_json::json!({ "url": url })),
    )
    .await;
    assert_eq!(resp["id"], 5);
    assert!(resp["result"]["frameId"].is_string());

    // 6. Runtime.evaluate: document.title
    let resp = send_command(
        &mut sink,
        &mut ws,
        6,
        "Runtime.evaluate",
        Some(serde_json::json!({ "expression": "document.title" })),
    )
    .await;
    assert_eq!(resp["id"], 6);
    assert_eq!(resp["result"]["result"]["type"], "string");
    assert_eq!(resp["result"]["result"]["value"], "Smoke Test Page");

    // 7. Runtime.evaluate: document.querySelector('#title').textContent
    let resp = send_command(
        &mut sink,
        &mut ws,
        7,
        "Runtime.evaluate",
        Some(serde_json::json!({
            "expression": "document.querySelector('#title').textContent"
        })),
    )
    .await;
    assert_eq!(resp["id"], 7);
    assert_eq!(resp["result"]["result"]["value"], "Hello from OxiBrowser");

    // 8. Runtime.evaluate: document.querySelectorAll('.item').length
    let resp = send_command(
        &mut sink,
        &mut ws,
        8,
        "Runtime.evaluate",
        Some(serde_json::json!({
            "expression": "document.querySelectorAll('.item').length"
        })),
    )
    .await;
    assert_eq!(resp["id"], 8);
    assert_eq!(resp["result"]["result"]["value"], 2);

    // 9. DOM.getDocument
    let resp = send_command(&mut sink, &mut ws, 9, "DOM.getDocument", None).await;
    assert_eq!(resp["id"], 9);
    assert_eq!(resp["result"]["root"]["nodeType"], 9);

    // 10. DOM.querySelector (#title)
    let resp = send_command(
        &mut sink,
        &mut ws,
        10,
        "DOM.querySelector",
        Some(serde_json::json!({ "nodeId": 0, "selector": "#title" })),
    )
    .await;
    assert_eq!(resp["id"], 10);
    let heading_id = resp["result"]["nodeId"].as_u64().unwrap();
    assert!(heading_id > 0);

    // 11. Page.getFrameTree
    let resp = send_command(&mut sink, &mut ws, 11, "Page.getFrameTree", None).await;
    assert_eq!(resp["id"], 11);
    let frame_url = resp["result"]["frameTree"]["frame"]["url"]
        .as_str()
        .unwrap();
    assert!(frame_url.contains("127.0.0.1"));

    // 12. Browser.close
    let resp = send_command(&mut sink, &mut ws, 12, "Browser.close", None).await;
    assert_eq!(resp["id"], 12);
}
