//! `oxibrowser agent eval <dir>` — corpus runner (execution design M5).
//!
//! A corpus is a directory of JSON cases:
//!
//! ```json
//! {
//!   "name": "fill-and-click",
//!   "task": "…",
//!   "script": ["await page.goto(…)", "… task.done(…)"],
//!   "expect": { "ok": true, "result_contains": "matched" }
//! }
//! ```
//!
//! Each case runs against the built-in loopback fixture server (deterministic,
//! offline) through the real `agent run` binary (scripted model provider),
//! exactly like the e2e tests. The runner reports a pass-rate JSON — the
//! number the design doc says is the only claim harness quality may make.

use std::io::{BufRead, Read, Write};
use std::net::TcpListener;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// One corpus case.
#[derive(Debug, Deserialize)]
pub struct Case {
    pub name: String,
    pub task: String,
    pub script: Vec<String>,
    #[serde(default)]
    pub max_steps: usize,
    #[serde(default)]
    pub expect: Expect,
}

/// Pass criteria.
#[derive(Debug, Default, Deserialize)]
pub struct Expect {
    #[serde(default = "default_true")]
    pub ok: bool,
    pub result_contains: Option<String>,
}
fn default_true() -> bool {
    true
}

/// Per-case result.
#[derive(Debug, Serialize)]
pub struct CaseResult {
    pub name: String,
    pub passed: bool,
    pub reason: Option<String>,
    pub steps: u64,
    pub approx_tokens: u64,
    pub elapsed_ms: u128,
}

const FIXTURE_SEARCH: &str = r#"<!doctype html><html><head><title>Search Fixture</title></head><body>
<h1>Search Fixture</h1>
<input id="q" aria-label="Query">
<button id="go">Search</button>
<ul id="list">start</ul>
<script>
document.getElementById('go').addEventListener('click', function () {
  var q = document.getElementById('q').value.toLowerCase();
  document.getElementById('list').textContent = q.indexOf('oxibrowser') >= 0 ? 'matched' : 'no-match';
});
</script></body></html>"#;

const FIXTURE_DIALOG: &str = r#"<!doctype html><html><body>
<button id="go">Go</button><span id="out">none</span>
<script>
document.getElementById('go').addEventListener('click', function () {
  alert('hello from the page');
  document.getElementById('out').textContent = 'after-dialog';
});
</script></body></html>"#;

const FIXTURE_BLANK_LINK: &str =
    r#"<html><body><a id="ext" href="/elsewhere" target="_blank">External</a></body></html>"#;

const FIXTURE_FORM: &str = r#"<!doctype html><html><body>
<form action="/submitted" method="post">
<input name="user" aria-label="Username">
<button type="submit">Sign in</button>
</form></body></html>"#;

const FIXTURE_API_PAGE: &str = r#"<html><body><p id="out">boot</p>
<script>fetch('/api').then(r => r.text()).then(t => { document.getElementById('out').textContent = 'api:' + t; });</script>
</body></html>"#;

const FIXTURE_CONSOLE: &str =
    r#"<html><body><script>console.log('hello-corpus');</script></body></html>"#;

fn http_response(content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Start the built-in fixture server; returns its port.
pub fn start_fixtures() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture server");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            let head = String::from_utf8_lossy(&buf);
            let (method, path) = head
                .lines()
                .next()
                .and_then(|l| {
                    let mut parts = l.split_whitespace();
                    Some((parts.next()?.to_string(), parts.next()?.to_string()))
                })
                .unwrap_or_default();
            let body = if path.starts_with("/search") {
                http_response("text/html", FIXTURE_SEARCH)
            } else if path.starts_with("/dialog-page") {
                http_response("text/html", FIXTURE_DIALOG)
            } else if path.starts_with("/blank-link") {
                http_response("text/html", FIXTURE_BLANK_LINK)
            } else if path.starts_with("/form") {
                http_response("text/html", FIXTURE_FORM)
            } else if path == "/submitted" && method == "POST" {
                http_response("text/html", "<html><body><p>submitted-ok</p></body></html>")
            } else if path == "/api" {
                http_response("application/json", r#"{"v":7}"#)
            } else if path.starts_with("/api-page") {
                http_response("text/html", FIXTURE_API_PAGE)
            } else if path.starts_with("/console-page") {
                http_response("text/html", FIXTURE_CONSOLE)
            } else {
                http_response("text/html", "<html><body>fixtures</body></html>")
            };
            let _ = stream.write_all(body.as_bytes());
        }
    });
    port
}

