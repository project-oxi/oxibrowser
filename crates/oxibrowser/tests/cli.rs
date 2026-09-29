//! CLI integration tests — exercise the `oxibrowser` binary directly.
//!
//! Run with: `cargo test --test cli -- --ignored`
//! (requires internet connection for #[ignore] tests)

use std::io::Write;
use std::process::{Command, Stdio};

/// Path to the built binary (works in any profile — debug or release).
fn oxibrowser() -> String {
    env!("CARGO_BIN_EXE_oxibrowser").to_string()
}

// ---------------------------------------------------------------------------
// fetch
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_fetch_markdown() {
    let output = Command::new(oxibrowser())
        .args(["fetch", "https://example.com", "--json"])
        .output()
        .expect("failed to run oxibrowser");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let resp: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(resp["ok"], true);
    assert!(
        resp["data"]["markdown"]
            .as_str()
            .unwrap()
            .contains("documentation examples")
    );
    assert_eq!(resp["data"]["status"], 200);
    assert!(resp["meta"]["elapsed_ms"].as_u64().unwrap() > 0);
}

#[test]
#[ignore]
fn test_fetch_text() {
    let output = Command::new(oxibrowser())
        .args(["fetch", "https://example.com", "--format", "text"])
        .output()
        .expect("failed to run oxibrowser");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("documentation examples"));
    assert!(stdout.contains("documentation"));
}

#[test]
#[ignore]
fn test_fetch_fields() {
    let output = Command::new(oxibrowser())
        .args([
            "fetch",
            "https://example.com",
            "--fields",
            "url,title,status",
            "--json",
        ])
        .output()
        .expect("failed to run oxibrowser");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let resp: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(resp["ok"], true);
    let data = resp["data"].as_object().unwrap();
    assert!(data.contains_key("url"));
    assert!(data.contains_key("title"));
    assert!(data.contains_key("status"));
    assert!(!data.contains_key("markdown")); // filtered out
}

#[test]
#[ignore]
fn test_fetch_max_bytes() {
    let output = Command::new(oxibrowser())
        .args([
            "fetch",
            "https://example.com",
            "--max-bytes",
            "50",
            "--json",
        ])
        .output()
        .expect("failed to run oxibrowser");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let resp: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(resp["ok"], true);
    assert_eq!(resp["data"]["truncated"], true);
    assert!(resp["data"]["total_bytes"].as_u64().unwrap() > 50);
    assert_eq!(resp["data"]["returned_bytes"], 50);
}

#[test]
#[ignore]
fn test_fetch_summary() {
    let output = Command::new(oxibrowser())
        .args(["fetch", "https://example.com", "--summary", "--json"])
        .output()
        .expect("failed to run oxibrowser");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let resp: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(resp["ok"], true);
    assert!(resp["data"]["title"].as_str().unwrap().contains("Example"));
    assert!(resp["data"]["links_count"].as_u64().unwrap() > 0);
}

#[test]
#[ignore]
fn test_fetch_eval() {
    let output = Command::new(oxibrowser())
        .args([
            "fetch",
            "https://example.com",
            "--eval",
            "document.title",
            "--json",
        ])
        .output()
        .expect("failed to run oxibrowser");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let resp: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(resp["ok"], true);
    assert_eq!(resp["data"]["value"], "Example Domain");
}

// ---------------------------------------------------------------------------
// extract
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_extract_links() {
    let output = Command::new(oxibrowser())
        .args(["extract", "https://example.com", "--links", "--json"])
        .output()
        .expect("failed to run oxibrowser");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let resp: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(resp["ok"], true);
    let links = resp["data"]["links"].as_array().unwrap();
    assert!(!links.is_empty());
}

#[test]
#[ignore]
fn test_extract_selector() {
    let output = Command::new(oxibrowser())
        .args([
            "extract",
            "https://example.com",
            "--selector",
            "a",
            "--attrs",
            "text,href",
            "--json",
        ])
        .output()
        .expect("failed to run oxibrowser");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let resp: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(resp["ok"], true);
    let m = resp["data"]["match"].as_object().unwrap();
    assert!(m.contains_key("text"));
    assert!(m.contains_key("href"));
    assert!(m["href"].as_str().unwrap().contains("iana.org"));
}

// ---------------------------------------------------------------------------
// error handling
// ---------------------------------------------------------------------------

