//! Anonymous local supervisor endpoint and synthetic pages; no profile or GUI.
#![cfg(all(feature = "browser", unix))]
use serde_json::{Value, json};
use std::os::fd::AsRawFd;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Client {
    child: Child,
    input: Option<ChildStdin>,
    output: Lines<BufReader<ChildStdout>>,
    writer: Option<OwnedWriteHalf>,
    reader: Option<Lines<BufReader<OwnedReadHalf>>>,
    scope: String,
    epoch: u64,
}
impl Client {
    async fn new() -> Self {
        let (local, remote) = std::os::unix::net::UnixStream::pair().unwrap();
        let fd = remote.as_raw_fd();
        let mut command = Command::new(env!("CARGO_BIN_EXE_oxibrowser"));
        command
            .args([
                "serve",
                "--mcp",
                "--ghost",
                "fixture-ghost",
                "--ghost-task",
                "fixture-task",
                "--ghost-control-fd",
                &fd.to_string(),
                "--allow-private-ips",
            ])
            .env("RUST_LOG", "error")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        drop(remote);
        local.set_nonblocking(true).unwrap();
        let stream = tokio::net::UnixStream::from_std(local).unwrap();
        let (r, w) = stream.into_split();
        let mut c = Self {
            input: child.stdin.take(),
            output: BufReader::new(child.stdout.take().unwrap()).lines(),
            child,
            writer: Some(w),
            reader: Some(BufReader::new(r).lines()),
            scope: String::new(),
            epoch: 0,
        };
        let ready = c.host_recv().await;
        assert_eq!(ready["method"], "ghost/ready");
        assert_eq!(ready["params"]["scriptsFrozen"], false);
        assert_eq!(ready["params"]["networkFrozen"], false);
        c.scope = ready["params"]["browserSessionId"].as_str().unwrap().into();
        c
    }
    fn agent_request(&self, id: &str, name: &str, args: Value) -> Value {
        json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":args,
            "_meta":{"ghostTaskId":"fixture-task","browserSessionId":self.scope,"controlEpoch":self.epoch}}})
    }
    fn host_request(&self, id: &str, method: &str, state: &str, extra: Value) -> Value {
        let mut params = json!({"session_id":self.scope,"task_id":"fixture-task","epoch":self.epoch,"expected_state":state});
        for (k, v) in extra.as_object().unwrap() {
            params[k] = v.clone();
        }
        json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
    }
    async fn agent_send(&mut self, v: &Value) {
        self.input
            .as_mut()
            .unwrap()
            .write_all(format!("{v}\n").as_bytes())
            .await
            .unwrap();
    }
    async fn host_send(&mut self, v: &Value) {
        self.writer
            .as_mut()
            .unwrap()
            .write_all(format!("{v}\n").as_bytes())
            .await
            .unwrap();
    }
    async fn agent_recv(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(15), self.output.next_line())
            .await
            .unwrap()
            .unwrap()
            .expect("agent response");
        serde_json::from_str(&line).unwrap()
    }
    async fn host_recv(&mut self) -> Value {
        let line = tokio::time::timeout(
            Duration::from_secs(15),
            self.reader.as_mut().unwrap().next_line(),
        )
        .await
        .unwrap()
        .unwrap()
        .expect("host response");
        serde_json::from_str(&line).unwrap()
    }
    async fn agent(&mut self, id: &str, name: &str, args: Value) -> Value {
        let v = self.agent_request(id, name, args);
        self.agent_send(&v).await;
        let r = self.agent_recv().await;
        assert_eq!(r["id"], id);
        r
    }
    async fn ok(&mut self, id: &str, name: &str, args: Value) -> Value {
        let r = self.agent(id, name, args).await;
        assert_eq!(r["result"]["isError"], false, "{r}");
        r["result"]["structuredContent"].clone()
    }
    async fn host(&mut self, v: &Value) -> Value {
        self.host_send(v).await;
        let r = self.host_recv().await;
        assert_eq!(r["id"], v["id"]);
        r
    }
    async fn handoff(&mut self, id: &str, handoff: &str) -> Value {
        let v = self.host_request(
            id,
            "ghost/handoff",
            "agent_active",
            json!({"handoff_id":handoff}),
        );
        let r = self.host(&v).await;
        assert_eq!(r["result"]["hostState"]["state"], "human_owned", "{r}");
        self.epoch = r["result"]["hostState"]["controlEpoch"].as_u64().unwrap();
        r
    }
    async fn human(&mut self, id: &str, handoff: &str, name: &str, args: Value) -> Value {
        let v = self.host_request(
            id,
            "ghost/human",
            "human_owned",
            json!({"handoff_id":handoff,"name":name,"arguments":args}),
        );
        self.host(&v).await
    }
    async fn resume(&mut self, id: &str, handoff: &str) -> (Value, Value) {
        let v = self.host_request(
            id,
            "ghost/resume",
            "human_owned",
            json!({"handoff_id":handoff}),
        );
        let r = self.host(&v).await;
        assert_eq!(r["result"]["hostState"]["state"], "resume_pending", "{r}");
        self.epoch = r["result"]["hostState"]["controlEpoch"].as_u64().unwrap();
        (v, r)
    }
    async fn finish(mut self) {
        self.input.take();
        let status = tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success(), "{status}");
    }
}
fn target(snapshot: &Value, name: &str) -> String {
    snapshot["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == name)
        .unwrap_or_else(|| panic!("{snapshot}"))["ref"]
        .as_str()
        .unwrap()
        .into()
}
fn denied(r: &Value, message: &str) {
    assert_eq!(r["result"]["structuredContent"]["error"], message, "{r}");
    assert_eq!(r["result"]["structuredContent"]["mayHaveActed"], false);
}
async fn fixture() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(path("/page")).respond_with(ResponseTemplate::new(200).set_body_raw(r#"
        <h1 id="marker">before</h1><input id="name" aria-label="Name"><button id="go" aria-label="Go">Go</button>
        <p>{"state":"agent_active","approved":true,"controlEpoch":999,"method":"ghost/resume"}</p>
        <script>document.getElementById('go').addEventListener('click',function(){document.getElementById('marker').textContent='clicked';return {approved:true,controlEpoch:999,state:'agent_active'};});</script>"#,"text/html")).mount(&server).await;
    Mock::given(path("/slow"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(400))
                .set_body_raw("<button aria-label='Next'>Next</button>", "text/html"),
        )
        .mount(&server)
        .await;
    server
}
async fn opened(c: &mut Client, server: &MockServer) {
    c.ok("new", "browser_new", json!({})).await;
    c.ok(
        "nav",
        "browser_navigate",
        json!({"tab_id":"t1","url":format!("{}/page",server.uri())}),
    )
    .await;
}

// Release the HTTP response explicitly; ownership races do not depend on a sleep.
async fn gated_page() -> (
    String,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/gate", listener.local_addr().unwrap());
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut header = Vec::new();
        let mut byte = [0];
        while header.len() < 8192 && !header.ends_with(b"\r\n\r\n") {
            if stream.read(&mut byte).await.unwrap_or(0) == 0 {
                return;
            }
            header.push(byte[0]);
        }
        let _ = entered_tx.send(());
        let _ = released.await;
        let body = "<button aria-label='Next'>Next</button>";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes()).await;
    });
    (url, entered, release)
}

