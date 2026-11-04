# OxiBrowser capability and performance baseline

Date: 2026-10-27 KST. Audited engine: `edee0cfee8bd95f2144b63f34be2b22e4ac2c69e`.

OxiBrowser can extract documents, preserve binary Fetch data, run a native module graph and render the retained Cheonggye scene. General interactive application compatibility remains incomplete: a minimal React 18.2 counter fails to mount, DOM queries lose JavaScript object identity, CSSOM disagrees with authored styles, and several input and CDP operations expose incomplete behavior.

The owner selected the whole browser—functionality, compatibility and performance—for investigation. Further engine implementation is on hold until workloads, success criteria and design are agreed. The ordering below is a proposal for an agent-automation product, not an approved implementation roadmap.

## Evidence and interpretation

[Machine-readable evidence](2026-10-27-capability-evidence.json) includes fixtures, diagnostic scripts, commands, artifact hashes and results. The main local comparison ran twice against the same HTML and hash-verified React assets in fresh OxiBrowser connections and an isolated Chrome profile. A supplemental layout/timer check ran once. Three independent CLI processes were sampled for each process/memory case.

Reference: Chrome `156.0.8078.48`; candidate: optimized OxiBrowser with `webgl` and `--background-event-loop`. Host: macOS 27.0.1, Mac16,9, 16 logical CPUs, 64 GiB RAM. All pages were served on loopback; no real account, external site or current user Chrome profile was used. Chrome is a behavioral reference, not an adopted engine dependency or an agreed full-parity goal.

The comparison covers selected behaviors across browser subsystems. It is not a full WPT/Test262/Khronos run. Screenshot viewport behavior was not normalized for pixel-parity comparisons. API presence alone is not a pass. Statements marked **measured** describe these fixtures; **source** describes an inspected implementation limit; **prior** refers to a retained result on the same engine revision; **unmeasured** remains a question.

## Foundational correctness findings

These failures affect ordinary stateful web applications before advanced media or graphics APIs become relevant.

| Finding | Candidate observation | Reference observation | Evidence and consequence |
| --- | --- | --- | --- |
| DOM identity and node-owned state | Repeated `getElementById` returns unequal objects; an assigned expando is absent on the next lookup | Same object; expando remains `42` | Measured twice, `window_and_dom`. Framework state stored on DOM objects cannot be relied upon. [Element factory](../../crates/oxibrowser-core/src/js/runtime.rs), `create_render_element_object` |
| Global/Window model | `window !== globalThis`, while `window === self` | Both equal | Measured twice. APIs are mirrored onto a distinct Window facade; constructor/prototype behavior needs a coherent realm contract. This inequality alone does not establish the cause of every application failure. |
| CSSOM versus rendered styles | Authored width `123px` and color `rgb(12,34,56)` read as `auto` and black | Correct authored computed values | Measured twice; `getComputedStyle` uses snapshot `LayoutEngine::compute_style`, while painting uses Stylo. [Implementation](../../crates/oxibrowser-core/src/js/runtime.rs), `get_computed_style_fn` |
| Geometry versus painted position | Layout fixture reports `(0,0,123,45)` although the screenshot paints the box inside the 8px body margin | `(8,8,123,45)` | Measured once, `layout`. Width/height work in this case; viewport coordinates do not match. [Binding](../../crates/oxibrowser-core/src/js/runtime.rs), `get_bounding_client_rect_fn`; [layout accessor](../../crates/oxibrowser-render/src/document.rs), `node_layout_rect` |
| Minimal framework startup | React 18.2 counter never mounts during the six-second polling window | Mounts and changes `count:0` to `count:1` after programmatic click | Measured twice, `react18`. First observed failure is `getActiveElementDeep`: `element instanceof win.HTMLIFrameElement`, with that constructor absent. This identifies the first failing operation, not proof that adding one constructor would finish React support. |

The layout screenshot is evidence that real CSS painting exists; the problem is inconsistent observable page state, not a claim that the entire renderer is a placeholder. The earlier [headless-browser study](../research/2026-10-25-headless-browser-feasibility.md) already identified several of these structural gaps. Its missing-ESM, binary-Fetch and crypto-presence findings are superseded by current implementations; the identity, realm and CSSOM issues still reproduce.

Unmodified layout captures: [OxiBrowser](assets/2026-10-27-oxibrowser-layout.png), [Chrome](assets/2026-10-27-chrome-layout.png). Capture extents differ; these images support the fixed-size box observation, not pixel-parity or screenshot-default equivalence.

