# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).


## [Unreleased]

## [0.21.1] - 2026-09-14

### Fixed
- **DocumentFragment `appendChild` no longer kills the JS thread** — `document.createDocumentFragment` was a stub returning a plain object with a bogus `__nodeId` (1100000); appending it into the document forwarded that id to the Blitz render doc, which panicked with "invalid key", and the unwind permanently killed the session's JS thread (every later `Runtime.evaluate` failed with "JS thread has died" while `captureScreenshot` kept returning 200 via its blank-PNG fallback). `DocumentFragment` now tracks its children, element `appendChild` splices them into the document per spec (emptying the fragment and flattening nesting), and native-binding panics on the eval path are contained into a per-evaluation error instead of killing the thread. Follow-up hardening: warn instead of silently dropping fragment children lacking a `__nodeId`, gate `connectedCallback` firing on an actual insertion, contain panics in `SetDocument`/`SetFrameDocument` navigation scripts too (the frame context stays registered), and guard the fragment stub against self-append (infinite flatten loop) and null children (TypeError per spec). Fixes #2.
- **Bing search results decoded past tracking redirects** — organic results wrapped in `bing.com/ck/a` tracking links now expose the real destination (base64url payload after `u=a1`) instead of the tracking URL, and response bytes are decoded as UTF-8 directly because Bing mislabels charset headers on some locales, which mojibake'd CJK snippets through reqwest's `.text()`.

### Security
- **h2 bumped to 0.4.19** — resolves the RUSTSEC advisory on unbounded DATA frames (Cargo.lock-only bump; no manifest change).

## [0.21.0] - 2026-08-11

### Added
- **`Tab::print_to_pdf`** — PDF export is now a first-class `oxibrowser-core` Rust API, not just a CDP method. `Tab::print_to_pdf(width)` captures the rendered page and returns raw PDF bytes (mirroring `Tab::screenshot`), emitting a `BrowserEvent::PdfExported` event and surfacing encode failures as `CoreError::PdfError`. The PNG→PDF transform moved out of the CDP crate into `oxibrowser_render::png_to_pdf` (sibling to `blank_png`), re-exported from core; `Page.printToPDF` now delegates to it. The `printpdf` dependency moved from `oxibrowser-cdp` to `oxibrowser-render`.
- **WebAssembly support** — `WebAssembly.validate`/`compile`/`instantiate`/`Module`/`Instance`/`Memory`/`Table` now work via a `wasmi` (pure-Rust WASM interpreter) ↔ `boa_engine` bridge, closing the "WASM: no" gap. Pages using WASM 1.0 (MVP) modules — compile, instantiate, call exported functions, host-function imports (JS callbacks), and linear memory (`new WebAssembly.Memory`/`grow`/`buffer.byteLength`) — run the same as in a real browser. `CompileError`/`LinkError`/`RuntimeError` are `instanceof`-correct subclasses. **Fuel metering is enabled**: the wasmi engine is built with `consume_fuel(true)` and each `Store` gets a 10M-instruction budget, so an infinite WASM loop traps as a `RuntimeError` instead of hanging the JS thread — the same termination guarantee V8 provides (verified by `test_wasm_fuel_exhaustion`). Everything runs synchronously on the JS thread (wasmi handles are `!Send`, matching boa); a thread-local raw pointer lets host-function imports reach the active `Context`. Binary cost: +2.1 MiB (46 MB stripped vs. 44 MB). New module `oxibrowser-core/src/js/wasm.rs`, wired into `create_context`. 10 unit tests + a 5/5 CDP smoke test cover the path end-to-end. Limitations (out of MVP scope): `i64` returns via JS `Number` (lossy past ±2⁵³), global imports, real `ArrayBuffer` memory views, SIMD/shared-memory, and streaming compilation are deferred; the fuel budget is fixed (not yet configurable via `JsRuntimeConfig`).

## [0.20.0] - 2026-08-10

### Fixed
- **Nested-iframe contexts (W3b)** — child frames beyond depth 1 are now discovered (`DomSnapshot::extract_iframes` / `iframe_srcs` walk the tree instead of scanning top-level nodes) and given their own JS execution context (`Session::populate_iframes` populates every frame in the tree via BFS; `Session::inject_child_frames` walks the full frame tree before issuing `SetFrameDocument` commands). `Page.getFrameTree` now reports the complete recursive hierarchy and `Runtime.evaluate { contextId: <grandchild> }` reaches the grandchild DOM. New `Frame::find_by_id`, `find_mut_by_id`, and `find_by_frame_id_str` helpers back the lookup. Closing this unblocks `window.parent`/`window.top` (W3c) and dynamic-iframe creation (W3d). Regression: `acceptance/nested/run.sh` 7/7 PASS.
- **External `<link rel=stylesheet>` applied (W2-pre / §5.2)** — pages with `<link rel=stylesheet href="/x.css">` no longer panic in Blitz's parser. New `dom_link` module (regex-based `external_stylesheet_links` / `strip_stylesheet_links` / `inject_inline_style`, compiled once via `std::sync::LazyLock`) is invoked from `Session::populate_html` *before* `Page::from_html`: each stylesheet is fetched through the existing `http_client`, folded into a single inline `<style>` block, and the `<link>` tag is stripped. Regression: `acceptance/external-stylesheet/run.sh` 4/4 PASS — pixels of the antialiased `#008000` rule reach paint. Note: `getComputedStyle` from `LayoutEngine` only inspects inline `style=` attributes; applied CSS rules show up in `Page.captureScreenshot` output (paint), not in the JS-side `getComputedStyle` object.
- **`window.addEventListener` mirror (W3a / §5.3)** — `window.addEventListener('load', cb)` no longer throws. The JS bootstrap installed `addEventListener`/`removeEventListener`/`dispatchEvent` on `globalThis`, but `window` was a distinct JS object without those methods. Mirror at the end of `HISTORY_LOCATION_BOOTSTRAP` (same shape as the existing `matchMedia` mirror) copies the three methods onto `globalThis.window` with `||` so a future Rust-side stub still wins. Regression: `acceptance/window-ael/run.sh` 3/3 PASS.
- **Relative fetch URL resolution (W3b / §5.3)** — `fetch('/api/x')` no longer rejects with `TypeError: Invalid URL`. The Rust-side fetch binding expects an absolute URL; the JS-side `resolveUrl` helper (already in `HISTORY_LOCATION_BOOTSTRAP` for history/location) now wraps `globalThis.fetch` and `window.fetch` to join relative refs against the current page URL before delegating. Regression: `acceptance/relative-fetch/run.sh` 3/3 PASS.
- **`hashchange` fires on `location.hash = '#x'` (W3c / §5.3)** — assigning `window.location.hash = '#x'` now dispatches a `hashchange` event with `oldURL`/`newURL`. The host `Location`'s native `hash` setter is non-configurable, so `Object.defineProperty` was shadowed by it; instead, `HISTORY_LOCATION_BOOTSTRAP` wraps `window.location` in a `Proxy` whose `set` handler intercepts the `hash` property, mutates the top history entry, and calls a small `fireHashchange` helper that dispatches to listeners installed via the standard `addEventListener('hashchange', cb)`. Initial fires a `hashchange` when PAGE_URL carries a fragment. Regression: `acceptance/hashchange/run.sh` 3/3 PASS.

