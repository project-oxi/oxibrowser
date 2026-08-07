# Blitz Rendering Integration — Design Spec

**Date:** 2026-08-07
**Status:** Phase 1 + Phase 2a/2b/2c SHIPPED on `feat/blitz-rendering` (commits `60b0042` `dfae6e9` `83cca96` `03d364d`). Real CSS screenshots — including correct post-JS live-DOM rendering — flow through CDP `Page.captureScreenshot` and CLI `session screenshot`. Remaining: full DOM unification (JsRuntime owns `RenderDocument`; retire `oxibrowser-webapi`) — see `2026-08-07-blitz-rendering-integration-plan.md`.
**Supersedes:** text-based screenshot rendering (`css/screenshot.rs`, `css/visual.rs`)

## Goal

Replace oxibrowser's text-based screenshot renderer (8×16 bitmap-font PNG) with a
real CSS-aware HTML rendering pipeline backed by the [Blitz](https://github.com/DioxusLabs/blitz)
engine, producing pixel-accurate-enough screenshots comparable to a headless
Chromium — while keeping OxiBrowser's differentiators (pure-Rust `boa_engine` JS,
CDP compatibility, AI extensions) intact.

## Background & Motivation

Today `Page::to_screenshot_png` extracts DOM text and draws it with a monospace
bitmap font (`css/screenshot.rs`). The author's own comment admits it: *"This is
NOT pixel-perfect Chromium rendering — it's a text-based visual approximation."*
CDP `Page.captureScreenshot` and `OXI.getBoxModelScreenshot` both feed through this
path, so Playwright/Puppeteer receive a terminal-style PNG rather than a faithful
render.

The Rust ecosystem now has every piece needed for a real pipeline:

