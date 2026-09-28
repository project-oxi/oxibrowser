//! OxiBrowser CLI 2.0 — headless browser for AI agents.
//!
//! Human is the default. `--json` opts into machine-readable output.
//!
//! 11 subcommands: fetch, extract, run, session, serve, search, describe,
//! skill, version, account, credential

use clap::{Parser, Subcommand};
use serde_json::Value;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::info;

mod describe;
mod mcp;
mod output;
mod session;
mod skill;
mod validate;

#[cfg(feature = "browser")]
mod account_cli;

// search is declared in lib.rs and re-exported as `pub mod search`.
use oxibrowser::search;

/// OxiBrowser — headless browser for AI agents.
#[derive(Parser)]
#[command(name = "oxibrowser")]
#[command(
    version,
    about = "Headless browser for AI agents — single static binary, no Chromium"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
    /// Audit log location (default `~/.oxibrowser/audit.jsonl`).
    #[arg(long, global = true, value_name = "PATH")]
    audit: Option<PathBuf>,
    /// Disable the audit log entirely.
    #[arg(long, global = true)]
    no_audit: bool,
    /// Additional sensitive header names to redact (repeatable), for
    /// organization-specific auth headers the default list does not cover.
    /// Applies to `--har` output and CDP network event URLs.
    #[arg(long = "redact-header", global = true, value_name = "NAME")]
    redact_headers: Vec<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Fetch a URL and return content. Supports interaction before output.
    Fetch {
        /// URL to fetch.
        url: String,
        /// Output format: html, markdown, or text.
        #[arg(long, default_value = "markdown")]
        format: String,
        /// Output as JSON (for agents / scripting).
        #[arg(long)]
        json: bool,
        /// Truncate output at N bytes (JSON mode).
        #[arg(long)]
        max_bytes: Option<u64>,
        /// Comma-separated fields to include: url,title,status,markdown,html,text,content_type.
        #[arg(long)]
        fields: Option<String>,
        /// Page metadata only (headings, links_count, text_length).
        #[arg(long)]
        summary: bool,
        /// Evaluate JS expression after page load.
        #[arg(long)]
        eval: Option<String>,
        /// Click element matching CSS selector.
        #[arg(long)]
        click: Option<String>,
        /// Fill input (format: "selector:value").
        #[arg(long)]
        fill: Option<String>,
        /// Press key (Enter, Tab, Ctrl+C, etc.).
        #[arg(long)]
        press: Option<String>,
        /// Wait for CSS selector before output.
        #[arg(long)]
        wait: Option<String>,
        /// Wait timeout in ms.
        #[arg(long, default_value_t = 5000)]
        wait_timeout: u64,
        /// Extract text from selector instead of full content.
        #[arg(long)]
        extract: Option<String>,
        /// With --extract: return all matches.
        #[arg(long)]
        all: bool,
        /// Print HTTP headers to stderr.
        #[arg(long)]
        headers: bool,
        /// Write the recorded network log as HAR JSON to PATH.
        #[arg(long, value_name = "PATH")]
        har: Option<PathBuf>,
        /// Write the HAR WITHOUT secret redaction. The file will contain
        /// cookies, bearer tokens, and POST bodies verbatim. Warns and
        /// audit-logs; use only for throwaway debugging.
        #[arg(long)]
        har_raw: bool,
        /// Allow navigation to private/internal IP ranges (disables SSRF filter).
        /// Use for local development only.
        #[arg(long)]
        allow_private_ips: bool,
        /// Count unsupported/polyfilled Web API accesses while fetching.
        /// Adds `meta.api_gaps` to `--json` output; over CDP, query with
        /// `OXI.getApiGaps`.
        #[arg(long)]
        telemetry: bool,
        /// Bind to an account's session (restores the stored envelope into a
        /// dedicated context, credential mode on).
        #[arg(long, value_name = "ID")]
        account: Option<String>,
        /// Timeout in seconds.
        #[arg(long, default_value_t = 30)]
        timeout: u64,
    },

    /// Extract structured data from a URL.
    Extract {
        /// URL to extract from.
        url: String,
        /// CSS selector to match elements.
        #[arg(long)]
        selector: Option<String>,
        /// Return all matches (not just first).
        #[arg(long)]
        all: bool,
        /// Comma-separated attributes to extract (text,href,data-*,src,...).
        #[arg(long, default_value = "text")]
        attrs: String,
        /// Extract all <a href> values.
        #[arg(long)]
        links: bool,
        /// Extract the <title> text.
        #[arg(long)]
        title: bool,
        /// Extract body text.
        #[arg(long)]
        text: bool,
        /// Extract page as markdown.
        #[arg(long)]
        markdown: bool,
        /// Truncate output at N bytes (JSON mode).
        #[arg(long)]
        max_bytes: Option<u64>,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
        /// Timeout in seconds.
        #[arg(long, default_value_t = 30)]
        timeout: u64,
    },

    /// Run a YAML browser automation script.
    Run {
        /// Path to YAML script file or inline YAML.
        script: String,
        /// Timeout in seconds.
        #[arg(long, default_value_t = 60)]
        timeout: u64,
        /// No-op (run always outputs JSON).
        #[arg(long, hide = true)]
        json: bool,
    },

    /// Start interactive session (stdin/stdout JSON REPL).
    Session {
        /// Allow navigation to private/internal IP ranges (disables SSRF filter).
        /// Use for local development only.
        #[arg(long)]
        allow_private_ips: bool,
    },

    /// Start CDP server for Puppeteer/Playwright.
    ///
    /// With `--mcp`, run as a stdio MCP server (newline-delimited JSON-RPC
    /// 2.0) instead of a CDP listener; host/port/auth-token are ignored.
    Serve {
        /// Host to bind to.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Port to listen on.
        #[arg(long, default_value_t = 9222)]
        port: u16,
        /// Cookie persistence file.
        #[arg(long)]
        cookie_file: Option<String>,
        /// Allow navigation to private/internal IP ranges (disables SSRF filter).
        /// Use for local development only.
        #[arg(long)]
        allow_private_ips: bool,
        /// HTTP/HTTPS/SOCKS proxy for all requests (e.g. http://host:port,
        /// socks5://host:port, socks5h://host:port).
        #[arg(long)]
        proxy: Option<String>,
        /// Authentication token for WebSocket connections.
        /// Required when binding to non-loopback addresses (0.0.0.0).
        /// Clients connect with ws://host:port/ws?token=<TOKEN>.
        #[arg(long)]
        auth_token: Option<String>,
        /// Serve as a stdio MCP server (JSON-RPC 2.0 on stdin/stdout)
        /// instead of a CDP listener.
        #[arg(long)]
        mcp: bool,
        /// Bind to one or more accounts (comma-separated): each gets a
        /// context with its stored envelope restored (credential mode on);
        /// the first account becomes the primary context. Enables the
        /// OXI.account* surface (login windows, viewer role).
        #[arg(long, value_name = "IDS")]
        account: Option<String>,
    },

    /// Print CLI schema as JSON (for agents).
    Describe {
        /// Specific command to describe.
        command: Option<String>,
        /// Minimal output (~200 tokens).
        #[arg(long)]
        compact: bool,
        /// No-op (describe always outputs JSON).
        #[arg(long, hide = true)]
        json: bool,
    },

    /// Print agent skill guide.
    Skill {
        /// Output as JSON.
        #[arg(long, hide = true)]
        json: bool,
    },

    /// Search the web or GitHub (lightweight HTTP, no browser needed).
    Search {
        /// Search query (all positional args are joined).
        query: Vec<String>,
        /// Search source: web, github, github-issues.
        #[arg(long, default_value = "web", value_parser = clap::builder::PossibleValuesParser::new(["web", "github", "github-issues"]))]
        source: String,
        #[arg(long, default_value = "ddg")]
        engine: String,
        /// Repository for github-issues (owner/repo).
        #[arg(long)]
        repo: Option<String>,
        /// GitHub personal access token (increases rate limit).
        #[arg(long)]
        token: Option<String>,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
        /// Max results (max 30).
        #[arg(long, default_value_t = 10)]
        max_results: u32,
        /// Timeout in seconds.
        #[arg(long, default_value_t = 15)]
        timeout: u64,
    },

    /// Print version information.
    Version {
        /// Output as JSON.
        #[arg(long, hide = true)]
        json: bool,
    },

    /// Account registry and login-state management.
    #[cfg(feature = "browser")]
    Account {
        #[command(subcommand)]
        command: account_cli::AccountCommand,
    },

    /// Keychain-backed agent credentials (values never via argv).
    #[cfg(feature = "browser")]
    Credential {
        #[command(subcommand)]
        command: account_cli::CredentialCommand,
    },
}