## [0.19.0] - 2026-08-10

### Added
- **`@font-face` webfont loading** — inline `<style>` `@font-face` rules are scanned for font URLs, the font files are fetched through the network stack, and the bytes are registered into a Parley `FontContext` (`Collection::register_fonts`) carried into layout via Blitz's public `DocumentConfig.font_ctx`. Custom webfonts now reach Stylo/Taffy text shaping — **no Blitz fork required** (the prior "fork required" premise confused the svg/usvg `FONT_DB` with the text-layout `FontContext`; verified by a spike + end-to-end render). `RenderDocument::from_html_with_fonts`, `HttpClient::fetch_bytes`, and a `fonts` module (`extract_font_face_urls`) are the new surface.
- **`srcdoc` / `about:blank` iframe contexts** — `<iframe srcdoc="…">` now builds a child frame from the inline HTML (no fetch), and non-http(s) `src` (`about:blank`, `javascript:`) builds an empty child frame. Both get their own JS execution context, so `Runtime.evaluate` with the child `contextId` reaches their DOM. A new `DomSnapshot::extract_iframes` surfaces `src` + `srcdoc`.
- **Acceptance harness** — a self-contained, committed end-to-end harness (`acceptance/`) drives a mock JS SPA (navigate → form fill → submit → async fetch → dashboard render → screenshot) over raw CDP against `oxibrowser serve`, proving the core automation loop and serving as the roadmap §6 acceptance-test baseline that was previously missing. Baseline: 8/8 PASS in ~6.5s.
- **JS-fetch interception e2e** — `acceptance/fetch-intercept.ts` verifies the full `Fetch` domain round-trip left unit-tested-but-unverified since `e031352`: page `fetch()` → `Fetch.requestPaused` → `Fetch.fulfillRequest` → the fetch promise resolves to the fulfilled body. 4/4 PASS.

### Fixed
- **`Fetch.fulfillRequest` body decoding** — the handler only base64-decoded the body when a `base64Encoded` param was present, but CDP's `Fetch.fulfillRequest.body` is base64-encoded by spec with no such flag (Chrome/Playwright/Puppeteer always send base64). An intercepted fetch therefore resolved to the raw base64 string instead of the decoded body. Now decoded unconditionally.
## [0.18.0] - 2026-08-10

