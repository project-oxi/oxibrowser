# Oxibrowser Handoff — SPA Rendering Completion Plan

> **Date**: 2026-10-25
> **From**: Goal-mode session (boa 0.22 upgrade + compat loop, 10 commits)
> **To**: Next session or human contributor
> **Status**: Corpus 17/21 (80%), 0 crashes, 0 timeouts. Core 710/710.
> **Branch**: `main` at `d8c9dbd` (all pushed)

---

## 1. What works today

| Capability | Status |
|---|---|
| SSR/static HTML rendering (HN, Wikipedia, docs sites) | ✅ Full |
| CSS layout (Stylo + Taffy: flexbox, grid, blocks) | ✅ Full |
| Text rendering (Parley shaping + vello_cpu paint) | ✅ Full |
| Screenshots via CDP `Page.captureScreenshot` | ✅ 1280×720 PNG |
| Markdown/text extraction | ✅ Full |
| JS execution (boa 0.22, Test262 95.6%) | ✅ Core language |
| Dynamic `<script src>` fetch+execute | ✅ Added (iter.7) |
| Tree traversal (childNodes/children/siblings/parent) | ✅ Added (iter.7) |
| ParentNode prepend/append/replaceChildren (native) | ✅ Added (iter.7) |
| Event system (addEventListener/dispatchEvent) | ✅ Works |
| Headers/Request/Response data containers | ✅ Added (iter.7) |
| Intl.Locale/PluralRules/RelativeTimeFormat/ListFormat/Segmenter/DisplayNames | ✅ Polyfilled |
| File/ImageData/DataTransfer/ClipboardEvent | ✅ Polyfilled |
| DOM interface stubs (Document/Element/Node/HTMLElement/Window/...) | ✅ Added (iter.4) |
| Compression (gzip/brotli/deflate/zstd sub-resource decode) | ✅ Fixed (iter.5) |
| Measurement (corpus 21 URL, WARN locator, dump gate) | ✅ Operational |

## 2. What doesn't work — precise root causes

### 2a. JS-rendered SPAs render blank (GitHub, BBC, MDN, daum, spotify, dice6.win)

**Root cause chain** (in blocking-order):

| # | Missing API | Who needs it | Why it blocks |
|---|---|---|---|
| 1 | `window.getComputedStyle(el)` | styled-components, emotion, CSS-in-JS,Bootstrap | Returns undefined → CSS-in-JS throws → React render dies → blank |
| 2 | `EventTarget` constructor | React 18 (root.render uses `new EventTarget()` for synthetic events) | ReferenceError → hydration fails |
| 3 | `DOMException` constructor | fetch() rejects with DOMException, structuredClone, Web Workers | Unhandled rejection in fetch chains |
| 4 | `MessageChannel` / `MessagePort` | React 18 concurrent mode (scheduler uses MessageChannel) | React scheduler never starts → no rendering |
| 5 | `ReadableStream` / `WritableStream` / `TransformStream` | Next.js streaming SSR, React 18 renderToPipeableStream | Import fails → module load error |
| 6 | `document.createRange()` / `createTreeWalker()` / `createNodeIterator()` | Rich text editors, DOM traversal in frameworks | Framework init fails |
| 7 | `window.getSelection()` | Text editors, annotation libs | Returns undefined |
| 8 | `crypto.subtle` | HTTPS-only crypto (SubtleCrypto) | Web Crypto operations fail |
| 9 | `FileReader` | File upload handling, image previews | File upload breaks |
| 10 | `PerformanceObserver` | Web vitals, performance monitoring | Silent failure |
| 11 | `EventSource` (SSE) | Real-time updates | Connection fails |
| 12 | `Notification` | Push notifications | N/A for headless |
| 13 | `BroadcastChannel` | Cross-tab communication | Single-tab use case OK |
| 14 | `scrollX` / `scrollY` | Scroll position tracking | Returns undefined |

### 2b. Canvas/WebGL rendering

`CanvasRenderingContext2D`, `WebGLRenderingContext`, `OffscreenCanvas` — all undefined.
Canvas 2D needs a 2D rasterizer (can delegate to vello_cpu or tiny-skia).
WebGL needs GPU passthrough or software GL (SwiftShader-level) — this is a
Non-goal per roadmap but dice6.win-class sites need at least Canvas 2D.

### 2c. Dynamic script execution is fetch+eval (CORS-limited)

Current: `__oxi_load_script(src)` uses `fetch(src).then(r=>r.text()).then(eval)`.
This breaks when:
- The CDN doesn't send CORS headers (`Access-Control-Allow-Origin`) — script
  tags in real browsers bypass CORS for classic scripts