// ---------------------------------------------------------------------------
// Output decision: --json → agent, otherwise → human.
// ---------------------------------------------------------------------------

/// Whether to use JSON output. Only true when --json is explicitly set.
fn use_json(explicit_json: bool) -> bool {
    explicit_json
}

// ---------------------------------------------------------------------------
// Entry
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();

    // Audit log: default path unless `--audit PATH`; `--no-audit` disables.
    oxibrowser_core::security::audit::init(if cli.no_audit {
        None
    } else {
        Some(cli.audit.clone())
    });

    // Organization-specific redaction headers (global `--redact-header`).
    if !cli.redact_headers.is_empty() {
        oxibrowser_core::security::redact::set_extra_sensitive_headers(cli.redact_headers.clone());
    }

    #[cfg(feature = "browser")]
    let audit_ctx = account_cli::AuditContext {
        path: effective_audit_path(&cli),
    };

    let exit_code = match cli.command {
        Commands::Fetch {
            url,
            format,
            json,
            max_bytes,
            fields,
            summary,
            eval,
            click,
            fill,
            press,
            wait,
            wait_timeout,
            extract,
            all,
            headers,
            har,
            har_raw,
            allow_private_ips,
            telemetry,
            account,
            timeout,
        } => {
            run_fetch(
                &url,
                &format,
                json,
                max_bytes,
                fields.as_deref(),
                summary,
                eval.as_deref(),
                click.as_deref(),
                fill.as_deref(),
                press.as_deref(),
                wait.as_deref(),
                wait_timeout,
                extract.as_deref(),
                all,
                headers,
                har.as_deref(),
                har_raw,
                allow_private_ips,
                telemetry,
                account.as_deref(),
                timeout,
            )
            .await
        }
        Commands::Extract {
            url,
            selector,
            all,
            attrs,
            links,
            title,
            text,
            markdown,
            max_bytes,
            json,
            timeout,
        } => {
            run_extract(
                &url,
                selector.as_deref(),
                all,
                &attrs,
                links,
                title,
                text,
                markdown,
                max_bytes,
                json,
                timeout,
            )
            .await
        }
        Commands::Run {
            script, timeout, ..
        } => run_script(&script, timeout).await,
        Commands::Session { allow_private_ips } => session::run_session(allow_private_ips).await,
        Commands::Serve {
            host,
            port,
            cookie_file,
            allow_private_ips,
            proxy,
            auth_token,
            mcp,
            account,
        } => {
            if mcp {
                mcp::run_mcp_stdio(cookie_file.as_deref(), allow_private_ips, proxy).await
            } else {
                run_serve(
                    &host,
                    port,
                    cookie_file.as_deref(),
                    allow_private_ips,
                    proxy,
                    auth_token,
                    account.as_deref(),
                    audit_ctx.path.clone(),
                )
                .await
            }
        }
        Commands::Search {
            query,
            source,
            engine,
            repo,
            token,
            json,
            max_results,
            timeout,
        } => {
            run_search(
                &query,
                &source,
                &engine,
                repo.as_deref(),
                token.as_deref(),
                json,
                max_results,
                timeout,
            )
            .await
        }
        Commands::Describe {
            command, compact, ..
        } => run_describe(command.as_deref(), compact),
        Commands::Skill { json } => {
            if json {
                let resp =
                    output::CliResponse::success(serde_json::json!({"skill": skill::skill_text()}));
                resp.print_json();
                0
            } else {
                print!("{}", skill::skill_text());
                0
            }
        }
        Commands::Version { json } => {
            if json {
                let resp = output::CliResponse::success(
                    serde_json::json!({"version": env!("CARGO_PKG_VERSION"), "name": "oxibrowser"}),
                );
                resp.print_json();
                0
            } else {
                println!("oxibrowser {}", env!("CARGO_PKG_VERSION"));
                0
            }
        }
        #[cfg(feature = "browser")]
        Commands::Account { command } => account_cli::run_account(command, audit_ctx.clone()).await,
        #[cfg(feature = "browser")]
        Commands::Credential { command } => account_cli::run_credential(command, audit_ctx).await,
    };

    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}

