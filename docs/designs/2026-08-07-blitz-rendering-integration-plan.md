# Blitz Rendering Integration — Implementation Plan (remaining work)

> **Status of this plan:** Phases 1, 2a, 2b, 2c are SHIPPED on branch
> `feat/blitz-rendering` (commits `60b0042`, `dfae6e9`, `83cca96`, `03d364d`).
> Real CSS screenshots — including correct post-JS live-DOM rendering — already
> flow through CDP `Page.captureScreenshot` and the CLI `session screenshot`.
> This document covers the REMAINING architectural unification: retire the
> `oxibrowser-webapi` DOM and make `BaseDocument` the single source of truth
> that boa mutates directly. It is grounded in findings from the shipped work.

**Goal:** Eliminate the double-DOM (`oxibrowser-webapi::Document` + serialize
bridge) so boa's JS thread mutates a `RenderDocument` (Blitz `BaseDocument`)
directly — removing the snapshot/mutation channel plumbing and the per-capture
serialize→reparse cost.

**Architecture:** `JsRuntime`'s JS thread owns a `RenderDocument` alongside
boa's `Context`. JS DOM bindings call `RenderDocument`'s `&mut self` API
(proven in Phase 2b). The async side reaches the DOM only through channel
commands on `JsRuntime` (`capture_png`, `query_selector`, `dom_snapshot`).
See the "Threading Model" section of the design spec.

**Tech Stack:** `oxibrowser-render` (Blitz-dom/html/paint + anyrender_vello_cpu),
`boa_engine`, existing `mpsc` channel pattern in `runtime.rs`.

## Global Constraints

- `BaseDocument` is treated as `!Send` — it stays on the JS thread.
- Never break the workspace build between tasks; each task compiles + tests green.
- `oxibrowser-render`'s public API (Phase 2b) is the contract JS bindings target:
  `from_html`, `create_element`, `create_text_node`, `append_child`,
  `set_attribute`, `remove_attribute`, `set_inline_style`, `set_text`,
  `remove_node`, `query_selector(_all)`, `tag_name`, `node_attr`, `node_text`,
  `capture_png`.
- Conventional commits; squash-merge.

---

### Task 1: Add `RenderDocument` ownership + async façades to `JsRuntime`

**Files:**
- Modify: `crates/oxibrowser-core/src/js/runtime.rs` (`JsRuntime` struct ~336, command/response enums ~167-225, `js_thread_loop` ~601)
- Modify: `crates/oxibrowser-core/src/js.rs` (re-exports)

**Interfaces:**
- Consumes: `oxibrowser_render::RenderDocument` (Phase 2b API)
- Produces:
  - `JsRuntime::set_document(&self, html: &str, base_url: &str, viewport)` — async, ships a command
  - `JsRuntime::capture_png(&self, opts) -> Result<Vec<u8>>` — async façade
  - `JsRuntime::query_selector_all(&self, sel) -> Vec<NodeInfo>` — async façade
  - New `JsCommand` variants: `SetDocument{..}`, `Capture{opts, reply: oneshot::Sender}`, `Query{sel, reply}`
  - The JS thread stores `Option<RenderDocument>` and handles these commands.

- [ ] **Step 1:** Extend `JsCommand`/`JsResponse` with `SetDocument`, `Capture`, `Query` variants (and a serializable `NodeInfo`).
- [ ] **Step 2:** Add `render_doc: Option<RenderDocument>` to the JS thread state in `js_thread_loop`; handle `SetDocument` (build via `RenderDocument::from_html`).
- [ ] **Step 3:** Handle `Capture` by calling `render_doc.capture_png` and sending bytes back over the `oneshot`.
- [ ] **Step 4:** Add the async façade methods on `JsRuntime` mirroring `evaluate()`'s channel pattern.
- [ ] **Step 5:** Unit test: `set_document` then `capture_png` returns a valid PNG (no JS yet).
- [ ] **Step 6:** Commit `feat(render): JsRuntime owns RenderDocument + async façades`.

### Task 2: Rewrite `document`/`element` JS bindings to target `RenderDocument`

**Files:**
- Modify: `crates/oxibrowser-core/src/js/runtime.rs` — `register_document_object` (~3065), `create_element_object` (~4030), `register_window_globals` (~6176)
- The JS thread's `render_doc: &mut RenderDocument` replaces the `Arc<RwLock<Option<DomSnapshot>>>` + `Arc<RwLock<Vec<DomMutation>>>` arguments.

