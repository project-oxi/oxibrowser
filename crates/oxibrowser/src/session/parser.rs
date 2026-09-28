//! Session command parser.
//!
//! Parses a line of text into a `SessionCommand`. Uses a simple
//! whitespace-split approach with positional args and `--flag value` flags.

/// Strip leading/trailing matching quotes (single or double) from a string.
fn strip_quotes(s: &str) -> &str {
    let s = s.trim();
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        return &s[1..s.len() - 1];
    }
    s
}

/// Parsed session command.
#[derive(Debug, Clone)]
pub enum SessionCommand {
    /// Create a new tab.
    New,
    /// Navigate a tab to a URL.
    Goto {
        tab_id: String,
        url: String,
        wait_selector: Option<String>,
        timeout_ms: Option<u64>,
    },
    /// Go back in history.
    Back { tab_id: String },
    /// Go forward in history.
    Forward { tab_id: String },
    /// Reload the current page.
    Reload { tab_id: String },
    /// Click an element.
    Click { tab_id: String, selector: String },
    /// Fill an input with a value.
    Fill {
        tab_id: String,
        selector: String,
        value: String,
    },
    /// Press a key combo.
    Press { tab_id: String, key: String },
    /// Type text into an element.
    Type {
        tab_id: String,
        selector: String,
        text: String,
    },
    /// Select an option by value.
    Select {
        tab_id: String,
        selector: String,
        value: String,
    },
    /// Check a checkbox/radio.
    Check { tab_id: String, selector: String },
    /// Uncheck a checkbox/radio.
    Uncheck { tab_id: String, selector: String },
    /// Scroll by dx, dy pixels.
    Scroll { tab_id: String, dx: f64, dy: f64 },
    /// Evaluate JS expression.
    Eval {
        tab_id: String,
        expression: String,
        await_promise: bool,
    },
    /// Extract structured data from a tab.
    Extract {
        tab_id: String,
        selector: Option<String>,
        all: bool,
        attrs: Option<String>,
        links: bool,
        title: bool,
        text: bool,
        markdown: bool,
        max_bytes: Option<u64>,
    },
    /// Get page content in a format.
    Content {
        tab_id: String,
        format: String,
        max_bytes: Option<u64>,
    },
    /// Take a screenshot.
    Screenshot {
        tab_id: String,
        output_path: Option<String>,
        width: Option<u32>,
    },
    /// Wait for a CSS selector.
    Wait {
        tab_id: String,
        selector: String,
        timeout_ms: Option<u64>,
    },
    /// Close a tab.
    Close { tab_id: String },
    /// Close all tabs.
    CloseAll,
    /// List tabs.
    List,
    /// Save the active tab's storage state (cookies + localStorage) to a JSON file.
    SaveState { path: String },
    /// Load storage state from a JSON file into the active tab.
    LoadState { path: String },
    /// List accounts with state (§7.2).
    AccountList,
    /// One account's state/detail/session horizon.
    AccountStatus { id: String },
    /// Dispose the envelope; back to needs_login.
    AccountLogout { id: String },
    /// Start a login flow: `--mode import` seeds from files; `--mode user`
    /// opens a login window on a fresh account-context tab, finished by
    /// `takeover done|abort`; `--mode agent` runs the unattended M-D flow
    /// (broker fills credentials + TOTP) and reports the outcome.
    AccountLogin {
        id: String,
        mode: Option<String>,
        storage_state: Option<String>,
        cookies: Option<String>,
        agent: Option<String>,
    },
    /// Takeover window control: `takeover` status, `takeover done` capture,
    /// `takeover abort` revert (§5.2 / M5′).
    Takeover { action: Option<String> },
    /// Print help.
    Help,
    /// Exit the session.
    Exit,
}