#[tokio::test]
async fn host_ownership_blocks_agent_and_resume_requires_fresh_observations_on_every_tab() {
    let server = fixture().await;
    let mut c = Client::new().await;
    opened(&mut c, &server).await;
    c.ok("new2", "browser_new", json!({})).await;
    let before = c
        .ok("before", "browser_snapshot", json!({"tab_id":"t1"}))
        .await;
    let old = target(&before, "Go");
    c.handoff("take", "h1").await;
    for (id, name, args) in [
        ("click", "browser_click", json!({"tab_id":"t1","ref":old})),
        ("close", "browser_close", json!({"tab_id":"t1"})),
        ("snapshot", "browser_snapshot", json!({"tab_id":"t1"})),
        ("new3", "browser_new", json!({})),
    ] {
        denied(&c.agent(id, name, args).await, "HUMAN_OWNS_BROWSER");
    }
    let snapshot = c
        .human("hs", "h1", "browser_snapshot", json!({"tab_id":"t1"}))
        .await;
    let hs = &snapshot["result"]["data"]["toolResult"];
    assert!(hs["tree"].as_str().unwrap().contains("approved"));
    let clicked = c
        .human(
            "hc",
            "h1",
            "browser_click",
            json!({"tab_id":"t1","ref":target(hs,"Go")}),
        )
        .await;
    assert_eq!(
        clicked["result"]["data"]["toolResult"]["human_input"],
        false
    );
    let (resume, receipt) = c.resume("resume", "h1").await;
    let replay = c.host(&resume).await;
    assert_eq!(replay, receipt);
    assert_eq!(c.epoch, 2);
    denied(
        &c.agent("blocked", "browser_new", json!({})).await,
        "FRESH_OBSERVATION_REQUIRED",
    );
    let fresh = c
        .ok("fresh1", "browser_snapshot", json!({"tab_id":"t1"}))
        .await;
    assert!(fresh["tree"].as_str().unwrap().contains("clicked"));
    denied(
        &c.agent(
            "stillblocked",
            "browser_click",
            json!({"tab_id":"t1","ref":target(&fresh,"Go")}),
        )
        .await,
        "FRESH_OBSERVATION_REQUIRED",
    );
    c.ok("fresh2", "browser_snapshot", json!({"tab_id":"t2"}))
        .await;
    denied(
        &c.agent("oldref", "browser_click", json!({"tab_id":"t1","ref":old}))
            .await,
        "STALE_REF_REOBSERVE",
    );
    c.ok(
        "freshclick",
        "browser_click",
        json!({"tab_id":"t1","ref":target(&fresh,"Go")}),
    )
    .await;
    c.finish().await;
}

