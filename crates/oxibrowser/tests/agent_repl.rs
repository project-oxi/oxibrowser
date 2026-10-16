//! M1 acceptance: the `oxibrowser repl` binary end-to-end.
//!
//! Spawns the real REPL against loopback fixtures and checks:
//! - `getByRole(...).click()` drives a JS handler to a visible DOM change;
//! - the locator error contract reaches the model (strict-mode, not-found,
//!   UNSUPPORTED for Node-isms);
//! - `page.observe({budgetTokens})` honors the budget;
//! - `oxi.fillCredential` without `--account` fails closed.

#![cfg(feature = "browser")]

use std::io::Write;
use std::net::TcpListener;
use std::process::{Command, Stdio};

use serde_json::Value;

fn serve(html: String) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buf = [0u8; 4096];
            let _ = std::io::Read::read(&mut stream, &mut buf);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                html.len(),
                html
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    port
}

/// Feed `programs` (one per line) to the REPL; return parsed JSONL.
fn run_repl(programs: &[String]) -> Vec<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_oxibrowser"))
        .arg("repl")
        .arg("--allow-private-ips")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn oxibrowser repl");

    {
        let stdin = child.stdin.as_mut().expect("stdin");
        for p in programs {
            writeln!(stdin, "{p}").expect("write program");
        }
    }
    let out = child.wait_with_output().expect("wait for repl");
    assert!(out.status.success(), "repl exited with {:?}", out.status);

    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad JSONL {l:?}: {e}")))
        .collect()
}

const CLICK_HTML: &str = r#"<!doctype html>
<html><head><title>Click</title></head><body>
<h1>Click Fixture</h1>
<button id="go">Go</button>
<span id="out">none</span>
<script>
document.getElementById('go').addEventListener('click', function () {
  document.getElementById('out').textContent = 'clicked';
});
</script>
</body></html>"#;

#[test]
fn repl_click_drives_js_handler() {
    let port = serve(CLICK_HTML.into());
    let url = format!("http://127.0.0.1:{port}/");
    let resps = run_repl(&[format!(
        r#"await page.goto('{url}'); await page.getByRole('button', {{ name: 'Go' }}).click(); return await page.ariaSnapshot();"#
    )]);
    let yaml = resps[0]["data"].as_str().unwrap();
    assert!(yaml.contains("clicked"), "handler effect missing: {yaml}");
    assert!(
        !yaml.contains("\"none\""),
        "initial text should be replaced: {yaml}"
    );
}

#[test]
fn repl_locator_error_contract() {
    let port = serve(CLICK_HTML.into());
    let url = format!("http://127.0.0.1:{port}/");

    let resps = run_repl(&[
        // strict-mode: both the button and the span match "Go"/text loosely?
        // no — use a page with two buttons to force the violation.
        format!(
            r#"await page.goto('{url}'); return await page.getByRole('button', {{ name: 'nope' }}).click();"#
        ),
        r#"return require('fs');"#.to_string(), // Node-ism → UNSUPPORTED guidance
    ]);
    let err0 = resps[0]["error"].as_str().unwrap();
    assert!(err0.contains("LOCATOR_NOT_FOUND"), "got {err0:?}");
    assert!(
        err0.contains("re-observe"),
        "must hint re-observe: {err0:?}"
    );

    let err1 = resps[1]["error"].as_str().unwrap();
    assert!(err1.contains("UNSUPPORTED"), "got {err1:?}");
    assert!(
        err1.contains("page"),
        "must point at the browser surface: {err1:?}"
    );
}

#[test]
fn repl_strict_mode_violation_lists_hint() {
    let html = r#"<!doctype html>
<html><body><button>Go</button><button>Go too</button></body></html>"#
        .to_string();
    let port = serve(html);
    let url = format!("http://127.0.0.1:{port}/");

    let resps = run_repl(&[format!(
        r#"await page.goto('{url}'); return await page.getByRole('button', {{ name: 'Go' }}).click();"#
    )]);
    let err = resps[0]["error"].as_str().unwrap();
    assert!(err.contains("STRICT_MODE_VIOLATION"), "got {err:?}");
    assert!(err.contains("first()"), "must suggest narrowing: {err:?}");
}

#[test]
fn repl_observe_honors_token_budget() {
    let port = serve(CLICK_HTML.into());
    let url = format!("http://127.0.0.1:{port}/");
    let resps = run_repl(&[format!(
        r#"await page.goto('{url}'); return await page.observe({{ budgetTokens: 10 }});"#
    )]);
    let data = resps[0].get("data").unwrap();
    assert_eq!(data["truncated"], true);
    assert!(data["omitted_bytes"].as_u64().unwrap() > 0);
    // The budget is soft by the truncation-marker line (~16 tokens of
    // metadata the model also pays for); content itself fits the cap.
    assert!(data["approximate_tokens"].as_u64().unwrap() <= 10 + 16);
}

#[test]
fn repl_fill_credential_fails_closed_without_account() {
    let port = serve(r#"<input type="password" aria-label="Password">"#.to_string());
    let url = format!("http://127.0.0.1:{port}/");

    let resps = run_repl(&[format!(
        r#"await page.goto('{url}'); return await oxi.fillCredential(page.getByRole('textbox', {{ name: 'Password' }}), 'acc');"#
    )]);
    let err = resps[0]["error"].as_str().unwrap();
    assert!(err.contains("BROKER_UNAVAILABLE"), "got {err:?}");
}
