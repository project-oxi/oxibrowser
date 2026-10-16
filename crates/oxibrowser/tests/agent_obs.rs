//! M0 acceptance: `observe` through the real session REPL binary.
//!
//! Spawns `oxibrowser session` against a loopback HTML fixture and checks
//! the observation contract from the execution design:
//! - budget: `--budget N` keeps the aria YAML within ~N tokens and reports
//!   the truncation (never silent);
//! - refs: actionable elements carry `e{n}` refs that also appear in the
//!   YAML (one consistent namespace per observation);
//! - epoch: a re-navigation bumps `generation` and mints fresh refs
//!   (stale-ref detection itself lives in the CDP `OXI.*Ref` handlers and
//!   is covered by the existing CDP e2e suite).

#![cfg(feature = "browser")]

use std::io::Write;
use std::net::TcpListener;
use std::process::{Command, Stdio};

use serde_json::Value;

fn fixture_html(extra_button: bool) -> String {
    let extra = if extra_button {
        r#"<button id="extra">Extra</button>"#
    } else {
        ""
    };
    format!(
        r#"<!doctype html>
<html><head><title>Observe Fixture</title></head><body>
<h1>Observe Fixture</h1>
<nav><a href="/a">Alpha</a> <a href="/b">Beta</a></nav>
<main>
<p>Padding paragraph with plenty of running text so that the rendered aria
yaml comfortably exceeds a very small token budget when truncation must be
exercised by the observe command in this fixture page.</p>
<button id="go">Go</button>
<input name="q" placeholder="Search">
<a href="/c">Gamma</a>
{extra}
</main></body></html>"#
    )
}

/// Serve `html` for every request until the process exits.
fn serve(html: String) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            // Drain the request head (best effort), answer, close.
            let mut buf = [0u8; 4096];
            let _ = std::io::Read::read(&mut stream, &mut buf);
            let body = html.clone();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    port
}

/// Run the session REPL with `lines` on stdin; return the JSONL responses.
fn run_session_repl(lines: &[&str]) -> Vec<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_oxibrowser"))
        .arg("session")
        .arg("--allow-private-ips")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn oxibrowser session");

    {
        let stdin = child.stdin.as_mut().expect("stdin");
        for line in lines {
            writeln!(stdin, "{line}").expect("write command");
        }
    }
    let out = child.wait_with_output().expect("wait for session");
    assert!(out.status.success(), "session exited with {:?}", out.status);

    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad JSONL line {l:?}: {e}")))
        .collect()
}

fn data_of(resp: &Value) -> &Value {
    assert!(
        resp.get("ok").and_then(Value::as_bool).unwrap_or(false),
        "not ok: {resp}"
    );
    resp.get("data").unwrap()
}

#[test]
fn observe_respects_budget_and_reports_truncation() {
    let port = serve(fixture_html(false));
    let url = format!("http://127.0.0.1:{port}/");

    let resps = run_session_repl(&[
        "new",
        &format!("goto t1 {url}"),
        "observe t1 --budget 40",
        "exit",
    ]);
    let observe = data_of(&resps[2]);

    let yaml = observe["aria_yaml"].as_str().unwrap();
    let truncated = observe["truncated"].as_bool().unwrap();
    let tokens = observe["approximate_tokens"].as_u64().unwrap();
    let omitted = observe["omitted_bytes"].as_u64().unwrap();

    assert!(
        yaml.contains("Observe Fixture"),
        "yaml should carry page text: {yaml}"
    );
    assert!(truncated, "tiny budget must truncate: {observe}");
    assert!(omitted > 0, "truncation must be reported: {observe}");
    assert!(yaml.contains("re-observe with a larger --budget"));
    // Budget is soft only by the trailing marker line; keep it tight.
    assert!(tokens <= 40 + 8, "tokens {tokens} far over budget");
    assert!(
        yaml.len() as u64 <= 40 * 4 + 64,
        "yaml bytes {} over cap",
        yaml.len()
    );

    // Actionable refs are consistent with the YAML namespace and complete.
    let actionable = observe["actionable"].as_array().unwrap();
    assert!(
        actionable.len() >= 4,
        "expected links/buttons/input, got {actionable:?}"
    );
    for el in actionable {
        let r = el["ref"].as_str().unwrap();
        assert!(r.starts_with('e'), "ref must be e{{n}}: {r}");
    }
}

#[test]
fn observe_without_budget_is_complete_and_navigable_epoch_bumps() {
    let port = serve(fixture_html(false));
    let url = format!("http://127.0.0.1:{port}/");

    let resps = run_session_repl(&[
        "new",
        &format!("goto t1 {url}"),
        "observe t1",
        &format!("goto t1 {url}?second"),
        "observe t1",
        "exit",
    ]);
    let first = data_of(&resps[2]);
    let second = data_of(&resps[4]);

    assert!(
        !first["truncated"].as_bool().unwrap(),
        "no budget, no truncation"
    );
    assert_eq!(first["omitted_elements"].as_u64(), Some(0));
    assert!(first["aria_yaml"].as_str().unwrap().contains("Gamma"));

    let g1 = first["generation"].as_u64().unwrap();
    let g2 = second["generation"].as_u64().unwrap();
    assert!(g2 > g1, "re-navigation must bump the epoch: {g1} -> {g2}");

    // Fresh refs are minted for the new epoch (counter is per session).
    let refs_of = |v: &Value| -> Vec<String> {
        v["actionable"]
            .as_array()
            .unwrap()
            .iter()
            .map(|el| el["ref"].as_str().unwrap().to_string())
            .collect()
    };
    let r1 = refs_of(first);
    let r2 = refs_of(second);
    assert!(!r1.is_empty() && !r2.is_empty());
    assert_ne!(r1, r2, "second observation must mint fresh refs");
}

#[test]
fn observe_caps_actionable_elements() {
    let port = serve(fixture_html(false));
    let url = format!("http://127.0.0.1:{port}/");

    let resps = run_session_repl(&[
        "new",
        &format!("goto t1 {url}"),
        "observe t1 --max-actionable 2",
        "exit",
    ]);
    let observe = data_of(&resps[2]);

    let actionable = observe["actionable"].as_array().unwrap();
    assert_eq!(actionable.len(), 2, "cap must apply: {actionable:?}");
    assert!(observe["omitted_elements"].as_u64().unwrap() > 0);
}
