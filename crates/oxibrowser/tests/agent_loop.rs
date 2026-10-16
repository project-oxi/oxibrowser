//! M3 acceptance: `oxibrowser agent run` with a scripted model provider
//! (deterministic, offline). The scripted provider replays one assistant
//! program per line from `OXIBROWSER_LLM_SCRIPT`.

#![cfg(feature = "browser")]

use std::io::Write;
use std::net::TcpListener;
use std::process::{Command, Stdio};

use serde_json::Value;

/// Fixture: a search page whose button filters a list via a JS handler.
const SEARCH_HTML: &str = r#"<!doctype html>
<html><head><title>Search</title></head><body>
<h1>Search Fixture</h1>
<input id="q" aria-label="Query" placeholder="type here">
<button id="go">Search</button>
<ul id="list">
  <li>unrelated thing</li>
  <li>oxibrowser agent plane</li>
  <li>another row</li>
</ul>
<script>
document.getElementById('go').addEventListener('click', function () {
  var q = document.getElementById('q').value.toLowerCase();
  var items = document.getElementById('list').children;
  for (var i = 0; i < items.length; i++) {
    items[i].setAttribute('data-match', items[i].textContent.toLowerCase().indexOf(q) >= 0 ? '1' : '0');
  }
});
</script>
</body></html>"#;

fn serve(html: &str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().unwrap().port();
    let html = html.to_string();
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

fn script_file(lines: &[String]) -> String {
    let dir = std::env::temp_dir().join(format!(
        "oxi-agent-script-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("script.jsonl");
    let body = lines
        .iter()
        .map(|l| serde_json::to_string(l).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, body).unwrap();
    path.to_string_lossy().to_string()
}

fn run_agent(port: u16, script: &str, extra: &[&str]) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_oxibrowser"))
        .arg("agent")
        .arg("run")
        .arg(format!("Find the row about the agent plane and report whether the search worked. Start at http://127.0.0.1:{port}/"))
        .arg("--allow-private-ips")
        .args(extra)
        .env("OXIBROWSER_LLM_SCRIPT", script)
        .stdin(Stdio::null())
        .output()
        .expect("spawn oxibrowser agent run");
    assert!(
        out.status.success(),
        "agent run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(stdout.trim_end().lines().last().unwrap_or(""))
        .unwrap_or_else(|e| panic!("bad result JSON {stdout:?}: {e}"))
}

#[test]
fn agent_run_completes_three_step_task() {
    let port = serve(SEARCH_HTML);
    let script = script_file(&[
        format!(
            r#"await page.goto('http://127.0.0.1:{port}/'); return await page.observe({{ budgetTokens: 400 }});"#
        ),
        format!(
            r#"await page.getByLabel('Query').fill('oxibrowser'); await page.getByRole('button', {{ name: 'Search' }}).click(); await page.waitForTimeout(300); return await page.ariaSnapshot();"#
        ),
        "const yaml = await page.ariaSnapshot(); task.done(yaml)".to_string(),
    ]);

    let result = run_agent(port, &script, &["--max-steps", "8"]);
    assert_eq!(result["ok"], true, "agent run did not finish: {result}");
    assert_eq!(result["steps"], 3, "expected exactly 3 steps: {result}");
    assert_eq!(result["model_calls"], 3);
    // The final aria YAML reflects the filtered page (script effects land
    // in the RenderDocument, which observe reads).
    let yaml = result["result"].as_str().unwrap_or_default();
    assert!(
        yaml.contains("Search Fixture"),
        "final answer should carry the page: {yaml}"
    );
}

#[test]
fn agent_run_steps_exhausted_is_reported() {
    let port = serve(SEARCH_HTML);
    // The script never calls task.done and repeats an observation forever.
    let script = script_file(&[format!(
        r#"await page.goto('http://127.0.0.1:{port}/'); return await page.ariaSnapshot();"#
    )]);

    let out = Command::new(env!("CARGO_BIN_EXE_oxibrowser"))
        .arg("agent")
        .arg("run")
        .arg("loop forever")
        .arg("--allow-private-ips")
        .arg("--max-steps")
        .arg("2")
        .env("OXIBROWSER_LLM_SCRIPT", &script)
        .stdin(Stdio::null())
        .output()
        .expect("spawn");
    assert_eq!(out.status.code(), Some(2), "exhaustion must exit 2");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: Value =
        serde_json::from_str(stdout.trim_end().lines().last().unwrap()).expect("result json");
    assert_eq!(v["ok"], false);
    assert_eq!(v["steps"], 2);
    assert!(v["error"].as_str().unwrap().contains("steps exhausted"));
}

/// Real-model smoke — opt-in (needs OXIBROWSER_LLM_* pointing at a live
/// endpoint). Run: cargo test --test agent_loop real_model -- --ignored.
#[test]
#[ignore = "requires a live model endpoint via OXIBROWSER_LLM_*"]
fn real_model_smoke() {
    let port = serve(SEARCH_HTML);
    let out = Command::new(env!("CARGO_BIN_EXE_oxibrowser"))
        .arg("agent")
        .arg("run")
        .arg(format!(
            "On the page at http://127.0.0.1:{port}/, fill the query with 'oxibrowser', click Search, and report the visible list."
        ))
        .arg("--allow-private-ips")
        .arg("--max-steps")
        .arg("8")
        .stdin(Stdio::null())
        .output()
        .expect("spawn");
    let stdout = String::from_utf8_lossy(&out.stdout);
    eprintln!("{stdout}");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
