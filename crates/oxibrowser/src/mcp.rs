//! stdio MCP (Model Context Protocol) server — newline-delimited JSON-RPC 2.0.
//!
//! `oxibrowser serve --mcp` speaks JSON-RPC 2.0 over stdin/stdout, one message
//! per line. stdout carries only protocol responses; all logging goes to
//! stderr via `tracing` (the subscriber in `main` writes to stderr).
//!
//! Supported methods:
//! - `initialize` — echoes the client's `protocolVersion` (default
//!   [`DEFAULT_PROTOCOL_VERSION`]), advertises the `tools` capability.
//! - `tools/list` — the 9 browser tools ([`tool_definitions`]).
//! - `tools/call` — executes a tool against a single lazily-created, reused
//!   [`Tab`].
//!
//! Notifications (messages without an `id`) are ignored. Unknown methods
//! answer with `-32601`; unparseable lines with `-32700`.

use base64::Engine as _;
use serde_json::{Value, json};
use std::io::BufRead;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;

use oxibrowser_core::Browser;
use oxibrowser_core::Tab;
use oxibrowser_core::tab::WaitCondition;

/// Protocol version advertised when the client sends none.
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";
/// Viewport width used by `browser_screenshot`.
const SCREENSHOT_WIDTH: u32 = 1280;
/// Default timeout for `browser_wait`.
const DEFAULT_WAIT_TIMEOUT_MS: u64 = 10_000;
/// Characters of markdown kept in `browser_navigate` results (full text via
/// `browser_read`).
const MARKDOWN_PREVIEW_CHARS: usize = 2_000;

// ---------------------------------------------------------------------------
// Envelope parsing
// ---------------------------------------------------------------------------

/// One parsed inbound JSON-RPC 2.0 message.
#[derive(Debug, PartialEq)]
enum Envelope {
    /// A request (has an `id`) that requires a response.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// A notification (no `id`) — answered by ignoring it.
    Notification,
    /// Invalid JSON or not a request object — answered with `-32700`.
    Malformed,
}

/// Parse one newline-delimited JSON-RPC 2.0 message.
///
/// A line that fails `serde_json` parsing, is not a JSON object, or carries
/// an `id` without a string `method` is [`Envelope::Malformed`]. An object
/// with a method but no `id` is a notification.
fn parse_envelope(line: &str) -> Envelope {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Envelope::Malformed;
    };
    let Some(object) = value.as_object() else {
        return Envelope::Malformed;
    };
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        // No method: a stray response-like object with an id is malformed;
        // without an id there is nothing to answer — ignore it.
        return if object.contains_key("id") {
            Envelope::Malformed
        } else {
            Envelope::Notification
        };
    };
    match object.get("id") {
        Some(id) => Envelope::Request {
            id: id.clone(),
            method: method.to_string(),
            params: object.get("params").cloned().unwrap_or(Value::Null),
        },
        None => Envelope::Notification,
    }
}

/// Build a success response envelope.
fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// Build an error response envelope.
fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

/// Build the `initialize` result, echoing the client's protocol version.
fn initialize_result(params: &Value) -> Value {
    let version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_PROTOCOL_VERSION);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "oxibrowser",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

// ---------------------------------------------------------------------------
// Tool definitions
// ---------------------------------------------------------------------------