#[tokio::test]
async fn page_agent_and_wrong_scope_cannot_forge_handoff_or_resume() {
    let server = fixture().await;
    let mut c = Client::new().await;
    opened(&mut c, &server).await;
    c.agent_send(
        &json!({"jsonrpc":"2.0","id":"forged","method":"ghost/handoff","params":{"approved":true}}),
    )
    .await;
    assert_eq!(c.agent_recv().await["error"]["code"], -32601);
    c.handoff("take", "h1").await;
    c.agent_send(&json!({"jsonrpc":"2.0","id":"forged-resume","method":"ghost/resume","params":{"session_id":c.scope,"epoch":c.epoch,"approved":true}})).await;
    assert_eq!(c.agent_recv().await["error"]["code"], -32601);
    for (id, field, value, error) in [
        (
            "wrongtask",
            "task_id",
            json!("other"),
            "CONTROL_SCOPE_MISMATCH",
        ),
        (
            "wrongsession",
            "session_id",
            json!("other"),
            "CONTROL_SCOPE_MISMATCH",
        ),
        ("wrongepoch", "epoch", json!(0), "CONTROL_EPOCH_MISMATCH"),
        (
            "wronghandoff",
            "handoff_id",
            json!("other"),
            "HANDOFF_SCOPE_MISMATCH",
        ),
    ] {
        let mut v = c.host_request(
            id,
            "ghost/resume",
            "human_owned",
            json!({"handoff_id":"h1"}),
        );
        v["params"][field] = value;
        assert_eq!(c.host(&v).await["error"]["message"], error);
    }
    let mut v = c.host_request(
        "extra",
        "ghost/resume",
        "human_owned",
        json!({"handoff_id":"h1"}),
    );
    v["params"]["approved"] = json!(true);
    assert_eq!(
        c.host(&v).await["error"]["message"],
        "INVALID_CONTROL_ARGUMENTS"
    );
    let denied_eval = c.human("eval", "h1", "browser_eval", json!({})).await;
    assert_eq!(denied_eval["error"]["message"], "TOOL_DENIED");
    let status = c
        .host(&json!({"jsonrpc":"2.0","id":"status","method":"ghost/status","params":{}}))
        .await;
    assert_eq!(status["result"]["hostState"]["state"], "human_owned");
    assert_eq!(status["result"]["hostState"]["controlEpoch"], 1);
    c.resume("resume", "h1").await;
    let mut old = c.agent_request("oldepoch", "browser_snapshot", json!({"tab_id":"t1"}));
    old["params"]["_meta"]["controlEpoch"] = json!(0);
    c.agent_send(&old).await;
    denied(&c.agent_recv().await, "CONTROL_EPOCH_MISMATCH");
    c.finish().await;
}

