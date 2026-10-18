<div align="center">

<img src="logo-readme.png" alt="OxiBrowser logo" width="120">

# 🌐 OxiBrowser

**The headless browser built in pure Rust for AI agents.**

Not a Chromium fork. Not a C++ wrapper. A browser engine written from scratch in Rust,
designed from day one for automation, web scraping, and AI-driven workflows.

[![CI](https://img.shields.io/github/actions/workflow/status/project-oxi/oxibrowser/ci.yml?branch=main&style=flat-square&logo=github&label=CI)](https://github.com/project-oxi/oxibrowser/actions)
[![Crates.io](https://img.shields.io/crates/v/oxibrowser?style=flat-square&logo=rust&label=crates.io)](https://crates.io/crates/oxibrowser)
[![docs.rs](https://img.shields.io/docsrs/oxibrowser?style=flat-square&label=docs.rs)](https://docs.rs/oxibrowser)
[![GitHub release](https://img.shields.io/github/v/release/project-oxi/oxibrowser?style=flat-square&include_prereleases&label=release)](https://github.com/project-oxi/oxibrowser/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue?style=flat-square)](https://github.com/project-oxi/oxibrowser/blob/main/LICENSE)
[![GitHub stars](https://img.shields.io/github/stars/project-oxi/oxibrowser?style=flat-square&logo=github)](https://github.com/project-oxi/oxibrowser/stargazers)
[![Rust](https://img.shields.io/badge/rust-1.96%2B-orange?style=flat-square&logo=rust)](https://www.rust-lang.org/)

[Report Bug](https://github.com/project-oxi/oxibrowser/issues) · [Request Feature](https://github.com/project-oxi/oxibrowser/issues) · [Read the Docs](https://github.com/project-oxi/oxibrowser/blob/main/docs/ARCHITECTURE.md) · [Discord](https://discord.gg/oxibrowser)

</div>

---

<div align="center">

<table>
<tr>
<td align="center"><strong>~44 MB</strong><br><sub>Single static binary</sub></td>
<td align="center"><strong>~50 ms</strong><br><sub>Cold start time</sub></td>
<td align="center"><strong>~8 MB</strong><br><sub>Base memory</sub></td>
<td align="center"><strong>697 tests</strong><br><sub>Full coverage</sub></td>
<td align="center"><strong>Rust-first</strong><br><sub>C toolchain for TLS only</sub></td>
</tr>
</table>

<table>
<tr>
<th>OxiBrowser</th>
<th>Headless Chrome</th>
</tr>
<tr>
<td align="center">~44 MB binary</td>
<td align="center">~400 MB install</td>

</tr>
<tr>
<td align="center">~8 MB RAM base</td>
<td align="center">~200 MB RAM base</td>

</tr>
<tr>
<td align="center">~50 ms startup</td>
<td align="center">~800 ms startup</td>

</tr>
<tr>
<td align="center">Pure Rust (boa)</td>
<td align="center">C++ (V8)</td>

</tr>
<tr>
<td align="center">MIT</td>
<td align="center">BSD / ToS</td>

</tr>
</table>

</div>

---

## ✨ Why OxiBrowser?

**You're building AI agents that need to browse the web.** You don't need a full browser with GPU rendering, audio output, and extension support. You need something fast, small, and programmable.

OxiBrowser is built for exactly that use case:

- 🤖 **AI-Agent First** — CLI designed for agents: `--json` output, `describe` for schema, `skill` for prompts, `session` for multi-step
- ⚡ **Blazing Fast** — Cold starts in ~50ms, no Chromium overhead, no Node.js required
- 🦀 **Rust-First** — `boa_engine` (JS, no V8), `html5ever` (HTML) are pure Rust. TLS uses `btls` (BoringSSL C binding) for stealth fingerprint emulation. Single static binary.
- 🔌 **CDP Compatible** — Puppeteer, Playwright, and any Chrome DevTools Protocol client works out of the box
- 🛡️ **Secure by Default** — SSRF protection with CIDR blocking, `robots.txt` respect, no sandbox escape surface
- 📦 **Tiny Footprint** — ~44 MB binary, ~8 MB base memory. Run 100 instances without breaking a sweat

---


## 📋 Changelog

See **[CHANGELOG.md](CHANGELOG.md)** for the full version history and **[GitHub Releases](https://github.com/project-oxi/oxibrowser/releases)** for release notes.

Recent milestones (v0.25–v0.26): **the agent plane** — `repl` (Playwright-shaped JS over native actions), `agent run/resume/eval` (model-agnostic loop with token budgets, consent flows, site memory, corpus 10/10), form activation, page-driven popups, the restricted ghost MCP lane, and native-action-only input (page scripts cannot forge targets). Earlier (v0.17–v0.20): per-frame JS contexts, Shadow DOM, async `fetch`/XHR/WebSocket, CORS, multi-tab, PDF export, webfonts, the Blitz/Stylo/Taffy/Parley pipeline, stealth, Canvas 2D, custom elements, 12-domain CDP.

## 🚀 Quick Start

### Install

```bash
cargo install oxibrowser
```

### Fetch a page (human-readable)

```bash
$ oxibrowser fetch https://example.com

Example Domain

# Example Domain

This domain is for use in documentation examples...
[Learn more](https://iana.org/domains/example)
```

### Fetch a page (agent mode)

```bash
$ oxibrowser fetch https://example.com --json
{"ok":true,"data":{"url":"https://example.com/","title":"Example Domain","status":200,"markdown":"..."},"meta":{"elapsed_ms":152}}
```

### Extract structured data

```bash
$ oxibrowser extract https://example.com --links --json
{"ok":true,"data":{"links":["https://iana.org/domains/example"],"title":"Example Domain"}}
```

### Multi-step session (stdin/stdout JSON REPL)

```bash
$ oxibrowser session
new
{"ok":true,"data":{"tab_id":"t1"}}
goto t1 https://example.com
{"ok":true,"data":{"status":200,"title":"Example Domain"}}
eval t1 document.title
{"ok":true,"data":{"value":"Example Domain"}}
close t1
{"ok":true,"data":{"closed":"t1"}}
exit
{"ok":true,"data":{"exit":true}}
```

### Isolated Ghost session

```bash
oxibrowser serve --mcp --ghost <identity>
```

This restricted stdio MCP mode starts with empty in-memory state, exposes
explicit tabs and snapshot-bound refs, and never opens the account registry
or a network listener. Optional `--ghost-control-fd` plus `--ghost-task` accepts
an inherited anonymous local supervisor socketpair for handoff/resume, blocks
agent operations during human ownership and requires fresh observations after
resume. Page scripts/network continue; see [the handoff contract](docs/ghost-handoff.md).
See [the Ghost integration contract](docs/ghost-mcp.md)
for correlation, cancellation, uncertainty, and the remaining handoff work.

### Start CDP server (Puppeteer/Playwright)

```bash
oxibrowser serve --port 9222
```

```javascript
import puppeteer from 'puppeteer-core';

const browser = await puppeteer.connect({
    browserWSEndpoint: 'ws://127.0.0.1:9222',
});

const page = await browser.newPage();
await page.goto('https://news.ycombinator.com');
console.log(await page.title());
await browser.close();
```

---

## 📋 CLI Reference

```
oxibrowser <COMMAND>

COMMANDS:
  fetch      Fetch a URL and return content (markdown default)
  extract    Extract structured data (links, text, elements)
  run        Run a YAML automation script
  session    Interactive stdin/stdout JSON REPL (23 commands)
  repl       Interactive JS REPL with a Playwright-shaped API (agents)
  agent      Autonomous agent: run / resume / eval (model-agnostic loop)
  serve      Start CDP WebSocket server
  search     Web / GitHub / GitHub-issues search (no browser needed)
  account    Account registry and login-state management
  credential Keychain-backed agent credentials
  describe   Print CLI schema as JSON (for agents)
  skill      Print agent skill guide
  version    Print version information
```

### repl — Playwright-shaped JS REPL (agents)

```bash
# One stdin line = one program (top-level await), one JSON response per line
: oxibrowser repl --allow-private-ips
await page.goto('https://example.com')
await page.getByRole('link', { name: 'More' }).click()
await page.observe({ budgetTokens: 1500 })   # token-budgeted aria snapshot
await page.mouse.click(120, 40)              # CUA fallback
await oxi.fillCredential(page.getByLabel('Password'), 'github')  # needs --account
```

### agent — autonomous task loop

```bash
# Model via env: OXIBROWSER_LLM_BASE_URL / OXIBROWSER_LLM_API_KEY / OXIBROWSER_LLM_MODEL
oxibrowser agent run "find the pricing page and report the tiers" --account github
oxibrowser agent resume t-1730000000000     # continue a saved transcript
oxibrowser agent eval corpus                # deterministic corpus (10/10)
```

### fetch — One-shot page fetch

```bash
# Human-readable (markdown, default)
oxibrowser fetch https://example.com

# Agent mode
oxibrowser fetch https://example.com --json

# Click then read
oxibrowser fetch https://example.com --click button --wait .result --json

# Quick page summary
oxibrowser fetch https://example.com --summary --json

# Run JS
oxibrowser fetch https://example.com --eval "document.title" --json

# Limit response size
oxibrowser fetch https://example.com --max-bytes 8000 --json

# Select specific fields
oxibrowser fetch https://example.com --fields url,title,status --json
```

### extract — Structured data extraction

```bash
# Get all links
oxibrowser extract https://example.com --links --json

# Extract elements by CSS selector
oxibrowser extract https://example.com --selector "a" --all --attrs text,href --json

# Title + full text
oxibrowser extract https://example.com --title --text --json
```

### session — Multi-step automation

```bash
oxibrowser session  # Start REPL

# 22 commands:
new, goto, back, forward, reload, click, fill, press, type,
select, check, uncheck, scroll, eval, extract, content,
screenshot, wait, close, close --all, list, help, exit
```

### describe — Agent introspection

```bash
# Compact (~200 tokens)
oxibrowser describe --compact

# Full command details
oxibrowser describe fetch
oxibrowser describe session
```


### search — Web / GitHub search (no browser needed)

```bash
# Web search (DuckDuckGo)
oxibrowser search "rust async" --engine ddg --max-results 5 --json

# GitHub search
oxibrowser search "memory pool" --source github --json

# GitHub issues for a specific repo
oxibrowser search "panic on shutdown" --source github-issues --repo project-oxi/oxibrowser --json
```
### run — YAML automation


```yaml
name: example
steps:
  - step_type: goto
    data:
      goto: https://example.com
  - step_type: content
    data:
      format: markdown
```

```bash
oxibrowser run script.yaml
```

### JSON Output Format

All `--json` responses follow the same schema:

```json
{
  "ok": true,
  "data": { ... },
  "meta": { "elapsed_ms": 152 }
}
```

On error:

```json
{
  "ok": false,
  "error": "URL scheme must be http or https",
  "error_code": "INVALID_URL"
}
```

**Exit codes**: 0=success, 1=runtime, 2=input validation, 3=timeout, 4=network

---

## 🏗 Architecture

```
┌──────────────────────────────────────────────────────┐
│            Puppeteer / Playwright / Rust CDP          │
└────────────────────────┬─────────────────────────────┘
                         │ CDP WebSocket
                         ▼
┌──────────────────────────────────────────────────────┐
│                 CDP Server (12 domains)               │
│  Browser · DOM · Emulation · Fetch · Input            │
│  Log · Network · OXI · Page · Runtime                 │
│  Target · Tracing                                      │
├──────────────────────────────────────────────────────┤
│          Browser → Session → Page → Frame            │
├──────────┬──────────┬──────────────┬─────────────────┤
│  WebAPI  │  Network │  JS Runtime  │  Rendering      │
│  DOM     │  HTTP    │  boa_engine  │  Blitz+Stylo    │
│  Tree    │  Cookies │  ES2024+     │  Taffy+Parley   │
│  Storage │  SSRF    │  per-frame   │  PNG/PDF/font   │
├──────────┴──────────┴──────────────┴─────────────────┤
│  html5ever · encoding_rs · reqwest · boa · blitz-dom  │
└──────────────────────────────────────────────────────┘
```

### Crate Structure
| Crate | Lines | Purpose |
|-------|-------|---------|
| [`oxibrowser`](crates/oxibrowser/) | 6,103 | Binary + CLI (8 subcommands, session REPL, agent features) |
| [`oxibrowser-core`](crates/oxibrowser-core/) | 35,287 | Browser engine: Session, Page, Frame, JS Runtime, Network |
| [`oxibrowser-cdp`](crates/oxibrowser-cdp/) | 5,643 | CDP WebSocket server with 12 domain handlers |
| [`oxibrowser-render`](crates/oxibrowser-render/) | 513 | Blitz/Stylo/Taffy/Parley rendering pipeline (PNG/PDF/fonts) |
| **Total** | **~47,500** | |

---

## 🌟 Features

### Agent-First CLI

Designed for AI agent workflows — no daemon, no socket, single binary:

| Feature | Description |
|---------|-------------|
| **`--json`** | Machine-readable output (opt-in, human by default) |
| **`--max-bytes N`** | Truncate response to N bytes |
| **`--fields a,b,c`** | Select specific output fields |
| **`--summary`** | Quick page metadata (title, links, headings) |
| **`describe`** | CLI schema as JSON for agent introspection |
| **`skill`** | Agent skill guide for prompt injection |
| **`session`** | Stdin/stdout JSON REPL with 22 commands |
| **Exit codes** | 0=success, 1=runtime, 2=input, 3=timeout, 4=network |

### JavaScript Runtime (ES2024+)

Powered by [`boa_engine`](https://boajs.dev/) — pure Rust, no V8 dependency:

| Web API | Status |
|---------|--------|
| `document.querySelector` / `querySelectorAll` | ✅ Full |
| `document.createElement` / `createTextNode` | ✅ Full |
| `element.appendChild` / `removeChild` / `insertBefore` | ✅ Full |
| `element.getAttribute` / `setAttribute` / `removeAttribute` | ✅ Full |
| `element.cloneNode` / `remove()` | ✅ Full |
| `element.style` (CSSStyleDeclaration) | ✅ Property accessor |
| `element.classList` (DOMTokenList) | ✅ Property accessor |
| `element.textContent` / `innerHTML` | ✅ Read/Write |
| `element.addEventListener` / `dispatchEvent` | ✅ Full |
| `element.click()` | ✅ With event handlers |
| `fetch()` | ✅ Full (channel bridge) |
| `XMLHttpRequest` | ✅ Full with callbacks |
| `localStorage` | ✅ Persistent |
| `MutationObserver` | ✅ observe/disconnect/takeRecords |
| `setTimeout` / `setInterval` | ✅ TokioJobQueue |
| `console.log/warn/error` | ✅ With formatting |
| `URL` / `URLSearchParams` | ✅ Full |
| `crypto.getRandomValues` | ✅ Pseudo-random |
| `TextEncoder` / `TextDecoder` | ✅ UTF-8 |
| `atob` / `btoa` | ✅ Base64 |
| `requestAnimationFrame` | ✅ Polyfill |

### CDP Protocol (Chrome DevTools Protocol)

12 domain handlers — Puppeteer and Playwright compatible:

| Domain | Key Methods |
|--------|------------|
| **Browser** | `getVersion`, `close` |
| **DOM** | `getDocument`, `describeNode`, `querySelector`, `querySelectorAll`, `getBoxModel`, `getContentQuads` |
| **Emulation** | `setDeviceMetricsOverride`, `clearDeviceMetricsOverride`, `setUserAgentOverride` |
| **Fetch** | `enable/disable`, `continueRequest`, `failRequest`, `fulfillRequest`, `getResponseBody` |
| **Input** | `dispatchKeyEvent`, `dispatchMouseEvent`, `dispatchDragEvent`, `insertText` |
| **Log** | `enable`, `entryAdded` |
| **Network** | `enable/disable`, `setExtraHTTPHeaders`, `getResponseBody`, JS-fetch/XHR/WS lifecycle events |
| **OXI** 🤖 | `getMarkdown`, `getPageInfo` — AI-native extensions |
| **Page** | `navigate`, `captureScreenshot`, `printToPDF`, `getFrameTree`, `getTitle` |
| **Runtime** | `evaluate`, `callFunctionOn`, `enable`, `consoleAPICalled`, `exceptionThrown` |
| **Target** | `getTargets`, `createTarget`, `attachToTarget`, `setAutoAttach` (multi-tab) |
| **Tracing** | `start`, `end`, `getCategories` (Playwright tracing compatible) |

### OXI Domain — Built for AI Agents

```python
import websockets, json, asyncio

async def ai_scrape():
    ws = await websockets.connect('ws://localhost:9222/ws')
    
    await ws.send(json.dumps({
        "id": 1, "method": "Page.navigate",
        "params": {"url": "https://news.ycombinator.com"}
    }))
    await asyncio.sleep(2)
    
    # Clean markdown — perfect for LLM ingestion
    await ws.send(json.dumps({"id": 2, "method": "OXI.getMarkdown"}))
    resp = json.loads(await ws.recv())
    print(resp['result']['markdown'])
```

### Network Layer
| **HTTP Client** | `reqwest` with cookie persistence, redirect following |
| **Cookie Jar** | Domain-scoped cookies: PSL, `__Host-`/`__Secure-` prefixes, expiry, CHIPS partitioning |
| **CORS** | Preflight (`OPTIONS`), `Access-Control-*` validation (Fetch §3.2–3.3) |
| **Auth** | Basic + Digest (401-challenge retry) |
| **Proxy** | HTTP / HTTPS / SOCKS proxy via `BrowserConfig.proxy` |
| **SSRF Protection** | CIDR blocking for private network ranges, scheme-aware |
| **robots.txt** | RFC 9309 compliant parser, `--obey-robots` flag |
| **Network Interception** | Pause, modify, or block any request via Fetch domain |
| **WebSocket** | Full browser WebSocket API (ws + wss) |
| **Stealth** | Chrome JA4+ fingerprint emulation, bot-detection challenge retry |

### Rendering Pipeline

Powered by the Blitz rendering stack — isolated in `oxibrowser-render` so Stylo/html5ever dependency trees stay out of the core workspace:

- **Blitz-dom** — DOM tree representation for layout and paint
- **Stylo** (Firefox CSS engine) — CSS cascade, computed styles
- **Taffy** — block/inline/flex layout
- **Parley** — text shaping with `@font-face` webfont support
- **PNG screenshots** — shadow-DOM-aware rasterization (flattened tree compose)
- **PDF export** — `Page.printToPDF` / `Tab::print_to_pdf` via `printpdf`
- **HTML → Markdown** — full conversion with heading, link, and list support

---

## 🧪 Testing

```bash
# Run all tests
cargo test --workspace

# CLI integration tests (fast, no network)
cargo test -p oxibrowser --test cli

# E2E CDP tests
cargo test -p oxibrowser-cdp

# Integration tests (real websites, requires internet)
cargo test --workspace -- --ignored
```

---

## 🔧 Advanced Usage

### Rust API

```rust
use oxibrowser_core::Browser;
use oxibrowser_core::config::BrowserConfig;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let browser = Browser::new(BrowserConfig::default()).await?;
    let session = browser.new_session().await?;
    
    session.navigate("https://example.com").await?;
    
    let title = session.evaluate("document.title").await?;
    println!("Title: {:?}", title);
    
    Ok(())
)
}
```

### Use as a library

```toml
[dependencies]
oxibrowser-core = "0.11"
# Or the CDP server:
oxibrowser-cdp = "0.11"
```

### Request Interception

```javascript
const client = await page.target().createCDPSession();

await client.send('Fetch.enable', {
    patterns: [{ urlPattern: '*ads*' }]
});

client.on('Fetch.requestPaused', async ({ requestId }) => {
    await client.send('Fetch.failRequest', {
        requestId,
        reason: 'BlockedByClient'
    });
});
```

---

## 🤝 Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for full guidelines.

```bash
git clone https://github.com/project-oxi/oxibrowser.git
cd oxibrowser
cargo build
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

---

## 📄 License

OxiBrowser is licensed under the [MIT License](LICENSE).

## 🙏 Acknowledgments

- [boa_engine](https://boajs.dev/) — Pure Rust JavaScript engine (ES2024+)
- [html5ever](https://github.com/servo/html5ever) — HTML parser from the Servo project
- [reqwest](https://github.com/seanmonstar/reqwest) — Ergonomic HTTP client for Rust
- [tokio](https://tokio.rs/) — Async runtime powering the entire networking stack

---

<div align="center">

**[⬆ Back to Top](#-oxibrowser)**

Made with 🦀 in Rust

</div>