/// Recursively-joined text of the document's `<body>`, used by `extract --text`.
/// Lives here (rather than on `DomSnapshot`) because it is purely a CLI concern.
fn body_text(doc: &oxibrowser_core::js::dom_snapshot::DomSnapshot) -> Option<String> {
    let body_id = doc.body_id?;
    doc.text_content(body_id)
}

/// Resolved audit log path for subcommands needing an owned sink
/// ([`account_cli::AuditContext`]): `--no-audit` → `None`, else `--audit
/// PATH` or the default location.
#[cfg(feature = "browser")]
fn effective_audit_path(cli: &Cli) -> Option<PathBuf> {
    if cli.no_audit {
        None
    } else {
        Some(cli.audit.clone().unwrap_or_else(|| {
            oxibrowser_core::security::audit::default_path()
                .unwrap_or_else(|| PathBuf::from("/dev/null"))
        }))
    }
}

// ---------------------------------------------------------------------------
// Error output — human vs JSON
// ---------------------------------------------------------------------------

/// Print an error and return the exit code.
pub(crate) fn print_error(msg: &str, error_code: &str, json: bool) -> i32 {
    let code = match error_code {
        "INVALID_URL" | "INVALID_SELECTOR" | "INPUT_VALIDATION" | "PATH_TRAVERSAL"
        | "SSRF_BLOCKED" => 2,
        "TIMEOUT" => 3,
        "NETWORK_ERROR" | "HTTP_ERROR" => 4,
        _ => 1,
    };

    if json {
        let resp = output::CliResponse::error(msg, error_code);
        resp.print_json();
    } else {
        eprintln!("Error: {msg}");
    }
    code
}

// ---------------------------------------------------------------------------
// fetch
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn run_fetch(
    url: &str,
    format: &str,
    json: bool,
    max_bytes: Option<u64>,
    fields: Option<&str>,
    summary: bool,
    eval: Option<&str>,
    click: Option<&str>,
    fill: Option<&str>,
    press: Option<&str>,
    wait: Option<&str>,
    wait_timeout: u64,
    extract_sel: Option<&str>,
    all: bool,
    headers: bool,
    har: Option<&Path>,
    har_raw: bool,
    allow_private_ips: bool,
    telemetry: bool,
    account: Option<&str>,
    timeout: u64,
) -> i32 {
    let start = Instant::now();
    let json = use_json(json);

    if let (true, Some(path)) = (har_raw, har) {
        eprintln!("⚠ --har-raw: HAR will contain UNREDACTED cookies, tokens, and POST bodies.");
        audit_sensitive_action("har_raw_export", &format!("path={}", path.display()));
    }

    // Validate
    if let Some(e) = validate_fetch_inputs(url, click, fill, wait, extract_sel, eval) {
        return print_error(
            &e.error.unwrap_or_default(),
            &e.error_code.unwrap_or_default(),
            json,
        );
    }

    let needs_tab =
        click.is_some() || fill.is_some() || press.is_some() || wait.is_some() || eval.is_some();

    let mut config = oxibrowser_core::BrowserConfig::headless();
    if allow_private_ips {
        config.enable_ssrf_filter = false;
        eprintln!("⚠ SSRF filter disabled: private/internal IP ranges accessible.");
    }
    config.telemetry = telemetry;
    FETCH_TELEMETRY.store(telemetry, std::sync::atomic::Ordering::Relaxed);
    let browser = match oxibrowser_core::Browser::new(config).await {
        Ok(b) => b,
        Err(e) => return print_error(&format!("browser init failed: {e}"), "RUNTIME_ERROR", json),
    };
    let browser = Arc::new(browser);

    // `--account`: restore the envelope into a bound context and take the
    // tab path (the only one that can run inside a non-default context).
    let account_ctx = match account {
        Some(id) => match account_cli::bind_account_context(&browser, id).await {
            Ok(ctx) => Some(ctx),
            Err(e) => return print_error(&e, "RUNTIME_ERROR", json),
        },
        None => None,
    };
    let needs_tab = needs_tab || account_ctx.is_some();

    let result = if needs_tab {
        fetch_with_tab(
            start,
            &browser,
            account_ctx.as_ref(),
            url,
            format,
            json,
            max_bytes,
            fields,
            summary,
            eval,
            click,
            fill,
            press,
            wait,
            wait_timeout,
            extract_sel,
            all,
            headers,
            har,
            har_raw,
            timeout,
        )
        .await
    } else {
        fetch_direct(
            start,
            &browser,
            url,
            format,
            json,
            max_bytes,
            fields,
            summary,
            extract_sel,
            all,
            headers,
            har,
            har_raw,
        )
        .await
    };

    browser.close().await.ok();

    // Telemetry (#15): report the recorded API gaps on exit. Note the
    // default log filter is `warn` — set RUST_LOG=info (or more specific)
    // to see this on stderr.
    if telemetry {
        let gaps = oxibrowser_core::js::runtime::telemetry_snapshot()
            .into_iter()
            .take(20)
            .collect::<Vec<_>>();
        info!(gaps = ?gaps, "web api gaps (top 20 by access count)");
    }

    match result {
        Ok(()) => 0,
        Err(FetchError { msg, code }) => print_error(&msg, &code, json),
    }
}