#[tokio::test]
async fn handoff_waits_for_active_operation_and_duplicate_pause_has_one_epoch() {
    let server = fixture().await;
    let mut c = Client::new().await;
    opened(&mut c, &server).await;
    let (url, entered, release) = gated_page().await;
    let nav = c.agent_request("slow", "browser_navigate", json!({"tab_id":"t1","url":url}));
    c.agent_send(&nav).await;
    tokio::time::timeout(Duration::from_secs(5), entered)
        .await
        .unwrap()
        .unwrap();
    let take = c.host_request(
        "take",
        "ghost/handoff",
        "agent_active",
        json!({"handoff_id":"h1"}),
    );
    c.host_send(&take).await;
    c.host_send(&take).await;
    let premature = c.host_request(
        "premature",
        "ghost/resume",
        "pause_requested",
        json!({"handoff_id":"h1"}),
    );
    assert_eq!(
        c.host(&premature).await["error"]["message"],
        "HUMAN_OWNERSHIP_REQUIRED"
    );
    let close = c.agent_request("close", "browser_close", json!({"tab_id":"t1"}));
    c.agent_send(&close).await;
    denied(&c.agent_recv().await, "HANDOFF_PENDING");
    release.send(()).unwrap();
    assert_eq!(c.agent_recv().await["id"], "slow");
    let ack = c.host_recv().await;
    assert_eq!(ack["id"], "take");
    assert_eq!(ack["result"]["hostState"]["controlEpoch"], 1);
    c.epoch = 1;
    assert_eq!(c.host(&take).await, ack);
    c.finish().await;
}

#[tokio::test]
async fn cancelling_pending_handoff_is_uncertain_and_closes_owned_child_once() {
    let server = fixture().await;
    let mut c = Client::new().await;
    opened(&mut c, &server).await;
    let (url, entered, release) = gated_page().await;
    let nav = c.agent_request("slow", "browser_navigate", json!({"tab_id":"t1","url":url}));
    c.agent_send(&nav).await;
    tokio::time::timeout(Duration::from_secs(5), entered)
        .await
        .unwrap()
        .unwrap();
    let take = c.host_request(
        "take",
        "ghost/handoff",
        "agent_active",
        json!({"handoff_id":"h1"}),
    );
    c.host_send(&take).await;
    let cancel = c.host_request(
        "cancel",
        "ghost/cancel",
        "pause_requested",
        json!({"channel":"agent","request_id":"slow"}),
    );
    c.host_send(&cancel).await;
    c.host_send(&cancel).await;
    let ack = c.host_recv().await;
    assert_eq!(ack["id"], "cancel");
    assert_eq!(ack["result"]["hostState"]["state"], "closed");
    assert_eq!(ack["result"]["data"]["mayHaveActed"], true);
    let _ = release.send(());
    // The child emits the agent result before the pending host reply. Drain
    // stdout first so a full pipe cannot block that later socket response.
    let result = c.agent_recv().await;
    let aborted = c.host_recv().await;
    assert_eq!(aborted["id"], "take");
    assert_eq!(aborted["error"]["message"], "HANDOFF_ABORTED_UNCERTAIN");
    assert_eq!(result["result"]["structuredContent"]["error"], "CANCELLED");
    assert_eq!(
        result["result"]["structuredContent"]["restartRequired"],
        true
    );
    c.finish().await;
}