/// Parse a line into a SessionCommand.
pub fn parse_session_command(line: &str) -> Result<SessionCommand, String> {
    let line = line.trim();

    // Skip empty lines and comments
    if line.is_empty() || line.starts_with('#') {
        return Err("empty".into());
    }

    let tokens: Vec<&str> = line.split_whitespace().collect();
    if tokens.is_empty() {
        return Err("empty".into());
    }

    let cmd = tokens[0];
    let args = &tokens[1..];

    match cmd {
        "new" => Ok(SessionCommand::New),

        "goto" => parse_goto(args),

        "back" => {
            let tab_id = args.first().ok_or("back <tab_id>")?.to_string();
            Ok(SessionCommand::Back { tab_id })
        }

        "forward" => {
            let tab_id = args.first().ok_or("forward <tab_id>")?.to_string();
            Ok(SessionCommand::Forward { tab_id })
        }

        "reload" => {
            let tab_id = args.first().ok_or("reload <tab_id>")?.to_string();
            Ok(SessionCommand::Reload { tab_id })
        }

        "click" => {
            let (pos, tab_id) = require_tab_id(args, "click <tab_id> <selector>")?;
            let selector = pos.first().ok_or("click <tab_id> <selector>")?.to_string();
            Ok(SessionCommand::Click { tab_id, selector })
        }

        "fill" => {
            let (pos, tab_id) = require_tab_id(args, "fill <tab_id> <selector> <value>")?;
            if pos.len() < 2 {
                return Err("fill <tab_id> <selector> <value>".into());
            }
            Ok(SessionCommand::Fill {
                tab_id,
                selector: pos[0].to_string(),
                value: pos[1].to_string(),
            })
        }

        "press" => {
            let (pos, tab_id) = require_tab_id(args, "press <tab_id> <key>")?;
            let key = pos.first().ok_or("press <tab_id> <key>")?.to_string();
            Ok(SessionCommand::Press { tab_id, key })
        }

        "type" => {
            let (pos, tab_id) = require_tab_id(args, "type <tab_id> <selector> <text>")?;
            if pos.len() < 2 {
                return Err("type <tab_id> <selector> <text>".into());
            }
            Ok(SessionCommand::Type {
                tab_id,
                selector: pos[0].to_string(),
                text: pos[1..].join(" "),
            })
        }

        "select" => {
            let (pos, tab_id) = require_tab_id(args, "select <tab_id> <selector> <value>")?;
            if pos.len() < 2 {
                return Err("select <tab_id> <selector> <value>".into());
            }
            Ok(SessionCommand::Select {
                tab_id,
                selector: pos[0].to_string(),
                value: pos[1].to_string(),
            })
        }

        "check" => {
            let (pos, tab_id) = require_tab_id(args, "check <tab_id> <selector>")?;
            let selector = pos.first().ok_or("check <tab_id> <selector>")?.to_string();
            Ok(SessionCommand::Check { tab_id, selector })
        }

        "uncheck" => {
            let (pos, tab_id) = require_tab_id(args, "uncheck <tab_id> <selector>")?;
            let selector = pos
                .first()
                .ok_or("uncheck <tab_id> <selector>")?
                .to_string();
            Ok(SessionCommand::Uncheck { tab_id, selector })
        }

        "scroll" => {
            let (pos, tab_id) = require_tab_id(args, "scroll <tab_id> <dx> <dy>")?;
            if pos.len() < 2 {
                return Err("scroll <tab_id> <dx> <dy>".into());
            }
            let dx: f64 = pos[0].parse().map_err(|_| "scroll: dx must be a number")?;
            let dy: f64 = pos[1].parse().map_err(|_| "scroll: dy must be a number")?;
            Ok(SessionCommand::Scroll { tab_id, dx, dy })
        }

        "eval" => parse_eval(args),

        "extract" => parse_extract(args),

        "content" => parse_content(args),

        "screenshot" => parse_screenshot(args),

        "wait" => parse_wait(args),

        "close" => {
            if args.first().copied() == Some("--all") {
                Ok(SessionCommand::CloseAll)
            } else {
                let tab_id = args
                    .first()
                    .ok_or("close <tab_id> | close --all")?
                    .to_string();
                Ok(SessionCommand::Close { tab_id })
            }
        }

        "list" => Ok(SessionCommand::List),

        "save-state" => {
            if args.len() != 1 {
                return Err("save-state <path>".into());
            }
            Ok(SessionCommand::SaveState {
                path: args[0].to_string(),
            })
        }

        "load-state" => {
            if args.len() != 1 {
                return Err("load-state <path>".into());
            }
            Ok(SessionCommand::LoadState {
                path: args[0].to_string(),
            })
        }

        "help" => Ok(SessionCommand::Help),

        "account_list" => Ok(SessionCommand::AccountList),

        "account_status" => {
            let id = args.first().ok_or("account_status <id>")?.to_string();
            Ok(SessionCommand::AccountStatus { id })
        }

        "account_logout" => {
            let id = args.first().ok_or("account_logout <id>")?.to_string();
            Ok(SessionCommand::AccountLogout { id })
        }

        "account_login" => {
            let mut pos: Vec<&str> = Vec::new();
            let mut mode = None;
            let mut storage_state = None;
            let mut cookies = None;
            let mut agent = None;
            let mut i = 0;
            while i < args.len() {
                match args[i] {
                    "--mode" => {
                        mode = args.get(i + 1).map(|s| s.to_string());
                        i += 2;
                    }
                    "--storage-state" => {
                        storage_state = args.get(i + 1).map(|s| s.to_string());
                        i += 2;
                    }
                    "--cookies" => {
                        cookies = args.get(i + 1).map(|s| s.to_string());
                        i += 2;
                    }
                    "--agent" => {
                        agent = args.get(i + 1).map(|s| s.to_string());
                        i += 2;
                    }
                    other => {
                        pos.push(other);
                        i += 1;
                    }
                }
            }
            let id = pos.first().ok_or("account_login <id> [--mode user|agent|import] [--agent ID] [--storage-state F] [--cookies F]")?.to_string();
            Ok(SessionCommand::AccountLogin {
                id,
                mode,
                storage_state,
                cookies,
                agent,
            })
        }

        "takeover" => Ok(SessionCommand::Takeover {
            action: args.first().map(|s| s.to_string()),
        }),

        "exit" | "quit" => Ok(SessionCommand::Exit),

        _ => Err(format!("unknown command: {cmd}")),
    }
}

