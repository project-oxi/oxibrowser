//! Compatibility corpus runner (roadmap Next #1 — the number every engine
//! change is judged by).
//!
//! Fetches every URL in `tests/data/compat_corpus.txt` in a fresh CLI
//! subprocess, classifies the outcome (pass / empty / crash / timeout /
//! fetch-error), and writes the pass-rate table to
//! `docs/quality/compat-report.md`. A crash in the child is data, not a
//! test failure — that is what lets the corpus measure hostile pages.
//!
//! Run:
//!   cargo test -p oxibrowser --test compat_corpus -- --ignored --nocapture
//!
//! CI enforcement (opt-in, so a red corpus doesn't hide the report):
//!   OXIBROWSER_CORPUS_ENFORCE=1 \
//!   OXIBROWSER_CORPUS_MIN_RATE=95 \
//!   cargo test -p oxibrowser --test compat_corpus -- --ignored
//!
//! Environment:
//!   OXIBROWSER_CORPUS_TIMEOUT_SECS  per-URL wall clock (default 60)

use std::fs;
use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn corpus_path() -> String {
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/compat_corpus.txt").to_string()
}

fn report_path() -> String {
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/quality/compat-report.md"
    )
    .to_string()
}

fn timeout_secs() -> u64 {
    std::env::var("OXIBROWSER_CORPUS_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60)
}

/// One measured outcome.
#[derive(Debug)]
struct Outcome {
    url: String,
    note: String,
    status: &'static str,
    bytes: usize,
    js_errors: usize,
    detail: String,
}

fn urls() -> Vec<(String, String)> {
    fs::read_to_string(corpus_path())
        .expect("corpus file readable")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| match line.split_once('|') {
            Some((url, note)) => (url.trim().to_string(), note.trim().to_string()),
            None => (line.to_string(), String::new()),
        })
        .collect()
}

/// Spawn `fetch`, drain pipes on side threads (so a chatty page can't
/// deadlock us), and wait with a wall-clock cap. Killing on timeout is the
/// classification, not a test error.
fn fetch_with_timeout(
    bin: &str,
    url: &str,
) -> (Option<std::process::ExitStatus>, Vec<u8>, Vec<u8>, bool) {
    let mut child = Command::new(bin)
        .args(["fetch", url, "--format", "text"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn oxibrowser");

    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    let stdout_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let stderr_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });

    let deadline = Instant::now() + Duration::from_secs(timeout_secs());
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(e) => panic!("try_wait failed: {e}"),
        }
    };

    let stdout = stdout_handle.join().expect("stdout reader");
    let stderr = stderr_handle.join().expect("stderr reader");
    (status, stdout, stderr, status.is_none())
}

fn js_error_count(stderr: &[u8]) -> usize {
    String::from_utf8_lossy(stderr)
        .matches("page script threw")
        .count()
}

fn classify(
    url: String,
    note: String,
    out: (Option<std::process::ExitStatus>, Vec<u8>, Vec<u8>, bool),
) -> Outcome {
    let (status, stdout, stderr, timed_out) = out;
    let stderr_text = String::from_utf8_lossy(&stderr);
    let js_errors = js_error_count(&stderr);
    let bytes = stdout.len();

    if timed_out {
        return Outcome {
            url,
            note,
            status: "TIMEOUT",
            bytes,
            js_errors,
            detail: format!("killed after {}s", timeout_secs()),
        };
    }
    let status = status.expect("timed_out false implies status");
    if !status.success() {
        // 134 = SIGABRT (native stack overflow / abort) — the crash class
        // this runner exists to catch.
        let kind = match status.code() {
            Some(134) => "CRASH",
            Some(_) => "FETCH_ERR",
            None => "CRASH", // signaled
        };
        let detail: String = stderr_text
            .lines()
            .rev()
            .take(3)
            .collect::<Vec<_>>()
            .join(" | ");
        return Outcome {
            url,
            note,
            status: kind,
            bytes,
            js_errors,
            detail,
        };
    }
    if bytes == 0 {
        return Outcome {
            url,
            note,
            status: "EMPTY",
            bytes,
            js_errors,
            detail: "exit 0 but no markdown".into(),
        };
    }
    Outcome {
        url,
        note,
        status: "PASS",
        bytes,
        js_errors,
        detail: String::new(),
    }
}

#[test]
#[ignore]
fn compat_corpus_pass_rate_report() {
    let entries = urls();
    assert!(!entries.is_empty(), "corpus is empty");

    let bin = env!("CARGO_BIN_EXE_oxibrowser").to_string();
    let outcomes: Vec<Outcome> = entries
        .into_iter()
        .map(|(url, note)| {
            println!("fetching {url} …");
            let out = fetch_with_timeout(&bin, &url);
            classify(url, note, out)
        })
        .collect();

    let total = outcomes.len();
    let passed = outcomes.iter().filter(|o| o.status == "PASS").count();
    let crashes = outcomes.iter().filter(|o| o.status == "CRASH").count();
    let timeouts = outcomes.iter().filter(|o| o.status == "TIMEOUT").count();
    let rate = (passed * 100) / total;

    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    let mut md = String::new();
    md.push_str("# Compatibility corpus report\n\n");
    md.push_str(&format!(
        "Generated from `crates/oxibrowser/tests/data/compat_corpus.txt` (epoch {epoch}).\n\n"
    ));
    md.push_str("| URL | status | bytes | JS errors | note / detail |\n|---|---|---|---|---|\n");
    for o in &outcomes {
        let detail = if o.note.is_empty() {
            o.detail.clone()
        } else {
            o.note.clone()
        };
        md.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            o.url,
            o.status,
            o.bytes,
            o.js_errors,
            detail.replace('|', "\\|")
        ));
    }
    md.push_str(&format!(
        "\n**Pass rate: {passed}/{total} ({rate}%) · crashes: {crashes} · timeouts: {timeouts}**\n"
    ));

    fs::create_dir_all(std::path::Path::new(&report_path()).parent().unwrap()).expect("report dir");
    fs::write(report_path(), &md).expect("write report");
    println!("\n{md}");

    if std::env::var("OXIBROWSER_CORPUS_ENFORCE").as_deref() == Ok("1") {
        let min: u32 = std::env::var("OXIBROWSER_CORPUS_MIN_RATE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100);
        assert!(
            rate as u32 >= min,
            "compat pass rate {rate}% < required {min}% (crashes: {crashes}, timeouts: {timeouts})"
        );
    }
}