struct FetchError {
    msg: String,
    code: String,
}

/// Record a CLI-sensitive action on the audit log. No-op when audit is
/// disabled/uninitialized.
fn audit_sensitive_action(action: &str, reason: &str) {
    use oxibrowser_core::security::audit::{self, AuditDecision, AuditEvent, AuditEventKind};
    audit::record(AuditEvent {
        action: Some(action.to_string()),
        ..audit::event(
            AuditEventKind::SensitiveAction,
            AuditDecision::Allow,
            reason,
        )
    });
}

/// Write the network log as pretty HAR JSON to the `--har` path.
///
/// Returns `(path, entries)` for output metadata, or `None` when `--har` was
/// not given.
fn write_har(path: Option<&Path>, har: Value) -> Result<Option<(String, usize)>, FetchError> {
    let Some(path) = path else {
        return Ok(None);
    };
    let entries = har["log"]["entries"].as_array().map_or(0, |a| a.len());
    let body = serde_json::to_string_pretty(&har).map_err(|e| FetchError {
        msg: format!("HAR serialization failed: {e}"),
        code: "RUNTIME_ERROR".into(),
    })?;
    std::fs::write(path, body).map_err(|e| FetchError {
        msg: format!("failed to write HAR file {}: {e}", path.display()),
        code: "RUNTIME_ERROR".into(),
    })?;
    Ok(Some((path.display().to_string(), entries)))
}

/// Whether the current fetch run enabled Web API gap telemetry
/// (`fetch --telemetry`). Read by `print_json_with_har` to inject the
/// gap snapshot into `meta.api_gaps` for `--json` output.
static FETCH_TELEMETRY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Print a success response, injecting `har_path`/`entries` into `meta` when
/// `--har` was given.
fn print_json_with_har(resp: output::CliResponse, har_meta: Option<&(String, usize)>) {
    let mut value = serde_json::to_value(&resp).unwrap_or_else(|e| {
        serde_json::json!({"ok": false, "error": format!("serialization: {e}"), "error_code": "INTERNAL"})
    });
    if let (Some(meta), Some((path, entries))) = (value.get_mut("meta"), har_meta) {
        meta["har_path"] = Value::String(path.clone());
        meta["entries"] = serde_json::json!(entries);
    }
    // Telemetry (#15): with `--telemetry`, add the recorded API-gap snapshot
    // (count-descending) to `meta`. Read-only — counters are not reset.
    if FETCH_TELEMETRY.load(std::sync::atomic::Ordering::Relaxed) {
        let gaps: Vec<Value> = oxibrowser_core::js::runtime::telemetry_snapshot()
            .into_iter()
            .map(|(name, count)| serde_json::json!({ "name": name, "count": count }))
            .collect();
        if !gaps.is_empty() {
            let meta = match value.get_mut("meta").and_then(|m| m.as_object_mut()) {
                Some(meta) => meta,
                None => {
                    // CliResponse serializes `meta` only when present.
                    if let Some(obj) = value.as_object_mut() {
                        obj.insert("meta".to_string(), serde_json::json!({}));
                    }
                    value
                        .get_mut("meta")
                        .and_then(|m| m.as_object_mut())
                        .expect("meta just inserted")
                }
            };
            meta.insert("api_gaps".to_string(), Value::Array(gaps));
        }
    }
    println!("{}", serde_json::to_string(&value).unwrap_or_default());
}

impl From<oxibrowser_core::error::CoreError> for FetchError {
    fn from(e: oxibrowser_core::error::CoreError) -> Self {
        FetchError {
            msg: format!("{e}"),
            code: output::core_error_code(&e).to_string(),
        }
    }
}