// ---------------------------------------------------------------------------
// Parsers for complex commands
// ---------------------------------------------------------------------------

fn parse_goto(args: &[&str]) -> Result<SessionCommand, String> {
    let usage = "goto <tab_id> <url> [--wait <selector>] [--timeout <ms>]";
    let (pos, tab_id) = require_tab_id(args, usage)?;
    let url = pos.first().ok_or(usage.to_string())?.to_string();

    let mut wait_selector: Option<String> = None;
    let mut timeout_ms: Option<u64> = None;
    let mut i = 1; // skip URL
    while i < pos.len() {
        match pos[i] {
            "--wait" => {
                i += 1;
                wait_selector = Some(
                    pos.get(i)
                        .ok_or("goto: --wait requires a selector")?
                        .to_string(),
                );
            }
            "--timeout" => {
                i += 1;
                timeout_ms = Some(
                    pos.get(i)
                        .ok_or("goto: --timeout requires a value")?
                        .parse::<u64>()
                        .map_err(|_| "goto: --timeout must be a number")?,
                );
            }
            other => return Err(format!("goto: unexpected argument: {other}")),
        }
        i += 1;
    }

    Ok(SessionCommand::Goto {
        tab_id,
        url,
        wait_selector,
        timeout_ms,
    })
}

fn parse_eval(args: &[&str]) -> Result<SessionCommand, String> {
    let usage = "eval <tab_id> <expression> [--await]";
    let (pos, tab_id) = require_tab_id(args, usage)?;

    let mut await_promise = false;
    let mut expr_parts: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < pos.len() {
        match pos[i] {
            "--await" => {
                await_promise = true;
            }
            other => expr_parts.push(other),
        }
        i += 1;
    }

    let expression = if expr_parts.is_empty() {
        return Err(usage.into());
    } else {
        strip_quotes(&expr_parts.join(" ")).to_string()
    };

    Ok(SessionCommand::Eval {
        tab_id,
        expression,
        await_promise,
    })
}

