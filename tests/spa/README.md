# OxiBrowser SPA acceptance fixtures & runner

Deterministic local SPA suite and Playwright actual-input acceptance runner, per
`docs/quality/2026-10-27-spa-goal-and-design.md` (step 0: fixed fixtures, isolated
reference Chrome + OxiBrowser runner, error/input/capture collection).

Pinned assets (no runtime CDN; everything served from a local ephemeral server):

- React `18.2.0` UMD production (`react`, `react-dom` from `node_modules`)
- Vue `3.5.13` (`vue.global.prod.js`)
- Playwright `1.48.2`

sha256 of every served bundle is recorded in `report.json` on each run.

## Layout

- `fixtures/*.html` — fixed SPA fixtures (vanilla, React, Vue, async, storage, frames)
- `fixtures/css/dynamic.css`, `fixtures/api/data.json` — fixed async resources
  (`/api/data.json` is served with a 150 ms delay to make fetch observably async)
- `runner.mjs` — server + acceptance runner
- `results/` — default output (gitignored)

## Cases (same suite for every browser)

| case | fixture | exercises |
| --- | --- | --- |
| vanilla-form | vanilla.html | required-field rejection, focus, typing, Tab focus move, checkbox check, form submit result |
| react-counter | react.html | React 18.2 state via 3 real clicks |
| react-controlled-input | react.html | controlled input via real keystrokes |
| react-list | react.html | list add + controlled draft reset |
| react-conditional | react.html | conditional render on/off |
| vue-counter / vue-controlled-input / vue-list / vue-conditional | vue.html | Vue 3.5.13 equivalents (v-model, v-for, v-if) |
| async-fetch | async.html | delayed fetch → loading → success render |
| async-router | async.html | hash routing via real link clicks + back/forward |
| async-stylesheet | async.html | dynamically injected stylesheet applies (computed color) |
| storage-reload | storage.html | localStorage persistence across reload |
| frame-nested | frames.html | same-origin iframe: child clicks/typing + parent reads contentDocument |
| geometry-rect | frames.html | getBoundingClientRect, computed width, elementFromPoint hit test |

`frame-nested` and `geometry-rect` are separate cases so frame and geometry
failures are reported independently.

## Usage

```bash
npm ci --prefix tests/spa

# Reference Chrome (must pass; test is invalid otherwise)
node tests/spa/runner.mjs --chrome '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' \
  --runs 1 --out /tmp/oxi-spa-reference

# OxiBrowser over CDP (start the engine separately, e.g. `oxibrowser serve`)
node tests/spa/runner.mjs --endpoint http://127.0.0.1:9222 --runs 10 --out tests/spa/results/oxi

# Both targets in one invocation (same fixture suite, one report)
node tests/spa/runner.mjs --chrome <path> --endpoint http://127.0.0.1:9222 --runs 10 --out <dir>
```

Flags: `--endpoint` (connectOverCDP to a running engine), `--chrome` (isolated
headless reference launch, temp profile), `--runs N` (default 10), `--out DIR`
(default `tests/spa/results`). At least one target required.

For acceptance, run the suite in a new engine process on each repetition:

```bash
cargo build --release --features browser --bin oxibrowser
python3 tests/spa/run_processes.py --binary target/release/oxibrowser \
  --chrome /path/to/chrome --runs 10 --out tests/spa/results/acceptance
node tests/spa/stability.mjs --binary target/release/oxibrowser \
  --cycles 100 --out tests/spa/results/stability
```

The process launcher records per-run results, binary and fixture hashes, host
metadata, successful-case timings and failures in `summary.json`. Readiness is
sampled at 250 ms intervals; its observed time is not a precise startup benchmark.
The stability runner reloads one React page repeatedly, checks two concurrent
storage contexts, and records sampled process RSS separately from latency.
Each runner closes the contexts, servers and processes it creates.

## Rules the runner enforces

- Input only via Playwright input APIs (`locator.click`, `keyboard.type`,
  `keyboard.press`); `evaluate` is observation-only (reading DOM/CSS/geometry).
- 30 s hard timeout per case; timeout = failure.
- Uncaptured `pageerror`, console errors, and failed requests fail the case.
- Steps execute sequentially; the first failure aborts the case and is recorded
  (nothing silently skipped or retried).
- Per case-run: status, errors, step log, timing, end-of-case screenshot in
  `<out>/screenshots/`. PNG dimensions must match the configured viewport.
- `report.json` + `report.md` written to `--out`; non-zero exit on any failure.

## Interpreting results

Reference Chrome must pass every case; a reference failure invalidates the
comparison. The process launcher stops before candidate runs when that reference
fails. Candidate failures, timeouts and missing screenshots fail acceptance.
Timings include only successful cases. A pass covers these pinned fixtures and
versions; it does not establish complete framework or Web Platform conformance.