/// Direct fetch: no interaction needed.
#[allow(clippy::too_many_arguments)]
async fn fetch_direct(
    start: Instant,
    browser: &oxibrowser_core::Browser,
    url: &str,
    format: &str,
    json: bool,
    max_bytes: Option<u64>,
    fields: Option<&str>,
    summary: bool,
    extract_sel: Option<&str>,
    all: bool,
    headers: bool,
    har: Option<&Path>,
    har_raw: bool,
) -> Result<(), FetchError> {
    let session = browser.new_page(url).await.map_err(FetchError::from)?;
    let guard = session.read().await;
    let page = guard.page().ok_or_else(|| FetchError {
        msg: "no page loaded".into(),
        code: "PAGE_NOT_LOADED".into(),
    })?;

    let har_meta = if har_raw {
        write_har(har, guard.network_log_har_raw())?
    } else {
        write_har(har, guard.network_log_har())?
    };
    if let Some((path, entries)) = &har_meta {
        info!("HAR written to {path} ({entries} entries)");
    }

    if headers {
        eprintln!("HTTP {}", page.status());
        eprintln!("Content-Type: {}", page.content_type());
    }

    // Summary — always JSON (structured metadata)
    if summary {
        let data = output::build_summary(page);
        if json {
            let resp = output::CliResponse::success_with_meta(
                data,
                None,
                start.elapsed().as_millis() as u64,
            );
            print_json_with_har(resp, har_meta.as_ref());
        } else {
            // Human: print summary as key-value
            let obj = data.as_object().unwrap();
            if let Some(v) = obj.get("url").and_then(|v| v.as_str()) {
                eprintln!("URL: {v}");
            }
            if let Some(v) = obj.get("title").and_then(|v| v.as_str()) {
                eprintln!("Title: {v}");
            }
            eprintln!("Status: {}", obj.get("status").unwrap());
            if let Some(h) = obj.get("headings").and_then(|v| v.as_array()) {
                eprintln!("Headings: {}", h.len());
                for h in h {
                    if let Some(s) = h.as_str() {
                        eprintln!("  - {s}");
                    }
                }
            }
            eprintln!("Links: {}", obj.get("links_count").unwrap());
            eprintln!("Forms: {}", obj.get("forms_count").unwrap());
            eprintln!("Images: {}", obj.get("images_count").unwrap());
            eprintln!("Text length: {}", obj.get("text_length").unwrap());
        }
        return Ok(());
    }

    // Extract — human gets text, agent gets JSON
    if let Some(sel) = extract_sel {
        let doc = page.root_frame().document();
        if all {
            let texts: Vec<String> = doc
                .query_selector_all(sel)
                .iter()
                .filter_map(|id| doc.text_content(*id))
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect();
            if json {
                let resp = output::CliResponse::success(serde_json::json!({
                    "selector": sel, "count": texts.len(), "items": texts
                }));
                print_json_with_har(resp, har_meta.as_ref());
            } else {
                for t in &texts {
                    println!("{t}");
                }
            }
        } else {
            let text = doc
                .query_selector(sel)
                .and_then(|id| doc.text_content(id))
                .map(|t| t.trim().to_string())
                .unwrap_or_default();
            if json {
                let resp = output::CliResponse::success(serde_json::json!({
                    "selector": sel, "match": text
                }));
                print_json_with_har(resp, har_meta.as_ref());
            } else {
                println!("{text}");
            }
        }
        return Ok(());
    }

    // Full content
    let body = match format {
        "markdown" | "md" => page.to_markdown(),
        "text" => {
            // textContent has no line breaks (no CSS layout).
            // Use markdown → strip formatting for readable plain text.
            let md = page.to_markdown();
            // Strip markdown syntax: # headings, **bold**, [links](url), etc.

            md.lines()
                .map(|line| {
                    let l = line.trim();
                    // Strip heading markers
                    let l = l.strip_prefix('#').map(|s| s.trim()).unwrap_or(l);
                    let l = l.strip_prefix('#').map(|s| s.trim()).unwrap_or(l);
                    // Strip bold/italic markers
                    let l = l.replace("**", "").replace("__", "");
                    let l = l.replace("* ", "");
                    regex_strip_link(&l)
                })
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => page.content().to_string(),
    };

    if json {
        let mut data = serde_json::json!({
            "url": page.url().to_string(),
            "title": page.title().unwrap_or("").to_string(),
            "status": page.status(),
            "content_type": page.content_type().to_string(),
        });
        let key = match format {
            "markdown" | "md" => "markdown",
            "text" => "text",
            _ => "html",
        };
        data.as_object_mut()
            .unwrap()
            .insert(key.into(), Value::String(body));
        if let Some(mb) = max_bytes {
            output::truncate_fields(&mut data, mb);
        }
        if let Some(f) = fields {
            output::filter_fields(&mut data, &output::parse_fields(f));
        }
        let resp =
            output::CliResponse::success_with_meta(data, None, start.elapsed().as_millis() as u64);
        print_json_with_har(resp, har_meta.as_ref());
    } else {
        print!("{body}");
    }
    Ok(())
}

/// Tab-based fetch: for interaction and JS eval.
#[allow(clippy::too_many_arguments)]
async fn fetch_with_tab(
    start: Instant,
    browser: &oxibrowser_core::Browser,
    account_ctx: Option<&Arc<oxibrowser_core::context::BrowserContext>>,
    url: &str,
    format: &str,
    json: bool,
    max_bytes: Option<u64>,
    fields: Option<&str>,
    summary: bool,
    eval: Option<&str>,
    click: Option<&str>,
    fill: Option<&str>,
    press: Option<&str>,
    wait: Option<&str>,
    wait_timeout: u64,
    extract_sel: Option<&str>,
    all: bool,
    headers: bool,
    har: Option<&Path>,
    har_raw: bool,
    timeout: u64,
) -> Result<(), FetchError> {
    let tab = match account_ctx {
        Some(ctx) => browser.new_tab_in(ctx).await.map_err(FetchError::from)?,
        None => browser.new_tab().await.map_err(FetchError::from)?,
    };

    let nav_result = tokio::time::timeout(Duration::from_secs(timeout), tab.goto(url)).await;
    match nav_result {
        Ok(Ok(nav)) => {
            if headers {
                eprintln!("HTTP {}", nav.status);
                eprintln!("URL: {}", nav.url);
                eprintln!("Title: {}", nav.title);
            }
        }
        Ok(Err(e)) => return Err(FetchError::from(e)),
        Err(_) => {
            return Err(FetchError {
                msg: format!("timed out after {timeout}s"),
                code: "TIMEOUT".into(),
            });
        }
    }

    // Interaction: wait → fill → click → press
    if let Some(sel) = wait {
        tab.wait_for(sel, wait_timeout)
            .await
            .map_err(FetchError::from)?;
    }
    if let Some(spec) = fill {
        let (sel, val) = spec.split_once(':').ok_or_else(|| FetchError {
            msg: "--fill must be selector:value".into(),
            code: "INPUT_VALIDATION".into(),
        })?;
        tab.fill(sel, val).await.map_err(FetchError::from)?;
    }
    if let Some(sel) = click {
        tab.click(sel).await.map_err(FetchError::from)?;
    }
    if let Some(keys) = press {
        tab.press(keys).await.map_err(FetchError::from)?;
    }

    let har_meta = if har_raw {
        write_har(
            har,
            tab.network_log_har_raw_json()
                .await
                .map_err(FetchError::from)?,
        )?
    } else {
        write_har(
            har,
            tab.network_log_har_json().await.map_err(FetchError::from)?,
        )?
    };
    if let Some((path, entries)) = &har_meta {
        info!("HAR written to {path} ({entries} entries)");
    }

    // Eval
    if let Some(expr) = eval {
        let value = tab.evaluate(expr).await.map_err(FetchError::from)?;
        if json {
            print_json_with_har(
                output::CliResponse::success(serde_json::json!({"value": value})),
                har_meta.as_ref(),
            );
        } else {
            match value {
                Value::String(s) => println!("{s}"),
                Value::Null => {}
                other => println!("{other}"),
            }
        }
        return Ok(());
    }

    // Summary
    if summary {
        let content = tab.content().await.map_err(FetchError::from)?;
        let data = serde_json::json!({
            "url": content.url, "title": content.title,
            "status": content.status, "text_length": content.markdown.len(),
        });
        if json {
            print_json_with_har(
                output::CliResponse::success_with_meta(
                    data,
                    None,
                    start.elapsed().as_millis() as u64,
                ),
                har_meta.as_ref(),
            );
        } else {
            eprintln!("URL: {}", content.url);
            eprintln!("Title: {}", content.title);
            eprintln!("Status: {}", content.status);
            eprintln!("Text length: {}", content.markdown.len());
        }
        return Ok(());
    }

    // Extract
    if let Some(sel) = extract_sel {
        let matches = tab.query_all(sel).await.map_err(FetchError::from)?;
        if all {
            let items: Vec<String> = matches
                .into_iter()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect();
            if json {
                print_json_with_har(
                    output::CliResponse::success(serde_json::json!({
                        "selector": sel, "count": items.len(), "items": items
                    })),
                    har_meta.as_ref(),
                );
            } else {
                for t in &items {
                    println!("{t}");
                }
            }
        } else {
            let text = matches
                .first()
                .map(|t| t.trim().to_string())
                .unwrap_or_default();
            if json {
                output::CliResponse::success(serde_json::json!({
                    "selector": sel, "match": text
                }))
                .print_json();
            } else {
                println!("{text}");
            }
        }
        return Ok(());
    }

    // Full content
    let content = tab.content().await.map_err(FetchError::from)?;
    if json {
        let mut data = match format {
            "markdown" | "md" => serde_json::json!({
                "url": content.url, "title": content.title,
                "status": content.status, "markdown": content.markdown,
            }),
            "text" => {
                let body = content
                    .markdown
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                serde_json::json!({
                    "url": content.url, "title": content.title,
                    "status": content.status, "text": body,
                })
            }
            _ => serde_json::json!({
                "url": content.url, "title": content.title,
                "status": content.status, "html": content.html,
            }),
        };
        if let Some(mb) = max_bytes {
            output::truncate_fields(&mut data, mb);
        }
        if let Some(f) = fields {
            output::filter_fields(&mut data, &output::parse_fields(f));
        }
        print_json_with_har(
            output::CliResponse::success_with_meta(data, None, start.elapsed().as_millis() as u64),
            har_meta.as_ref(),
        );
    } else {
        match format {
            "html" => print!("{}", content.html),
            "text" => {
                let body = content
                    .markdown
                    .lines()
                    .map(|l| l.trim())
                    .filter(|l| !l.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n");
                println!("{body}");
            }
            _ => print!("{}", content.markdown),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// extract
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn run_extract(
    url: &str,
    selector: Option<&str>,
    all: bool,
    attrs: &str,
    links: bool,
    title: bool,
    text: bool,
    markdown: bool,
    max_bytes: Option<u64>,
    json: bool,
    timeout: u64,
) -> i32 {
    let json = use_json(json);
    let start = Instant::now();

    // Validate
    if let Err(e) = validate::validate_url(url) {
        return print_error(&e.to_string(), e.error_code(), json);
    }
    if let Some(sel) = selector
        && let Err(e) = validate::validate_selector(sel)
    {
        return print_error(&e.to_string(), e.error_code(), json);
    }

    let requested_attrs: Vec<&str> = output::parse_fields(attrs);

    let config = oxibrowser_core::BrowserConfig::headless();
    let browser = match oxibrowser_core::Browser::new(config).await {
        Ok(b) => b,
        Err(e) => return print_error(&format!("browser init failed: {e}"), "RUNTIME_ERROR", json),
    };

    let session_result =
        tokio::time::timeout(Duration::from_secs(timeout), browser.new_page(url)).await;

    let session = match session_result {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            browser.close().await.ok();
            return print_error(&format!("{e}"), output::core_error_code(&e), json);
        }
        Err(_) => {
            browser.close().await.ok();
            return print_error(&format!("timed out after {timeout}s"), "TIMEOUT", json);
        }
    };

    let guard = session.read().await;
    let page = match guard.page() {
        Some(p) => p,
        None => {
            browser.close().await.ok();
            return print_error("no page loaded", "PAGE_NOT_LOADED", json);
        }
    };
    let mut json_map = serde_json::Map::new();

    let doc = page.root_frame().document();

    if links {
        let hrefs: Vec<Value> = doc
            .query_selector_all("a[href]")
            .iter()
            .filter_map(|id| {
                doc.nodes
                    .get(id)
                    .and_then(|n| n.attributes.get("href").cloned())
                    .map(Value::String)
            })
            .collect();
        json_map.insert("links".into(), Value::Array(hrefs));
    }
    if text {
        json_map.insert(
            "text".into(),
            Value::String(body_text(doc).unwrap_or_default()),
        );
    }
    if markdown {
        json_map.insert("markdown".into(), Value::String(page.to_markdown()));
    }

    if let Some(sel) = selector {
        let ids = doc.query_selector_all(sel);
        if all {
            let items: Vec<Value> = ids
                .iter()
                .filter_map(|id| {
                    let mut item = serde_json::Map::new();
                    for &attr in &requested_attrs {
                        let val = if attr == "text" {
                            doc.text_content(*id)
                                .map(|t| t.trim().to_string())
                                .unwrap_or_default()
                        } else {
                            doc.nodes
                                .get(id)
                                .and_then(|n| n.attributes.get(attr).cloned())
                                .unwrap_or_default()
                        };
                        item.insert(attr.into(), Value::String(val));
                    }
                    if !item.is_empty() {
                        Some(Value::Object(item))
                    } else {
                        None
                    }
                })
                .collect();
            json_map.insert("selector".into(), Value::String(sel.into()));
            json_map.insert(
                "count".into(),
                Value::Number(serde_json::Number::from(items.len())),
            );
            json_map.insert("items".into(), Value::Array(items));
        } else {
            let mut item = serde_json::Map::new();
            if let Some(id) = ids.first() {
                for &attr in &requested_attrs {
                    let val = if attr == "text" {
                        doc.text_content(*id)
                            .map(|t| t.trim().to_string())
                            .unwrap_or_default()
                    } else {
                        doc.nodes
                            .get(id)
                            .and_then(|n| n.attributes.get(attr).cloned())
                            .unwrap_or_default()
                    };
                    item.insert(attr.into(), Value::String(val));
                }
            }
            json_map.insert("selector".into(), Value::String(sel.into()));
            json_map.insert("match".into(), Value::Object(item));
        }
    }

    // Default: title + text
    if !title && !links && !text && !markdown && selector.is_none() {
        json_map.insert(
            "title".into(),
            Value::String(page.title().unwrap_or("").to_string()),
        );
        json_map.insert(
            "text".into(),
            Value::String(body_text(doc).unwrap_or_default()),
        );
    }

    drop(guard);
    browser.close().await.ok();

    let mut data = Value::Object(json_map);
    if let Some(mb) = max_bytes {
        output::truncate_fields(&mut data, mb);
    }

    if json {
        output::CliResponse::success_with_meta(data, None, start.elapsed().as_millis() as u64)
            .print_json();
    } else {
        print_extract_human(&data);
    }
    0
}

/// Print extract data in human-friendly format.
fn print_extract_human(data: &Value) {
    let obj = match data.as_object() {
        Some(o) => o,
        None => {
            println!("{data}");
            return;
        }
    };

    // Title
    if let Some(title) = obj.get("title").and_then(|v| v.as_str())
        && !title.is_empty()
    {
        println!("Title: {title}");
    }
    // Blank line after title for visual separation
    if obj.contains_key("title")
        && (obj.contains_key("text") || obj.contains_key("items") || obj.contains_key("links"))
    {
        println!();
    }
    // Links: one per line
    if let Some(links) = obj.get("links").and_then(|v| v.as_array()) {
        for link in links {
            if let Some(s) = link.as_str() {
                println!("{s}");
            }
        }
    }
    // Selector items
    if let Some(items) = obj.get("items").and_then(|v| v.as_array()) {
        for item in items {
            if let Some(s) = item.as_str() {
                println!("{s}");
            } else {
                let vals: Vec<&str> = item
                    .as_object()
                    .map(|o| o.values().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();
                println!("{}", vals.join("\t"));
            }
        }
    }
    // Single match
    if let Some(m) = obj.get("match") {
        if let Some(s) = m.as_str() {
            println!("{s}");
        } else {
            let vals: Vec<&str> = m
                .as_object()
                .map(|o| o.values().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            println!("{}", vals.join("\t"));
        }
    }
    // Body text
    if let Some(text) = obj.get("text").and_then(|v| v.as_str())
        && !text.is_empty()
    {
        for line in text.lines() {
            println!("{line}");
        }
    }
    // Markdown
    if let Some(md) = obj.get("markdown").and_then(|v| v.as_str())
        && !md.is_empty()
    {
        print!("{md}");
    }
}

// ---------------------------------------------------------------------------
// run (YAML script)
// ---------------------------------------------------------------------------

async fn run_script(script_path_or_yaml: &str, timeout: u64) -> i32 {
    let script_config = if std::path::Path::new(script_path_or_yaml).exists() {
        match std::fs::read_to_string(script_path_or_yaml) {
            Ok(content) => match oxibrowser_core::script::parse_script(&content) {
                Ok(cfg) => cfg,
                Err(e) => {
                    eprintln!("Error: parse error: {e}");
                    return 1;
                }
            },
            Err(e) => {
                eprintln!("Error: cannot read script: {e}");
                return 1;
            }
        }
    } else {
        match oxibrowser_core::script::parse_script(script_path_or_yaml) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("Error: parse error: {e}");
                return 1;
            }
        }
    };

    let browser_config = oxibrowser_core::BrowserConfig::headless();
    let browser = match oxibrowser_core::Browser::new(browser_config).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Error: browser init failed: {e}");
            return 1;
        }
    };

    let tab = match browser.new_tab().await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Error: tab creation failed: {e}");
            return 1;
        }
    };

    let mut runner = oxibrowser_core::script::ScriptRunner::new(&tab);
    let script_result = match tokio::time::timeout(
        Duration::from_secs(timeout),
        runner.run_config(&script_config),
    )
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            browser.close().await.ok();
            eprintln!("Error: {e}");
            return 1;
        }
        Err(_) => {
            browser.close().await.ok();
            eprintln!("Error: timed out after {timeout}s");
            return 3;
        }
    };
    browser.close().await.ok();
    let elapsed = script_result.duration_ms;
    let data = serde_json::to_value(&script_result).unwrap_or_default();
    output::CliResponse::success_with_meta(data, None, elapsed).print_json();
    0
}