/// Run every case in `dir`; print the report JSON; return the exit code
/// (0 when all pass, 2 otherwise).
pub fn run_corpus(dir: &str) -> i32 {
    let cases: Vec<Case> = match std::fs::read_dir(dir) {
        Ok(entries) => {
            let mut paths: Vec<_> = entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "json"))
                .collect();
            paths.sort();
            let mut cases = Vec::new();
            for path in paths {
                match std::fs::read_to_string(&path) {
                    Ok(text) => match serde_json::from_str::<Case>(&text) {
                        Ok(c) => cases.push(c),
                        Err(e) => {
                            eprintln!("case {:?} invalid: {e}", path.display());
                            return 1;
                        }
                    },
                    Err(e) => {
                        eprintln!("case {:?} unreadable: {e}", path.display());
                        return 1;
                    }
                }
            }
            cases
        }
        Err(e) => {
            eprintln!("corpus dir {dir:?}: {e}");
            return 1;
        }
    };
    if cases.is_empty() {
        eprintln!("corpus dir {dir:?} has no cases");
        return 1;
    }

    let port = start_fixtures();
    let tmp = std::env::temp_dir().join(format!(
        "oxi-agent-eval-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();

    let mut results = Vec::new();
    for case in &cases {
        let started = std::time::Instant::now();
        let script_path = tmp.join(format!("{}.jsonl", case.name));
        let body = case
            .script
            .iter()
            .map(|p| serde_json::to_string(&p.replace("{PORT}", &port.to_string())).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&script_path, body).unwrap();
        let state = tmp.join(format!("state-{}", case.name));
        std::fs::create_dir_all(&state).unwrap();

        let out = std::process::Command::new(std::env::current_exe().unwrap_or_default())
            .args(["agent", "run", &case.task])
            .arg("--allow-private-ips")
            .arg("--max-steps")
            .arg(case.max_steps.to_string())
            .env("OXIBROWSER_LLM_SCRIPT", &script_path)
            .env("OXIBROWSER_STATE_DIR", &state)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output();

        let (passed, reason, steps, tokens) = match out {
            Ok(out) => {
                let last = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .last()
                    .unwrap_or("")
                    .to_string();
                match serde_json::from_str::<Value>(&last) {
                    Ok(v) => {
                        let ok = v["ok"].as_bool().unwrap_or(false);
                        let steps = v["steps"].as_u64().unwrap_or(0);
                        let tokens = v["approx_tokens"].as_u64().unwrap_or(0);
                        let mut why = None;
                        let mut pass = ok == case.expect.ok && ok;
                        if let Some(needle) = &case.expect.result_contains {
                            let hay = v["result"].to_string();
                            if !hay.contains(needle.as_str()) {
                                pass = false;
                                why = Some(format!("result missing {needle:?}"));
                            }
                        } else if !pass {
                            why = Some(v["error"].as_str().unwrap_or("not ok").to_string());
                        }
                        (pass, why, steps, tokens)
                    }
                    Err(e) => (false, Some(format!("bad result line: {e}")), 0, 0),
                }
            }
            Err(e) => (false, Some(format!("spawn failed: {e}")), 0, 0),
        };

        results.push(CaseResult {
            name: case.name.clone(),
            passed,
            reason,
            steps,
            approx_tokens: tokens,
            elapsed_ms: started.elapsed().as_millis(),
        });
    }

    let total = results.len();
    let passed = results.iter().filter(|r| r.passed).count();
    let report = json!({
        "total": total,
        "passed": passed,
        "failed": total - passed,
        "pass_rate": passed as f64 / total as f64,
        "cases": results,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&report).unwrap_or_default()
    );
    if passed == total { 0 } else { 2 }
}

// BufRead import kept for future stdin-driven corpus input.
#[allow(dead_code)]
fn _unused(_: &dyn BufRead) {}
