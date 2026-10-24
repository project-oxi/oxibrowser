//! Crash-isolation goal tests: a hostile page script must never take the
//! browser process down with it.
//!
//! Offline tests (run by default) drive `fetch` against a loopback mock
//! server and pin the tolerated-failure contract: thrown script errors and
//! limit-tripped recursion render the page and exit 0.
//!
//! Real-website goal tests (`#[ignore]`) pin pages measured to hard-abort
//! the process (SIGABRT, empty output) on 2026-10-24. They stay red until
//! the engine survives them — run with
//! `cargo test -p oxibrowser --test crash_isolation -- --ignored`.

mod common;

use std::process::{Command, Output};

fn oxibrowser() -> String {
    env!("CARGO_BIN_EXE_oxibrowser").to_string()
}

fn run_fetch(url: &str) -> Output {
    Command::new(oxibrowser())
        .args(["fetch", url, "--allow-private-ips", "--format", "text"])
        .output()
        .expect("failed to spawn oxibrowser")
}

/// The process must exit cleanly and render something.
fn assert_clean_render(what: &str, out: &Output) {
    assert!(
        out.status.success(),
        "{what}: process failed (status {:?}, code {:?})\nstderr:\n{}",
        out.status,
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        !out.stdout.is_empty(),
        "{what}: empty render\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
}

// ---------------------------------------------------------------------------
// Offline contract (loopback, no external network)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn page_render_survives_script_throw() {
    let (server, url) = common::mock_html_server(
        r#"<html><body><h1>throw-marker</h1><script>throw new Error("boom");</script></body></html>"#,
    )
    .await;
    let out = run_fetch(&url);
    drop(server);
    assert_clean_render("script throw", &out);
    assert!(String::from_utf8_lossy(&out.stdout).contains("throw-marker"));
}

#[tokio::test]
async fn page_render_survives_deep_recursion() {
    // Trips the nav-script recursion RuntimeLimit (RangeError) — the page
    // must still render. Regression guard: this must degrade to a JS error,
    // never a native stack overflow.
    let (server, url) = common::mock_html_server(
        r#"<html><body><h1>recursion-marker</h1><script>function f(){f();} f();</script></body></html>"#,
    )
    .await;
    let out = run_fetch(&url);
    drop(server);
    assert_clean_render("deep recursion", &out);
    assert!(String::from_utf8_lossy(&out.stdout).contains("recursion-marker"));
}

// ---------------------------------------------------------------------------
// Real-website crash goals (#[ignore]) — measured 2026-10-24, both sites
// aborted the process (SIGABRT) with empty output.
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn spotify_com_never_aborts_the_process() {
    let out = run_fetch("https://www.spotify.com");
    assert_clean_render("spotify.com", &out);
}

#[test]
#[ignore]
fn figma_com_never_aborts_the_process() {
    let out = run_fetch("https://www.figma.com");
    assert_clean_render("figma.com", &out);
}