// ---------------------------------------------------------------------------
// session — see session/mod.rs
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// serve (CDP server)
// ---------------------------------------------------------------------------
async fn run_serve(
    host: &str,
    port: u16,
    cookie_file: Option<&str>,
    allow_private_ips: bool,
    proxy: Option<String>,
    auth_token: Option<String>,
    accounts: Option<&str>,
    audit_path: Option<std::path::PathBuf>,
) -> i32 {
    let addr: SocketAddr = match format!("{host}:{port}").parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Error: invalid address: {e}");
            return 2;
        }
    };

    // Warn on non-loopback bind without auth token
    if !addr.ip().is_loopback() && auth_token.is_none() {
        eprintln!("⚠ WARNING: binding to non-loopback address {addr} without --auth-token.");
        eprintln!("  Any network client can control the browser. Use --auth-token <TOKEN>.");
    }

    info!(addr = %addr, "starting CDP server");

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

    let browser = match oxibrowser_core::Browser::new(config).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Error: browser init failed: {e}");
            return 1;
        }
    };
    let browser = Arc::new(browser);

    let mut cdp_server = oxibrowser_cdp::CdpServer::new(addr, browser.clone());
    if let Some(token) = &auth_token {
        cdp_server = cdp_server.with_auth(token.clone());
    }

    // `--account id[,id…]`: one context per account with its envelope
    // restored (credential mode on); the first is the primary context. The
    // login surface enables OXI.account* + viewer-role upgrades (§5.2).
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
        let registry = match account_cli::oxi_accounts_registry() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Error: {e}");
                return 1;
            }
        };
        let (_manager, _login_browser, orch) =
            match account_cli::login_stack(&registry, allow_private_ips).await {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("Error: {e}");
                    return 1;
                }
            };
        let surface = oxibrowser_cdp::LoginSurface::new(orch, browser.clone());
        let mut primary = None;
        for id in ids {
            match account_cli::bind_account_context(&browser, id).await {
                Ok(ctx) => {
                    surface.bind(id, Arc::clone(&ctx));
                    if primary.is_none() {
                        primary = Some(ctx);
                    }
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    return 1;
                }
            }
        }
        cdp_server = cdp_server.with_login(surface);
        if let Some(ctx) = primary {
            cdp_server = cdp_server.with_primary_context(ctx);
        }
    }

    // Credential broker for account-bound serving: keychain provider +
    // policy engine over the shared consent store. Fail-closed surface —
    // when the keystore is unavailable the OXI credential/account commands
    // answer `credentialsUnavailable` instead of half-working.
    {
        let broker = match account_cli::serve_credential_broker(account_cli::AuditContext {
            path: audit_path.clone(),
        }) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("Warning: credential broker unavailable — {e}");
                None
            }
        };
        if let Some(b) = broker {
            cdp_server = cdp_server.with_credentials_broker(b);
        }
    }

    let server = Arc::new(cdp_server);
    let bound_addr = match server.start().await {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Error: server bind failed: {e}");
            return 4;
        }
    };

    info!(addr = %bound_addr, "CDP server ready");
    println!("OxiBrowser CDP server listening on {bound_addr}");
    println!("  DevTools: http://{bound_addr}/json/version");
    println!("  WebSocket: ws://{bound_addr}/ws");

    tokio::signal::ctrl_c().await.ok();
    info!("shutting down");

    server.shutdown();
    browser.close().await.ok();
    0
}

