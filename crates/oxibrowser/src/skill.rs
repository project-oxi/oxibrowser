//! Agent skill guide — printed by `oxibrowser skill`.
//!
//! Designed to be injected into an agent's system prompt or context.
//! ~150 tokens, covers all 3 modes.

/// Return the skill guide as a markdown string.
pub fn skill_text() -> &'static str {
    r#"# OxiBrowser Agent Skills

## 3 Modes

1. **One-shot**: `oxibrowser fetch <url> [flags]` or `oxibrowser extract <url> [flags]`
2. **Automation**: `oxibrowser run <script.yaml>`
3. **Interactive**: `oxibrowser session --json` (stdin commands, stdout JSON)
4. **Server**: `oxibrowser serve` (CDP for Puppeteer/Playwright) or `oxibrowser serve --mcp` (stdio MCP tools: navigate/observe/click/fill/eval/read/wait/screenshot)

## Invariant Rules

- ALWAYS add `--json` for machine-readable output (automatic when piped)
- ALWAYS add `--max-bytes 8000` to limit response size
- Use `--summary` first to check if a page is relevant before full read
- Use `--fields url,title,status` to skip large content fields
- Use `session` for multi-step interactions (click → read → click → read)
- Use `run` for complex multi-step automation (YAML scripts)
- NEVER trust eval with untrusted input — use `extract` instead

## One-shot Patterns

```bash
# Read page
oxibrowser fetch <url> --format markdown --json --max-bytes 8000

# Get links
oxibrowser extract <url> --links --json

# Extract elements with attributes
oxibrowser extract <url> --selector "a" --all --attrs text,href --json

# Click then read
oxibrowser fetch <url> --click <selector> --wait <selector> --format markdown --json

# Page metadata only (fast check)
oxibrowser fetch <url> --summary --json

# Run JS
oxibrowser fetch <url> --eval "document.title" --json

# Extract specific text
oxibrowser fetch <url> --extract "h1" --json
```

## Session Workflow

1. Start: `oxibrowser session --json` (run as subprocess)
2. Create tab: `new` → get tab_id
3. Navigate: `goto <tab_id> <url>`
4. Interact: `click/fill/press/eval <tab_id> ...`
5. Extract: `extract/content <tab_id> ... --max-bytes 8000`
6. Close: `close <tab_id>` then `exit`

## Session Commands

`new`, `goto`, `back`, `forward`, `reload`, `click`, `fill`, `press`, `type`, `select`, `check`, `scroll`, `eval`, `extract`, `content`, `screenshot`, `wait`, `save-state <path>`, `load-state <path>`, `close`, `list`, `help`, `exit`

## Storage State

Login/session reuse (Playwright-compatible JSON):
- `save-state <path>` — export cookies + localStorage
- `load-state <path>` — import; applies from the next navigation
- Over CDP: `OXI.exportStorageState` / `OXI.importStorageState`

## Ref Loop (CDP) — cheapest agent interaction

1. `OXI.getInteractiveElements` → each element carries a stable `ref` (`e1`, `e2`, …)
2. Act by ref: `OXI.clickRef {ref}`, `OXI.fillRef {ref, value}`, `OXI.waitRef {ref, timeoutMs}`
3. Document changed → refs go stale → error says "re-observe". Re-run step 1.
4. `OXI.ariaSnapshot` → YAML tree with `[ref=eN]` annotations (visibility-filtered)

## New in fetch / serve

```bash
# HAR 1.2 of every request made while fetching
oxibrowser fetch <url> --har out.har --json

# Report Web APIs the page needs but oxibrowser lacks (meta.api_gaps)
oxibrowser fetch <url> --telemetry --json

# localhost targets (SSRF filter off) — fetch, session, serve, serve --mcp
oxibrowser fetch http://127.0.0.1:8080/ --allow-private-ips --json
```

## Accounts & Credentials (login persistence)

```bash
# Register an account and check its state board
oxibrowser account add --site github.com --login me@corp.io --json
oxibrowser account list --json            # id/scope/state/session horizon
oxibrowser account status gh-work --json  # add --probe to re-validate live

# Secrets live in the OS keychain; the value NEVER goes through argv
echo "$PASS" | oxibrowser credential put --agent main --site github.com \
  --kind password --slug work --origin https://github.com/ --stdin
oxibrowser credential list --json         # metadata only, never values
oxibrowser credential totp --id "kch:main/github.com/totp/work"

# Rules: get prints the raw secret (no --json). Deny rules → consent →
# local-user confirmation. account rm deletes envelopes, not keychain creds.
```

## Output Format

All JSON: `{"ok": true/false, "data": {...}, "error": "...", "error_code": "...", "meta": {"elapsed_ms": N}}`
Check `ok` first. If `false`, read `error_code`.

## Exit Codes

0=success, 1=runtime error, 2=input validation, 3=timeout, 4=network

## Discover Commands

`oxibrowser describe --compact --json` (~200 tokens)
`oxibrowser describe <command> --json` (specific command details)
"#
}
