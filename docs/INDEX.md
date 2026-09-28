# OxiBrowser Docs Index

> Canonical unified design system: `project-oxi/.github/DESIGN.md`.
> Each project's own `DESIGN.md` is now a project-specific working fork with a pointer header.

## Layout

| Path | Purpose | Files |
|------|---------|------:|
| `./` | **Top-level architecture, design rationale, quickstart, roadmap.** | 7 |
| `design/` | Focused design notes for in-flight subsystems (observability, search-as-library). | 2 |
| `designs/` | Dated design documents per planning cycle (v0.3 → v0.6, plus topic designs). | 14 |
| `research/` | Dated multi-agent research reports (sources cited, roadmap inputs). | 7 |

Total active docs (root + `design/` + `designs/` + `research/`): **30** `.md` files.

## Top-level docs (root)

- **`DESIGN.md`** — OxiBrowser design rationale (why pure-Rust headless, Servo ecosystem, CDP-compat, agent workloads). *Project-specific fork; canonical unified design system lives at `project-oxi/.github/DESIGN.md`.*
- **`ARCHITECTURE.md`** — Module layout, lifecycle, threading model, integration points.
- **`CDP.md`** — Chrome DevTools Protocol surface supported by OxiBrowser, mappings, and quirks.
- **`QUICKSTART.md`** — First-run, embedding, and example agent usage.
- **`roadmap.md`** — living roadmap: current state, prioritized next work, non-goals. Historical milestone plans are in git history; dated designs live in `designs/`.
- **`search-command-proposal.md`** — Proposal for a search-driven CLI surface.
- **`design-agent-layout-eval.md`** — Layout evaluation notes produced during agent-mode design passes.

## `design/` — focused subsystem notes

- `observability.md` — Tracing, metrics, and logging strategy.
- `search-as-library.md` — Embedding OxiBrowser's search/index primitives.

## `designs/` — dated design documents (chronological)

| Date | Topic |
|------|-------|
| `2026-05-16-cli-enhancement-design.md` | CLI ergonomics pass (v0.3 era) |
| `2026-06-04-oxibrowser-observability.md` | Observability design v1 |
| `2026-06-04-oxibrowser-observability-followup.md` | Observability follow-ups |
| `2026-06-25-pure-rust-stealth.md` | Pure-Rust stealth / detection-resistance design |
| `2026-06-real-web-readiness.md` | Real-web readiness gap analysis |
| `v0.3-headless-browser.md` | v0.3 architecture baseline |
| `v0.4-production-grade.md` | v0.4 hardening targets |
| `v0.5-completion-master.md` | v0.5 completion master plan |
| `v0.6-cdp-events-perf.md` | v0.6 CDP event-throughput work |
| `agent-os-sdk.md` | Agent-OS SDK surface |
| `merge-guide.md` | Merge / contribution walkthrough |
| `session-a-web-platform.md` | Session A working notes — web platform side |
| `session-b-cdp-perf.md` | Session B working notes — CDP perf side |
| `2026-09-27-agent-auth-implementation.md` | Agent unattended auth — implementation design (from `research/` 2026-09-27) |
| `2026-09-28-account-login-session-management.md` | Account layer: login session management, browser-context sandbox, login orchestration (amends 2026-09-27 M5/M6/M8, adds M-A~M-D) |


## `research/` — dated research reports (2026-09-27: agent unattended authentication)

Six parallel sub-agent investigations + synthesis, sourced from official docs. Roadmap input for
credential/session/auth capabilities. Concretized into `designs/2026-09-27-agent-auth-implementation.md`
(same date) — implementation design with code re-verification and drift notes.

- `00-SYNTHESIS.md` — Master synthesis: 4-layer strategy (API-first → session persistence → credential broker → human escalation), gap analysis vs current code, P0/P1/P2 roadmap.
- `01-keychain.md` — macOS keychain model (TN3137): ACL/partition-ID, Rust paths (`keyring`), broker comparison, JSON record format with `otpauth://`.
- `02-session-persistence.md` — storageState/profile reuse, cookie partitioning (CHIPS), Cloudflare bot-detection constraints, encrypted session-store design.
- `03-api-first-matrix.md` — Unattended-path matrix: Cloudflare tunnel 100% API, Tailscale autoApprovers, Apple iCloud sign-out structurally impossible.
- `04-2fa-passkeys.md` — WebAuthn virtual authenticator (CDP), TOTP pipeline, iCloud passkey limits, human-escalation state machine, step-up policy.
- `05-landscape.md` — How Operator/computer-use/browser-use/Steel/Browserbase/Stagehand/Skyvern handle auth; Top-5 patterns to copy.
- `06-safety-design.md` — Threat model, consent records (credential × origin × action), exact-origin matching, HAR redaction, confirmation protocol, 8 concrete integration points.

## `archive/` — superseded designs and transient reports

- **Superseded designs** (kept for history): `CLI-V2-DESIGN.md`, `CLI-V2-REMAINING.md`, `DOM_API_DESIGN.md`, `FIX_DESIGN.md`, `HEADLESS_ROADMAP.md`, `IMPROVEMENT_DESIGN.md`, `SCENARIO_TEST_REPORT.md`, `V0.7.0_DESIGN.md`, `phase3-spec.md`.
- **`archive/transient/`** — Agent-generated transient reports (`progress.md`, `.oxi-explore-parser-report.md`, `.oxi-fixraf-final.md`, `.oxi-fixraf-result.md`). Moved out of repo root; not authoritative.

## Canonical design pointer

For the unified oxi design system (tokens, typography, components, motion, dark mode, accessibility), see:

> **`project-oxi/.github/DESIGN.md`** (v1.0, dated 2026-07-31)

OxiBrowser-specific design rationale remains at `docs/DESIGN.md`.