- The script has `integrity` attribute (SRI) — needs SubtleCrypto.verify
- The script is a module (`type=module`) — needs ES module resolution

**Fix**: Move script fetching to the native side (same path as nav scripts),
bypassing CORS, and evaluate on the JS thread via the existing job queue.

### 2d. ResizeObserver / IntersectionObserver fire once with fake data

Current polyfills fire the callback once on `observe()` with placeholder
measurements. Real SPAs need:
- ResizeObserver: actual size changes when layout mutates (via Taffy)
- IntersectionObserver: actual viewport intersection via Taffy layout rects

**Fix**: Wire RO/IO to the Taffy layout pass — after each layout, compute
element rects and diff against previous → fire callbacks with real data.

### 2e. Bot wall (stackoverflow.com)

403 from Cloudflare, reproducible with curl + Chrome UA. wreq's Chrome149
TLS fingerprint is the latest available target. This is a stealth/anti-detect
problem, not a rendering problem. Possible approaches:
- wreq newer fingerprint (when released)
- Residential proxy integration
- HTTP/2 frame ordering tuning

---

## 3. Fix plan (ordered by impact)

### Phase 1: SPA rendering unblock (highest impact)

**Target**: GitHub, BBC, daum, MDN render visible content.

#### 1.1 `window.getComputedStyle(el)` → CSSStyleDeclaration-like object

**Where**: `create_context` bootstrap (JS-level) or native in `runtime.rs`

**Approach**: Return a live proxy that reads from Taffy's computed style:
```rust
// In create_render_element_object factory:
let get_computed_style_fn = /* NativeFunction */;
// registered on window:
// window.getComputedStyle(el) → CSSStyleDeclaration
```
The returned object needs:
- `getPropertyValue(name)` → computed value from Taffy (color, font-size, display, etc.)
- Direct property access (`style.color`, `style.fontSize`)
- `cssText` (serialized)

**Minimum viable**: Return the inline style + Taffy-computed layout properties
(display, width, height, margin, padding, color, font-size, background-color).
This unblocks CSS-in-JS libraries that read computed values for injection.

**Files**: `oxibrowser-render/src/document.rs` (add `computed_style(node_id, prop) -> Option<String>`), `oxibrowser-core/src/js/runtime.rs` (window.getComputedStyle)

**Unblocks**: styled-components, emotion, Bootstrap JS, Tailwind JIT, daum.net rendering

**Complexity**: Medium (2-4 hours). The hard part is mapping CSS property names to Taffy's internal representation.

#### 1.2 `EventTarget` constructor + `DOMException`

**Where**: Bootstrap JS in `create_context`

**Approach**:
```js
// EventTarget: minimal constructible class
if (typeof EventTarget === 'undefined') {
    EventTarget = function() {
        this.__listeners = {};
        this.addEventListener = function(type, fn, opts) {
            (this.__listeners[type] = this.__listeners[type] || []).push(fn);
        };
        this.removeEventListener = function(type, fn) {
            var l = this.__listeners[type] || [];
            var i = l.indexOf(fn);
            if (i >= 0) l.splice(i, 1);
        };
        this.dispatchEvent = function(ev) {
            (this.__listeners[ev.type] || []).forEach(function(fn) { fn(ev); });
        };
    };
}
// DOMException
if (typeof DOMException === 'undefined') {
    DOMException = function(msg, name) {
        this.message = msg || '';
        this.name = name || 'DOMException';
    };
    DOMException.prototype = Object.create(Error.prototype);
}
```

**Unblocks**: React 18 EventTarget usage, fetch() DOMException rejections, structuredClone error handling

**Complexity**: Low (30 min). Pure JS, no native code needed.

#### 1.3 `MessageChannel` / `MessagePort`

**Where**: Bootstrap JS in `create_context`

**Approach**: In-memory message passing:
```js
if (typeof MessageChannel === 'undefined') {
    MessageChannel = function() {
        var port1 = { onmessage: null, postMessage: null };
        var port2 = { onmessage: null, postMessage: null };
        // Wire: port1.postMessage → port2.onmessage (async via queueMicrotask)
        port1.postMessage = function(data) {
            queueMicrotask(function() {
                if (port2.onmessage) port2.onmessage({ data: data });
            });
        };
        port2.postMessage = function(data) {
            queueMicrotask(function() {
                if (port1.onmessage) port1.onmessage({ data: data });
            });
        };
        this.port1 = port1;
        this.port2 = port2;
    };
}
```

**Unblocks**: React 18 concurrent mode scheduler (uses MessageChannel for yield points)

