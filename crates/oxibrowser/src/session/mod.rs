//! Session REPL — stdin/stdout JSON interface for AI agents.
//!
//! Reads commands from stdin (one per line), executes them, and prints
//! JSON responses to stdout. This is the machine-friendly interface
//! for subprocess-based automation.
//!
//! Cleanup: EOF (stdin close), `exit` command, SIGTERM, and Ctrl+C all trigger
//! clean shutdown: close all tabs → close browser → exit 0.

pub mod executor;
pub mod parser;
pub mod tab_manager;

use crate::output::CliResponse;
use std::io::BufRead;
use std::sync::Arc;
use tab_manager::TabManager;

/// Run the session REPL. Returns exit code.
pub async fn run_session(
    allow_private_ips: bool,
    accounts: Option<&str>,
    ref_tag: Option<&str>,
    lock_policy: oxibrowser_core::account::LockPolicy,
) -> i32 {
    let mut config = oxibrowser_core::BrowserConfig::headless();
    if allow_private_ips {
        config.enable_ssrf_filter = false;
        eprintln!("⚠ SSRF filter disabled: private/internal IP ranges accessible.");
    }
    let browser = match oxibrowser_core::Browser::new(config).await {
        Ok(b) => b,
        Err(e) => {
            let resp = CliResponse::error(format!("browser init failed: {e}"), "RUNTIME_ERROR");
            resp.print_json();
            return 1;
        }
    };
    let browser = Arc::new(browser);

    let mut manager = TabManager::new();
    let mut accounts_rt = executor::AccountRuntime::new(browser.clone());
    accounts_rt.set_account_options(lock_policy, ref_tag.map(str::to_string));

    // `--account id[,…]` (roadmap item 4): bind each account's envelope into
    // a context; the first is primary — REPL `new` tabs land inside it.
    if let Some(list) = accounts {
        let ids: Vec<&str> = list
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if ids.is_empty() {
            let resp = CliResponse::error(
                "--account requires at least one account id",
                "INPUT_VALIDATION",
            );
            resp.print_json();
            return 2;
        }
        let mut primary_set = false;
        for id in ids {
            match crate::account_cli::bind_account_context(&browser, id, ref_tag).await {
                Ok(ctx) => {
                    if !primary_set {
                        accounts_rt.set_primary_context(ctx);
                        primary_set = true;
                    }
                }
                Err(e) => {
                    let resp = CliResponse::error(e, "RUNTIME_ERROR");
                    resp.print_json();
                    return 1;
                }
            }
        }
    }
    // Read from stdin on a blocking thread so we can select with signals
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Option<String>>(32);
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut lines = stdin.lock().lines();
        loop {
            match lines.next() {
                Some(Ok(line)) => {
                    if tx.blocking_send(Some(line)).is_err() {
                        break;
                    }
                }
                _ => {
                    let _ = tx.blocking_send(None); // signal EOF
                    break;
                }
            }
        }
    });

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .unwrap_or_else(|_| {
            // SIGTERM not available (e.g. Windows) — create a dummy that never fires
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1()).unwrap()
        });

    let exit_code = loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Some(Some(line)) => {
                        let line = line.trim().to_string();
                        if line.is_empty() || line.starts_with('#') {
                            continue;
                        }

                        let cmd = match parser::parse_session_command(&line) {
                            Ok(c) => c,
                            Err(e) => {
                                if e == "empty" {
                                    continue;
                                }
                                let resp = CliResponse::error(e, "PARSE_ERROR");
                                resp.print_json();
                                continue;
                            }
                        };

                        if matches!(cmd, parser::SessionCommand::Exit) {
                            let resp = CliResponse::success(serde_json::json!({ "exit": true }));
                            resp.print_json();
                            break 0;
                        }

                        let resp = executor::execute(cmd, &browser, &mut manager, &mut accounts_rt).await;
                        resp.print_json();
                    }
                    Some(None) | None => {
                        // EOF — stdin closed
                        break 0;
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                let resp = CliResponse::success(serde_json::json!({ "exit": true, "reason": "ctrl_c" }));
                resp.print_json();
                break 0;
            }
            _ = sigterm.recv() => {
                let resp = CliResponse::success(serde_json::json!({ "exit": true, "reason": "sigterm" }));
                resp.print_json();
                break 0;
            }
        }
    };

    // Cleanup
    manager.close_all().await;
    browser.close().await.ok();
    exit_code
}