fn parse_extract(args: &[&str]) -> Result<SessionCommand, String> {
    let usage = "extract <tab_id> [--selector <s>] [--all] [--attrs a,b] [--links] [--title] [--text] [--markdown] [--max-bytes N]";
    let (pos, tab_id) = require_tab_id(args, usage)?;

    let mut selector: Option<String> = None;
    let mut all = false;
    let mut attrs: Option<String> = None;
    let mut links = false;
    let mut title = false;
    let mut text = false;
    let mut markdown = false;
    let mut max_bytes: Option<u64> = None;

    let mut i = 0;
    while i < pos.len() {
        match pos[i] {
            "--selector" => {
                i += 1;
                selector = Some(
                    pos.get(i)
                        .ok_or("extract: --selector requires a value")?
                        .to_string(),
                );
            }
            "--all" => {
                all = true;
            }
            "--attrs" => {
                i += 1;
                attrs = Some(
                    pos.get(i)
                        .ok_or("extract: --attrs requires a value")?
                        .to_string(),
                );
            }
            "--links" => {
                links = true;
            }
            "--title" => {
                title = true;
            }
            "--text" => {
                text = true;
            }
            "--markdown" => {
                markdown = true;
            }
            "--max-bytes" => {
                i += 1;
                max_bytes = Some(
                    pos.get(i)
                        .ok_or("extract: --max-bytes requires a value")?
                        .parse::<u64>()
                        .map_err(|_| "extract: --max-bytes must be a number")?,
                );
            }
            other => return Err(format!("extract: unexpected argument: {other}")),
        }
        i += 1;
    }

    Ok(SessionCommand::Extract {
        tab_id,
        selector,
        all,
        attrs,
        links,
        title,
        text,
        markdown,
        max_bytes,
    })
}

fn parse_content(args: &[&str]) -> Result<SessionCommand, String> {
    let usage = "content <tab_id> [--format markdown|html|text] [--max-bytes N]";
    let (pos, tab_id) = require_tab_id(args, usage)?;

    let mut format = "markdown".to_string();
    let mut max_bytes: Option<u64> = None;

    let mut i = 0;
    while i < pos.len() {
        match pos[i] {
            "--format" => {
                i += 1;
                format = pos
                    .get(i)
                    .ok_or("content: --format requires a value")?
                    .to_string();
            }
            "--max-bytes" => {
                i += 1;
                max_bytes = Some(
                    pos.get(i)
                        .ok_or("content: --max-bytes requires a value")?
                        .parse::<u64>()
                        .map_err(|_| "content: --max-bytes must be a number")?,
                );
            }
            other => return Err(format!("content: unexpected argument: {other}")),
        }
        i += 1;
    }

    Ok(SessionCommand::Content {
        tab_id,
        format,
        max_bytes,
    })
}

fn parse_screenshot(args: &[&str]) -> Result<SessionCommand, String> {
    let usage = "screenshot <tab_id> [-o path] [--width N]";
    let (pos, tab_id) = require_tab_id(args, usage)?;

    let mut output_path: Option<String> = None;
    let mut width: Option<u32> = None;

    let mut i = 0;
    while i < pos.len() {
        match pos[i] {
            "-o" => {
                i += 1;
                output_path = Some(
                    pos.get(i)
                        .ok_or("screenshot: -o requires a path")?
                        .to_string(),
                );
            }
            "--width" => {
                i += 1;
                width = Some(
                    pos.get(i)
                        .ok_or("screenshot: --width requires a value")?
                        .parse::<u32>()
                        .map_err(|_| "screenshot: --width must be a number")?,
                );
            }
            other => return Err(format!("screenshot: unexpected argument: {other}")),
        }
        i += 1;
    }

    Ok(SessionCommand::Screenshot {
        tab_id,
        output_path,
        width,
    })
}

