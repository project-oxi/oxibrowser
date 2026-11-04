#![cfg(feature = "browser")]
//! Observe idle progress through DOM.getDocument, which does not drain JS jobs.
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn idle_document(extra: &[&str]) -> Value {
    let fixture = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_string("<!doctype html><body>idle fixture</body>"),
        )
        .mount(&fixture)
        .await;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_oxibrowser"))
            .args([
                "--no-audit",
                "serve",
                "--allow-private-ips",
                "--port",
                &port.to_string(),
            ])
            .args(extra)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let client = reqwest::Client::new();
    let endpoint = format!("http://127.0.0.1:{port}/json/list");
    let targets: Value = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(response) = client.get(&endpoint).send().await
                && let Ok(targets) = response.json::<Value>().await
            {
                break targets;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let url = targets[0]["webSocketDebuggerUrl"].as_str().unwrap();
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    socket
        .send(Message::Text(
            json!({"id":0,"method":"Page.navigate","params":{"url":fixture.uri()}})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    loop {
        if let Message::Text(text) = socket.next().await.unwrap().unwrap()
            && serde_json::from_str::<Value>(&text).unwrap()["id"] == 0
        {
            break;
        }
    }
    socket.send(Message::Text(json!({"id":1,"method":"Runtime.evaluate","params":{
        "expression":"setTimeout(() => document.body.setAttribute('data-idle', 'progressed'), 500)",
        "returnByValue":true
    }}).to_string().into())).await.unwrap();
    loop {
        let message = socket.next().await.unwrap().unwrap();
        if let Message::Text(text) = message
            && serde_json::from_str::<Value>(&text).unwrap()["id"] == 1
        {
            break;
        }
    }
    tokio::time::sleep(Duration::from_millis(1200)).await;
    socket
        .send(Message::Text(
            json!({"id":2,"method":"DOM.getDocument","params":{"depth":-1}})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Message::Text(text) = socket.next().await.unwrap().unwrap() {
                let response: Value = serde_json::from_str(&text).unwrap();
                if response["id"] == 2 {
                    break response;
                }
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn serve_defaults_to_autonomous_progress_and_allows_explicit_opt_out() {
    let enabled = tokio::time::timeout(Duration::from_secs(30), idle_document(&[]))
        .await
        .unwrap()
        .to_string();
    assert!(enabled.contains("progressed"), "{enabled}");
    let disabled = tokio::time::timeout(
        Duration::from_secs(30),
        idle_document(&["--no-background-event-loop"]),
    )
    .await
    .unwrap()
    .to_string();
    assert!(!disabled.contains("progressed"), "{disabled}");
}