**Interfaces:**
- Consumes: Task 1's `RenderDocument` handle on the JS thread
- Produces: JS `document.createElement/appendChild/...`, `element.style`/`setAttribute`/`textContent`, `querySelector` — all calling `RenderDocument` methods directly (no mutation log).

- [ ] **Step 1:** Scope a Web API subset to port first: `document.{createElement,createTextNode,getElementById,querySelector,querySelectorAll,body,head,title}`, `element.{appendChild,setAttribute,removeAttribute,style,textContent,innerHTML}`, `document.createTextNode`. Port these to call `RenderDocument`.
- [ ] **Step 2:** Remove the `DomSnapshot`/`DomMutation` plumbing from these functions; thread `&mut RenderDocument` instead.
- [ ] **Step 3:** Keep `document.title`/form/event bindings working (they may read the doc).
- [ ] **Step 4:** Test: a JS snippet that `createElement`s a styled node, then `capture_png` reflects it (reuse the Phase 2b `mutation_reflected_in_capture` idea but driven by JS).
- [ ] **Step 5:** Commit `feat(render): JS DOM bindings target BaseDocument`.

### Task 3: Rewire `Session` mutation flow + `Frame`/`Page`

**Files:**
- Modify: `crates/oxibrowser-core/src/session.rs` (`apply_mutations` ~740, `inject_dom_snapshot` ~718, the JS-runner loop ~705)
- Modify: `crates/oxibrowser-core/src/frame.rs`, `page.rs`

**Interfaces:**
- Consumes: Task 1/2 (JS now mutates `RenderDocument` directly)
- Produces: `Page::to_screenshot_png_live` delegates to `JsRuntime::capture_png` (no more serialize bridge); `Frame` no longer owns a `oxibrowser-webapi::Document`.

- [ ] **Step 1:** Remove `drain_mutations`/`apply_mutations`/`inject_dom_snapshot` from the JS run loop (mutations are now live on `RenderDocument`).
- [ ] **Step 2:** `Page::to_screenshot_png_live` → `self.js_runtime.capture_png(opts).await` (delete the serialize→reparse body).
- [ ] **Step 3:** Update CDP DOM domain (`dom.rs`) and `extract`/`output` to read via `JsRuntime::query_selector_all`/`dom_snapshot` rather than `Frame::document`.
- [ ] **Step 4:** Test: existing CDP + extract test suites pass against the new path.
- [ ] **Step 5:** Commit `refactor(render): drop snapshot/mutation bridge, route via JsRuntime`.

### Task 4 (Phase 3): Delete `oxibrowser-webapi` + legacy renderers

**Files:**
- Remove: `crates/oxibrowser-webapi/**`
- Remove: `crates/oxibrowser-core/src/css/screenshot.rs`, `css/visual.rs`, legacy `css/layout.rs`
- Modify: workspace `Cargo.toml` (drop member + `html5ever 0.29`/`markup5ever`/`string_cache` workspace deps from core), `crates/oxibrowser-core/Cargo.toml`
- Modify: `crates/oxibrowser-core/src/css/mod.rs` (drop re-exports)

- [ ] **Step 1:** Confirm no remaining references to `oxibrowser_webapi` (`grep -r oxibrowser_webapi crates/`).
- [ ] **Step 2:** Delete the crate and its workspace member entry; remove the path dep from `oxibrowser-core`.
- [ ] **Step 3:** Replace the CDP `text_to_png` fallback (`cdp/page.rs` ~373) with a blank-PNG fallback; delete `css::screenshot`/`css::visual`.
- [ ] **Step 4:** Remove `html5ever 0.29`/`markup5ever`/`string_cache` from core (Blitz owns parsing now).
- [ ] **Step 5:** `cargo build --workspace` + `cargo test -p oxibrowser-core -p oxibrowser-cdp -p oxibrowser-render` green; note binary-size/build-time delta.
- [ ] **Step 6:** Commit `chore(render): remove legacy webapi DOM and bitmap renderers`.

---

## Self-Review

- **Spec coverage:** Tasks 1–4 cover the design spec's Phase 2 (cutover) and Phase 3 (cleanup). Task 2 is the largest (JS bindings) — scoped to a Web API subset first.
- **Type consistency:** All tasks use the `RenderDocument` API as defined in Phase 2b (`oxibrowser-render/src/document.rs`).
- **Pre-existing caveat:** `crates/oxibrowser/tests/integration.rs` references `oxibrowser_core` without a dev-dep and fails to compile on `main` — unrelated to this work; fix separately if desired.
