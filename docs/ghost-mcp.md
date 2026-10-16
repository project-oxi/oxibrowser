# Ghost MCP and native DOM integration

Launch one supervised child process for each `(Ghost UUID, Ghost session UUID)`:

```sh
oxibrowser serve --mcp --ghost <Ghost-UUID>
```

This reuses OxiBrowser's newline-delimited stdio MCP transport, BrowserContext,
Session, DomSnapshot and RefRegistry token naming. Ghost observation/actions
use a dedicated Rust command on the JS thread; regular OXI dispatch remains
available to existing CDP clients. No TCP
listener, account registry, keychain, saved profile, cookie file, or persistent
audit log is opened. All browsing state starts empty in memory. The Ghost ID is
a correlation label, not authentication. The host owns process handles and must
never share a child between Ghosts. Different tabs in a child share its isolated
context; a restarted child starts empty. Existing regular MCP/CDP modes retain
their existing behavior.

The host must enforce Ghost's capability policy **before launching this child or
forwarding a request**. This adapter does not grant browser access or authenticate
humans. It cannot replace Ghost's effect ledger. No Ghost core files are changed.

## Request and result contract

Use `initialize`, `tools/list`, `ping`, and `tools/call`. Each call requires a
string `params._meta.ghostTaskId` of 1–128 bytes. Timeout is 1–30,000 ms, default
10,000. JSON-RPC IDs are nonempty strings up to 128 bytes or signed integers.
The same ID is reserved for the same method and parameters within a child:

```json
{"jsonrpc":"2.0","id":"open-1","method":"tools/call","params":{"name":"browser_new","arguments":{},"_meta":{"ghostTaskId":"task-uuid","timeoutMs":10000}}}
{"jsonrpc":"2.0","id":"nav-1","method":"tools/call","params":{"name":"browser_navigate","arguments":{"tab_id":"t1","url":"https://example.com"},"_meta":{"ghostTaskId":"task-uuid"}}}
{"jsonrpc":"2.0","id":"observe-1","method":"tools/call","params":{"name":"browser_snapshot","arguments":{"tab_id":"t1"},"_meta":{"ghostTaskId":"task-uuid"}}}
```

The result contains MCP text content and the identical `structuredContent` object,
`isError`, and host-owned `_meta` correlation (`ghostId`, `ghostTaskId`,
`requestId`, `browserSessionId`). Initialize returns the newly minted
`_meta.browserSessionId`. The host should bind that ID to its child process; it
is an ephemeral namespace, not an access token.
Page strings live inside data, labelled `untrusted_page_data`; tool results are
labelled `untrusted_tool_data`. They are never parsed as requests or evaluated as
host commands. A page cannot mint approval, resume, identity, or correlation
metadata. The host must retain that separation when constructing model prompts;
string labels alone do not defend against prompt injection.

Tools are `browser_new`, `browser_tabs`, `browser_navigate`, `browser_snapshot`,
`browser_click`, `browser_fill`, and `browser_close`. All except new/tabs require
an explicit `tab_id`. Action tools require the opaque `ref` returned by snapshot;
fill also requires `value` and a target marked `fillable`. Extra arguments are
rejected. There is no eval,
arbitrary-selector action, filesystem export, account, credential, cookie,
storage-export, approval, or user-input impersonation tool. Password/file/hidden controls
are omitted from the actionable targets.

Navigation accepts HTTP(S) and exactly `about:blank`, rejects embedded URL
credentials and file/javascript/data URLs, and retains the normal SSRF filter.
`--allow-private-ips` is a process-local development option for local fixtures;
it changes no persistent setting. It is not needed for ordinary public pages.
The engine may run page scripts and fetch subresources. This is not an OS sandbox
or an origin/domain authorization system; network policy remains the host's job.

Snapshot returns live URL, navigation title metadata, generation, accessibility
tree and up to 64
actionable elements. Tree is capped at 8,000 UTF-8 bytes; URL 2,048, title 256,
target names/text 256. `tree_truncated` and `omitted_elements` report omissions.
`title_source:navigation_metadata` identifies the title as metadata seeded on
navigation, rather than a promise of dynamic document.title support. Snapshot
processing still depends on engine DOM size. Refs are scoped to a freshly minted
process-session UUID, tab,
snapshot epoch, native document ID, whole-tree structural revision, node
incarnation, attachment revision, exact attribute map and fingerprint. Target fields are private
Rust values stored by the host, not page strings or selectors.
Every snapshot, navigation or action attempt invalidates older refs. Close is
terminal for a tab ID; a call for a closed/unknown tab fails instead of choosing
another tab. After an action, re-observe before choosing the next action.