// ---------------------------------------------------------------------------
// search
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn run_search(
    query_parts: &[String],
    source: &str,
    engine: &str,
    repo: Option<&str>,
    token: Option<&str>,
    json: bool,
    max_results: u32,
    timeout: u64,
) -> i32 {
    let start = std::time::Instant::now();
    let json = crate::output::should_output_json(json);

    let query = query_parts.join(" ");
    let query = query.trim();
    if query.is_empty() {
        return print_error("empty search query", "INPUT_VALIDATION", json);
    }

    let result = search::dispatch(
        query,
        source,
        engine,
        repo,
        token,
        max_results as usize,
        timeout,
    )
    .await;

    match result {
        Ok(output) => {
            let elapsed = start.elapsed().as_millis() as u64;
            if json {
                let data = serde_json::to_value(&output).unwrap_or_default();
                let resp = output::CliResponse::success_with_search_meta(
                    data,
                    elapsed,
                    &output.source,
                    &output.engine,
                );
                resp.print_json()
            } else {
                search::format_human(&output);
                0
            }
        }
        Err(e) => print_error(&e.to_string(), "SEARCH_ERROR", json),
    }
}

// ---------------------------------------------------------------------------
// describe
// ---------------------------------------------------------------------------

fn run_describe(command: Option<&str>, compact: bool) -> i32 {
    // describe is agent-only — always JSON
    let response = match command {
        Some(cmd) => describe::describe_command(cmd),
        None => describe::describe_all(compact),
    };
    response.print_json()
}

