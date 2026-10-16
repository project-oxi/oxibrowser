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
  document.getElementById('list').textContent = q.indexOf('oxibrowser') >= 0 ? 'matched' : 'no-match';
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

#[test]
fn agent_run_records_site_hints_for_second_run() {
    let port = serve(SEARCH_HTML);
    let state_dir = std::env::temp_dir().join(format!(
        "oxi-agent-state-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&state_dir).unwrap();

    // Run 1: fill + click records the verified selector for this origin.
    let script1 = script_file(&[format!(
        r#"await page.goto('http://127.0.0.1:{port}/'); await page.getByLabel('Query').fill('oxibrowser'); await page.getByRole('button', {{ name: 'Search' }}).click(); task.done('done')"#
    )]);
    let out = Command::new(env!("CARGO_BIN_EXE_oxibrowser"))
        .arg("agent")
        .arg("run")
        .arg("warm the site file")
        .arg("--allow-private-ips")
        .arg("--max-steps")
        .arg("4")
        .env("OXIBROWSER_LLM_SCRIPT", &script1)
        .env("OXIBROWSER_STATE_DIR", &state_dir)
        .stdin(Stdio::null())
        .output()
        .expect("spawn run1");
    assert!(
        out.status.success(),
        "run1: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Run 2: the first observe carries the recorded selector as a hint.
    let script2 = script_file(&[format!(
        r#"await page.goto('http://127.0.0.1:{port}/'); const o = await page.observe({{ budgetTokens: 300 }}); task.done(o.site_hints || [])"#
    )]);
    let out = Command::new(env!("CARGO_BIN_EXE_oxibrowser"))
        .arg("agent")
        .arg("run")
        .arg("use the hints")
        .arg("--allow-private-ips")
        .arg("--max-steps")
        .arg("4")
        .env("OXIBROWSER_LLM_SCRIPT", &script2)
        .env("OXIBROWSER_STATE_DIR", &state_dir)
        .stdin(Stdio::null())
        .output()
        .expect("spawn run2");
    assert!(
        out.status.success(),
        "run2: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: Value = serde_json::from_str(stdout.trim_end().lines().last().unwrap()).expect("json");
    let hints = v["result"].to_string();
    assert!(
        hints.contains("verified 1x") || hints.contains("verified "),
        "second run should see the recorded selector: {hints}"
    );
}

#[test]
fn agent_replay_replays_last_captured_fetch() {
    // The page fetches /api on load; page.replay('/api') re-issues it
    // through the page's own fetch and reports the status.
    let html = r#"<!doctype html>
<html><body><p id="out">boot</p>
<script>fetch('/api').then(r => r.text()).then(t => { document.getElementById('out').textContent = 'api:' + t; });</script>
</body></html>"#;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buf = [0u8; 4096];
            let _ = std::io::Read::read(&mut stream, &mut buf);
            let req = String::from_utf8_lossy(&buf);
            let (ctype, body) = if req.contains("GET /api") {
                ("application/json", "{\"v\":7}".to_string())
            } else {
                ("text/html", html.to_string())
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });

    let state_dir = std::env::temp_dir().join("oxi-agent-replay-state");
    let _ = std::fs::remove_dir_all(&state_dir);
    std::fs::create_dir_all(&state_dir).unwrap();
    let script = script_file(&[
        format!(
            r#"await page.goto('http://127.0.0.1:{port}/'); await page.waitForTimeout(400); return await page.ariaSnapshot();"#
        ),
        "const r = await page.replay({ url: '/api' }); task.done(r.status)".to_string(),
    ]);
    let out = Command::new(env!("CARGO_BIN_EXE_oxibrowser"))
        .arg("agent")
        .arg("run")
        .arg("call the api twice")
        .arg("--allow-private-ips")
        .arg("--max-steps")
        .arg("4")
        .env("OXIBROWSER_LLM_SCRIPT", &script)
        .env("OXIBROWSER_STATE_DIR", &state_dir)
        .stdin(Stdio::null())
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "replay run: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: Value = serde_json::from_str(stdout.trim_end().lines().last().unwrap()).expect("json");
    assert_eq!(v["result"], 200, "replayed fetch should return 200: {v}");
}

#[test]
fn agent_resume_continues_saved_transcript() {
    let port = serve(SEARCH_HTML);
    let state_dir = std::env::temp_dir().join(format!(
        "oxi-agent-resume-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&state_dir).unwrap();

    // Run 1: exhausts at step 2 without task.done.
    let script1 = script_file(&[format!(
        r#"await page.goto('http://127.0.0.1:{port}/'); return await page.ariaSnapshot();"#
    )]);
    let out = Command::new(env!("CARGO_BIN_EXE_oxibrowser"))
        .arg("agent")
        .arg("run")
        .arg("two steps then stop")
        .arg("--allow-private-ips")
        .arg("--max-steps")
        .arg("2")
        .env("OXIBROWSER_LLM_SCRIPT", &script1)
        .env("OXIBROWSER_STATE_DIR", &state_dir)
        .stdin(Stdio::null())
        .output()
        .expect("spawn run1");
    assert_eq!(out.status.code(), Some(2));
    let task_id = String::from_utf8_lossy(&out.stderr)
        .lines()
        .find_map(|l| l.strip_prefix("task id: "))
        .expect("task id on stderr")
        .to_string();

    // Resume: fresh script finishes the task.
    let script2 = script_file(&["task.done('resumed after ' + 2 + ' prior steps')".to_string()]);
    let out = Command::new(env!("CARGO_BIN_EXE_oxibrowser"))
        .arg("agent")
        .arg("resume")
        .arg(&task_id)
        .arg("--max-steps")
        .arg("4")
        .env("OXIBROWSER_LLM_SCRIPT", &script2)
        .env("OXIBROWSER_STATE_DIR", &state_dir)
        .stdin(Stdio::null())
        .output()
        .expect("spawn resume");
    assert!(
        out.status.success(),
        "resume: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: Value = serde_json::from_str(stdout.trim_end().lines().last().unwrap()).expect("json");
    assert_eq!(v["ok"], true, "resume should finish: {v}");
    assert!(v["result"].as_str().unwrap_or("").contains("resumed"));
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