/// The tool surface as a JSON array of MCP tool descriptors
/// (`name`, `description`, `inputSchema`): 9 browser tools + 3 account
/// tools (roadmap item 4 / upper design §7.4 M9).
fn tool_definitions() -> Value {
    let schema_object = |properties: Value, required: &[&str]| {
        let mut schema = json!({ "type": "object", "properties": properties });
        if !required.is_empty()
            && let Some(obj) = schema.as_object_mut()
        {
            obj.insert("required".into(), json!(required));
        }
        schema
    };
    json!([
        {
            "name": "browser_navigate",
            "description": "Navigate the tab to a URL. Returns {url, title, status, markdown} \
                            with the first ~2000 characters of page markdown.",
            "inputSchema": schema_object(
                json!({ "url": { "type": "string", "description": "Absolute URL to navigate to." } }),
                &["url"],
            ),
        },
        {
            "name": "browser_observe",
            "description": "List the current page's interactive elements (links, buttons, \
                            inputs) as a JSON array with role, text, href and CSS selector.",
            "inputSchema": schema_object(json!({}), &[]),
        },
        {
            "name": "browser_click",
            "description": "Click the first element matching a CSS selector.",
            "inputSchema": schema_object(
                json!({ "selector": { "type": "string", "description": "CSS selector." } }),
                &["selector"],
            ),
        },
        {
            "name": "browser_fill",
            "description": "Fill an input, textarea or contentEditable element with a value.",
            "inputSchema": schema_object(
                json!({
                    "selector": { "type": "string", "description": "CSS selector." },
                    "value": { "type": "string", "description": "Value to insert." },
                }),
                &["selector", "value"],
            ),
        },
        {
            "name": "browser_eval",
            "description": "Evaluate a JavaScript expression in the page, awaiting Promise \
                            resolution.",
            "inputSchema": schema_object(
                json!({ "expression": { "type": "string", "description": "JavaScript to run." } }),
                &["expression"],
            ),
        },
        {
            "name": "browser_read",
            "description": "Read the current page as {url, title, markdown}.",
            "inputSchema": schema_object(json!({}), &[]),
        },
        {
            "name": "browser_screenshot",
            "description": "Capture a PNG screenshot at width 1280. With `path`, saves the PNG \
                            to disk; without, returns it base64-encoded.",
            "inputSchema": schema_object(
                json!({ "path": { "type": "string", "description": "Destination file path; \
                                  omit to receive base64." } }),
                &[],
            ),
        },
        {
            "name": "browser_wait",
            "description": "Wait for a CSS selector to match and/or the network to go idle.",
            "inputSchema": schema_object(
                json!({
                    "selector": { "type": "string", "description": "CSS selector to wait for." },
                    "networkIdle": { "type": "boolean",
                                     "description": "Wait until no requests are in flight." },
                    "timeoutMs": { "type": "integer", "default": 10_000 },
                }),
                &[],
            ),
        },
        {
            "name": "browser_close",
            "description": "Close the current tab. The next tool call opens a fresh one.",
            "inputSchema": schema_object(json!({}), &[]),
        },
        {
            "name": "account_list",
            "description": "List registered accounts (metadata + state only — no secrets, \
                            no session values). Read-only.",
            "inputSchema": schema_object(json!({}), &[]),
        },
        {
            "name": "account_status",
            "description": "One account's state, detail, and session horizon. Read-only \
                            (network probing stays on the CLI: `account status --probe`).",
            "inputSchema": schema_object(
                json!({ "account_id": { "type": "string", "description": "Account slug." } }),
                &["account_id"],
            ),
        },
        {
            "name": "login_request",
            "description": "Request a human login escalation for an account (stale, \
                            needs_login, or challenge). Returns the exact CLI commands \
                            the operator should run — the MCP server cannot open a \
                            viewer window itself.",
            "inputSchema": schema_object(
                json!({
                    "account_id": { "type": "string", "description": "Account slug." },
                    "reason": { "type": "string", "description": "Why the agent needs the login." },
                }),
                &["account_id"],
            ),
        },
    ])
}

// ---------------------------------------------------------------------------
// Tool execution
// ---------------------------------------------------------------------------

/// Read a required string argument from a tool call's `arguments`.
fn string_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Account plane for the MCP surface (roadmap item 4): registry handle for
/// the account tools, the acting agent id (`serve --mcp --as-agent`), and
/// the primary bound context browser tools create tabs in
/// (`serve --mcp --account id[,…]`).
struct McpAccounts {
    registry: Option<oxibrowser_core::account::AccountRegistry>,
    agent: Option<String>,
    primary: Option<std::sync::Arc<oxibrowser_core::context::BrowserContext>>,
}

impl McpAccounts {
    fn empty() -> Self {
        McpAccounts {
            registry: None,
            agent: None,
            primary: None,
        }
    }

    fn registry(&self) -> Result<&oxibrowser_core::account::AccountRegistry, String> {
        self.registry
            .as_ref()
            .ok_or_else(|| "accountsUnavailable: account registry could not be opened".to_string())
    }
}

/// Return the reused tab, creating one on first use (or after a close) —
/// inside the primary bound account context when one exists.
async fn reused_tab(
    browser: &Browser,
    tab_slot: &Mutex<Option<Tab>>,
    primary: Option<&std::sync::Arc<oxibrowser_core::context::BrowserContext>>,
) -> Result<Tab, String> {
    let mut guard = tab_slot.lock().await;
    if guard.as_ref().is_none_or(Tab::is_closed) {
        let tab = match primary {
            Some(ctx) => browser
                .new_tab_in(ctx)
                .await
                .map_err(|e| format!("new_tab failed: {e}"))?,
            None => browser
                .new_tab()
                .await
                .map_err(|e| format!("new_tab failed: {e}"))?,
        };
        *guard = Some(tab);
    }
    Ok(guard.as_ref().expect("tab just created").clone())
}

