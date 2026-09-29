# OxiBrowser Roadmap

> Living roadmap. Historical milestone plans (v0.5 era) live in git history;
> dated design docs live in `designs/`.

## Current state (v0.24)

| | |
|---|---|
| Crates | 5 (`oxibrowser`, `oxibrowser-cdp`, `oxibrowser-core`, `oxibrowser-credentials`, `oxibrowser-render`) |
| Rust LOC | ~55k |
| Tests | ~700 (unit + CDP e2e + acceptance); real-website suite `--ignored` (cli 11 · integration 7 · smoke 3, measured 2026-09-29) |
| Render stack | Blitz (Stylo CSS, Taffy layout, Parley text, vello_cpu paint) |
| JS | boa_engine 0.20 (ES2024+), wasmi WASM bridge, dedicated JS thread |
| Protocols | CDP (WebSocket), Puppeteer/Playwright compatible; stdio MCP (`serve --mcp`, 12 tools) |
| Anti-bot | wreq TLS fingerprint impersonation, stealth navigator surface, challenge handling |
| Security | Redaction by default, JSONL audit (schema v1: per-event `schema_version`/`event_id`/`ref`), origin-policy primitives, consent/policy engine |
| Accounts | Registry + AEAD session envelopes (OXSESS1), login orchestration (import/wizard/viewer/agent), agent-scoped grants, `account exec` volatile grants, per-account flock, irreversible-pattern gate |

Recently shipped (account process model — `designs/2026-09-29-account-process-execution.md`):

- Consent error contract: exit 5 `CONSENT_REQUIRED` with structured `details`
  (`request_id`, `ttl`, `account`, `action`); identical envelope on `--json`
  stdout and a bare stderr JSON line otherwise.
- Audit schema v1: per-event `schema_version` + globally-unique `event_id`
  (join key; `seq` is process-local) + `--ref` correlation on fetch/serve/
  grant/revoke/exec.
- `account grants` ledger view; `account exec` (grant → run → tombstone on
  any exit, TTL-bounded crash window); `account capture` + `OXI.captureSession`
  (stop-the-work sealing); per-account flock (`--lock-wait`/`--lock-timeout`).
- MCP: `serve --mcp --account/--as-agent` + `account_list`/`account_status`/
  `login_request` tools; `session --account`.
- Ops: `version --json` (install path + effective keychain prefix),
  `OXIBROWSER_KEYCHAIN_PREFIX` for parallel installs.
- Irreversible gate: deny-biased pattern list + per-account injection
  (`account irreversible`), enforced at `OXI.clickRef`/`fillRef`.
- Reference viewer web page at `/viewer` (screencast mirror + confirmation
  cards; viewer role claimable via query parameters for browser WebSocket).

## Next

Prioritized by agent-workload impact. Storage completeness (IndexedDB) is
**priority-raised**: in the CLI process model every run restores from the
envelope, so sites whose auth tokens live in IndexedDB lose the session on
every invocation — the single biggest real-site reliability gap (FM-L5).

1. **IndexedDB** — ✅ v1 shipped (2026-09-30): JS `indexedDB` API subset
   (open/upgrade/createObjectStore/transaction/put/get/getAll/delete/count)
   with origin-keyed state carried in the envelope (`OriginState.indexed_db`)
   — IDB-auth tokens survive restore. Deferred-event pump, string keys,
   JSON-serialized values; indexes/cursors on demand. Remaining: full
   spec surface for non-JSON payloads.
2. **Public compatibility benchmark** — committed corpus runner (WPT subset
   or fixed public-URL set) aggregated in CI, publishing a pass-rate number.
   Everything else on this list is judged by it.
3. **Login viewer hardening** — the `/viewer` reference page is functional;
   next: clipboard-safe token handoff, mobile viewport input, i18n.
4. **ReadableStream** — completes `fetch().body`; frequent in modern scripts.
5. **Web platform gaps, small batch** — `crypto.subtle`, `history` breadth,
   `requestSubmit`, form validation API.
6. **Web Workers** — dedicated + shared worker contexts over the existing
   per-frame context machinery.
7. **WebDriver BiDi** — second automation protocol on the same serve endpoint.
8. **HTTP Basic `Fetch.authRequired`** (stretch) — event + `continueWithAuth`
   wired to broker credentials; today only the JS `fetch()` bridge retries
   with globally-configured credentials and navigation does not retry.

## Storage compatibility matrix (auth persistence, 2026-09-30)

What a login survives, given the current envelope = cookies +
localStorage (+ fingerprint/egress). This is the operational answer to
"which sites can knock drive headless" — measure before trusting; entries
marked [INFERENCE] lack a live capture.

| Storage class | Envelope covers | Behavior | Representative sites |
|---|---|---|---|
| Cookie-session (HttpOnly) | ✅ | login survives every restore | GitHub (`user_session`), most forums [measured: probe suite] |
| localStorage token | ✅ | survives | many SPAs [INFERENCE] |
| sessionStorage token | ❌ (by design) | dropped — matches a fresh real browser tab | rare as sole factor [INFERENCE] |
| **IndexedDB token** | ✅ (v1: JSON records via `indexedDB` API) | login survives restore | Google properties, some banking/SSO portals [INFERENCE — verify per site] |
| Bot-clearance (`cf_clearance`) | ✅ (30-min corridor) | survives short windows; challenge state otherwise | Cloudflare-fronted sites |

Curated probe markers (`core/account/probes.rs`) seed `account add` with a
verified URL+marker (today: GitHub `meta[name=user-login]`); extend by PR
with evidence. Sites that fail the bot wall fall back to guide capture:
log in inside your real browser, export a Playwright `storageState` (float
epochs and `httpOnly` casing accepted), then `account login <id> --mode
import --storage-state <file>`.

## Non-goals

- V8 or any C/C++ JS engine (identity: pure-Rust, small binary, fast cold start).
- GUI window / GPU compositor / pixel-perfect Chrome parity.
- Workspace fragmentation beyond 5 crates (contribution surface stays small).