**Complexity**: Low (30 min). The React scheduler specifically uses `MessageChannel` to yield to the browser — without it, it falls back to setTimeout, but only if it can CONSTRUCT MessageChannel.

#### 1.4 `ReadableStream` / `WritableStream` / `TransformStream`

**Where**: Native (Rust) or JS polyfill

**Approach**: JS-level minimal implementation (byte streams not needed for SPA hydration):
- ReadableStream: wrap an async iterator, support `getReader()`, `[Symbol.asyncIterator]`
- WritableStream: accept writes, no backpressure
- TransformStream: pipe through

**Unblocks**: Next.js streaming SSR, React 18 `renderToPipeableStream`, fetch response bodies

**Complexity**: Medium (2-3 hours). The web streams spec is complex, but SPAs only use the happy path.

**Note**: roadmap already lists this as "Next #5: ReadableStream". This handoff confirms it's the #1 priority after getComputedStyle.

#### 1.5 `document.createRange()` / `createTreeWalker()` / `createNodeIterator()`

**Where**: Bootstrap JS on the document object

**Approach**: Minimal implementations over the childWrapper tree:
- Range: `setStart/setEnd/cloneContents/extractContents` — operate on child indices
- TreeWalker: iterator over childWrapper tree with `nextNode/previousNode`
- NodeIterator: simplified TreeWalker (forward only)

**Unblocks**: Rich text editors (Slate, ProseMirror), DOM traversal in frameworks

**Complexity**: Medium (2 hours). The tree structure is already accessible via childNodes.

#### 1.6 `crypto.subtle` (SubtleCrypto)

**Where**: Bootstrap JS or native (Rust crypto ops)

**Approach**: Wire to Rust `sha2`/`ring`/`aws-lc-rs`:
- `digest(algo, data)` → SHA-256/384/512 (sha2 crate ✓ already in workspace)
- `sign/verify` → HMAC (sha2 + hmac crate)
- RSA/ECDSA → defer (needs key management)

**Unblocks**: Web Crypto API, HTTPS-only features, password hashing in SPAs

**Complexity**: Low for digest (1 hour). High for full spec.

#### 1.7 Dynamic script execution hardening

**Current**: `__oxi_load_script(src)` uses `fetch(src).then(eval)` — CORS-limited.

**Fix**: Move to the native appendChild closure — when a script element with src is
inserted, the native code fetches the script using the same `fetch_text_with_overrides`
path as nav scripts (no CORS, uses TLS fingerprint), then evaluates on the JS thread
via the job queue.

**Where**: `oxibrowser-core/src/js/runtime.rs` — appendChild/insertBefore closures
already detect script elements. Add a channel message to the main thread: fetch →
send response back to JS thread → `ctx.eval(body)`.

**Unblocks**: rspack/webpack chunk loading from CDNs without CORS headers, SRI scripts

**Complexity**: Medium (2 hours). The channel mechanism exists (PENDING_FETCH).

### Phase 2: Real observers (medium impact)

#### 2.1 ResizeObserver with real layout data

**Where**: After Taffy layout pass in the render pipeline

**Approach**: After each `layout()` call, compute element rects (already available
via `getBoundingClientRect` native) and diff against a stored snapshot. Fire
callbacks for changed elements.

**Unblocks**: Virtualized lists (react-window, react-virtualized), responsive components

**Complexity**: Medium (3-4 hours). Need to hook into the layout cycle.

#### 2.2 IntersectionObserver with real viewport intersection

**Where**: Same as above, after layout

**Approach**: Compute element rect vs viewport rect → intersection ratio → fire callbacks

**Unblocks**: Lazy loading, infinite scroll, viewport-aware components

**Complexity**: Medium (2-3 hours)

### Phase 3: Canvas 2D (dice6.win-class sites)

#### 3.1 CanvasRenderingContext2D

**Where**: New native binding in `runtime.rs` + 2D rasterizer

**Approach**: Wrap `tiny-skia` (already a transitive dep of vello_cpu) or
`vello_cpu` 2D API:
- `fillRect`, `strokeRect`, `clearRect` → direct raster ops
- `drawImage` → decode + blit
- `beginPath/moveTo/lineTo/fill/stroke` → path building + raster
- `getImageData/putImageData` → pixel buffer access

**Unblocks**: dice6.win, canvas-based games, charts (Chart.js, D3)

**Complexity**: High (1-2 weeks for core 2D ops). But 80/20 — fillRect+drawImage covers most usage.

### Phase 4: Bot wall (stackoverflow)

#### 4.1 TLS fingerprint upgrade