### Added
- **Tracing domain** — `Tracing.start`/`Tracing.end`/`Tracing.getCategories` implemented. `end` emits `Tracing.dataCollected` (a minimal Chromium-format trace with a `TracingStartedInBrowser` metadata event) + `Tracing.tracingComplete`, satisfying the Playwright `page.tracing.start()`/`stop()` contract. A full timeline/network tracer is out of scope.
- **Multi-tab** — `Target.createTarget` now creates a real Browser session (was a fake-targetId stub) and emits `Target.targetCreated`/`Target.attachedToTarget`. The flat-protocol dispatcher routes incoming commands by `sessionId` to the attached child session (a `child_targets` map), so `context.newPage()` yields a drivable tab (navigate/evaluate/DOM). Child-target lifecycle events (load, etc.) still require a per-child CoreEvent drainer — noted as remaining work.
- **Cookie expiry / `Max-Age`** — `CookieEntry` now parses `Expires` (HTTP-date via `httpdate`) and `Max-Age`; `CookieJar::store` computes an absolute expiry. `Max-Age <= 0` and past `Expires` delete any existing matching cookie; expired cookies are purged lazily on read. Closes the Phase 6 cookie-expiry gap.
- **Public Suffix List** — cookie `Domain=` attributes are rejected when they scope to a bare public suffix (e.g. `co.uk`, `com`) via the bundled Mozilla PSL (`psl` crate). A `registrable_domain` (eTLD+1) helper is exposed for partition keys.
- **Cookie-name prefixes (`__Host-` / `__Secure-`)** — RFC 6265bis §4.1.3 prefix validation: `__Secure-` requires the `Secure` attribute; `__Host-` requires `Secure` + `Path=/` + no `Domain`. Violations are rejected.
- **CORS + preflight** — cross-origin requests now carry an `Origin` header and perform a CORS preflight (`OPTIONS`) when the request is not "simple" (non-safelisted method/header); the preflight response's `Access-Control-Allow-Origin/-Methods/-Headers` are validated and the request is blocked on denial. New `network::cors` module (Fetch §3.2–3.3 policy).
- **`Page.printToPDF`** — now returns a real PDF (was an empty stub). Captures the rendered page and embeds it in a single-page PDF sized to the image via `printpdf` (`png` feature). The page-matched image replaces the previous empty `data: ""` response.
- **Per-frame JS execution contexts (Phase 8)** — each child iframe now gets its own isolated `boa_engine::Context` + `RenderDocument` on the JS thread, with independent globals, scripts, and DOM. `Runtime.evaluate` honors `contextId` to target a specific frame; `Page.getFrameTree` reports the full recursive frame tree; `executionContextCreated` is emitted per frame. Thread-local registries (listeners, fetch, WebSocket) are namespaced by `(context_id, node_id)` to prevent cross-frame collisions. Main-frame code paths (`contextId=1`) are unchanged — child frames are additive.
- **`Runtime.consoleAPICalled`** — every `console.log/info/warn/error` now mirrors to the sink (in addition to the existing captured-output buffer).
- **`Runtime.exceptionThrown`** — uncaught exceptions from `Runtime.evaluate` and navigation `<script>` tags push an exception event.
- **`Log.entryAdded`** — console messages are mirrored into the Log domain (gated by `Log.enable`, which now toggles a `log_enabled` flag).
- **JS-initiated `Network.*` lifecycle** — `fetch()` and `XMLHttpRequest` now emit `Network.requestWillBeSent` / `responseReceived` / `loadingFinished` (correlated via `oxi-{id}` request ids); WebSocket `send`/receive emit `Network.webSocketFrameSent` / `webSocketFrameReceived`.
- **Event-driven dialogs** — `alert` / `confirm` / `prompt` are now native closures that push `CoreEvent::Dialog` and block on a shared `DialogGate`, resolved by `Page.handleJavaScriptDialog`. Emits `Page.javascriptDialogOpening`; default-dismisses on timeout / no observer (matching real-browser unhandled-dialog semantics).
- **Custom-element lifecycle callbacks** — `connectedCallback` / `disconnectedCallback` fire on the render-doc `appendChild` / `remove` hooks; `attributeChangedCallback` fires on `setAttribute` (gated by `observedAttributes`). Driven by `__oxi_fire_connected` / `__oxi_fire_disconnected` / `__oxi_fire_attr_changed` helpers installed by the web-components bootstrap.
- **DOM layout-geometry methods** — `DOM.getBoxModel`, `DOM.getContentQuads`, and `DOM.getNodeForLocation` are now implemented, backed by `LayoutEngine::compute_rect` (the existing estimated-rect layout).
- **Shadow DOM slot composition (DomSnapshot-level)** — `attachShadow` now materializes a real shadow tree (a `SHADOW_ROOTS` registry on the JS thread), and `DomSnapshot::from_render_document` runs a compose pass that merges each host's shadow subtree and distributes its light-DOM children into `<slot>` positions by name (default + named slots; non-matching children dropped; slot fallback content) — the standard flattened tree. Shadow/slot content is now visible to every DomSnapshot-backed read: CDP `DOM.*`, `getBoxModel`/`getContentQuads`/`getNodeForLocation`, `OXI.*`, `extract`, accessibility, `LayoutEngine`, and (via the compose-then-feed path above) `capture_png`.
- **Shadow-aware screenshot rasterization** — `Page.captureScreenshot` / `capture_png` now reflect Shadow DOM composition. Blitz's `BaseDocument` is a single flat tree with no shadow/host/slot concept, so shadow + slotted content was invisible to rasterization. When shadow roots are registered, `capture_png` now builds the flattened `DomSnapshot` (compose pass), serializes it to HTML via `DomSnapshot::to_html`, reparses into a throwaway `RenderDocument` at the same viewport, and rasterizes that. The no-shadow fast path rasterizes the live document directly. Lossy by design (CSSOM inline styles / listeners / stylesheet computed styles are not in the snapshot); structural + `style=` fidelity is preserved. `RenderDocument::viewport()` accessor added.
- **Shadow DOM slot APIs** — `slot.assignedNodes()` / `slot.assignedElements()` return the light-DOM children distributed into a `<slot>` (refreshed from the live tree); `node.assignedSlot` resolves back to the slot for open shadow trees. Backed by `SLOT_ASSIGNMENTS` + `ASSIGNED_SLOT` registries populated during `distribute_slots`.
- **Closed-mode Shadow DOM** — `attachShadow({mode:'closed'})` threads `mode` into the `SHADOW_ROOTS` registry. Closed roots still render (Chrome paints closed shadow content) but are hidden from `element.shadowRoot` and `node.assignedSlot` (per the HTML spec).
- **`shadowRoot.innerHTML` setter + `append`** — the native shadow root now exposes `append(a, b, …)` and an `innerHTML` setter that parses the fragment and appends the nodes as shadow children (recreated in the live render doc, detached so the compose pass owns them).
- **Declarative shadow DOM** — `<template shadowrootmode="open|closed">` parsed at navigate time attaches a shadow root to its host; the template's content becomes the shadow tree and the host's light children distribute into `<slot>`s (`process_declarative_shadow_dom`, runs after `from_html`, before page scripts).
- **Typed `Runtime.consoleAPICalled` RemoteObjects** — console args now emit typed `RemoteObject`s (number/boolean/object/null/undefined) instead of always-stringifying. A new `ConsoleArg` enum (core, neutral) is classified in `console_fn` and mapped to `RemoteObject` by the CDP layer; `Log.entryAdded` text is reconstructed from `ConsoleArg::display()`.
- **`Runtime.exceptionThrown` error name** — the exception's `className` now uses the real error constructor name (e.g. `TypeError`) instead of a hardcoded `Error` (new `CoreEvent::Exception.name` field). Real source-level stack frames remain unavailable — boa 0.20 carries no locations on `JsNativeError` and leaves `Error.stack` undefined; the `.stack` string is surfaced best-effort.
- **CoreEvent drainer graceful shutdown** — the CDP CoreEvent drainer now exits promptly on a `tokio::sync::oneshot` shutdown signal fired at the end of `CdpSession::run` (in addition to the existing channel-disconnect fallback).
- **Page `<script>` execution on navigation (Phase 1 keystone)** — `Session::navigate` now runs the page's `<script>` tags in document order, fires `DOMContentLoaded`/`load`, and settles the timer/microtask queue. External `<script src>` are fetched and executed; inline + external ordering preserved; a thrown script does not abort siblings.
- **`window.matchMedia`** — minimal `MediaQueryList` (min/max-width derived from viewport; other queries default to `matches:false`). Installed on both `window` and `globalThis`.
- **Async (non-blocking) `fetch` / `XMLHttpRequest` (Phase 3)** — `fetch()` and `xhr.send()` return immediately instead of blocking the JS thread. Concurrent fetches run in parallel via per-request background tasks.
- **`WebSocket` (Phase 4)** — standard browser WebSocket API (full surface, ws + wss). One background tokio task per socket; events pump on the JS event loop.
- **`Element.matches`/`closest` + `URL.createObjectURL` + `AbortController`/`AbortSignal` (Phase 4 quick wins)** — selector-engine-based matches/closest, blob: URL minting, JS-bootstrap AbortController/AbortSignal with abort-event propagation.
- **`fetch` honors `AbortSignal`** — `fetch(url, { signal })` rejects with `AbortError` when the signal is already aborted or becomes aborted during the request.
- **`fetch` method/headers/body reach the wire** — `fetch(url, { method:'POST', body:'...' })` now sends method + headers + body on the wire (was GET-only). `FetchRequestMsg.body` changed from `Option<String>` to `Option<Vec<u8>>`.
- **WebSocket binary send** — `ws.send(Uint8Array/ArrayBuffer)` now emits a binary frame (was always text).
- **`FormData` + `Blob` + multipart `fetch` body (Phase 4)** — `Blob` and `FormData` Web APIs with `__oxi_serialize_body` producing `multipart/form-data` or raw bytes.
- **Canvas 2D context shim** — canvas elements gain a 2D context (full surface: fillRect, arc, fillText, drawImage, gradients, transforms, getImageData, etc.), plus a best-effort WebGL/WebGL2 context.
- **`customElements` define-and-createElement upgrade** — `createElement` of a registered custom element now upgrades the returned element with the constructor's prototype.
- **`window.alert`/`confirm`/`prompt`/`print` no-throw defaults (Dialog MVP)** — no-throw defaults installed on both `window` and `globalThis`.
- **CDP flat-protocol `sessionId` multiplex** — `Target.setAutoAttach`/`attachToTarget` stamps `sessionId` onto all subsequent target events. Verified end-to-end against `oxibrowser serve`.
- **CDP `Emulation` domain** — `setDeviceMetricsOverride`/`clearDeviceMetricsOverride`/`setVisibleSize`/`setUserAgentOverride` acknowledged.
- **CDP `DOM.*` method coverage expansion** — `requestNode`, `setAttributeValue`, `removeAttribute`, `removeNode`, `getProperty`, `setNodeValue`, `focus`, `scrollIntoViewIfNeeded`, `setFileInputFiles`.
- **CDP `Page.*` stubs** — `handleJavaScriptDialog`, `addScriptToEvaluateOnNewDocument`, `bringToFront`, `getNavigationHistory`, `setBypassCSP` acknowledged as no-ops.