## Capability inventory

| Area | Current boundary | Evidence | Impact |
| --- | --- | --- | --- |
| Events | Target/ancestor/document/window bubbling exists. Capture order and `{once:true}` are wrong: capture runs after target; once-listener fires twice | Measured twice, `event_options`; [dispatcher](../../crates/oxibrowser-core/src/js/runtime.rs), `dispatch_bubbling_event` | Event delegation and frameworks need more than successful listener registration |
| Focus and validation | `focus()` leaves active element at body; required input has no `checkValidity` | Measured twice, `focus_validation`; [runtime](../../crates/oxibrowser-core/src/js/runtime.rs) | Keyboard targeting, selection restoration and browser form validation are incomplete |
| File upload | CDP sets input value to a path, but no usable `files` list appears; reference selects one actual file | Measured twice, `file_upload`; [best-effort implementation](../../crates/oxibrowser-cdp/src/domains/dom.rs), `set_file_input_files` | An acknowledged CDP method does not establish file-upload support |
| Frames | Per-frame execution contexts exist in source/tests; a same-origin iframe has no parent-facing `contentWindow` or `contentDocument` in this fixture | Measured twice, `iframe`; [frame setup](../../crates/oxibrowser-core/src/session.rs), `inject_child_frames` | Isolated child evaluation and normal parent/child DOM interaction are different capabilities |
| Canvas 2D | Context exists, but red fill reads back `[0,0,0,0]`; drawing methods are no-ops and text metrics are approximated | Measured twice, `canvas2d`; [bootstrap](../../crates/oxibrowser-core/src/js/runtime.rs), `CANVAS_BOOTSTRAP` | Feature detection can select a drawing path that produces no pixels |
| WebGL2 | Native context, query and UBO methods exist; `texImage3D`, `drawBuffers`, multisample renderbuffer storage, transform feedback, sync and immutable texture-storage methods are absent in the tested set | Measured twice, `webgl2`; [bindings](../../crates/oxibrowser-core/src/js/webgl/api/webgl2.rs) | A non-null context is not a full WebGL2 contract; framework compatibility must be measured |
| Parallel graphics | Prior concurrent controls intermittently retain an earlier canvas color or return zero occlusion results; serial controls pass | Prior; [retained failure record](cheonggye-runtime.md) | General concurrent-context stability is not established by a single game session |
| Binary Fetch and streaming | `[0,128,255,65]` is preserved. Network response has no readable `body`; constructed Response/byte streams are a separate buffered subset | Measured twice, `binary_fetch`; [stream implementation](../../crates/oxibrowser-core/src/js/runtime/binary_streams.rs) | Binary correctness is supported here; incremental/backpressure-driven network streams are not |
| Modules and asynchronous work | Native ESM/TLA exists, but module I/O blocks the JS thread. Borrowed native futures retain a bounded external-wait limitation | Source; [module loader](../../crates/oxibrowser-core/src/js/runtime/modules.rs), [executor](../../crates/oxibrowser-core/src/js/job_queue.rs) | Loading and responsiveness are coupled to native calls and cooperative draining |
| Resource lifecycle | Initial stylesheet/font resources exist. Dynamically appended stylesheet does not fire load in the fixture; dynamic stylesheet fetching and recursive network imports are outside the implementation scope | Measured twice, `styles_and_timers`; [font/resource scope](../../crates/oxibrowser-core/src/fonts.rs), [resource resolver](../../crates/oxibrowser-render/src/resources.rs) | Late-loaded routes, themes and widgets need explicit resource-lifecycle coverage |
| CSS animation | After waiting, the animation-width getter is `auto`; reference reaches `80px`. The render document resolves with time `0.0` | Measured getter difference plus source; [resolve path](../../crates/oxibrowser-render/src/document.rs) | This probe does not independently prove painted animation behavior because the candidate CSSOM is already wrong; visual animation verification remains open |
| Workers and channels | Dedicated/shared workers and MessageChannel are absent; Service Worker was absent in the preceding presence probe | Measured presence; source inventory | Background computation, worker-hosted runtimes and offline application lifecycle are unavailable in the tested surface |
| Media and newer graphics | AudioContext, OfflineAudioContext, MediaSource, MediaRecorder, RTCPeerConnection and OffscreenCanvas are absent; navigator.gpu was absent in the preceding probe | Measured presence | No claim of real Web Audio, media-source playback, RTC or WebGPU support; media-element playback itself was not exercised |
| IndexedDB | JSON/string-oriented subset. Index/cursor methods and upgrade-request transaction abort are absent; writes apply immediately rather than at a real transaction commit | Measured twice, `idb`, plus explicit [implementation deviations](../../crates/oxibrowser-core/src/js/idb.rs) | Structured records, transactional applications and offline state need more than token persistence |
| Web Crypto | Only the tested `digest` method exists; encrypt/decrypt/sign/verify/key derivation/key generation do not | Measured twice, `crypto_wasm`; [digest implementation](../../crates/oxibrowser-core/src/js/runtime/crypto_digest.rs) | Hash verification is implemented; broad Web Crypto compatibility is not |
| WebAssembly | Basic namespace, modules, instances, memory and tables exist; Global and streaming APIs are absent; global imports and JS-host re-entry have implementation limits | Measured API differences plus [source](../../crates/oxibrowser-core/src/js/wasm.rs) | WASM support is a bounded bridge, not complete browser-WASM compatibility |
| CDP objects and debugging | `returnByValue:false` still returns an inline object without objectId; Debugger.enable returns unknown-domain | Measured twice; [Runtime](../../crates/oxibrowser-cdp/src/domains/runtime.rs), [domain routing](../../crates/oxibrowser-cdp/src/domains/mod.rs) | Remote object lifetimes, JS handles and debugger-based failure investigation are missing |
| Tracing | Trace output contains a metadata event, not execution/network timelines | Source; [trace end](../../crates/oxibrowser-cdp/src/domains/tracing.rs), `end` | Current Tracing does not attribute loading/frame cost |
| PDF | PDF pages wrap raster screenshots; header/footer, ranges and scale are accepted but ignored | Source; [printToPDF](../../crates/oxibrowser-cdp/src/domains/page.rs), [PDF paint](../../crates/oxibrowser-render/src/paint.rs) | Valid PDF output is narrower than browser print-layout/text semantics |
| HTTP caching and authentication | No HTTP cache layer. Legacy Network interception/auth acknowledgements are no-ops; the Fetch domain has a separate implemented interception path | Source; [Network](../../crates/oxibrowser-cdp/src/domains/network.rs), [Fetch](../../crates/oxibrowser-cdp/src/domains/fetch.rs) | Repeated-load behavior and exact client methods need separate verification; module memoization is not HTTP caching |

