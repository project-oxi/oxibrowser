# Cheonggye runtime compatibility

Development candidate, 2026-10-26. This is browser-engine work; the game and
its retained release assets are unchanged.

## Acceptance

The target is the retained Cheonggye release `e84f8842-d6722ab`, using Babylon.js
9.31.0. Browser support must load its real module graph and verified map
package, render the scene, and accept representative game input. Passing the
browser fixtures alone does not establish playability, visual quality, audio,
or frame rate. No deployment is part of this change.

Candidate assets used for integration:

| File | SHA-256 |
| --- | --- |
| `index.html` | `15bcd9fe180e20a0ed6d51c6319ce1a7195e857473e62563e6d4b0227c2a3e1f` |
| `assets/runtime-BBlrYGBF.js` | `158a53333bcca71dadcb1438f61e138d33a2a2374106a38842b13e346a2731cb` |
| `migration/babylon/map-package-riverbank-20261023-v4/city.json` | `87cd9d16267ed138df86700e4fde16dfbbf04e786465f9cd3dfbea8a563afb36` |

## Implemented foundations

- Native Boa modules: static and dynamic imports, cycles/live bindings,
  per-document caching, import metadata and top-level await. Module requests
  use the existing session network policy, bounded loading deadlines, status,
  JavaScript MIME and conservative credentialed CORS checks.
- Live DOM dataset, attribute reflection, subtree selectors/cloning, layout
  properties and CSS declarations backed by the render DOM and Stylo.
- Rust URL parsing and live URLSearchParams; viewport/window bindings and
  source-bearing JavaScript error stacks.
- Byte-preserving fetch ArrayBuffers, native BufferSource conversion,
  TextEncoder Uint8Arrays, SHA-256/384/512, Blob/Response byte consumption and
  bounded native gzip/deflate decompression.
- Shader precision constants used by Babylon's engine capability checks.
- An explicit CDP response-size setting. Oversized JS fetches reject instead
  of returning truncated bytes that could silently fail asset integrity.

Modules still use synchronous network waits on the JS thread. Import maps,
import attributes, non-HTTP module URLs, anonymous wildcard CDN imports and
exact async script timing are not implemented. Buffered byte streams have a
128 MiB limit and omit BYOB, tee, strategies and incremental decompression.
The partial WebGL2 surface causes the tested Babylon engine to select WebGL1.

## Actual-game observations

The candidate progressed from failing to parse `import.meta` to creating a
real Babylon WebGL1 engine, downloading the verified map package and decoding
gzip chunks. The initial 10 MiB response cap truncated the roughly 11 MiB city
file; the test server now uses an explicit 64 MiB cap.

The opt-in background loop is implemented and independently tested. The
optimized real-game candidate then reached character initialization, revealing
missing `AbortSignal.throwIfAborted`; exact-reason and default DOMException
handling now pass focused tests. A subsequent unmodified-game run remained
`aria-busy=true` for four minutes and produced a blank capture. Scene readiness
and visual acceptance therefore **fail** at this checkpoint; input, audio and
performance remain **unverified**.

The investigation is focused on an unresolved asset-preparation promise before
the game's rendering-readiness timeout. Application timer counters stop while
an independent heartbeat and `performance.now()` continue. Job-queue diagnostics
have shown a drain cap with zero pending native futures, so dropped native async
futures are a candidate mechanism, not an established cause of this run. Timer
exceptions now reach the CDP event sink rather than being silently discarded;
shader/draw calls have opt-in trace logging.

## Verification and remaining work

- Full workspace with WebGL, serial: 1,177 passed, 0 failed, 32 ignored. This run
  preceded the final buffer-cap and AbortSignal/diagnostic refinements.
- Final focused checks: 18 module/DOM/URL/hash/AbortSignal, 11 background-loop,
  10 binary-stream, 6 binary-XHR and 3 binary-fetch tests passed. Five existing
  abort-related library tests also passed.
- Core/render Clippy with all targets, WebGL and warnings denied passed after
  the timer/GL diagnostic additions; formatting and diff checks passed.
- Optimized WebGL builds passed. A pre-existing unused Result warning remains
  in the CLI agent world runtime.

A separate module-concurrency regression reproduced loss of 207 native
continuations after 1,000 productive passes. The executor now yields unstarted
jobs only when no local native futures remain, and counts consecutive lack of
progress separately. Fixed 32-root graphs with and without top-level await,
plus a 25,000-Promise responsiveness fixture, pass. Retained shader/glTF import
controls pass too, but passed before this correction and do not establish the
game's readiness.

The native executor's borrowed futures still cannot survive an indefinitely
external wait; its 1,000-consecutive-no-progress guard remains explicit.
Individual native calls and cooperative async drains may exceed the background
loop's ordinary task budget. These are limits, not a promise of full browser
scheduling or frame-rate parity.

Earlier workspace runs timed out in the Ghost handoff cancellation fixture.
A native stack sample showed the child stalled writing agent stdout before
the host abort reply. The fixture now drains the agent response before awaiting
the host reply, matching the existing production order and preserving its
assertions. The corrected Ghost suite passed 14/14. This does not establish
the exact cause of the observed stdout backpressure; production output
backpressure remains a separate robustness concern.

Reproduce browser checks with:

```sh
cargo test -p oxibrowser-core --features webgl --test es_modules --test binary_fetch --test binary_streams
cargo test --workspace --features oxibrowser/webgl -- --test-threads=1
cargo clippy -p oxibrowser-core -p oxibrowser-render --features oxibrowser-core/webgl --all-targets -- -D warnings
```

For the local retained game, serve its `public` directory over HTTP and start:

```sh
oxibrowser serve --allow-private-ips --background-event-loop --max-response-body-bytes 67108864
```

`--background-event-loop` is opt-in; MCP/Ghost sessions retain command-driven
execution. Synchronous native calls and module I/O cannot be preempted at a
short frame deadline, so the mode does not promise a hard real-time tick.