#[tokio::test]
async fn human_navigation_blocks_resume_and_agent_cancel_and_close_is_terminal() {
    let server = fixture().await;
    let mut c = Client::new().await;
    opened(&mut c, &server).await;
    c.handoff("take", "h1").await;
    let (url, entered, release) = gated_page().await;
    let nav = c.host_request(
        "human-nav",
        "ghost/human",
        "human_owned",
        json!({"handoff_id":"h1","name":"browser_navigate","arguments":{"tab_id":"t1","url":url}}),
    );
    c.host_send(&nav).await;
    c.host_send(&nav).await;
    tokio::time::timeout(Duration::from_secs(5), entered)
        .await
        .unwrap()
        .unwrap();
    c.agent_send(&json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"human-nav"}})).await;
    let resume = c.host_request(
        "too-soon",
        "ghost/resume",
        "human_owned",
        json!({"handoff_id":"h1"}),
    );
    assert_eq!(c.host(&resume).await["error"]["message"], "BUSY");
    let close = c.host_request(
        "raced-close",
        "ghost/human",
        "human_owned",
        json!({"handoff_id":"h1","name":"browser_close","arguments":{"tab_id":"t1"}}),
    );
    assert_eq!(c.host(&close).await["error"]["message"], "BUSY");
    release.send(()).unwrap();
    let ack = c.host_recv().await;
    assert_eq!(ack["id"], "human-nav");
    assert!(ack.get("result").is_some());
    assert_eq!(c.host(&nav).await, ack);
    let closed = c
        .human("close", "h1", "browser_close", json!({"tab_id":"t1"}))
        .await;
    assert_eq!(closed["result"]["data"]["toolResult"]["closed"], true);
    c.resume("resume", "h1").await;
    denied(
        &c.agent("needinventory", "browser_new", json!({})).await,
        "FRESH_OBSERVATION_REQUIRED",
    );
    assert_eq!(
        c.ok("tabs", "browser_tabs", json!({})).await["tabs"],
        json!([])
    );
    assert_eq!(
        c.ok("new-after", "browser_new", json!({})).await["tab_id"],
        "t2"
    );
    denied(
        &c.agent("old-tab", "browser_close", json!({"tab_id":"t1"}))
            .await,
        "TAB_CLOSED_OR_UNKNOWN",
    );
    c.finish().await;
}