Absence observations apply to this artifact/configuration. Existing Range/Selection constructors or other named interfaces are not counted as complete implementations merely because `typeof` returns `function`.

## Scheduling and execution modes

The background loop is opt-in and incompatible with `--mcp`. In a single 500ms command-free wait, the tested interval counter advanced 20 times with background mode and zero times with the default server; Chrome advanced 25 times. These are coarse behavior samples, not a timer-precision benchmark. The default server can advance queues when subsequent commands execute; zero autonomous ticks is a mode boundary, not proof that all asynchronous automation fails.

The [background loop](../../crates/oxibrowser-core/src/js/runtime/background.rs) checks a soft 16ms budget between callbacks; synchronous host calls cannot be interrupted by that check. Foreground evaluation also drains application queues under foreground runtime limits in `run_eval`. Whether those policy boundaries cancel legitimate application work needs a focused regression before being labelled a diagnosed failure.

## Performance baseline and missing measurements

Candidate binary: 67,089,664 bytes, approximately 64.0 MiB, built with `webgl`. The following are fresh CLI processes with warm OS caches. Wall time includes process launch; maximum RSS is the Darwin process peak, not steady memory or browser-tree aggregate memory.

| Workload | Samples | Median wall time | Largest process peak RSS |
| --- | --- | --- | --- |
| Print version; no page | 3 | 11.3ms | 12.2 MiB |
| Create one blank tab, exit | 3 | 27.7ms | 28.0 MiB |
| Fetch one plain local document, extract text, exit | 3 | 141.2ms | 48.2 MiB |

These samples do not establish p95, steady-state memory, concurrent-tab cost or a Chrome performance ratio. README startup/base-memory values need a stated artifact and workload before comparison; a WebGL-enabled artifact is not interchangeable with a browser-only release profile.