### Changed

- **Concurrent CDP command dispatch** — `CdpSession::run` now spawns each command's dispatch as a task and routes responses back through a channel, so a long-running command (e.g. a dialog-blocked `Runtime.evaluate`) can no longer stall event forwarding or other commands. `Page.handleJavaScriptDialog` writes the shared dialog gate directly (no session lock) so it resolves a dialog even while a blocking evaluate holds the session write lock.
- **Blocking JS-thread recvs moved to `spawn_blocking`** — `JsRuntime::evaluate*` and `set_document_with_scripts` receive their command responses via `tokio::task::spawn_blocking`, so a long block (e.g. `alert()`) never stalls the async runtime or starves the CoreEvent drainer.

### Fixed

- **`wait_for` observes the live DOM + advances the event loop (Phase 2)** — `Tab::wait_for` and `Session::wait_for` polled the static navigate-time snapshot, so elements rendered after load by JS were invisible. Both now check via `evaluate`, which queries the live `RenderDocument` AND drains microtasks + due timers.
- **`inject_dom_snapshot` order** — `set_page_url` now runs BEFORE script execution. `SetPageUrl` re-registers the whole `window` global, so running it after scripts wiped any `window.*` properties the scripts set.

## [0.17.0] - 2026-07-11

### Added

- **HTML serializer** (`crates/oxibrowser-core/src/js/dom_serializer.rs`) — pure-Rust DOM-to-HTML serialization. Void elements self-close, attributes are HTML-escaped, text/comment nodes handled. `serialize_node` and `serialize_children` are the public API; 12 unit tests cover elements/text/comments/void/attrs/document/unknown types.
- **`Element.outerHTML` getter** — reads the serialized tag + attributes + children. Read-only per spec.
- **`innerHTML` setter rewires to a real parser** — assignments now go through `DomSnapshot::set_inner_html`, which calls `oxibrowser_webapi::Document::parse` to parse the fragment, removes the target's old children via `remove_subtree`, and inserts the new subtree via `insert_subtree` (DFS pre-order, fresh node ids, proper parent links). `rebuild_indices` is called after so id/class/tag indices pick up the new nodes and `querySelector` / `getElementById` can find them on the next read.
- **Event constructor init dictionaries** — `new MouseEvent('click', { clientX, clientY, button, ctrlKey, ... })` now copies the init dict onto the event object. Same for `KeyboardEvent`, `FocusEvent`, `Event`, and `DragEvent` (extends MouseEvent + `dataTransfer`).
- **`Event.prototype` methods on every event instance** — `preventDefault`, `stopPropagation`, `stopImmediatePropagation` are set as own properties on each event object so they resolve without depending on the JS class hierarchy.
- **`dispatchEvent` sets `event.target` and `event.currentTarget`** before firing listeners; returns `!defaultPrevented`; respects `stopImmediatePropagation` between callbacks.
- **Event bubbling** — when `event.bubbles === true` and `stopPropagation` wasn't called, dispatch walks the DomSnapshot parent chain and fires ancestor listeners found via the new thread-local `LISTENER_REGISTRY` (keyed by `node_id` so listeners registered through any element-object instance are reached). `event.currentTarget` is updated to the current ancestor.
- **`requestAnimationFrame` / `cancelAnimationFrame`** — proper implementation via `TokioJobQueue::schedule_timer` with a 16 ms deadline. The callback receives a `DOMHighResTimeStamp` (ms since `UNIX_EPOCH`); `cancelAnimationFrame` uses the timer's handle id and `cancel_timer`.
- **`Element.innerText`** — read-only alias for `textContent`.
- **`performance` global** — registered as a standalone global (`window.performance === performance`) alongside the existing `window.performance` accessor. Provides `now()` returning `ms since UNIX_EPOCH`.
- **`Response` improvements** — `__body` is stored as a hidden property on the Response object. `text()`, `json()`, and `arrayBuffer()` all read from `this.__body` (not from a captured closure) and return `Promise.resolve(...)`. `arrayBuffer()` returns a `Uint8Array`. `bodyUsed` is exposed as a property (still hardcoded to `false`).
- **`fetch` options `headers`** — common header keys (`content-type`, `accept`, `authorization`, `user-agent`, `cookie`) from the init dict are now read and forwarded to the HTTP client. Previously silently dropped.
- **CDP `Input.dispatchMouseEvent` multi-event sequence** — `mousePressed` fires `mousedown`; `mouseReleased` fires `mouseup` + `click` (left button only); `mouseMoved` fires `mousemove`. Matches real-browser behavior.
- **CDP `Input.dispatchDragEvent`** — real implementation that evaluates `js_dispatch_drag_event(x, y, event_type)` and dispatches a `DragEvent` on the element at the point. Previously a no-op.
- **`js_dispatch_drag_event` JS code generator** — used by both the CDP handler and the Tab drag API.

