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
- Initial external stylesheets retain their original link identity and cascade
  order. Their vetted HTTP responses reach the renderer through per-document
  resources, with charset decoding and final response URLs for CSS bases.
- An opt-in WebGL1 compatibility setting that declines WebGL2 before canvas
  type pinning, retaining real GLES2 shader compilation and rendering.
- Actual ANGLE requestable extension activation, including derivative shader
  support; unsupported extension methods are not advertised as empty objects.
- Bounded native raster decoding for Image, real Blob URL storage, and image
  uploads with intrinsic dimensions, flip/premultiply and unpack-state handling.
  Web Base64 preserves Latin-1 binary bytes instead of replacing invalid UTF-8.
- Correct GL binding tracking when another canvas is created or resized.
  Context destruction preserves another canvas's current binding. Construction
  is guarded against the pinned ANGLE Metal first-context capability race.
- Native WebGL2 queries, uniform-buffer binding/reflection and depth comparison
  constants keep Babylon on the genuine WebGL2 shader path. Indexed draws use
  byte offsets for all index widths, including instanced draws.
- KHR_parallel_shader_compile activates real ANGLE compiler workers and exposes
  native shader/program completion. Babylon selects its asynchronous pipeline.

Modules still use synchronous network waits on the JS thread. Import maps,
import attributes, non-HTTP module URLs, anonymous wildcard CDN imports and
exact async script timing are not implemented. Buffered byte streams have a
128 MiB limit and omit BYOB, tee, strategies and incremental decompression.
Dynamic stylesheet loading and recursive network CSS imports remain unsupported.
WebGL2 is partial and remains enabled by default. Query availability uses native
completion without browser task-boundary snapshots. Unimplemented WebGL2 groups
such as MSAA/MRT/3D textures remain outside this increment.
Image decoding is limited to 16k dimensions and 128 MiB. Image loading supports
data/Blob URLs and same-origin HTTP; cross-origin images and cross-origin
redirects are rejected. WebGL image uploads currently support RGB/RGBA UNSIGNED_BYTE;
ImageBitmap and canvas TexImageSources remain unsupported.

## Actual-game observations

The candidate progressed from failing to parse `import.meta` to creating a
real Babylon WebGL1 engine, downloading the verified map package and decoding
gzip chunks. The initial 10 MiB response cap truncated the roughly 11 MiB city
file; the test server now uses an explicit 64 MiB cap.

The opt-in background loop is implemented and independently tested. The
optimized real-game candidate then reached character initialization, revealing
missing `AbortSignal.throwIfAborted`; exact-reason and default DOMException
handling now pass focused tests. A subsequent unmodified-game run remained
`aria-busy=true` for four minutes and produced a blank capture.

The stalled GLTF extension imports were waiting for a CSS preload promise.
Inlining CSS had removed the original stylesheet link, so Vite created a new
dynamic link and waited for an unsupported load event. A controlled run restoring
only the already-downloaded link passed that stage and downloaded character
accessories. The implementation now preserves and renders the original links;
it does not synthesize successful resource events.

That controlled run then exposed inconsistent Babylon version detection: its
engine downgraded a partial WebGL2 context to WebGL1 because query APIs were
missing, while shader state detected WebGL2 constants and prepended GLSL 300 to
WebGL1 shaders. A vertex shader failed on the reserved `attribute` keyword.
Declining WebGL2 requests allows a consistent, real WebGL1 context instead.
That path exposed disabled optional ANGLE extensions. Genuine extension
activation resolved the derivative shader compilation error.

The next actual run reached 1,946 scene meshes and 15 compiled, ready effects,
but timed out during prepareMeshes. Inspection found 93 of 94 textures ready;
the sole unready texture was the embedded EnvironmentBRDF image. Image loading,
Blob URL decoding and TexImageSource uploads now pass actual PNG/GPU fixtures.
The subsequent genuine WebGL1 run passed that image gate and reached PBR
compilation. Its final fragment source uses `fwidth` in the custom stone
program at line 909, with no derivative extension directive. ANGLE correctly
rejects it. Native query/UBO support now keeps Babylon on WebGL2 and compiles
those shaders correctly. The retained Engine fixture reports version 2 and GL
error 0. Its default Mac Chrome driver workaround disables UBO usage; independent
UBO tests verify actual GPU updates.

The next real runs, including genuine parallel compilation, still exceeded the
45-second readiness deadline. All 34 shared effects and all textures were ready,
but the first readiness sweep had not reached a render (render ID 0). A controlled
recreation of an actual PBR effect took 1,891 ms to dispatch and 1,912 ms to
complete. Native samples put 338 of 408 JS-thread samples in Boa String.split /
StringIndexOf: its Windows iterator repeatedly skips the whole prefix rather
than jumping to the requested offset. A bounded string-library optimization is
the next step before scene and movement acceptance. Audio and frame-rate
performance remain **unverified**; game sources and assets are unchanged.
Timer exceptions reach the CDP event sink; shader/draw calls and original shader
sources have separate opt-in trace targets.

Parallel native tests isolated a second failure below Boa: the pinned ANGLE
Metal backend sets its shared capability-initialized flag before filling the
capabilities. Cold concurrent contexts copied zero precision/viewport limits
and returned black pixels with GL_NO_ERROR. A standalone native control failed
cold and passed after prewarming. Guarding construction through its first
makeCurrent makes the cold parallel regression pass; normal draw calls remain
independent across contexts.

## Verification and remaining work

- Full workspace with WebGL, serial: 1,213 passed, 0 failed, 34 ignored after
  the Image and GL capability/lifecycle additions. After the final subtree
  retention sweep, the core library passed 715 tests with two ignored, and
  all three forced-GC cases and ten image tests passed.
- Ten image lifecycle/network tests, a forced-GC connected-image regression,
  real PNG-to-GPU uploads, cold parallel native context initialization and
  cross-canvas creation/resize tests passed. Eight GL session tests passed
  in parallel; binary Latin-1 Base64 checks passed.
- Final focused checks: 18 module/DOM/URL/hash/AbortSignal, 11 background-loop,
  10 binary-stream, 6 binary-XHR and 3 binary-fetch tests passed. Five existing
  abort-related library tests also passed.
- Core/render Clippy with all targets, WebGL and warnings denied passed after
  these additions; formatting and diff checks passed.
- Optimized WebGL builds passed. A pre-existing unused Result warning remains
  in the CLI agent world runtime.
- Native WebGL2 GPU/query/UBO tests, the parallel-compiler test in both context
  versions, nonzero-offset draws and serial existing GL tests pass. The retained
  Babylon fixture uses its real async pipeline and completes a synthetic tiny
  effect callback; this is not a game startup or frame-rate result.

Parallel controls remain non-green: the pre-existing two-canvas screenshot
fixture can retain an earlier color, and an occlusion result can be zero in a
concurrent run. Isolated/serial controls pass; expectations remain unchanged.
The native cold-capability race fix does not establish general parallel GPU
isolation. These failures remain separate follow-up evidence.

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

At this checkpoint readiness still fails. Adding `--disable-webgl2` is a
diagnostic fallback that stops at the custom PBR shader; neither recipe yet
establishes playable game support.

`--background-event-loop` is opt-in; MCP/Ghost sessions retain command-driven
execution. Synchronous native calls and module I/O cannot be preempted at a
short frame deadline, so the mode does not promise a hard real-time tick.