#[test]
fn test_error_invalid_url() {
    let output = Command::new(oxibrowser())
        .args(["fetch", "ftp://evil.com", "--json"])
        .output()
        .expect("failed to run oxibrowser");

    assert_eq!(output.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let resp: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error_code"], "INVALID_URL");
}

#[test]
fn test_error_bad_selector() {
    let output = Command::new(oxibrowser())
        .args([
            "fetch",
            "https://example.com",
            "--click",
            "div\x01",
            "--json",
        ])
        .output()
        .expect("failed to run oxibrowser");

    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn test_error_no_args() {
    let output = Command::new(oxibrowser())
        .args(["fetch"])
        .output()
        .expect("failed to run oxibrowser");

    assert_ne!(output.status.code(), Some(0));
}

// ---------------------------------------------------------------------------
// session
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_session_basic() {
    let mut child = Command::new(oxibrowser())
        .args(["session"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn oxibrowser session");

    let stdin = child.stdin.as_mut().unwrap();

    // new
    writeln!(stdin, "new").unwrap();
    // goto
    writeln!(stdin, "goto t1 https://example.com").unwrap();
    // content
    writeln!(stdin, "content t1 --format markdown --max-bytes 200").unwrap();
    // list
    writeln!(stdin, "list").unwrap();
    // close
    writeln!(stdin, "close t1").unwrap();
    // list again — the tab must now be gone
    writeln!(stdin, "list").unwrap();
    // exit
    writeln!(stdin, "exit").unwrap();

    let output = child.wait_with_output().expect("wait failed");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Parse each line as JSON
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert!(
        lines.len() >= 5,
        "expected at least 5 output lines, got {}",
        lines.len()
    );

    // new response
    let resp: serde_json::Value = serde_json::from_str(lines[0]).expect("new JSON");
    assert_eq!(resp["ok"], true);
    assert_eq!(resp["data"]["tab_id"], "t1");

    // goto response
    let resp: serde_json::Value = serde_json::from_str(lines[1]).expect("goto JSON");
    assert_eq!(resp["ok"], true);
    assert_eq!(resp["data"]["status"], 200);

    // content response
    let resp: serde_json::Value = serde_json::from_str(lines[2]).expect("content JSON");
    assert_eq!(resp["ok"], true);
    assert!(
        resp["data"]["markdown"]
            .as_str()
            .unwrap_or("")
            .contains("documentation examples")
    );

    // list response (list runs BEFORE close — the tab must still be there)
    let resp: serde_json::Value = serde_json::from_str(lines[3]).expect("list JSON");
    assert_eq!(resp["ok"], true);
    assert_eq!(
        resp["data"]["tabs"].as_array().map(|t| !t.is_empty()),
        Some(true)
    );

    // second list (after close) — the tab must be gone
    let resp: serde_json::Value = serde_json::from_str(lines[5]).expect("post-close list JSON");
    assert_eq!(resp["ok"], true);
    assert_eq!(
        resp["data"]["tabs"].as_array().map(|t| t.is_empty()),
        Some(true)
    );
}

#[test]
#[ignore]
fn test_session_tab_not_found() {
    let mut child = Command::new(oxibrowser())
        .args(["session"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn oxibrowser session");

    let stdin = child.stdin.as_mut().unwrap();
    writeln!(stdin, "goto t99 https://example.com").unwrap();
    writeln!(stdin, "exit").unwrap();

    let output = child.wait_with_output().expect("wait failed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();

    let resp: serde_json::Value = serde_json::from_str(lines[0]).expect("JSON");
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error_code"], "TAB_NOT_FOUND");
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_run_simple_script() {
    let script = r#"
name: test
steps:
  - step_type: goto
    data:
      goto: https://example.com
  - step_type: content
    data:
      format: markdown
"#;

    let dir = std::env::temp_dir().join("oxibrowser-test-run.yaml");
    std::fs::write(&dir, script).unwrap();

    let output = Command::new(oxibrowser())
        .args(["run", dir.to_str().unwrap()])
        .output()
        .expect("failed to run oxibrowser");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let resp: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(resp["ok"], true);
    assert_eq!(resp["data"]["success"], true);
    assert!(resp["meta"]["elapsed_ms"].as_u64().unwrap() > 0);
}

// ---------------------------------------------------------------------------
// describe / skill / version
// ---------------------------------------------------------------------------

#[test]
fn test_describe_compact() {
    let output = Command::new(oxibrowser())
        .args(["describe", "--compact"])
        .output()
        .expect("failed to run oxibrowser");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let resp: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(resp["ok"], true);
    assert!(resp["data"].as_object().unwrap().contains_key("fetch"));
    assert!(resp["data"].as_object().unwrap().contains_key("session"));
}

#[test]
fn test_skill() {
    let output = Command::new(oxibrowser())
        .args(["skill"])
        .output()
        .expect("failed to run oxibrowser");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("OxiBrowser Agent Skills"));
    assert!(stdout.contains("session"));
}

#[test]
fn test_version() {
    let output = Command::new(oxibrowser())
        .args(["version"])
        .output()
        .expect("failed to run oxibrowser");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("oxibrowser"));
    assert!(stdout.contains("0."));
}