## Concurrency, deduplication and interruption

Only one tool operation executes at a time. A competing call gets `BUSY` and is
remembered under that ID; use a new request ID after observing current state.
Initialize/list/ping and cancellation remain responsive while an operation runs.
An identical in-flight duplicate shares the original response. An identical
completed duplicate replays the exact response without performing an effect.
Changed payloads with the same ID fail `REQUEST_ID_CONFLICT`. The ledger holds
at most 1,024 IDs without eviction; additional new IDs fail
`REQUEST_LIMIT_RESTART`. Input lines are bounded at 65,536 bytes; oversized or
non-UTF-8 input emits an invalid request and ends input. IDs are not durable
across restarts: persist Ghost's effect intent/receipt outside the browser.

Cancel the current request using the MCP notification:

```json
{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"nav-1"}}
```

A cancellation for another/completed ID has no effect. Cancellation, timeout,
Ctrl+C, SIGTERM, or input closure during an operation return `CANCELLED`,
`TIMEOUT`, `INTERRUPTED`, or `INPUT_CLOSED` with `mayHaveActed:true` and
`restartRequired:true`, then close this child process. In-engine/network work
may already have acted; dropping a future does not prove rollback. Never retry
an uncertain click, fill or navigation automatically. Restart only after the
supervisor reconciles the task. EOF while idle closes normally. The host should
bound process shutdown independently and mark effects uncertain if no receipt
arrives. Validation errors are data errors with `mayHaveActed:false`. Engine
failures
during navigation or an action conservatively return `mayHaveActed:true` and
end the child, because an error does not prove rollback. Browser engine error
text is also untrusted.

## Remaining integration work

Ghost must implement capability checks, child ownership, request correlation,
effect-intent persistence, response reconciliation, process watchdog and real
local-user validation in its separately owned task. Policy approvals use Ghost's
trusted local user channel. The optional inherited socketpair now implements a
local ownership state machine and restricted host operations, documented in
[ghost-handoff.md](ghost-handoff.md). It adds no public listener or human viewer.
Page scripts/network are not frozen. No production-site compatibility is claimed.
Ghost click/fill now validate and mutate on the JS thread without page eval,
page selectors, global Event/MouseEvent/InputEvent constructors, prototype methods
or callback return-value conversion. Native listener entries bind to a node
incarnation; new documents clear them. Detach/reinsert preserves ordinary
listeners but invalidates observed targets, including a detach/reinsert during
input delivery. Parent links must agree with the native child lists.

Fill writes native input/textarea value attributes or contenteditable text, then
delivers input and change. Click delivers click to the native target and ancestor
listener registry. Host-built null-prototype event objects have read-only type,
target/currentTarget and isTrusted=false, with Rust-backed cancellation controls.
Returns and thrown JS objects cannot construct a receipt or approval. Receipts
identify `execution:native_dom_synthetic_event`, `human_input:false`, callback
count and whether a navigation was queued, including by a microtask. The count
is not proof of an intended business effect: always re-observe actual effects.

This is a bounded native DOM path, not an OS input driver or a full isolated
world. Only physical main-document targets are actionable; detached shadow
content and child frames are omitted. There is no capture phase, listener
options/once semantics, native anchor/form/checkbox defaults, focus management,
OS keyboard/pointer interaction or faithful Event class identity. The handler
list is captured when delivery reaches each node. Event objects are fresh per
ancestor. Inline onclick attributes are not registered by the current engine.
The legacy document.title setter is absent, so the existing title-source label
is retained; native observations read the live title node.