/// Execute a `tools/call`. Returns the tool's JSON result, or an error string
/// surfaced as `isError: true` per the MCP tool-call contract.
async fn call_tool(
    browser: &Browser,
    tab_slot: &Mutex<Option<Tab>>,
    accounts: &McpAccounts,
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    // Account-plane tools (roadmap item 4): read-only registry views + the
    // human-escalation request. They never touch the tab.
    match name {
        "account_list" => {
            let records = accounts.registry()?.list().map_err(|e| e.to_string())?;
            let rows: Vec<Value> = records
                .iter()
                .map(|r| {
                    json!({
                        "account_id": r.account_id,
                        "scope": r.scope,
                        "state": r.state.as_str(),
                        "state_detail": r.state_detail,
                        "login_hint": r.identity.login_hint,
                    })
                })
                .collect();
            return Ok(json!({ "accounts": rows }));
        }
        "account_status" => {
            let id =
                string_arg(args, "account_id").ok_or("missing required argument: account_id")?;
            let r = accounts
                .registry()?
                .get(&id)
                .map_err(|e| format!("accountNotFound: {e}"))?;
            return Ok(json!({
                "account_id": r.account_id,
                "scope": r.scope,
                "state": r.state.as_str(),
                "state_detail": r.state_detail,
                "session_summary": r.session_summary,
            }));
        }
        "login_request" => {
            let id =
                string_arg(args, "account_id").ok_or("missing required argument: account_id")?;
            let reason = string_arg(args, "reason");
            let r = accounts
                .registry()?
                .get(&id)
                .map_err(|e| format!("accountNotFound: {e}"))?;
            return Ok(json!({
                "account_id": r.account_id,
                "state": r.state.as_str(),
                "action_required": "human_login",
                "command": format!("oxibrowser account login {id}"),
                "host_command": format!("oxibrowser account login {id} --json"),
                "agent": accounts.agent,
                "reason": reason,
            }));
        }
        _ => {}
    }

    if name == "browser_close" {
        if let Some(tab) = tab_slot.lock().await.take() {
            tab.close().await.map_err(|e| e.to_string())?;
        }
        return Ok(json!({ "closed": true }));
    }

    let tab = reused_tab(browser, tab_slot, accounts.primary.as_ref()).await?;
    match name {
        "browser_navigate" => {
            let url = string_arg(args, "url").ok_or("missing required argument: url")?;
            let r = tab.goto(&url).await.map_err(|e| e.to_string())?;
            let markdown: String = r.markdown.chars().take(MARKDOWN_PREVIEW_CHARS).collect();
            Ok(json!({
                "url": r.url,
                "title": r.title,
                "status": r.status,
                "markdown": markdown,
            }))
        }
        "browser_observe" => tab
            .interactive_elements_json()
            .await
            .map_err(|e| e.to_string()),
        "browser_click" => {
            let selector =
                string_arg(args, "selector").ok_or("missing required argument: selector")?;
            tab.click(&selector).await.map_err(|e| e.to_string())?;
            Ok(json!({ "clicked": selector }))
        }
        "browser_fill" => {
            let selector =
                string_arg(args, "selector").ok_or("missing required argument: selector")?;
            let value = string_arg(args, "value").ok_or("missing required argument: value")?;
            // Credential-mode literal gate (design §6.2): passwords flow only
            // through the audited credential path.
            if tab.in_credential_mode().await && tab.selector_targets_password(&selector).await {
                return Err(
                    "passwordFillRequiresCredential: fill password fields via the credential path"
                        .to_string(),
                );
            }
            tab.fill(&selector, &value)
                .await
                .map_err(|e| e.to_string())?;
            Ok(json!({ "filled": selector }))
        }
        "browser_eval" => {
            let expression =
                string_arg(args, "expression").ok_or("missing required argument: expression")?;
            tab.evaluate_await(&expression)
                .await
                .map_err(|e| e.to_string())
        }
        "browser_read" => {
            let r = tab.content().await.map_err(|e| e.to_string())?;
            Ok(json!({
                "url": r.url,
                "title": r.title,
                "markdown": r.markdown,
            }))
        }
        "browser_screenshot" => {
            let png = tab
                .screenshot(SCREENSHOT_WIDTH)
                .await
                .map_err(|e| e.to_string())?;
            match string_arg(args, "path") {
                Some(path) => {
                    std::fs::write(&path, &png).map_err(|e| format!("write failed: {e}"))?;
                    Ok(json!({
                        "saved": path,
                        "size": png.len(),
                        "width": SCREENSHOT_WIDTH,
                    }))
                }
                None => {
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
                    Ok(json!({
                        "screenshot": b64,
                        "encoding": "base64",
                        "size": png.len(),
                        "width": SCREENSHOT_WIDTH,
                    }))
                }
            }
        }
        "browser_wait" => {
            let timeout_ms = args
                .get("timeoutMs")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
            let selector = args.get("selector").and_then(Value::as_str);
            let network_idle = args
                .get("networkIdle")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if selector.is_none() && !network_idle {
                return Err("browser_wait requires `selector` and/or `networkIdle`".into());
            }
            let mut waited = Vec::new();
            if let Some(sel) = selector {
                tab.wait_for(sel, timeout_ms)
                    .await
                    .map_err(|e| e.to_string())?;
                waited.push(json!({ "selector": sel }));
            }
            if network_idle {
                tab.wait_for_condition(WaitCondition::NetworkIdle, timeout_ms)
                    .await
                    .map_err(|e| e.to_string())?;
                waited.push(json!({ "networkIdle": true }));
            }
            Ok(json!({ "waited": waited, "timeoutMs": timeout_ms }))
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

// ---------------------------------------------------------------------------
// Message loop
// ---------------------------------------------------------------------------

/// Handle one inbound line: parse the envelope, dispatch, and return the
/// serialized JSON-RPC response when one is due (never for notifications).
async fn handle_line(
    line: &str,
    browser: &Browser,
    tab_slot: &Mutex<Option<Tab>>,
    accounts: &McpAccounts,
) -> Option<String> {
    match parse_envelope(line) {
        Envelope::Malformed => Some(rpc_error(Value::Null, -32700, "Parse error").to_string()),
        Envelope::Notification => None,
        Envelope::Request { id, method, params } => {
            let response = match method.as_str() {
                "initialize" => rpc_result(id, initialize_result(&params)),
                "tools/list" => rpc_result(id, json!({ "tools": tool_definitions() })),
                "tools/call" => {
                    let name = params
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let arguments = params
                        .get("arguments")
                        .cloned()
                        .unwrap_or_else(|| json!({}));
                    match call_tool(browser, tab_slot, accounts, name, &arguments).await {
                        Ok(result) => rpc_result(
                            id,
                            json!({ "content": [{ "type": "text", "text": result.to_string() }] }),
                        ),
                        Err(err) => rpc_result(
                            id,
                            json!({
                                "content": [{ "type": "text", "text": err }],
                                "isError": true,
                            }),
                        ),
                    }
                }
                other => rpc_error(id, -32601, &format!("Method not found: {other}")),
            };
            Some(response.to_string())
        }
    }
}

/// Run the stdio MCP server. Returns the process exit code.
///
/// Mirrors `run_serve`'s browser configuration (cookie file, SSRF filter,
/// proxy) without any network listener: the protocol runs on stdin/stdout.
/// EOF, Ctrl+C, and SIGTERM all trigger clean shutdown (close tab → close
/// browser → exit 0), reusing the stdin-thread + mpsc + select pattern from
/// the session REPL.
pub async fn run_mcp_stdio(
    cookie_file: Option<&str>,
    allow_private_ips: bool,
    proxy: Option<String>,
    accounts: Option<&str>,
    agent: Option<&str>,
    ref_tag: Option<&str>,
) -> i32 {
    let mut config = oxibrowser_core::BrowserConfig::headless();
    if let Some(path) = cookie_file {
        config.cookie_file = Some(std::path::PathBuf::from(path));
    }
    if allow_private_ips {
        config.enable_ssrf_filter = false;
        eprintln!("⚠ SSRF filter disabled: private/internal IP ranges accessible.");
    }
    if let Some(p) = proxy {
        config.proxy = Some(p);
    }

    let browser = match Browser::new(config).await {
        Ok(b) => Arc::new(b),
        Err(e) => {
            eprintln!("Error: browser init failed: {e}");
            return 1;
        }
    };

    // Account plane (roadmap item 4): `--account id[,…]` binds each account's
    // envelope into a context (credential mode on); the first is primary and
    // hosts the browser tools' tab. `--as-agent` is the ledger identity.
    let mut mcp_accounts = McpAccounts {
        registry: crate::account_cli::oxi_accounts_registry().ok(),
        agent: agent.map(str::to_string),
        primary: None,
    };
    if let Some(list) = accounts {
        let ids: Vec<&str> = list
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if ids.is_empty() {
            eprintln!("Error: --account requires at least one account id");
            return 2;
        }
        for id in ids {
            match crate::account_cli::bind_account_context(&browser, id, ref_tag).await {
                Ok(ctx) => {
                    if mcp_accounts.primary.is_none() {
                        mcp_accounts.primary = Some(ctx);
                    }
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    return 1;
                }
            }
        }
    }

    info!("MCP stdio server ready (12 tools)");

    // Lazily-created, reused tab (see `reused_tab`).
    let tab_slot: Mutex<Option<Tab>> = Mutex::new(None);

    // Read from stdin on a blocking thread so we can select with signals.
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
            // SIGTERM not available (e.g. Windows) — dummy that never fires
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1()).unwrap()
        });

    let exit_code = loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Some(Some(line)) => {
                        let line = line.trim();
                        if line.is_empty() {
                            continue;
                        }
                        if let Some(response) =
                            handle_line(line, &browser, &tab_slot, &mcp_accounts).await
                        {
                            println!("{response}");
                            use std::io::Write;
                            let _ = std::io::stdout().flush();
                        }
                    }
                    Some(None) | None => break 0,
                }
            }
            _ = tokio::signal::ctrl_c() => break 0,
            _ = sigterm.recv() => break 0,
        }
    };

    // Cleanup
    if let Some(tab) = tab_slot.lock().await.take() {
        tab.close().await.ok();
    }
    browser.close().await.ok();
    exit_code
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Envelope parsing ───────────────────────────────────────────────────

    #[test]
    fn parses_request_envelope() {
        let line = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call",
                       "params":{"name":"browser_read","arguments":{}}}"#;
        match parse_envelope(line) {
            Envelope::Request { id, method, params } => {
                assert_eq!(id, json!(7));
                assert_eq!(method, "tools/call");
                assert_eq!(params["name"], "browser_read");
            }
            other => panic!("expected Request, got {other:?}"),
        }
    }

    #[test]
    fn request_without_params_defaults_to_null() {
        match parse_envelope(r#"{"jsonrpc":"2.0","id":"a","method":"tools/list"}"#) {
            Envelope::Request { id, params, .. } => {
                assert_eq!(id, json!("a"));
                assert_eq!(params, Value::Null);
            }
            other => panic!("expected Request, got {other:?}"),
        }
    }

    #[test]
    fn notification_without_id_is_ignored() {
        assert_eq!(
            parse_envelope(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
            Envelope::Notification
        );
    }

    #[test]
    fn invalid_json_is_malformed() {
        assert_eq!(parse_envelope("{not json"), Envelope::Malformed);
    }

    #[test]
    fn non_object_json_is_malformed() {
        assert_eq!(parse_envelope("42"), Envelope::Malformed);
        assert_eq!(parse_envelope(r#""hello""#), Envelope::Malformed);
    }

    #[test]
    fn id_without_method_is_malformed_but_idless_stray_is_ignored() {
        assert_eq!(
            parse_envelope(r#"{"jsonrpc":"2.0","id":1}"#),
            Envelope::Malformed
        );
        assert_eq!(
            parse_envelope(r#"{"jsonrpc":"2.0"}"#),
            Envelope::Notification
        );
    }

    // ── Response envelopes ─────────────────────────────────────────────────

    #[test]
    fn error_envelope_has_code_and_message() {
        let e = rpc_error(json!(3), -32601, "Method not found: nope");
        assert_eq!(e["jsonrpc"], "2.0");
        assert_eq!(e["id"], 3);
        assert_eq!(e["error"]["code"], -32601);
        assert_eq!(e["error"]["message"], "Method not found: nope");
        assert!(e.get("result").is_none());
    }

    #[test]
    fn initialize_echoes_client_protocol_version() {
        let result = initialize_result(&json!({ "protocolVersion": "1999-01-01" }));
        assert_eq!(result["protocolVersion"], "1999-01-01");
        assert_eq!(result["capabilities"]["tools"], json!({}));
        assert_eq!(result["serverInfo"]["name"], "oxibrowser");
        assert_eq!(result["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn initialize_falls_back_to_default_protocol_version() {
        assert_eq!(
            initialize_result(&json!({}))["protocolVersion"],
            DEFAULT_PROTOCOL_VERSION
        );
        assert_eq!(
            initialize_result(&Value::Null)["protocolVersion"],
            DEFAULT_PROTOCOL_VERSION
        );
    }

    // ── Tool definitions ───────────────────────────────────────────────────

    #[test]
    fn tools_list_exposes_all_documented_tools() {
        let tools = tool_definitions();
        let arr = tools.as_array().expect("tool array");
        assert_eq!(arr.len(), 12);
        for tool in arr {
            assert!(
                tool["name"].as_str().is_some_and(|n| !n.is_empty()),
                "tool missing name: {tool}"
            );
            assert!(tool["description"].is_string(), "tool missing description");
            assert_eq!(tool["inputSchema"]["type"], "object");
        }
        let names: Vec<&str> = arr.iter().filter_map(|t| t["name"].as_str()).collect();
        for expected in [
            "browser_navigate",
            "browser_observe",
            "browser_click",
            "browser_fill",
            "browser_eval",
            "browser_read",
            "browser_screenshot",
            "browser_wait",
            "browser_close",
            "account_list",
            "account_status",
            "login_request",
        ] {
            assert!(names.contains(&expected), "missing tool {expected}");
        }
    }

    #[test]
    fn required_arguments_are_declared_in_input_schema() {
        let tools = tool_definitions();
        let required = |name: &str| -> Vec<String> {
            tools
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["name"] == name)
                .map(|t| {
                    t["inputSchema"]["required"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .unwrap()
        };
        assert_eq!(required("browser_navigate"), vec!["url"]);
        assert_eq!(required("browser_click"), vec!["selector"]);
        assert_eq!(required("browser_fill"), vec!["selector", "value"]);
        assert_eq!(required("browser_eval"), vec!["expression"]);
        assert!(required("browser_screenshot").is_empty());
        assert!(required("browser_wait").is_empty());
    }
}

#[cfg(test)]
mod gate_tests {
    use super::*;

    const FORM_HTML: &str = "data:text/html,<html><body><form>\
        <input id='user' type='text'><input id='pw' type='password'>\
        </form></body></html>";

    /// Credential-mode literal gate (design §6.2, F5): MCP `browser_fill`
    /// into an `input[type=password]` is rejected while the context is in
    /// credential mode; non-password targets and non-credential mode pass.
    #[tokio::test]
    async fn browser_fill_rejects_password_literal_in_credential_mode() {
        let browser = Browser::new(oxibrowser_core::BrowserConfig::headless())
            .await
            .unwrap();
        browser.default_context().set_credential_mode(true);
        let tab_slot: Mutex<Option<Tab>> = Mutex::new(None);

        call_tool(
            &browser,
            &tab_slot,
            &McpAccounts::empty(),
            "browser_navigate",
            &json!({ "url": FORM_HTML }),
        )
        .await
        .unwrap();

        let err = call_tool(
            &browser,
            &tab_slot,
            &McpAccounts::empty(),
            "browser_fill",
            &json!({ "selector": "#pw", "value": "hunter2" }),
        )
        .await
        .unwrap_err();
        assert!(
            err.contains("passwordFillRequiresCredential"),
            "password literal must be gated: {err}"
        );

        // Non-password targets keep flowing.
        let ok = call_tool(
            &browser,
            &tab_slot,
            &McpAccounts::empty(),
            "browser_fill",
            &json!({ "selector": "#user", "value": "user@example.com" }),
        )
        .await;
        assert!(ok.is_ok(), "non-password fill must pass: {ok:?}");

        // Mode off → the same literal passes.
        browser.default_context().set_credential_mode(false);
        let ok = call_tool(
            &browser,
            &tab_slot,
            &McpAccounts::empty(),
            "browser_fill",
            &json!({ "selector": "#pw", "value": "hunter2" }),
        )
        .await;
        assert!(ok.is_ok(), "fill must pass outside credential mode: {ok:?}");
    }
}