| Concern         | Crate (Blitz-internal)                         | Pure Rust | CPU/headless |
| --------------- | ---------------------------------------------- | --------- | ------------ |
| HTML parsing    | `blitz-html` (html5ever 0.39)                  | ✅        | ✅           |
| CSS cascade     | `blitz-dom` → **Stylo** (Servo's style engine) | ✅        | ✅           |
| Box layout      | `blitz-dom` → **Taffy** (flexbox/grid/block)   | ✅        | ✅           |
| Text layout     | `blitz-dom` → **Parley** + skrifa              | ✅        | ✅           |
| Paint → scene   | `blitz-paint` → **anyrender**                  | ✅        | ✅           |
| Scene → pixels  | **anyrender_vello_cpu** (vello_cpu)            | ✅        | ✅           |

Blitz's own `apps/browser/src/capture.rs` already proves the headless capture
pattern: `render_to_buffer::<VelloCpuImageRenderer, _>(...)` produces a `Vec<u8>`
RGBA buffer with no GPU. We adopt that exact pattern.

## Decisions (locked)

1. **DOM strategy — BaseDocument unification.** `oxibrowser-webapi`'s
   `Document`/`Node`/`Tree` is retired. Blitz `BaseDocument` becomes the single
   source of truth for the DOM. Rationale: cleanest final state; avoids a
   snapshot/mutation bridge between two DOM representations; user preference for
   elegance over effort.
2. **Migration — parallel crate then cutover.** A new `oxibrowser-render` crate
   is built in isolation (existing code untouched, workspace stays green), then
   `oxibrowser-core` is rewired onto it in one cutover, then legacy DOM/render
   code is deleted. Rationale: isolates the heavy Blitz dependency tree
   (Stylo, html5ever 0.39, vello) from core's html5ever 0.29 during construction.
3. **Backend — `anyrender_vello_cpu` only.** Single CPU renderer; no GPU path
   (headless server target). The anyrender abstraction leaves a future
   tiny-skia/Skia/GPU swap open without a redesign.

## Architecture (final state)

```
                          ┌─────────── oxibrowser-core ───────────┐
                          │  Page ─ Frame ─ JsRuntime              │
                          │      │            │                    │
                          │      │            │  (channel commands)│
                          └──────┼────────────┼────────────────────┘
                                 │            │
                                 ▼            ▼
   ┌───────────── oxibrowser-render (new) ──────────────────────────┐
   │  RenderDocument  ◀── owns ── JsRuntime's JS thread             │
   │   │  (wraps BaseDocument)                                       │
   │   ├─ from_html      : blitz-html parse → Stylo cascade → Taffy  │
   │   ├─ DOM mutators   : &mut self (JS-thread only)                │
   │   └─ capture_png    : blitz-paint → vello_cpu → PNG             │
   │                                                                 │
   │  Dependency tree (isolated here):                               │
   │   blitz-dom, blitz-html, blitz-paint, anyrender_vello_cpu       │
   └─────────────────────────────────────────────────────────────────┘
```

The new crate is the **only** place that depends on Stylo / html5ever 0.39 /
vello. After cutover, `oxibrowser-core`'s direct html5ever 0.29 dependency is
removed (parsing delegated to the render crate).

## Threading Model

> This section resolves the constraint that `boa_engine::Context` is `!Send` and
> that Stylo-derived `BaseDocument` is `!Send` in practice.

### Current model (pre-integration)

- `JsRuntime` is `Send+Sync` *only* because boa's `Context` runs on a dedicated
  `std::thread` (`runtime.rs:8-10`). Async ↔ JS communication is via `mpsc`.
- The DOM (`oxibrowser-webapi::Document`) lives on the **async** side, owned by
  `Frame`. JS reads a serialized `DomSnapshot` copy
  (`Arc<RwLock<Option<DomSnapshot>>>`), and pushes `DomMutation`s back
  (`Arc<RwLock<Vec<DomMutation>>>`).
- `to_screenshot_png(&self)` runs on the async side and is cheap/Send-safe
  because it only extracts text.

### New model (post-integration)

`BaseDocument` is **`!Send`** (Stylo's `atomic_refcell`, parley/vello state), so
it cannot live behind `Arc<RwLock<_>>` on the async side. It moves **onto the JS
thread**, co-located with boa's `Context`. This mirrors a real browser's main
thread: DOM + JS + paint in one thread.

```
   async thread (tokio)                 JS thread (std::thread, !Send zone)
   ┌────────────────────┐               ┌──────────────────────────────────┐
   │ Page / Session     │               │ boa Context (!Send)              │
   │ CDP handlers       │── JsCommand →│  + RenderDocument (BaseDocument)  │
   │ extract            │               │                                   │
   │                    │← JsResponse ──│  evaluate()   → mutates DOM (&mut)│
   │ JsRuntime::        │               │  capture_png() → paints sync      │
   │  capture_png() ────┼──RenderCmd──→│  query_selector() → reads DOM     │
   │  query_dom()  ─────┼──QueryCmd──→│                                   │
   └────────────────────┘               └──────────────────────────────────┘
```

Consequences:

- `RenderDocument`'s `&mut self` mutators and `&self` `capture_png` are **valid**:
  they are only ever called from within the JS thread (by JS bindings during
  `evaluate`, or by a render command during capture). They are `pub(crate)`-ish
  to the render crate, never exposed across threads.
- **Capture blocks JS by design.** A capture request is processed between JS
  ticks on the JS thread, so the rendered state is a consistent snapshot (no
  half-applied mutations). This is a correctness benefit, not a limitation.
- **No more `DomSnapshot`/`DomMutation` plumbing.** JS mutates `BaseDocument`
  directly. The async side reads the DOM via query commands
  (`JsRuntime::query_dom`, `::serialize`) that ship a serializable result back —
  reusing the existing channel pattern with new message types rather than a new
  concurrency primitive.
- `JsRuntime` gains async façade methods (`capture_png`, `query_selector`,
  `dom_snapshot`) that internally enqueue a command and await a `oneshot`
  response — exactly how `evaluate` works today.

### Why not `Arc<RwLock<RenderDocument>>`?

Would require `BaseDocument: Send + Sync`. Stylo types use `atomic_refcell`
(`!Sync`) and the document holds non-Send renderer state. It would not compile.
Ruled out.

### Why not BaseDocument on the async side + message-based JS?

Would force every DOM touch from JS through a channel round-trip (today's
`DomMutation` pattern), serializing each `createElement`/`appendChild`/style
write into a command — far more traffic than the current batched mutation log,
and it re-introduces the exact two-DOM-representation problem unification was
meant to eliminate. Co-locating on the JS thread is strictly better.

## Public API

`oxibrowser-render` exposes two audiences:

### To `JsRuntime`'s JS thread (direct, in-thread)

```rust
pub struct RenderDocument { /* BaseDocument + viewport + base_url */ }

impl RenderDocument {
    // Construction (async side calls via a command; JS thread builds it)
    pub fn from_html(html: &str, base_url: &str, viewport: Viewport) -> Result<Self>;

    // Capture
    pub fn capture_png(&self, opts: &CaptureOpts) -> Result<Vec<u8>>;  // PNG bytes

    // In-thread DOM access — used by boa bindings (Phase 2) and query commands
    pub fn root_node_id(&self) -> NodeId;
    pub fn create_element(&mut self, tag: &str) -> NodeId;
    pub fn append_child(&mut self, parent: NodeId, child: NodeId);
    pub fn set_text(&mut self, node: NodeId, text: &str);
    pub fn set_attribute(&mut self, node: NodeId, k: &str, v: &str);
    pub fn set_inline_style(&mut self, node: NodeId, prop: &str, val: &str);
    pub fn remove_node(&mut self, node: NodeId);
    // Query (returns serializable data for async-side consumers)
    pub fn query_selector_all(&self, sel: &str) -> Vec<NodeId>;
    pub fn node_text(&self, node: NodeId) -> String;
    pub fn node_html(&self, node: NodeId) -> String;
    pub fn computed_style(&self, node: NodeId) -> Vec<(String, String)>;
    pub fn layout_rect(&self, node: NodeId) -> LayoutRect;
}
```

### To the async side (`oxibrowser-core` via `JsRuntime`)

```rust
impl JsRuntime {
    // Existing: pub async fn evaluate(&self, script: &str) -> Result<JsEvalResult>;

    // New async façades (ship a command to the JS thread, await oneshot):
    pub async fn capture_png(&self, opts: CaptureOpts) -> Result<Vec<u8>>;
    pub async fn set_document(&self, html: &str, base_url: &str, viewport: Viewport);
    pub async fn query_selector_all(&self, sel: &str) -> Result<Vec<NodeInfo>>;
    pub async fn dom_snapshot(&self) -> Result<DomSnapshot>; // serialized view for CDP/extract
}
```

`Page::to_screenshot_png` becomes `self.js_runtime.capture_png(opts).await`.

## Migration Phases

### Phase 1 — Isolated construction (`oxibrowser-render`, no cutover)

Build the static pipeline end-to-end in the new crate. Existing workspace code
is **not modified**. Exit criteria: given an HTML string,
`RenderDocument::from_html(...)?.capture_png(...)` produces a CSS-laid-out PNG.

- New crate `crates/oxibrowser-render` added to workspace.
- Dependencies: `blitz-dom`, `blitz-html`, `blitz-paint`, `anyrender_vello_cpu`
  (git rev pinned + the two `[patch.crates-io]` Blitz itself uses).
- Implement `from_html` (parse → create doc → resolve styles → layout) and
  `capture_png` (paint scene → vello_cpu → RGBA → PNG).
- Standalone test/example renders a few HTML fixtures to PNG; inspect output.

### Phase 2 — Cutover (`oxibrowser-core` rewired)

- `Frame`/`Page` own a `RenderDocument` handle (built and driven by the JS thread).
- `JsRuntime` gains the async façade methods; the JS thread constructs/owns
  `RenderDocument` and boa bindings call its `&mut self` mutators.
- CDP `Page.captureScreenshot`, `OXI.getBoxModelScreenshot`, CLI `screenshot`,
  `extract`/`output` are re-pointed at `RenderDocument` query methods.
- Existing `cargo test --workspace` must pass; screenshot regression suite added.

### Phase 3 — Cleanup

- Delete `oxibrowser-webapi` crate (`Document`/`Node`/`Tree`).
- Delete `css/screenshot.rs`, `css/visual.rs`, legacy `css/layout.rs`.
- Remove `oxibrowser-core`'s direct html5ever 0.29 / markup5ever / string_cache
  deps (now owned by `oxibrowser-render`).
- Confirm binary size and build time.

## Error Handling

- Blitz/Stylo/vello_cpu errors mapped into `CoreError` variants
  (`RenderError(String)`, `ParseError(String)`).
- Capture failure falls back to a blank white PNG of the requested viewport
  (preserves the current "never hard-fail a screenshot" contract).

## Testing

- **Phase 1:** HTML fixtures (`simple.html`, `flex.html`, `grid.html`,
  `styled-text.html`) rendered to PNG; assert non-trivial (not blank, correct
  dimensions, expected dominant colors in regions). Visual inspection checkpoint.
- **Phase 2:** JS-mutate-then-capture consistency (e.g.
  `el.style.backgroundColor='red'` reflected in capture).
- **Phase 2:** re-run the existing CDP/extract test suites against the new DOM.
- Reuse a slice of Blitz's WPT cases where tractable.

## Risks & Mitigations

| Risk | Severity | Mitigation |
| --- | --- | --- |
| vello_cpu maturity / render quality | Med | Phase 1 quality gate before Phase 2. Fallback: implement an anyrender tiny-skia backend (~hundreds of LOC) — a backend swap, not a redesign. |
| Blitz 0.3.0-beta.1 pre-alpha API churn | Med | Pin git rev; carry the two `[patch.crates-io]` entries Blitz uses. |
| `BaseDocument` `!Send` assumption wrong | Low | If it were `Send`, the threading section simplifies — but we assume not and design for it; verification in Phase 1. |
| JS binding rewrite scale (dom_snapshot.rs ≈ 57KB) | Med | Phase 2 scopes a Web API subset first, expands incrementally. |
| Build time / binary size growth (Stylo + vello) | Med | Phase 3 measures; profile `[profile.production]` lto/codegen already tuned in Blitz. |

## Out of Scope

- GPU rendering (vello). Headless-only for now.
- `<canvas>`, WebGL, WebGPU, video.
- Layout/animation of dynamic content beyond what JS mutation + re-capture covers.
- iframe subframe rendering (top-level document only initially; revisit in Phase 3).