**Where**: `network/client.rs` — `Emulation::Chrome149`

**Approach**: Monitor wreq releases for Chrome 150+ fingerprint targets.
Alternatively: implement JA3/JA4 fingerprint customization directly.

**Complexity**: Low (if wreq updates) / High (custom TLS stack)

---

## 4. Already fixed in this session (for context)

| Fix | Files | Impact |
|---|---|---|
| Recursive DOM walker → iterative stack | `css/render.rs`, `deep_text_content` | Stack overflow on deep DOM |
| JS thread stack 2MiB → 64MiB | `js/runtime.rs` | Deep recursion |
| Parser nesting guard (MAX_SOURCE_NESTING) | `js/runtime.rs` | Minified bundle parser crash |
| WebPlayer require missing module SafeGuard | `js/runtime.rs` | rspack chunk resolution |
| Sub-resource decompression (gzip/brotli/deflate/zstd) | `network/client.rs`, `Cargo.toml` | Compressed scripts arrived as garbage |
| RO/IO callbacks → microtask (spec delivery) | Bootstrap JS | Sync callback broke spotify virtualizer |
| DOM interface stubs (12 interfaces) | Bootstrap JS | `Document is not defined` |
| Native ParentNode prepend/append/replaceChildren | `js/runtime.rs` | `d.head.prepend` missing |
| DOM tree accessors (childNodes/children/siblings/...) | `js/runtime.rs` + `render/document.rs` | `head.childNodes` undefined |
| Native insertBefore (Blitz insert_nodes_before) | `js/runtime.rs` + `render/document.rs` | `s.parentNode.insertBefore` missing |
| Intl.Locale/PluralRules/RelativeTimeFormat/ListFormat/Segmenter/DisplayNames | Bootstrap JS | Locale-data module crash |
| File/ImageData/DataTransfer/ClipboardEvent | Bootstrap JS | core-js web.* module crash |
| Headers/Request/Response containers | Bootstrap JS | fetch-adjacent code |
| window.innerWidth/innerHeight/devicePixelRatio/screen | Bootstrap JS | Responsive layout checks |
| navigator.cookieDeprecationLabel | Bootstrap JS | recaptcha enterprise |
| document.location alias | Bootstrap JS | ad SDKs |
| Source::with_path(script URL) | `js/runtime.rs` | Backtrace attribution |
| Prototype filler for class-like callables | Bootstrap JS | core-js `c=s.prototype` crash |

## 5. Known limitations (not fixable without architecture change)

| Limitation | Why | Sites affected |
|---|---|---|
| No WebGL/GPU rendering | Software renderer (vello_cpu), no GPU passthrough | WebGL games, 3D visualizations |
| No video/audio playback | No media decoder (ffmpeg/gstreamer) | YouTube, Netflix |
| No real browser extension support | No extension loading mechanism | Extensions-dependent sites |
| Full ICU data for all locales | boa 0.22 Intl is partial; boa_wintertc migration incomplete | Non-English locale formatting |

## 6. Verification

After implementing Phase 1:
```bash
# 1. Run the corpus
cargo test -p oxibrowser --test compat_corpus -- --ignored --nocapture

# 2. Check the previously-failing sites
./target/release/oxibrowser fetch https://www.spotify.com --format text | wc -c
# Target: > 1000 (currently 0)
./target/release/oxibrowser fetch https://www.daum.net --format text | wc -c
# Target: > 500 (currently 0)

# 3. Run the full test suite
cargo test --workspace --lib

# 4. Check clippy
cargo clippy --workspace --all-targets -- -D warnings
```

Target: corpus 17/21 → 19/21+ (daum and at least one spotify domain render).

## 7. Handoff checklist

- [ ] Phase 1.1: getComputedStyle (2-4h)
- [ ] Phase 1.2: EventTarget + DOMException (30m)
- [ ] Phase 1.3: MessageChannel (30m)
- [ ] Phase 1.4: ReadableStream/WritableStream/TransformStream (2-3h)
- [ ] Phase 1.5: createRange/createTreeWalker/createNodeIterator (2h)
- [ ] Phase 1.6: crypto.subtle digest (1h)
- [ ] Phase 1.7: Dynamic script native fetch (2h)
- [ ] Phase 2.1: Real ResizeObserver (3-4h)
- [ ] Phase 2.2: Real IntersectionObserver (2-3h)
- [ ] Verify: corpus re-measure (target: ≥19/21)
- [ ] Verify: spotify renders visible content
- [ ] Verify: daum renders visible content
- [ ] Commit + push + tag v0.28.0