Each observation pump and pre-/post-action pump executes at most 256
microtasks/timer callbacks. Actions limit listener delivery to 256 and ancestry
to 1,024, and apply VM loop/recursion/stack limits. A 500ms elapsed check guards
timer delivery; it is not a hard wall-clock limit on a job or whole command. It does not pump fetch/WebSocket/IndexedDB completions;
those retain the existing engine path. A blocking native binding/dialog is not
preempted by VM instruction limits; the supervisor must independently kill an
unresponsive child and mark its effect uncertain. Snapshot creation still costs
work proportional to engine DOM size. General page navigation/script execution
uses the existing engine and is not a hostile-page sandbox. No OS sandbox or
production-site reliability is claimed.

The trusted handoff/resume implementation is in [ghost-handoff.md](ghost-handoff.md).
It binds a controlled child to one task and requires exact session/task/epoch
headers, private-host ownership changes, ref invalidation and fresh observations
of all open tabs after resume. Human UI and script/network freeze remain absent.

## Reproduction

```sh
CARGO_BUILD_JOBS=1 cargo test --offline -j1 -p oxibrowser --features browser --test ghost_handoff -- --test-threads=1
CARGO_BUILD_JOBS=1 cargo test --offline -j1 -p oxibrowser-core --test native_input -- --test-threads=1
CARGO_BUILD_JOBS=1 cargo test --offline -j1 -p oxibrowser --features browser --bin oxibrowser mcp::ghost::tests -- --test-threads=1
CARGO_BUILD_JOBS=1 cargo test --offline -j1 -p oxibrowser --features browser --test ghost_mcp -- --test-threads=1
```

Tests use temporary loopback HTTP fixtures and anonymous child processes only.
They exercise observe → fill → observe → click → observe, stale and cross-tab
refs, closed tabs, competing navigation/close, pending and completed duplicate
requests, ID conflicts, cancellation, timeout, denied capabilities, partial
effects followed by JS exceptions, and separate
Ghost fixture storage and rejection of another child's refs, even when tab/epoch
counters match. No existing browser is attached or closed. GUI/GPU and
real login/cookie/password/user-profile tests are not part of this unit.

## Verification on 2026-10-02

Verified on the selected macOS host with Rust/Cargo 1.98.1, offline builds at
`-j1`, and test harnesses at `--test-threads=1`:

| Suite | Result |
| --- | --- |
| `oxibrowser --test ghost_handoff` | 14 passed |
| `oxibrowser --test ghost_mcp` | 10 passed |
| `oxibrowser-core --test native_input` | 13 passed |
| `oxibrowser --bin oxibrowser mcp::` (includes 4 new Ghost unit tests) | 16 passed |
| `oxibrowser-cdp --lib` | 75 passed |
| CDP E2E `test_credential_mode_denies_password_literal_fill_and_import` | 1 passed |
| `oxibrowser-core --test script_exec` | 4 passed |
| `oxibrowser-core --lib test_mutation` | 5 passed |
| `cargo fmt --all -- --check` | passed |
| `git diff --check` | passed |

The initial inline-onclick fixture and dynamic-title assertion failed. Source
inspection established the engine limitations described above; the final
fixtures assert actual DOM effects and a visible counter to prove at-most-once
execution. A native combined same-parent-append/timer fixture initially failed: Blitz
keeps a parent link while its same-parent append removes the child from the
child list. The native path now checks both directions of the relationship.
The timer fixture uses a fresh document, and explicit detach/reinsert tests
verify reference invalidation and listener preservation. The existing
same-parent append compatibility defect is not repaired here. The two initial
Rust compile errors (a private Boa method and the CoreError import path) were
corrected. The native checkpoint totaled **124 passed, 0 failed**. The subsequent handoff
unit preserves those suites and adds 14 local supervisor fixtures. Final
combined validation on the same source: **138 passed, 0 failed**. All selected
suites were run offline with build/test parallelism limited to one. The build emitted the
existing `McpAccounts::empty` dead-code warning and a macOS debug-linker
`__eh_frame` size warning.

Not run: full workspace/ignored internet suites, live third-party websites,
real accounts or stored browser data, human handoff/resume UI, GUI/GPU,
release packaging, deployment or push. No screenshot was produced, so there
is no screenshot Library ID. The default development binary is
`target/debug/oxibrowser`; it was rebuilt by the subprocess integration suite.
The starting checkout was clean `main` at `0aaa7d0` and no existing browser
process was attached or terminated.