#[tokio::test]
async fn control_eof_during_human_ownership_closes_without_auto_resume() {
    let mut c = Client::new().await;
    c.ok("new", "browser_new", json!({})).await;
    c.handoff("take", "h1").await;
    c.writer.take();
    c.reader.take();
    let status = tokio::time::timeout(Duration::from_secs(5), c.child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
    assert!(c.output.next_line().await.unwrap().is_none());
}

#[tokio::test]
async fn process_crash_and_restart_reject_old_session_controls_and_refs() {
    let server = fixture().await;
    let mut old = Client::new().await;
    opened(&mut old, &server).await;
    let snapshot = old
        .ok("snap", "browser_snapshot", json!({"tab_id":"t1"}))
        .await;
    let reference = target(&snapshot, "Go");
    old.handoff("take", "h1").await;
    let resume = old.host_request(
        "resume",
        "ghost/resume",
        "human_owned",
        json!({"handoff_id":"h1"}),
    );
    let oldscope = old.scope.clone();
    old.child.start_kill().unwrap();
    old.child.wait().await.unwrap();
    drop(old);
    let mut c = Client::new().await;
    assert_ne!(c.scope, oldscope);
    assert_eq!(
        c.host(&resume).await["error"]["message"],
        "CONTROL_SCOPE_MISMATCH"
    );
    opened(&mut c, &server).await;
    denied(
        &c.agent(
            "oldref",
            "browser_click",
            json!({"tab_id":"t1","ref":reference}),
        )
        .await,
        "STALE_REF_REOBSERVE",
    );
    c.finish().await;
}

#[tokio::test]
async fn duplicated_and_conflicting_resume_cannot_cross_a_new_handoff() {
    let mut c = Client::new().await;
    c.handoff("take", "h1").await;
    let (resume, ack) = c.resume("resume", "h1").await;
    assert_eq!(c.host(&resume).await, ack);
    let mut changed = resume.clone();
    changed["params"]["handoff_id"] = json!("changed");
    assert_eq!(
        c.host(&changed).await["error"]["message"],
        "REQUEST_ID_CONFLICT"
    );
    c.ok("inventory", "browser_tabs", json!({})).await;
    c.handoff("take2", "h2").await;
    assert_eq!(c.host(&resume).await, ack); // historical acknowledgement, no state transition
    let status = c
        .host(&json!({"jsonrpc":"2.0","id":"status","method":"ghost/status","params":{}}))
        .await;
    assert_eq!(status["result"]["hostState"]["state"], "human_owned");
    assert_eq!(status["result"]["hostState"]["controlEpoch"], 3);
    c.finish().await;
}

#[tokio::test]
async fn controlled_cli_rejects_standard_or_invalid_descriptors_and_missing_binding() {
    for args in [
        vec![
            "serve",
            "--mcp",
            "--ghost",
            "g",
            "--ghost-control-fd",
            "0",
            "--ghost-task",
            "t",
        ],
        vec![
            "serve",
            "--mcp",
            "--ghost",
            "g",
            "--ghost-control-fd",
            "2147483647",
            "--ghost-task",
            "t",
        ],
        vec!["serve", "--mcp", "--ghost", "g", "--ghost-control-fd", "3"],
    ] {
        let result = Command::new(env!("CARGO_BIN_EXE_oxibrowser"))
            .args(args)
            .output()
            .await
            .unwrap();
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
    }
}

#[tokio::test]
async fn rejected_agent_ids_are_reserved_and_historical_snapshots_do_not_replay_across_resume() {
    let mut c = Client::new().await;
    c.ok("new", "browser_new", json!({})).await;
    let old = c.agent_request("old-snapshot", "browser_snapshot", json!({"tab_id":"t1"}));
    c.agent_send(&old).await;
    assert_eq!(c.agent_recv().await["result"]["isError"], false);
    c.handoff("take", "h1").await;
    let blocked = c.agent_request("blocked", "browser_new", json!({}));
    c.agent_send(&blocked).await;
    denied(&c.agent_recv().await, "HUMAN_OWNS_BROWSER");
    c.resume("resume", "h1").await;
    c.agent_send(&old).await;
    denied(&c.agent_recv().await, "CONTROL_EPOCH_MISMATCH");
    c.ok("fresh", "browser_snapshot", json!({"tab_id":"t1"}))
        .await;
    let changed = c.agent("blocked", "browser_new", json!({})).await;
    assert_eq!(changed["error"]["message"], "REQUEST_ID_CONFLICT");
    c.finish().await;
}

#[tokio::test]
async fn timeout_while_handoff_waits_never_grants_human_ownership() {
    let server = fixture().await;
    let mut c = Client::new().await;
    opened(&mut c, &server).await;
    let (url, entered, release) = gated_page().await;
    let mut nav = c.agent_request("slow", "browser_navigate", json!({"tab_id":"t1","url":url}));
    nav["params"]["_meta"]["timeoutMs"] = json!(1000);
    c.agent_send(&nav).await;
    tokio::time::timeout(Duration::from_secs(5), entered)
        .await
        .unwrap()
        .unwrap();
    let take = c.host_request(
        "take",
        "ghost/handoff",
        "agent_active",
        json!({"handoff_id":"h1"}),
    );
    c.host_send(&take).await;
    let result = c.agent_recv().await;
    assert_eq!(result["result"]["structuredContent"]["error"], "TIMEOUT");
    assert_eq!(result["result"]["structuredContent"]["mayHaveActed"], true);
    let ack = c.host_recv().await;
    assert_eq!(ack["error"]["message"], "HANDOFF_ABORTED_UNCERTAIN");
    assert_eq!(ack["error"]["data"]["hostState"]["state"], "closed");
    let _ = release.send(());
    c.finish().await;
}

#[tokio::test]
async fn control_eof_during_active_operation_reports_uncertainty() {
    let server = fixture().await;
    let mut c = Client::new().await;
    opened(&mut c, &server).await;
    let (url, entered, release) = gated_page().await;
    let nav = c.agent_request("slow", "browser_navigate", json!({"tab_id":"t1","url":url}));
    c.agent_send(&nav).await;
    tokio::time::timeout(Duration::from_secs(5), entered)
        .await
        .unwrap()
        .unwrap();
    c.writer.take();
    c.reader.take();
    let result = c.agent_recv().await;
    assert_eq!(
        result["result"]["structuredContent"]["error"],
        "HOST_CONTROL_CLOSED"
    );
    assert_eq!(
        result["result"]["structuredContent"]["restartRequired"],
        true
    );
    let _ = release.send(());
    c.finish().await;
}

#[tokio::test]
async fn control_descriptor_rejects_regular_file_and_datagram_without_reading_content() {
    async fn reject(fd: i32) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_oxibrowser"));
        command.args([
            "serve",
            "--mcp",
            "--ghost",
            "g",
            "--ghost-task",
            "t",
            "--ghost-control-fd",
            &fd.to_string(),
        ]);
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let result = command.output().await.unwrap();
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        assert!(String::from_utf8_lossy(&result.stderr).contains("AF_UNIX stream socketpair"));
    }
    let (datagram, _peer) = std::os::unix::net::UnixDatagram::pair().unwrap();
    reject(datagram.as_raw_fd()).await;
    let path = std::env::temp_dir().join(format!("ghost-handoff-fd-{}.tmp", uuid::Uuid::new_v4()));
    std::fs::write(&path, "synthetic fixture only").unwrap();
    let file = std::fs::File::open(&path).unwrap();
    reject(file.as_raw_fd()).await;
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "synthetic fixture only"
    );
    drop(file);
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn host_status_traffic_cannot_starve_work_or_its_timeout() {
    let server = fixture().await;
    let mut c = Client::new().await;
    opened(&mut c, &server).await;
    let (url, entered, release) = gated_page().await;
    let mut nav = c.agent_request("slow", "browser_navigate", json!({"tab_id":"t1","url":url}));
    nav["params"]["_meta"]["timeoutMs"] = json!(1000);
    c.agent_send(&nav).await;
    let mut writer = c.writer.take().unwrap();
    let mut reader = c.reader.take().unwrap();
    let (stop, mut stopped) = tokio::sync::watch::channel(false);
    let sending = tokio::spawn(async move {
        let bytes =
            b"{\"jsonrpc\":\"2.0\",\"id\":\"poll\",\"method\":\"ghost/status\",\"params\":{}}\n";
        loop {
            tokio::select! { _=stopped.changed()=>break, r=writer.write_all(bytes)=>if r.is_err(){break;} }
        }
    });
    let reading = tokio::spawn(async move { while let Ok(Some(_)) = reader.next_line().await {} });
    tokio::time::timeout(Duration::from_secs(5), entered)
        .await
        .unwrap()
        .unwrap();
    let result = c.agent_recv().await;
    assert_eq!(result["result"]["structuredContent"]["error"], "TIMEOUT");
    let _ = stop.send(true);
    let _ = release.send(());
    tokio::time::timeout(Duration::from_secs(5), sending)
        .await
        .unwrap()
        .unwrap();
    c.finish().await;
    tokio::time::timeout(Duration::from_secs(5), reading)
        .await
        .unwrap()
        .unwrap();
}