### Fixed

- **SSRF filter now scheme-aware** — `check_url_ssrf` short-circuits to `allow` for any non-`http`/`https` scheme (`about:`, `data:`, etc.). Previously, `about:blank` was rejected because the filter tried to resolve the hostname "blank".
- **`about:blank` navigation support** — `Session::navigate` routes `about:` URLs to a new `navigate_about` that builds an empty page from a minimal HTML template. The CDP server's default URL is `about:blank` and now works.

## [0.16.0] - 2026-06-26

### Added

- **Stealth/bot-detection layer** — `ChallengeDetector` for Cloudflare, Turnstile, reCAPTCHA, hCaptcha challenge detection (`challenge.rs`). Automatic retry with clearance cookie detection. Classifies challenges as NonInteractive (solved by retry), Interactive (needs human), or Blocked.
- **Extraction engine** (`extract.rs`) — structured HTML-to-markdown extraction with link collection, metadata parsing (`og:title`, `og:description`, `og:image`, `twitter:card`, canonical URL), and content normalization.
- **JS V8 parity bootstrap** — 818-line bootstrap script in `runtime.rs` simulating real V8 browser globals: `window.navigator` (platform, languages, hardwareConcurrency, maxTouchPoints, deviceMemory, plugins), `MimeTypeArray`/`PluginArray`, consistent error stack traces, timezone/locale detection.
- **Enhanced wait conditions** — `WaitOptions` with `poll_interval_ms`, `settle_timeout_ms`, `quiet_window_ms` for flexible NetworkIdle detection. `Tab::wait_for_condition_with()` for explicit options.
- **Challenge-aware HTTP client** — network retry/backoff for cleared challenges, interactive/blocked challenge short-circuit.
- **`DomSnapshot::extra_attr()`** — attribute content extraction for metadata parsing.

### Changed

- **Network client** — `HttpClient::request()` returned type updated to `FetchOutcome` with optional `Challenge` field. Interception-aware retry for non-interactive challenges.
- **Session/tab state** — challenge clearance cookie passthrough, structured fetch outcome reporting.
- **Config** — `stealth` flag added to `BrowserConfig` for stealth-mode opt-in.

### Internal

- **JS runtime boot sequence** — `register_window_globals()` replaced with a JS bootstrap script compiled from `V8_PARITY_BOOTSTRAP` template, avoiding native `ObjectInitializer` limitations (no getter/setter support in boa 0.20).
- **wreq 6.0.0-rc** — added as HTTP client dependency alongside `reqwest` for stealth-mode emulation (Chrome JA4+ fingerprint).
- **Code quality** — clippy warnings resolved (collapsible if, `contains_key`, `trim_split_whitespace`); formatting applied.

## [0.15.0] - 2026-06-07

### ⚠️ BREAKING CHANGES

- **MSRV raised from 1.82 to 1.96** — downstream crates must build on Rust 1.96 or later. Pinning an older toolchain against `oxibrowser` `0.15.0` will fail at dependency resolution.
- **Edition upgraded from 2021 to 2024** — the workspace now uses `edition = "2024"`. Transitive consumers in the same workspace will pick this up; downstream crates that depend on `oxibrowser` are unaffected unless they use `cargo metadata` to read our edition.

### Changed

- **Toolchain & edition** — all four crates (`oxibrowser`, `oxibrowser-core`, `oxibrowser-cdp`, `oxibrowser-webapi`) now declare `edition = "2024"` and `rust-version = "1.96"`. CI updated to `dtolnay/rust-toolchain@1.96` (was `@1.82`).
- **Documentation** — README and CONTRIBUTING updated to advertise Rust 1.96+ and Edition 2024.

### Internal

- **Match ergonomics** — `crates/oxibrowser-webapi/src/dom/node.rs:135` `set_text_content` rewritten to drop the explicit `ref mut` binding mode. Edition 2024 disallows explicit borrow modes in implicitly-borrowing patterns.
- **Clippy `collapsible_if` cleanup** — 143 nested-`if` blocks collapsed to let-chains (`if let X = y && condition { ... }`) via `cargo clippy --fix`. Let-chains are stable since Rust 1.88.
- **`assert_matches!` adoption** — 15 occurrences of `assert!(matches!(cmd, ...))` in `crates/oxibrowser/src/session/parser.rs` switched to the stable `std::assert_matches!` macro (Rust 1.96). Better failure diagnostics (prints the actual `Debug` value on mismatch).
- **`Vec::extract_if` adoption** — two retain+count patterns simplified to a single-pass `extract_if(.., predicate).collect()` / `.count()`:
  - `crates/oxibrowser-core/src/js/job_queue.rs::pop_due_timers` (was 8 lines, 2 passes; now 4 lines, 1 pass).
  - `crates/oxibrowser-core/src/browser.rs::cleanup_closed_sessions` (removed length before/after trick; `extract_if().count()` returns removed count directly). Predicate inverted to match: returns `true` for items to remove, `false` for items to keep.
- **Formatting** — `cargo fmt` applied across the workspace; no semantic changes.

## [0.13.0] - 2026-06-04

### ⚠️ BREAKING CHANGES