// ---------------------------------------------------------------------------
// Text formatting helpers
// ---------------------------------------------------------------------------

/// Strip markdown link syntax: [text](url) → text
fn regex_strip_link(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut result = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            // Find matching ](
            if let Some(close) = bytes[i..].iter().position(|&b| b == b']') {
                let close_idx = i + close;
                if close_idx + 1 < bytes.len() && bytes[close_idx + 1] == b'(' {
                    // Find closing )
                    if let Some(paren) = bytes[close_idx + 2..].iter().position(|&b| b == b')') {
                        // Extract text between [ and ]
                        let text = &s[i + 1..close_idx];
                        result.push_str(text);
                        i = close_idx + 2 + paren + 1;
                        continue;
                    }
                }
            }
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

// ---------------------------------------------------------------------------
// Validation helper
// ---------------------------------------------------------------------------

fn validate_fetch_inputs(
    url: &str,
    click: Option<&str>,
    fill: Option<&str>,
    wait: Option<&str>,
    extract: Option<&str>,
    eval: Option<&str>,
) -> Option<output::CliResponse> {
    if let Err(e) = validate::validate_url(url) {
        return Some(output::CliResponse::from_validation(e));
    }
    if let Some(sel) = click
        && let Err(e) = validate::validate_selector(sel)
    {
        return Some(output::CliResponse::from_validation(e));
    }
    if let Some(spec) = fill
        && !spec.contains(':')
    {
        return Some(output::CliResponse::error(
            "--fill must be in the format selector:value",
            "INPUT_VALIDATION",
        ));
    }
    if let Some(sel) = wait
        && let Err(e) = validate::validate_selector(sel)
    {
        return Some(output::CliResponse::from_validation(e));
    }
    if let Some(sel) = extract
        && let Err(e) = validate::validate_selector(sel)
    {
        return Some(output::CliResponse::from_validation(e));
    }
    if let Some(expr) = eval
        && let Err(e) = validate::validate_expression(expr)
    {
        return Some(output::CliResponse::from_validation(e));
    }
    None
}
