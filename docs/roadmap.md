# OxiBrowser Roadmap

> Living roadmap. Historical milestone plans (v0.5 era) live in git history;
> dated design docs live in `designs/`.

## Current state (v0.26 — agent plane shipped)

| | |
|---|---|
| Crates | 5 (`oxibrowser`, `oxibrowser-cdp`, `oxibrowser-core`, `oxibrowser-credentials`, `oxibrowser-render`) |
| Rust LOC | ~62k |
| Tests | ~840 (unit + CDP e2e + agent-plane e2e 18 + corpus 10/10); real-website suite `--ignored` |
| Render stack | Blitz (Stylo CSS, Taffy layout, Parley text, vello_cpu paint) |
| JS | boa_engine 0.20 (ES2024+), wasmi WASM bridge, dedicated JS thread; form activation (submit → encoded POST/GET on all three action paths) |
| Protocols | CDP (WebSocket), Puppeteer/Playwright compatible; stdio MCP (`serve --mcp`, 12 tools); ghost restricted MCP lane (`--ghost`) |
| Agent plane | `oxibrowser repl` (Playwright-shaped JS REPL over native actions: locators/strict-mode/error contract, `oxi.fillCredential`, `page.mouse.click` CUA, steering events dialog/popup/download/console) + `oxibrowser agent run/resume/eval` (model-agnostic loop: token budgets, compaction, consent flows, site-file memory, network replay, corpus `agent eval corpus` — see `designs/2026-10-16-agent-plane-execution.md`) |
| Anti-bot | wreq TLS fingerprint impersonation, stealth navigator surface, challenge handling |
| Security | Redaction by default (passwords `__REDACTED__` in every observation surface), JSONL audit (schema v1), origin-policy primitives, consent/policy engine, native-action-only input (page scripts cannot forge targets/descriptors) |
| Accounts | Registry + AEAD session envelopes (OXSESS1), login orchestration (import/wizard/viewer/agent), agent-scoped grants, `account exec` volatile grants, per-account flock, irreversible-pattern gate, `page.login` in the REPL |

Recently shipped (agent plane — `designs/2026-10-16-agent-first-browser.md`):

- M0: `observe` — token-budgeted aria YAML + actionable refs (one
  observation = one consistent `e{n}` namespace).
- M1: Oxiwright REPL — dedicated boa agent-world (page runtime untouched),
  Playwright locator subset on the native action path, snapshot-derived
  gates (password literals denied everywhere at the native layer; only
  broker-authorized `FillCredential` writes one).
- M1.5: form activation (submit controls drive encoded POST/GET
  navigation; `form.submit()` resolved).
- M2: steering — `waitForEvent`, dialog-deadlock-free action bridge,
  page-driven popups (`window.open` / `target=_blank` + `TabSpawned`
  observer contract), downloads.
- M3: the agent loop — `ModelProvider` (OpenAI-compatible or scripted),
  10K-token prompt discipline (CI-enforced), compaction, the two consent
  flows (credential grants via the file-replay ConsentStore; irreversible
  via live `mark_irreversible`), `agent run`/`resume`.
- M4: site files (verified selectors/markers/endpoints per origin),
  transcript resume, `page.replay` (captured request → in-page fetch).
- M5: CUA fallback (`page.mouse.click` via layout hit-test), 1440×900
  agent viewport, `agent eval` corpus runner (10/10),
  `OXIBROWSER_BYTES_PER_TOKEN` calibration.

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

Prioritized by agent-workload impact — the agent plane raises the floor of
what the harness can do; engine compatibility is now the ceiling.

1. **Public compatibility benchmark — harness shipped 2026-10-24.**
   `tests/compat_corpus.rs` + `tests/data/compat_corpus.txt` measure fresh
   subprocesses per URL and write `docs/quality/compat-report.md`; child
   crashes are data, not failures. Enforce a floor in CI with
   `OXIBROWSER_CORPUS_ENFORCE=1 OXIBROWSER_CORPUS_MIN_RATE=<n>`. The loop:
   measure → classify → fix the top failure class → re-measure. Everything
   else on this list is judged by this number; agent-harness quality stays
   measured by `agent eval corpus` (10/10).

   | metric | baseline 2026-10-24 AM | after loop iteration 1 | target |
   |---|---|---|---|
   | corpus pass rate | 4/6 (67%) | **5/6 (83%)** | 100%, then grow the corpus |
   | SIGABRT crashes | 2 (spotify, figma) | **0** | 0 — hard rule |

   Iteration 1 (crash-isolation class): iterative DOM walkers
   (`css/render.rs` text/markdown/children-text, `deep_text_content`),
   64 MiB JS-thread stack, and a parser nesting guard
   (`MAX_SOURCE_NESTING`, mirrors V8's parser depth limit) — figma.com now
   renders; spotify.com exits cleanly but renders empty (boa 0.20 ES gaps
   in its web-player bundle) → boa 0.22 upgrade is the next lever.
   Known residual: `set_element_text_recursive`, `copy_node_recursive`,
   `estimate_y_recursive` and the snapshot remove-descendants walk are
   still recursive (iterative-conversion candidates); debug-profile builds
   inflate frames ~10x, so pathological sources can still overflow them —
   release is the shipped target.
2. **Real-model smoke + corpus on live sites** — `agent run` against a live
   `OXIBROWSER_LLM_*` endpoint on the `--ignored` real-website fixtures;
   grow the corpus beyond loopback (login-gated tasks once accounts are in
   place).
3. **IndexedDB remainder** — full spec surface for non-JSON payloads
   (v1 shipped 2026-09-30: core subset with envelope-carried state).
4. **Login viewer hardening** — the `/viewer` reference page is functional;
   next: clipboard-safe token handoff, mobile viewport input, i18n; wire
   agent-loop consent cards into it (today `agent run` consents via the
   terminal).
5. **ReadableStream** — completes `fetch().body`; frequent in modern scripts
   (also unlocks `page.replay` on streaming endpoints).
6. **Web platform gaps, small batch** — `crypto.subtle`, `history` breadth,
   `requestSubmit`, form validation API; native-path input extensions
   (press/check on `NativeInput`).
7. **Web Workers** — dedicated + shared worker contexts over the existing
   per-frame context machinery.
8. **WebDriver BiDi** (deprioritized behind the agent plane) — second
   automation protocol on the same serve endpoint.
9. **HTTP Basic `Fetch.authRequired`** (stretch) — event + `continueWithAuth`
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