- **`BrowserEvent` variants now require a `tab_id: Uuid` field.** Every event is from a tab; the field is required at the Rust level. External `match` arms on the variants need to add a binding for `tab_id` (use `tab_id, ..` if you don't care about the value). The wire format is a non-breaking addition — `#[serde(default = "Uuid::nil")]` makes the field optional on deserialize, so older JSON payloads still parse.

### Added

- **`tab_id: Uuid` on every `BrowserEvent` variant** — `NavigationStarted`, `WaitingForSelector`, `DocumentReady`, and `ScreenshotCaptured` now carry the id of the `Tab` that emitted them. Stable for the lifetime of the tab and shared across `Tab::clone`. Exposed via `Tab::tab_id()`. `Browser::new_tab()` generates a fresh `Uuid::new_v4()` per tab.
- **Per-tab event routing** — the foundation for `oxi-agent`'s `OxiBrowserEngine` to route events to the right callback when multiple tabs are open in a single browser. The oxi-agent update follows in a coordinated PR (see `docs/designs/2026-06-04-oxibrowser-observability-followup.md`).
- **2 new unit tests**: `event_tab_id_preserved_in_serde` and `test_tab_id_is_stable_across_clones`.

### Changed

- **Workspace version bumped to `0.13.0`** (all four crates: `oxibrowser`, `oxibrowser-core`, `oxibrowser-cdp`, `oxibrowser-webapi`). The major bump signals the breaking `tab_id` requirement for downstream consumers.
- **Doc comment corrections on `DocumentReady`**:
  - `total_bytes` is the size of the **post-parse, re-serialized HTML body** (`result.html.len() as u64`), **not** the wire-level `Content-Length`. The doc comment previously claimed the latter.
  - `js_script_count` is the count of `<script>` **references** in the DOM's resource list, **not** the count of scripts the JS runtime actually executed. The doc comment now spells out the caveat.
- Workspace `uuid` dependency now enables the `serde` feature so `Uuid` can be a field on `Serialize`/`Deserialize` types.

## [0.12.0] - 2026-06-04

### Added — Browser Observability

- **`BrowserEvent` enum** (`oxibrowser_core::event::BrowserEvent`) — public observability surface for the browser lifecycle. Four variants:
  - `NavigationStarted { url }`
  - `WaitingForSelector { selector, timeout_ms }`
  - `DocumentReady { final_url, title, status, total_bytes, js_script_count, total_duration }`
  - `ScreenshotCaptured { bytes, viewport_width, duration }`
- **`Browser::subscribe_events()`** — returns a `tokio::sync::broadcast::Receiver<BrowserEvent>`. Multiple observers can subscribe; oldest event is dropped on overflow.
- **`BrowserEvent::short_label()`** — single source of truth for user-facing progress text (e.g. `Loaded "Example" — 200 · 1.2 KB · 4 scripts · 245 ms`).
- **Tab events** — `Tab::goto` emits `NavigationStarted` + `DocumentReady`; `Tab::wait_for` emits `WaitingForSelector`; `Tab::screenshot` emits `ScreenshotCaptured`. All emission is non-blocking; events are dropped silently if the observer queue is full.
- **9 new unit tests** in `event.rs` and `browser.rs` (label formatting, wire format, overflow safety, end-to-end subscribe/recv).

### Changed

- `Tab` now holds an optional `broadcast::Sender<BrowserEvent>`. Tabs created via `Browser::new_tab()` are wired to the browser's event stream; tabs built directly via `Tab::new()` (tests) are not.
- Workspace version bumped to `0.12.0` (additive — no breaking changes).

## [0.11.0] - 2026-05-20

### Added — CLI 2.0 (Agent-First Redesign)

- **`fetch`** — universal one-shot command (absorbs `browse` and `eval`)
  - `--format markdown|html|text` (markdown default, human-readable)
  - `--click`, `--fill`, `--press`, `--wait` for interaction
  - `--eval <expr>` for JS evaluation
  - `--summary` for quick page metadata
  - `--fields`, `--max-bytes` for agent-friendly output control
  - `--json` for machine-readable output (opt-in, not automatic)
- **`extract`** — structured data extraction: `--links`, `--title`, `--text`, `--selector`, `--attrs`, `--all`
- **`session`** — stdin/stdout JSON REPL for multi-step automation
  - 22 commands: `new`, `goto`, `back`, `forward`, `reload`, `click`, `fill`, `press`, `type`, `select`, `check`, `uncheck`, `scroll`, `eval`, `extract`, `content`, `screenshot`, `wait`, `close`, `list`, `help`, `exit`
  - Clean shutdown on EOF, `exit`, Ctrl+C, SIGTERM
  - Multi-tab support with tab IDs
- **`run`** — YAML automation scripts with `CliResponse` JSON wrapping
- **`describe`** — CLI schema as JSON (for agent introspection)
- **`skill`** — agent skill guide (markdown or `--json`)
- **`version`** — version info (text or `--json`)
- **Input validation** (`validate.rs`) — URL scheme, control chars, CSS selectors
- **`CliResponse` JSON wrapper** (`output.rs`) — consistent `{ok, data, meta, error, error_code}` format
- **Exit codes**: 0=success, 1=runtime, 2=input validation, 3=timeout, 4=network

### Fixed

- **scroll** — use `scrollTop`/`scrollLeft` instead of `window.scrollBy` (not available in boa_engine)
- **eval quotes** — session parser strips wrapping quotes from JS expressions
- **`--json` consistency** — all commands accept `--json` without error
- **text extraction** — block-level sibling separators for clean line breaks
- **`describe` schema** — added missing `uncheck` command, corrected default format to `markdown`

### Changed

- Default output format is **markdown** (was `html`)
- Human-readable by default; `--json` is opt-in for agents
- Errors are plain text on stderr; JSON with `--json`
- `describe` and `run` always output JSON (no `--json` needed)

## [0.10.0] - 2026-05-18

### Added
- **OXI.getAccessibilityTree** CDP method — semantic tree of page content with roles, labels, visibility, interactivity, and approximate Y positions
- **OXI.getBoxModelScreenshot** CDP method — PNG screenshot with colored boxes representing each DOM element
- **Box Model Renderer** (`css/visual.rs`) — renders elements as colored rectangles with background colors, borders, and text
- **Color parser** — full CSS color support: `#RGB`, `#RRGGBB`, `rgb()`, `rgba()`, `hsl()`, `hsla()`, `currentColor`, and 100+ named colors

### Added (JS API)
- **`getComputedStyle(el)`** — global function and `window.getComputedStyle`, returns CSSStyleDeclaration with computed values
- **`element.getBoundingClientRect()`** — returns DOMRect with x, y, width, height, top, right, bottom, left
- **`element.offsetWidth` / `element.offsetHeight`** — layout-based dimensions
- **`element._visible`** — boolean: `display !== none && visibility !== hidden && opacity !== 0`
- **`element._interactive`** — boolean: not `disabled` + `pointerEvents !== none`
- **`style.getPropertyValue(name)`** — get computed property value

### Added (CSS Layout Engine)
- **`LayoutEngine`** (`css/layout.rs`) — pure-Rust CSS layout approximation:
  - Tag defaults (block, inline, replaced elements)
  - Inline style parsing (`style="color:red"`)
  - CSS inheritance (font, color, visibility)
  - Color/length normalization
  - Width estimation with wrapping
  - Y-position estimation from DOM order
- **`ComputedStyle`** struct — full computed style map with visibility, interactive, colors, dimensions
- **`LayoutRect`** struct — position and size for each element

### Fixed
- Text duplication in accessibility tree (same text no longer shown twice)
- `parse_color_to_rgba` division by zero on scale=0
- Duplicate ID counter bug in test helper
- `take_while` consuming delimiter in test helper parser
- `parent_w - style.margin_top` bug in width estimation
- Clippy warnings throughout codebase

## [0.9.1] - 2026-05-16

### Fixed
- Clippy `doc_lazy_continuation` warning in `runner.rs`
- Cargo fmt formatting issues
- Unused import warnings in `parser.rs`

### CI
- All GitHub Actions CI checks now pass

## [0.9.0] - 2026-05-16

### Added
- **ScriptRunner module**: New `oxibrowser-core/src/script/` module for YAML-based browser automation:
  - `parser.rs`: YAML parsing to `ScriptConfig` (serde_yaml)
  - `runner.rs`: Step-by-step script execution on `Tab` with variable interpolation
  - `types.rs`: Step enum with 30+ step types (navigation, interaction, content, flow control)
  - Supports goto, click, fill, type, wait, evaluate, extract, screenshot, set, echo, sleep, if, retry
- **`oxibrowser run` CLI command**: Run YAML scripts from the CLI (`oxibrowser run <script.yaml>`)
- **Variable interpolation**: `${var}` substitution in step fields, `$$` for literal `$`
- **Error handling**: `on_error.action: abort | continue` with optional screenshot on error

### Changed
- CLI enhanced with `run` subcommand (developer tool only)

### Architecture
- ScriptRunner shared between CLI and future BrowserTool in agent contexts
- No `.programs/oxibrowser` registration needed — agents use BrowserTool directly

## [0.7.0] - 2026-05-16

### Added
- **Mutation persistence**: `createElement`, `createTextNode`, `appendChild`, `removeChild`, `insertBefore`, `setInnerHtml` now apply to webapi DOM — elements survive across `evaluate()` calls and are discoverable via `querySelector`
- **`element.style` as property**: `el.style` is now a CSSStyleDeclaration-like object (not a function) with `getPropertyValue()`, `setProperty()`, `removeProperty()`
- **`element.classList` as property**: `el.classList` is now a DOMTokenList-like object (not a function) with `add()`, `remove()`, `toggle()`, `contains()`
- **`element.textContent` setter**: Read/write — `el.textContent = 'new'` updates live snapshot + records mutation for webapi DOM
- **`element.innerHTML` setter**: Read/write — `el.innerHTML = 'html'` updates live snapshot + records mutation
- **`data-oxi-text` bridge**: Snapshot regeneration reads `data-oxi-text` attribute as fallback for text content set via JS
- **14 new DOM APIs** in `create_element_object()`:
  - Tree traversal (accessors): `firstChild`, `lastChild`, `nextSibling`, `previousSibling`
  - Tree manipulation (methods): `insertBefore`, `replaceChild`, `removeAttribute`, `cloneNode`, `remove()`
  - Style/Class (methods): `style()`, `classList()`
  - Focus/Form (noop): `focus()`, `blur()`, `submit()`

### Fixed
- **`getAttribute` / `hasAttribute`**: Was reading from static cloned HashMap, now reads from live `DomSnapshot` via `Arc<RwLock>`
- **`input.value` getter**: Was capturing initial value at creation, now reads from live snapshot
- **`input.value` setter**: Was only recording mutation, now also updates snapshot attribute immediately
- **`click()`**: Was only recording mutation, now also fires registered JS event handlers from `__listeners`
- **`createElement`**: Was returning minimal stub, now calls `create_element_object()` for full element with all APIs
- **110 code quality issues**: Security, data integrity, API completeness, CSS rendering, testing, dependency hygiene

### Changed
- `apply_mutations()` now applies all 5 structural mutations (CreateElement, CreateTextNode, AppendChild, RemoveChild, SetInnerHtml) to webapi DOM
- `Frame::document_mut()` bumps `dom_version` counter on each mutation
- webapi `Document` gains `create_element_node()`, `create_text_node()`, `tree_mut()`, `nodes_mut()`

### Tests
- 279 tests pass (223 core + 23 E2E + 20 webapi + 10 event + 3 smoke)
- 22/22 scenario tests pass (real websites: httpbin, Hacker News)

## [0.6.0] - 2026-05-14

### Added
- **Input domain**: `Input.dispatchKeyEvent`, `Input.dispatchMouseEvent`, `Input.insertText` — dispatch real `KeyboardEvent`/`MouseEvent` via JS evaluation on `document.activeElement` / `document.elementFromPoint()`
- **document.activeElement**: JS getter returning `document.body` (no real focus tracking)
- **document.elementFromPoint(x, y)**: JS method approximating element hit-testing by DOM order with estimated element heights
- **Page.captureScreenshot**: Real PNG output using built-in 8×16 bitmap font (ASCII 32–126) — renders DOM text content as white-background image, base64-encoded PNG response
- **Fetch domain complete**: `continueRequest` (modify headers/URL/postData, resume), `failRequest` (fail with error reason), `fulfillRequest` (synthetic response), `getResponseBody` — with `PausedRequestRegistry` for request tracking
- **HttpClient.intercept()**: HTTP fetch with `InterceptAction` (Continue/Fail/Fulfill) — enables Fetch domain interception integration
- **InterceptedResponse**: New error variant for synthetic HTTP responses from Fetch.fulfillRequest
- **Input JS helpers**: `js_dispatch_key_event()`, `js_dispatch_mouse_event()`, `js_insert_text()` — generate JS code strings for Input domain dispatch

### Tests
- 208 tests pass (164 core + 23 E2E + 18 webapi + 3 smoke)

## [0.5.0] - 2026-05-13

### Added
- **CSS text screenshot**: `page.to_text_screenshot()` — ASCII/Unicode DOM rendering with block element tags, indentation, BR/HR/IMG handling
- **document.write()**: Appends HTML content as text node to body
- **MutationObserver**: Constructor with `observe()`, `disconnect()`, `takeRecords()` stubs
- **Puppeteer smoke tests**: 3 E2E tests verifying Puppeteer/Playwright CDP compatibility (built-in HTTP server, WebSocket client, process spawning)

### Tests
- 205 tests pass (152 core + 3 smoke + 22 E2E + 18 webapi + 10 event)

## [0.4.0] - 2026-05-13

### Added
- **DOM Mutation**: `document.createElement(tag)`, `document.createTextNode(text)`, `element.appendChild(child)`, `element.removeChild(child)` — full DOM mutation with `DomSnapshot` sync
- **fetch Response**: `.text()` → `Promise<string>`, `.json()` → `Promise<object>`, `headers` object, `bodyUsed`, `type` properties
- **XMLHttpRequest**: Constructor with `.open()`, `.send()`, `.setRequestHeader()`, `.getResponseHeader()`, `.abort()`, `onload`/`onerror`/`onreadystatechange` callbacks
- **Real-world integration tests**: `createElement` on real page, window globals verification

### Changed
- **Clippy**: Zero warnings across entire workspace (was 48+)
- **fetch Response**: Body serialization via `serde_json` (no more string injection bugs)

### Tests
- 201 tests pass (151 core + 22 E2E + 18 webapi + 10 event)

## [0.3.0] - 2026-05-13

### Added
- **Logs to stderr**: `tracing_subscriber` now writes to stderr, stdout is clean data output
- **window global**: `navigator`, `location`, `performance`, `viewport`, `crypto` properties
- **document.body/head/documentElement**: Real DOM elements as JS objects
- **Real fetch()**: Channel-based JS↔HttpClient bridge with `FetchRequestMsg`/`FetchResponseMsg`
- **localStorage**: Full Storage interface (getItem/setItem/removeItem/clear/key/length)
- **atob/btoa**: Base64 encode/decode via `base64` crate
- **URLSearchParams**: Constructor with get/set/append/delete/has/forEach/toString
- **URL class**: Constructor with `url::Url` parsing and accessor getters
- **crypto.getRandomValues**: Pseudo-random byte generation
- **TextEncoder/TextDecoder**: UTF-8 encode/decode
- **EventTarget**: Real `addEventListener`/`removeEventListener`/`dispatchEvent` on document and elements
- **CDP Network cookies**: `getAllCookies`, `getCookies`, `setCookie`, `deleteCookies` with full CRUD
- **CDP Fetch domain**: Full implementation with event interception pattern

### Tests
- 194→201 tests

## [0.2.0] - 2026-05-13

### Added
- **CI/CD**: GitHub Actions workflow (check, test, clippy, fmt, release build)
- **Container**: Dockerfile with multi-stage build for minimal image size
- **Security**: CDP server connection limit (max 16 concurrent)
- **Security**: CDP message size validation (max 1MB)
- **Benchmarks**: Performance benchmarks for HTML parsing, DOM queries, Markdown conversion
- **CHANGELOG.md**: This file

### Changed
- **Runtime**: Replaced `std::sync::RwLock` with `parking_lot::RwLock` in JS runtime (poison-free)
- **Safety**: Eliminated all production `unwrap()` calls — replaced with `expect()`, safe patterns, or proper error propagation
- **Safety**: `as_object().unwrap()` replaced with safe `if-let` pattern in JSON serialization

### Fixed
- Potential runtime panics from poisonable `std::sync::RwLock` in JS runtime
- Potential runtime panics from `unwrap()` on `Option` and `Result` types in production code

## [0.1.0] - 2026-05-12

### Added
- **Browser lifecycle**: `Browser`, `Session`, `Page`, `Frame` hierarchy with thread-safe IDs
- **HTML parsing**: html5ever-based DOM parsing with CSS selectors
- **JS Runtime**: boa_engine integration for real JavaScript execution (ES2024+)
  - Persistent context across `evaluate()` calls
  - `console.log/warn/error/info` support
  - `document.querySelector`, `document.querySelectorAll`, `document.title`
  - DOM mutation tracking (click, setAttribute, input value)
  - Runtime limits (loop iteration, recursion, stack size, timeout)
- **CDP Server**: Chrome DevTools Protocol over WebSocket
  - HTTP endpoints: `/json/version`, `/json`
  - WebSocket upgrade with RFC 6455 compliance
  - 7 domain handlers: Browser, DOM, Fetch, Network, Page, Runtime, Target
  - Event broadcasting: frameNavigated, domContentLoadedEventFired, loadEventFired
  - Network events: requestWillBeSent, responseReceived, loadingFinished
  - Runtime events: executionContextCreated, consoleAPICalled
- **Network**: reqwest-based HTTP client with cookie injection
- **CookieJar**: Domain-scoped cookie storage
- **Document**: CSS selectors, text extraction, Markdown conversion, resource URL extraction
- **Tree**: Adjacency list with DFS/BFS traversal
- **CLI**: `fetch`, `serve`, `version` subcommands via clap
- **Tests**: 185 tests (142 core + 15 E2E + 18 webapi + 10 integration)
- **Encoding**: charset detection and encoding conversion via encoding_rs

[0.18.0]: https://github.com/project-oxi/oxibrowser/compare/v0.17.0...v0.18.0
[0.7.0]: https://github.com/a7garden/oxibrowser/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/a7garden/oxibrowser/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/a7garden/oxibrowser/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/a7garden/oxibrowser/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/a7garden/oxibrowser/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/a7garden/oxibrowser/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/a7garden/oxibrowser/releases/tag/v0.1.0
