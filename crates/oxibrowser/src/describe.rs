//! Schema introspection for agent consumption.
//!
//! `oxibrowser describe` outputs machine-readable JSON describing every
//! command, its arguments, flags, and output schema. Agents call this
//! instead of parsing `--help` text.

use crate::output::CliResponse;

/// Build the full describe response.
pub fn describe_all(compact: bool) -> CliResponse {
    if compact {
        CliResponse::success(serde_json::json!({
            "fetch": {
                "args": ["url"],
                "flags": ["format","json","max-bytes","fields","summary","eval","click","fill","press","wait","wait-timeout","extract","all","headers","timeout","har","telemetry","allow-private-ips"]
            },
            "extract": {
                "args": ["url"],
                "flags": ["selector","all","attrs","links","title","text","markdown","max-bytes","json","timeout"]
            },
            "run": {
                "args": ["script"]
            },
            "session": {
                "commands": ["new","goto","back","forward","reload","click","fill","press","type","select","check","uncheck","scroll","eval","extract","content","screenshot","wait","save-state","load-state","close","list","help","exit"]
            },
            "serve": {
                "flags": ["host","port","cookie-file","allow-private-ips","proxy","auth-token","mcp","account","ref","as-agent"]
            },
            "describe": {
                "args": ["command?"],
                "flags": ["compact","json"]
            },
            "search": {
                "args": ["query"],
                "flags": ["source","engine","repo","token","json","max-results","timeout"]
            },
            "account": {
                "subcommands": ["add","list","status","rm"],
                "add_flags": ["site","id","login","display","json"],
                "status_flags": ["probe","json"]
            },
            "credential": {
                "subcommands": ["put","get","list","rm","totp","onboard"],
                "put_flags": ["agent","site","kind","slug","login","origin","stdin","prompt"],
                "get_flags": ["id","field"],
                "note": "values never via argv; get has no --json (raw secret on stdout)"
            },
            "skill": {}
        }))
    } else {
        CliResponse::success(serde_json::json!({
            "name": "oxibrowser",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Headless browser for AI agents — single static binary, no Chromium",
            "commands": {
                "fetch": {
                    "description": "Fetch a URL and return content in one shot. Supports interaction (click, fill, wait) before output.",
                    "usage": "oxibrowser fetch <url> [flags]",
                    "args": [
                        {"name": "url", "type": "string", "format": "uri", "required": true}
                    ],
                    "flags": {
                        "format": {"type": "enum", "values": ["html", "markdown", "text"], "default": "markdown", "description": "Output format (markdown by default)"},
                        "json": {"type": "bool", "default": false, "description": "Output as JSON (automatic when piped)"},
                        "max-bytes": {"type": "int", "description": "Truncate output at N bytes"},
                        "fields": {"type": "string", "description": "Comma-separated fields: url,title,status,markdown,html,text,content_type"},
                        "summary": {"type": "bool", "description": "Page metadata only (headings, links_count, forms_count, text_length)"},
                        "eval": {"type": "string", "description": "Evaluate JS expression after page load"},
                        "click": {"type": "string", "description": "Click element matching CSS selector"},
                        "fill": {"type": "string", "format": "selector:value", "description": "Fill input (e.g., 'input[name=q]:hello')"},
                        "press": {"type": "string", "description": "Press key (Enter, Tab, Ctrl+C, etc.)"},
                        "wait": {"type": "string", "description": "Wait for CSS selector before output"},
                        "wait-timeout": {"type": "int", "default": 5000, "unit": "ms"},
                        "extract": {"type": "string", "description": "Extract text from CSS selector instead of full content"},
                        "all": {"type": "bool", "description": "With --extract: return all matches"},
                        "headers": {"type": "bool", "description": "Print HTTP headers to stderr"},
                        "timeout": {"type": "int", "default": 30, "unit": "seconds"},
                        "har": {"type": "string", "description": "Write a HAR 1.2 file of all recorded requests to this path"},
                        "telemetry": {"type": "bool", "description": "Record Web APIs the page uses but oxibrowser lacks; reported in meta.api_gaps"},
                        "allow-private-ips": {"type": "bool", "description": "Disable the SSRF filter (needed for localhost targets)"}
                    },
                    "output_schema": {
                        "type": "object",
                        "properties": {
                            "url": {"type": "string"},
                            "title": {"type": "string"},
                            "status": {"type": "integer"},
                            "content_type": {"type": "string"},
                            "markdown": {"type": "string"},
                            "html": {"type": "string"},
                            "text": {"type": "string"},
                            "truncated": {"type": "bool"},
                            "total_bytes": {"type": "integer"},
                            "returned_bytes": {"type": "integer"}
                        }
                    }
                },
                "extract": {
                    "description": "Extract structured data from a URL in one shot",
                    "usage": "oxibrowser extract <url> [flags]",
                    "args": [
                        {"name": "url", "type": "string", "format": "uri", "required": true}
                    ],
                    "flags": {
                        "selector": {"type": "string", "description": "CSS selector to match elements"},
                        "all": {"type": "bool", "description": "Return all matches (not just first)"},
                        "attrs": {"type": "string", "default": "text", "description": "Comma-separated attributes: text,href,data-*,src,..."},
                        "links": {"type": "bool", "description": "Extract all <a href> values"},
                        "title": {"type": "bool", "description": "Extract <title> text"},
                        "text": {"type": "bool", "description": "Extract body text"},
                        "markdown": {"type": "bool", "description": "Extract page as markdown"},
                        "max-bytes": {"type": "int", "description": "Truncate output at N bytes"},
                        "json": {"type": "bool", "default": false},
                        "timeout": {"type": "int", "default": 30, "unit": "seconds"}
                    }
                },
                "run": {
                    "description": "Run a YAML browser automation script",
                    "usage": "oxibrowser run <script.yaml>",
                    "args": [
                        {"name": "script", "type": "string", "description": "Path to YAML file or inline YAML"}
                    ]
                },
                "session": {
                    "description": "Start interactive session (stdin/stdout JSON REPL). Agent launches as subprocess.",
                    "usage": "oxibrowser session [--allow-private-ips] [--account <IDS>] [--ref <TAG>]",
                    "session_commands": {
                        "new": {"description": "Create new tab → tab_id"},
                        "goto": {"args": ["tab_id", "url"], "flags": ["wait"], "description": "Navigate tab"},
                        "back": {"args": ["tab_id"], "description": "Go back"},
                        "forward": {"args": ["tab_id"], "description": "Go forward"},
                        "reload": {"args": ["tab_id"], "description": "Reload page"},
                        "click": {"args": ["tab_id", "selector"], "description": "Click element"},
                        "fill": {"args": ["tab_id", "selector", "value"], "description": "Fill input"},
                        "press": {"args": ["tab_id", "key"], "description": "Press key"},
                        "type": {"args": ["tab_id", "selector", "text"], "description": "Type text char by char"},
                        "select": {"args": ["tab_id", "selector", "value"], "description": "Select option"},
                        "check": {"args": ["tab_id", "selector"], "description": "Check checkbox"},
                        "uncheck": {"args": ["tab_id", "selector"], "description": "Uncheck checkbox"},
                        "scroll": {"args": ["tab_id"], "flags": ["down", "up"], "description": "Scroll viewport"},
                        "eval": {"args": ["tab_id", "expression"], "flags": ["await"], "description": "Evaluate JS"},
                        "extract": {"args": ["tab_id"], "flags": ["selector","all","attrs","links","title","text","markdown","max-bytes"], "description": "Extract data"},
                        "content": {"args": ["tab_id"], "flags": ["format","fields","max-bytes","summary"], "description": "Get page content"},
                        "screenshot": {"args": ["tab_id"], "flags": ["output","width","base64"], "description": "Take screenshot"},
                        "wait": {"args": ["tab_id", "selector"], "flags": ["timeout"], "description": "Wait for selector"},
                        "save-state": {"args": ["path"], "description": "Export cookies + localStorage as Playwright-compatible storageState JSON"},
                        "load-state": {"args": ["path"], "description": "Import a storageState snapshot (applies to the next navigation)"},
                        "close": {"args": ["tab_id"], "description": "Close tab (or --all)"},
                        "list": {"description": "List active tabs"},
                        "help": {"description": "Show session command help"},
                        "exit": {"description": "End session"}
                    }
                },
                "serve": {
                    "description": "Start CDP server for Puppeteer/Playwright",
                    "usage": "oxibrowser serve [flags]",
                    "flags": {
                        "host": {"type": "string", "default": "127.0.0.1"},
                        "port": {"type": "int", "default": 9222},
                        "cookie-file": {"type": "string", "description": "Cookie persistence file path"},
                        "allow-private-ips": {"type": "bool", "description": "Disable the SSRF filter (localhost targets)"},
                        "proxy": {"type": "string", "description": "HTTP/HTTPS/SOCKS proxy for all requests"},
                        "auth-token": {"type": "string", "description": "WebSocket auth token (required for non-loopback bind)"},
                        "mcp": {"type": "bool", "description": "Serve as a stdio MCP server (JSON-RPC 2.0) instead of CDP; adds account tools (account_list/account_status/login_request) and binds --account contexts for browser tools"},
                        "account": {"type": "string", "description": "Bind account ids (comma list): CDP contexts per account, or the MCP browser tools' primary context"},
                        "ref": {"type": "string", "description": "Correlation tag stamped on audit events"},
                        "as-agent": {"type": "string", "description": "[mcp] Acting agent identity for the ledger and escalations"}
                    }
                },
                "describe": {
                    "description": "Print CLI schema as JSON for agent introspection",
                    "usage": "oxibrowser describe [command] [--compact] [--json]",
                    "args": [
                        {"name": "command", "type": "string", "required": false, "description": "Specific command to describe"}
                    ],
                    "flags": {
                        "compact": {"type": "bool", "description": "Minimal output (~200 tokens)"},
                        "json": {"type": "bool", "description": "Output as JSON"}
                    }
                },
                "skill": {
                    "description": "Print agent skill guide as markdown",
                    "usage": "oxibrowser skill"
                },
                "account": {
                    "description": "Account registry and login-state management (per-site account sandboxes)",
                    "usage": "oxibrowser account <add|list|status|rm|login|logout|grant|revoke|exec|grants|capture|irreversible|export-state> [args] [flags]",
                    "subcommands": {
                        "add": {
                            "usage": "oxibrowser account add --site <domain> [--id <slug>] [--login <hint>] [--display <name>] [--json]",
                            "description": "Register an account (state=needs_login); id derived from scope on conflict (-2, -3, …)"
                        },
                        "list": {
                            "usage": "oxibrowser account list [--json]",
                            "description": "Account status board: id/scope/state/login hint/session horizon"
                        },
                        "status": {
                            "usage": "oxibrowser account status <id> [--probe] [--json]",
                            "description": "One account; --probe re-validates the stored session over the network (keychain + browser)"
                        },
                        "rm": {
                            "usage": "oxibrowser account rm <id> [--json]",
                            "description": "Revoke: record + session envelopes deleted (keychain credentials kept)"
                        },
                        "grant": {
                            "usage": "oxibrowser account grant <id> --agent <A> [--actions navigate,interact] [--ttl SEC] [--max-uses N] [--ref TAG] [--json]",
                            "description": "Agent-scoped consent grant; --ref is the correlation tag stored on the grant and audit lines"
                        },
                        "revoke": {
                            "usage": "oxibrowser account revoke <id> --agent <A> [--ref TAG] [--json]",
                            "description": "Tombstone all of an agent's grants for the account"
                        },
                        "grants": {
                            "usage": "oxibrowser account grants <id> [--agent A] [--json]",
                            "description": "Live grants: consent_id, agent, actions, uses/max_uses, expiry, active, ref"
                        },
                        "exec": {
                            "usage": "oxibrowser account exec <id> --agent <A> [--actions …] [--ttl SEC] [--max-uses N] [--ref TAG] -- <CMD> [ARGS…]",
                            "description": "Volatile grant → run child (stdio passthrough, exit code propagated) → tombstone on any exit. Lifecycle JSONL on stderr. OXIBROWSER_ACCOUNT/AGENT_ID env set. Crash bound: TTL (default 3600s)"
                        },
                        "capture": {
                            "usage": "oxibrowser account capture <id> --ws <ws://127.0.0.1:PORT/ws> [--json]",
                            "description": "Explicitly seal the live context's cookies as the stored envelope (OXI.captureSession) — the stop-the-work path"
                        },
                        "irreversible": {
                            "usage": "oxibrowser account irreversible <id> [--add p1,p2] [--clear] [--json]",
                            "description": "Per-account irreversible-action patterns extending the built-in deny-biased list (click/fill descriptors in account contexts)"
                        }
                    }
                },
                "credential": {
                    "description": "Keychain-backed agent credentials; values never travel via argv and never serialize to JSON",
                    "usage": "oxibrowser credential <put|get|list|rm|totp|onboard> [args] [flags]",
                    "subcommands": {
                        "put": {
                            "usage": "oxibrowser credential put --agent <id> --site <domain> --kind <password|totp|api-key|note> [--slug <s>] [--login <hint>] --origin <origin>… [--stdin|--prompt]",
                            "description": "Store a secret; value read from stdin or hidden prompt (never argv). totp accepts otpauth:// URI or base32 secret"
                        },
                        "get": {
                            "usage": "oxibrowser credential get --id <handle> --field <password|otpauth>",
                            "description": "Print one secret field raw to stdout; no --json by design"
                        },
                        "list": {
                            "usage": "oxibrowser credential list [--agent <id>] [--json]",
                            "description": "Credential metadata only (handles, kinds, hints — never values)"
                        },
                        "rm": {
                            "usage": "oxibrowser credential rm --id <handle> [--json]",
                            "description": "Delete a credential from the OS keychain"
                        },
                        "totp": {
                            "usage": "oxibrowser credential totp --id <handle>",
                            "description": "Current TOTP code with remaining validity (human output)"
                        },
                        "onboard": {
                            "usage": "oxibrowser credential onboard --agent <id> --site <domain>",
                            "description": "Keychain ACL onboarding notes and diagnostics script"
                        }
                    }
                },
                "search": {
                    "description": "Search the web or GitHub (lightweight HTTP, no browser needed)",
                    "usage": "oxibrowser search <query> [--source web|github|github-issues] [--engine ddg|wiki|bing] [--repo owner/repo] [--token ghp_xxx] [--max-results N] [--timeout sec] [--json]",
                    "args": [
                        {"name": "query", "type": "string", "required": true, "description": "Search query (all positional args joined)"}
                    ],
                    "flags": {
                        "source": {"type": "enum", "values": ["web", "github", "github-issues"], "default": "web", "description": "Search source"},
                        "engine": {"type": "string", "default": "ddg", "description": "Comma-separated: ddg, wiki, bing"},
                        "repo": {"type": "string", "description": "owner/repo for github-issues"},
                        "token": {"type": "string", "description": "GitHub PAT (optional, increases rate limit)"},
                        "json": {"type": "bool", "default": false, "description": "Output as JSON (automatic when piped)"},
                        "max-results": {"type": "int", "default": 10, "description": "Max results (max 30)"},
                        "timeout": {"type": "int", "default": 15, "unit": "seconds", "description": "Per-engine timeout"}
                    },
                    "output_schema": {
                        "type": "object",
                        "properties": {
                            "query": {"type": "string"},
                            "source": {"type": "string"},
                            "engine": {"type": "string"},
                            "total_results": {"type": "integer"},
                            "results": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "title": {"type": "string"},
                                        "url": {"type": "string"},
                                        "snippet": {"type": "string"},
                                        "source": {"type": "string"},
                                        "extra": {"type": "object", "optional": true}
                                    }
                                }
                            }
                        }
                    }
                }
            },
            "output_format": {
                "ok": "bool",
                "data": "object on success",
                "error": "string on failure",
                "error_code": "string on failure",
                "details": "object on failure when structured (e.g. CONSENT_REQUIRED)",
                "meta": {"tab_id": "string?", "elapsed_ms": "int"}
            },
            "exit_codes": {
                "0": "success",
                "1": "runtime error (DOM, JS)",
                "2": "input validation (bad URL, control chars, path traversal)",
                "3": "timeout",
                "4": "network error",
                "5": "consent required — the JSON error envelope carries `details` {request_id, ttl, account, action}; request_id/ttl are null unless a confirmation is pending. Resolve via `account grant` / `credential authorize` or the viewer confirmation flow, then retry"
            }
        }))
    }
}

/// Describe a single command.
pub fn describe_command(name: &str) -> CliResponse {
    let all = describe_all(false);
    let commands = all.data.as_ref().and_then(|d| d.get("commands"));

    match commands.and_then(|c| c.get(name)) {
        Some(cmd) => CliResponse::success(cmd.clone()),
        None => CliResponse::error(format!("unknown command: {name}"), "INVALID_COMMAND"),
    }
}