The [earlier Cheonggye result](cheonggye-runtime.md) observed readiness at 160.4 seconds and approximately 2.9 fps in a short command-free sample. It proves a slow basic graphics workload, not that every browser operation takes that long. The current frame bottleneck has not been attributed among JS execution, scene computation, bindings, driver work, readback or composition. The prior shader-string optimization measures one resolved loading hot path, not a complete frame profile.

Missing performance baselines include successful stateful-SPA interaction, navigation and resource-phase timings, input-to-visible-update latency, frame-time distribution, network concurrency/backpressure, memory after repeated navigation, and concurrency scaling. A failed workload must not contribute a misleading fast completion time.

## Test and delivery gaps

- The compatibility corpus calls a successful nonempty text output `PASS`, even with JavaScript errors. Its 17/21 report is a text-extraction result, not an application-completion rate. [Classifier](../../crates/oxibrowser/tests/compat_corpus.rs)
- The pinned Khronos fixture lists six pages and is ignored in ordinary test runs. It is not a full WebGL conformance result. [Subset](../../crates/oxibrowser-core/tests/fixtures/webgl_conformance_subset.txt)
- The earlier same-revision full WebGL workspace run passed 1,229 tests with 35 ignored. The new differential findings demonstrate that this count does not cover the behavioral contract above. This audit did not rerun the full workspace because production code was unchanged.
- Current [CI](../../.github/workflows/ci.yml) tests/builds `browser`, and the [macOS release workflow](../../.github/workflows/release-macos.yml) also builds `browser`; neither enables `webgl`. Local graphics acceptance therefore does not establish the contents or GPU coverage of those artifacts.
- Account grants, native credential actions, redaction and origin/network policies have implementations and earlier tests. No actual-account workflow, formal sandbox/security audit or broad site authentication claim is established by this local comparison. Future engine design must preserve those contracts.

## Goals to decide before implementation

The candidate first goal, given the existing agent-automation product description, is **reliable completion of representative stateful page tasks with consistent DOM, Window, CSSOM and input behavior**. Adding every absent API is not implied by this audit.

| Decision | Proposed measurable form | Still to decide |
| --- | --- | --- |
| Supported workloads | A fixed set covering document extraction, vanilla forms, a stateful framework, an embedded frame, resource loading and optional graphics | Which tasks define product support; supported framework/artifact versions |
| Correctness | Explicit expected values, state changes and output for each task; capability labels distinguish unsupported from partial and verified | Which failures block a release; any accepted limits |
| Performance | Successful-task load/interaction/frame times plus peak/steady/aggregate memory under a fixed machine, viewport, network and cache policy | Per-workload budgets and repeat counts; reference-browser comparisons |
| Stability | Repeated navigation and concurrent-context runs with crashes, unhandled errors, lost work and memory growth recorded | Concurrency target and stress duration |
| Delivery | Feature-specific CI and artifact identity tied to the declared support profile | Which profiles/platforms are distributed and must pass |

Design should then answer these architectural questions:

1. How do document-owned JS objects retain identity, expandos, prototypes and listeners across queries without leaking across document replacement or origins?
2. How do the global object, Window facade/proxy and DOM interface constructors share a consistent realm contract?
3. How do CSSOM, geometry, hit-testing, painting and observers consume the same resolved page state and coordinate space?
4. How are module/resource I/O, application jobs, agent evaluation, cancellation and deadlines scheduled without discarding work or obscuring errors?
5. Which graphics/media/storage standards are full targets, which are explicit subsets, and how does capability detection communicate that truth?
6. Which real measurements expose cost before selecting JS, layout, graphics, caching or composition optimizations?

No architecture alternative or numeric performance target in this section has been approved or implemented.

## Reproduction

The evidence JSON contains the exact owned HTML/CSS/binary fixtures and diagnostic scripts. The React 18.2 development UMD files are not copied into the repository; their pinned archive members and SHA-256 values are recorded, and both local copies were verified before execution.

Restore those assets and fixtures into a fresh temporary directory, replace the original temporary-directory prefix in the recorded scripts, and use the recorded loopback commands/ports. Use a fresh headless Chrome profile. The main probe writes one report per browser; `TASK_AUDIT_RUN` preserves repeated receipts. The supplement distinguishes default and background OxiBrowser modes. The process sample commands and numerical runs are retained separately in the same JSON.

Keep raw local receipts when changing fixtures; new bytes invalidate only the affected comparisons. Remove only task-owned servers and Chrome profiles. All servers and browser processes used for this audit were stopped after collection.