fn parse_wait(args: &[&str]) -> Result<SessionCommand, String> {
    let usage = "wait <tab_id> <selector> [--timeout <ms>]";
    let (pos, tab_id) = require_tab_id(args, usage)?;
    let selector = pos.first().ok_or(usage.to_string())?.to_string();

    let mut timeout_ms: Option<u64> = None;
    let mut i = 1;
    while i < pos.len() {
        match pos[i] {
            "--timeout" => {
                i += 1;
                timeout_ms = Some(
                    pos.get(i)
                        .ok_or("wait: --timeout requires a value")?
                        .parse::<u64>()
                        .map_err(|_| "wait: --timeout must be a number")?,
                );
            }
            other => return Err(format!("wait: unexpected argument: {other}")),
        }
        i += 1;
    }

    Ok(SessionCommand::Wait {
        tab_id,
        selector,
        timeout_ms,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract the tab_id (first positional arg) and return the remaining
/// positional args (excluding flags and their values).
fn require_tab_id<'a>(args: &'a [&str], usage: &str) -> Result<(Vec<&'a str>, String), String> {
    if args.is_empty() {
        return Err(usage.into());
    }
    let tab_id = args[0].to_string();
    Ok((args[1..].to_vec(), tab_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    #[test]
    fn test_parse_new() {
        let cmd = parse_session_command("new").unwrap();
        assert_matches!(cmd, SessionCommand::New);
    }

    #[test]
    fn test_parse_exit() {
        let cmd = parse_session_command("exit").unwrap();
        assert_matches!(cmd, SessionCommand::Exit);
    }

    #[test]
    fn test_parse_quit() {
        let cmd = parse_session_command("quit").unwrap();
        assert_matches!(cmd, SessionCommand::Exit);
    }

    #[test]
    fn test_parse_list() {
        let cmd = parse_session_command("list").unwrap();
        assert_matches!(cmd, SessionCommand::List);
    }

    #[test]
    fn test_parse_help() {
        let cmd = parse_session_command("help").unwrap();
        assert_matches!(cmd, SessionCommand::Help);
    }

    #[test]
    fn test_parse_save_state() {
        let cmd = parse_session_command("save-state /tmp/state.json").unwrap();
        match cmd {
            SessionCommand::SaveState { path } => assert_eq!(path, "/tmp/state.json"),
            _ => panic!("expected SaveState"),
        }
    }

    #[test]
    fn test_parse_load_state() {
        let cmd = parse_session_command("load-state /tmp/state.json").unwrap();
        match cmd {
            SessionCommand::LoadState { path } => assert_eq!(path, "/tmp/state.json"),
            _ => panic!("expected LoadState"),
        }
    }

    #[test]
    fn test_parse_save_state_missing_path() {
        assert!(parse_session_command("save-state").is_err());
    }

    #[test]
    fn test_parse_load_state_missing_path() {
        assert!(parse_session_command("load-state").is_err());
    }

    #[test]
    fn test_parse_state_extra_args() {
        assert!(parse_session_command("save-state a.json b.json").is_err());
        assert!(parse_session_command("load-state a.json b.json").is_err());
    }

    #[test]
    fn test_parse_empty() {
        assert!(parse_session_command("").is_err());
        assert!(parse_session_command("  ").is_err());
    }

    #[test]
    fn test_parse_comment() {
        assert!(parse_session_command("# this is a comment").is_err());
    }

    #[test]
    fn test_parse_unknown() {
        assert!(parse_session_command("foobar").is_err());
    }

    #[test]
    fn test_parse_goto_simple() {
        let cmd = parse_session_command("goto t1 https://example.com").unwrap();
        match cmd {
            SessionCommand::Goto {
                tab_id,
                url,
                wait_selector,
                timeout_ms,
            } => {
                assert_eq!(tab_id, "t1");
                assert_eq!(url, "https://example.com");
                assert!(wait_selector.is_none());
                assert!(timeout_ms.is_none());
            }
            _ => panic!("expected Goto"),
        }
    }

    #[test]
    fn test_parse_goto_with_flags() {
        let cmd =
            parse_session_command("goto t1 https://example.com --wait .content --timeout 3000")
                .unwrap();
        match cmd {
            SessionCommand::Goto {
                tab_id,
                url,
                wait_selector,
                timeout_ms,
            } => {
                assert_eq!(tab_id, "t1");
                assert_eq!(url, "https://example.com");
                assert_eq!(wait_selector.unwrap(), ".content");
                assert_eq!(timeout_ms.unwrap(), 3000);
            }
            _ => panic!("expected Goto"),
        }
    }

    #[test]
    fn test_parse_goto_missing_url() {
        assert!(parse_session_command("goto t1").is_err());
    }

    #[test]
    fn test_parse_back() {
        let cmd = parse_session_command("back t1").unwrap();
        assert_matches!(cmd, SessionCommand::Back { tab_id } if tab_id == "t1");
    }

    #[test]
    fn test_parse_forward() {
        let cmd = parse_session_command("forward t1").unwrap();
        assert_matches!(cmd, SessionCommand::Forward { tab_id } if tab_id == "t1");
    }

    #[test]
    fn test_parse_reload() {
        let cmd = parse_session_command("reload t1").unwrap();
        assert_matches!(cmd, SessionCommand::Reload { tab_id } if tab_id == "t1");
    }

    #[test]
    fn test_parse_click() {
        let cmd = parse_session_command("click t1 button.submit").unwrap();
        assert_matches!(cmd, SessionCommand::Click { tab_id, selector }
            if tab_id == "t1" && selector == "button.submit");
    }

    #[test]
    fn test_parse_fill() {
        let cmd = parse_session_command("fill t1 input#name John").unwrap();
        match cmd {
            SessionCommand::Fill {
                tab_id,
                selector,
                value,
            } => {
                assert_eq!(tab_id, "t1");
                assert_eq!(selector, "input#name");
                assert_eq!(value, "John");
            }
            _ => panic!("expected Fill"),
        }
    }

    #[test]
    fn test_parse_press() {
        let cmd = parse_session_command("press t1 Enter").unwrap();
        assert_matches!(cmd, SessionCommand::Press { tab_id, key }
            if tab_id == "t1" && key == "Enter");
    }

    #[test]
    fn test_parse_type() {
        let cmd = parse_session_command("type t1 input#search hello world").unwrap();
        match cmd {
            SessionCommand::Type {
                tab_id,
                selector,
                text,
            } => {
                assert_eq!(tab_id, "t1");
                assert_eq!(selector, "input#search");
                assert_eq!(text, "hello world");
            }
            _ => panic!("expected Type"),
        }
    }

    #[test]
    fn test_parse_select() {
        let cmd = parse_session_command("select t1 select#country US").unwrap();
        match cmd {
            SessionCommand::Select {
                tab_id,
                selector,
                value,
            } => {
                assert_eq!(tab_id, "t1");
                assert_eq!(selector, "select#country");
                assert_eq!(value, "US");
            }
            _ => panic!("expected Select"),
        }
    }

    #[test]
    fn test_parse_check() {
        let cmd = parse_session_command("check t1 input#agree").unwrap();
        assert_matches!(cmd, SessionCommand::Check { tab_id, selector }
            if tab_id == "t1" && selector == "input#agree");
    }

    #[test]
    fn test_parse_uncheck() {
        let cmd = parse_session_command("uncheck t1 input#agree").unwrap();
        assert_matches!(cmd, SessionCommand::Uncheck { tab_id, selector }
            if tab_id == "t1" && selector == "input#agree");
    }

    #[test]
    fn test_parse_scroll() {
        let cmd = parse_session_command("scroll t1 0 500").unwrap();
        assert_matches!(cmd, SessionCommand::Scroll { tab_id, dx, dy }
            if tab_id == "t1" && dx == 0.0 && dy == 500.0);
    }

    #[test]
    fn test_parse_eval() {
        let cmd = parse_session_command("eval t1 document.title").unwrap();
        match cmd {
            SessionCommand::Eval {
                tab_id,
                expression,
                await_promise,
            } => {
                assert_eq!(tab_id, "t1");
                assert_eq!(expression, "document.title");
                assert!(!await_promise);
            }
            _ => panic!("expected Eval"),
        }
    }

    #[test]
    fn test_parse_eval_await() {
        let cmd = parse_session_command("eval t1 fetch('/api') --await").unwrap();
        match cmd {
            SessionCommand::Eval {
                tab_id,
                expression,
                await_promise,
            } => {
                assert_eq!(tab_id, "t1");
                assert!(await_promise);
                assert_eq!(expression, "fetch('/api')");
            }
            _ => panic!("expected Eval"),
        }
    }

    #[test]
    fn test_parse_extract() {
        let cmd = parse_session_command("extract t1 --selector h1 --all --links --title").unwrap();
        match cmd {
            SessionCommand::Extract {
                tab_id,
                selector,
                all,
                links,
                title,
                ..
            } => {
                assert_eq!(tab_id, "t1");
                assert_eq!(selector.unwrap(), "h1");
                assert!(all);
                assert!(links);
                assert!(title);
            }
            _ => panic!("expected Extract"),
        }
    }

    #[test]
    fn test_parse_content() {
        let cmd = parse_session_command("content t1 --format html --max-bytes 1024").unwrap();
        match cmd {
            SessionCommand::Content {
                tab_id,
                format,
                max_bytes,
            } => {
                assert_eq!(tab_id, "t1");
                assert_eq!(format, "html");
                assert_eq!(max_bytes.unwrap(), 1024);
            }
            _ => panic!("expected Content"),
        }
    }

    #[test]
    fn test_parse_screenshot() {
        let cmd = parse_session_command("screenshot t1 -o out.png --width 1024").unwrap();
        match cmd {
            SessionCommand::Screenshot {
                tab_id,
                output_path,
                width,
            } => {
                assert_eq!(tab_id, "t1");
                assert_eq!(output_path.unwrap(), "out.png");
                assert_eq!(width.unwrap(), 1024);
            }
            _ => panic!("expected Screenshot"),
        }
    }

    #[test]
    fn test_parse_wait() {
        let cmd = parse_session_command("wait t1 .loaded --timeout 5000").unwrap();
        match cmd {
            SessionCommand::Wait {
                tab_id,
                selector,
                timeout_ms,
            } => {
                assert_eq!(tab_id, "t1");
                assert_eq!(selector, ".loaded");
                assert_eq!(timeout_ms.unwrap(), 5000);
            }
            _ => panic!("expected Wait"),
        }
    }

    #[test]
    fn test_parse_close() {
        let cmd = parse_session_command("close t1").unwrap();
        assert_matches!(cmd, SessionCommand::Close { tab_id } if tab_id == "t1");
    }

    #[test]
    fn test_parse_close_all() {
        let cmd = parse_session_command("close --all").unwrap();
        assert_matches!(cmd, SessionCommand::CloseAll);
    }

    #[test]
    fn test_strip_quotes() {
        assert_eq!(strip_quotes("\"hello\""), "hello");
        assert_eq!(strip_quotes("'world'"), "world");
        assert_eq!(strip_quotes("no quotes"), "no quotes");
        assert_eq!(
            strip_quotes("\"unclosed, 'unclosed2'"),
            "\"unclosed, 'unclosed2'"
        );
    }
}
