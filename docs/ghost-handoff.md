# Ghost local supervisor handoff

Implemented on the selected Mac: a task-bound local ownership state machine over
an **inherited anonymous AF_UNIX stream socketpair**. Agent MCP remains on
stdin/stdout. This creates no listener, pathname socket, remote service, saved
credential, profile import or persistent access setting. The default Ghost mode
without a control descriptor retains the earlier seven-tool contract.

## Launch and trust boundary

The trusted supervisor creates a socketpair, passes only one endpoint into the
child (clear that endpoint's close-on-exec flag for this spawn), and closes its
copy of the child endpoint after spawning:

```sh
oxibrowser serve --mcp --ghost <Ghost-UUID> \
  --ghost-task <task-UUID> --ghost-control-fd <inherited-fd-number>
```

The descriptor must be >=3, valid, connected, anonymous, AF_UNIX and SOCK_STREAM.
Regular files, datagrams, stdin/stdout/stderr and invalid descriptors are rejected
before reading. OxiBrowser restores close-on-exec on its endpoint. Socket address
and type checks establish the transport shape; **possession of the private
endpoint is the authority**. They do not authenticate a human or isolate another
process with the supervisor's OS privileges. The host must keep this descriptor
outside agent access, continuously drain agent stdout in every ownership state,
and validate real local user interactions before issuing
handoff/resume/human commands. IDs are bindings, not access tokens. Ghost's
capability policy and effect ledger still belong to Ghost; no Ghost core changed.

The child sends `ghost/ready` on the host channel with a newly minted
`browserSessionId`, the bound task, controlEpoch=0 and state=agent_active. All
agent tool calls in controlled mode require the exact current values in
`params._meta.{browserSessionId,ghostTaskId,controlEpoch}`. The supervisor should
inject these from its own child handle/state, not from model/page strings.
Initialize/ping/tool-list remain available and include `_meta.ownership`.

Both channels use bounded newline-delimited JSON-RPC (64KiB input, 32 queued
frames); their independent request ledgers retain at most 1,024 IDs without
eviction. Host output has a 1-second write budget; failure closes ownership.
`ghost/*` methods on agent stdin are rejected. There is no browser_handoff,
browser_resume, approval, eval, cookie/credential or profile tool.

## Host protocol

`ghost/status` takes an empty params object and returns `result.hostState`.
All other host requests require exact `session_id`, `task_id`, current `epoch`
and `expected_state`; unknown fields fail validation. State changes are made by
Rust only. Page text, JS returns/errors and console output never become commands.

```json
{"jsonrpc":"2.0","id":"take-1","method":"ghost/handoff","params":{"session_id":"<browserSessionId>","task_id":"<task-UUID>","epoch":0,"expected_state":"agent_active","handoff_id":"handoff-UUID"}}
{"jsonrpc":"2.0","id":"resume-1","method":"ghost/resume","params":{"session_id":"<browserSessionId>","task_id":"<task-UUID>","epoch":1,"expected_state":"human_owned","handoff_id":"handoff-UUID"}}
```

| Method | Additional params | Behavior |
| --- | --- | --- |
| ghost/handoff | handoff_id (new, 1..128 bytes) | Block new agent calls, wait for any admitted operation to settle, invalidate all refs, advance epoch and acknowledge human_owned. Allowed from agent_active or resume_pending. |
| ghost/resume | exact handoff_id | Allowed only while idle and human_owned. Invalidate all refs, advance epoch, enter resume_pending. |
| ghost/human | exact handoff_id, name, arguments; optional timeoutMs (1..30000, default 10000) | Execute one of the existing seven restricted browser operations through the host channel, only while human_owned. Agent stdin cannot invoke it. |
| ghost/cancel | channel (agent or human), request_id | Cancel that exact currently active operation; effects become uncertain and the child closes. A different/completed target is a no-op. It never resumes ownership. |

Host results separate Rust-owned `hostState` from `data`. Human-operation data is
wrapped as `data.toolResult` with `trust:untrusted_tool_data`; page strings retain
their native untrusted labels. These are still synthetic native DOM events with
`human_input:false` and `isTrusted:false`, not OS human input or a user approval.
The host must verify a real local interaction before forwarding a human command.

## States, admission and reconciliation

`agent_active → pause_requested → human_owned → resume_pending → agent_active`.
Any uncertain operation, timeout/cancellation, input closure, signal, failed host
write or lost host channel moves to `closed` and ends this child. There is no
automatic resume after supervisor loss. Cancellation/death are not rollback.

Pause has no transfer acknowledgement until the admitted operation settles.
Agent mutation **and observation** are blocked while pause waits and during human
ownership: native observation pumps JS and must not race a human command. An
identical admitted in-flight duplicate shares its eventual receipt. Resume or a
second human command while a human operation runs gets BUSY; it cannot transfer
ownership early. Agent cancellation notifications cannot cancel a host operation.
If the pending operation becomes uncertain, the pending handoff receives
HANDOFF_ABORTED_UNCERTAIN, never an ownership grant.

Resume requires a successful fresh native snapshot of **each currently open tab**
before other agent tools are admitted. Agent browser_tabs can inspect inventory
but does not substitute for these snapshots. If no tabs remain, a fresh
browser_tabs call releases that inventory barrier. Failed observations do not
release it. Old snapshots/actions, including completed response replays, cannot
cross an ownership/epoch boundary. Denied request IDs are still reserved; use new
IDs for retries. Closing a tab remains terminal for its ID.

An identical completed host request replays its original acknowledgement without
changing state. A changed payload with the same ID fails REQUEST_ID_CONFLICT.
Replayed acknowledgements are **historical receipts**, not current UI state; the
host must never roll its epoch backwards and should use ghost/status to render
current ownership using a fresh status request ID (replayed probes are historical
too). A new handoff cannot reuse an old handoff_id. A matching
cancel ends the child once; duplicate cancels queued after that closing decision
need not receive another response. No ledger survives crash/restart. Bind a new
child's fresh browserSessionId, reconcile effects and start empty; old-session
resume requests and refs fail even if task/tab counters match.

## No freeze and minimal UI proposal

Every hostState reports `scriptsFrozen:false` and `networkFrozen:false` in every
state. **Page scripts, timers and network work are not frozen.** Ownership controls
command admission only; background page/network effects may continue. Do not
show a frozen-page label or treat human_owned as a quiescent DOM. Blocking native
bindings/dialogs and engine work still need an independent supervisor process
watchdog; a missing receipt is an uncertain effect.

Source inspection found only `crates/oxibrowser-cdp/src/viewer.html`, served by the
regular CDP `/viewer` route and using account-login viewer tokens/WebSocket. Ghost
mode has neither that listener nor an account-backed viewer. Reusing it would
expand the transport and credential scope, so it was not changed or launched.

The small next UI is a host-owned ownership row: Agent / Waiting for current
operation / Human / Fresh observation required, current epoch, tabs left to
observe, and a persistent **"Page scripts and network continue"** indicator.
Take-control/Resume buttons must write the private supervisor channel only after
real user input. A human browser surface and that host UI are not implemented
here; the host can already perform the restricted local operations on its
channel. Do not route these buttons through page JS or agent tools.

## Verification and reproduction

```sh
cargo test --offline -j1 -p oxibrowser --features browser --test ghost_handoff -- --test-threads=1
```

The fixture launcher in `crates/oxibrowser/tests/ghost_handoff.rs` demonstrates
socketpair inheritance and both protocols. Race fixtures hold/release localhost
HTTP responses explicitly, instead of relying on scheduling sleeps. They test
human ownership, all-tab refresh, old ref and response invalidation, wrong
session/task/epoch/handoff, agent/page result forgery, duplicate/conflicting
resume across another handoff, duplicate pause/human navigation/cancel,
resume/tab-close races, agent cancellation of a host operation, continuous host
status traffic without starving work/deadlines, timeout during
pause, host EOF (idle and busy), crash/restart, and invalid/file/datagram fds.
No actual user browser, login, cookie, password, profile, OS UI or GPU is used.
On 2026-10-02, **14 handoff fixtures passed, 0 failed**; the prior 124 selected
regressions also passed on the same source (**138 total**). Format and diff checks
passed. Remaining engine limits and the suite breakdown are in
[ghost-mcp.md](ghost-mcp.md).
