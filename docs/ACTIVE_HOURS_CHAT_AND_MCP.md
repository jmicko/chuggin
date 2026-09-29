# Active hours, operator chat, and MCP

Design record, September 28, 2026. Implemented as the local **0.13.0 candidate**,
based on released 0.12.5 (`b99be154e2af6220f54e656375c4d6b5abba30f3`).
The README describes the shipped interfaces. This record also retains longer-term
design considerations: non-Unix transport, opt-in detached background execution,
finer client capability profiles, and generalized queued-operation cancellation
remain extensions. Commands already yield handles and support explicit cancellation;
pending editing acquisition can be cancelled with end_edit. MCP resources expose
status; paginated history/log access is through the shared tools.

No files or hidden checkouts are migrated for this change. Existing projects
remain Always allowed; scheduling is opt-in. Manual resume offers a one-cycle
override or running until the next scheduled stop, without modifying saved hours.

## Decision

Build one project controller used by the loop, the TUI, an interactive chat, and
an MCP adapter. The controller owns when work may proceed, which actor may edit,
commands, settings revisions, and recovery. Chat and MCP call the same tools;
neither independently rewrites files containing live runner state.

Keep the existing persistent loop conversation, visible project folder, retained
unfinished work, recovery autosaves, and normal task commits. Interactive chat has
its own persistent conversation and runs only in response to user messages.
Do not turn the loop back into the old multi-stage acceptance/rejection process.
No mandatory project types, helper hierarchy, or fresh context every cycle.

Everything stays Rust and installs as one executable. Extract shared behavior
before adding entry points; avoid three implementations of pause/edit/resume.

## 1. Active hours

Settings, accessible from Home and the observation view, contains:

```
Active hours             Use shared hours / Custom / Always allowed
Days                     Every day (select days if needed)
Hours                    01:00–06:00
Time zone                America/Chicago
At closing time          Finish current call / Finish current cycle
```

Default closing behavior: **Finish current call**. Its explanation says:
"Finish the current model response or tool action, then pause. Commands already
running can still finish." A cycle may take much longer; the alternative explains
that it continues the current cycle through checking and saving before pausing.

Shared hours are a global convenience, disabled initially. Projects explicitly
using shared hours follow subsequent shared changes; custom/always projects do
not. Show the effective hours and their source. New projects may use the shared
setting by default. Missing settings in existing projects mean Always allowed,
preserving existing behavior until the user opts in. Connection-wide GPU policy
is a future extension: a project schedule does not stop unrelated projects/apps.

Choosing Resume outside hours offers **Run until next scheduled stop**,
**Run one cycle now**, or **Wait for active hours**. Overrides leave the saved
schedule unchanged and never clear another operator’s editing hold. Starting Chuggin at Home or connecting MCP does
not arm autonomous work. The process must remain running; no OS wake, login
launch, catch-up for missed windows, or system scheduling integration in v1.

### Boundaries

- At closing time in call mode, stop admitting autonomous inference, new tools,
  and new command launches. Finish an in-flight response and save it, but park
  its returned tool calls before execution. Finish an already executing atomic
  tool action. Persist the next operation position and completed results.
- A returned command handle is not command completion. Observe existing processes
  independently while the loop is paused; do not freeze or kill them to meet a
  wall-clock deadline. Do not start the next command in a validation batch.
- Cycle mode admits remaining work for the current cycle, including its checks,
  review, and save, then parks before the next cycle. Clearly show an overrun.
  If the provider becomes unavailable or is already in backoff after hours, park
  the unfinished cycle instead of retrying all day to achieve that boundary.
  At the next opening resume it. This exception should be described in Settings.
- Watchdog, repetition recovery, and future helper inference obey autonomous
  scheduling too. Ordinary process/output monitoring requires no model and stays
  alive. A scheduled hold must not look like a stuck tool or consume repetition
  recovery attempts.
- Explicit human chat may run outside loop hours. Its commands and direct work
  belong to that interactive turn; it cannot quietly launch another autonomous
  loop under the chat label. Display the chat's model/provider so GPU use is clear.
- Opening removes only the schedule hold. A manual pause, stop, elapsed timer,
  goal completion, operator edit hold, or provider condition remains in force.
  Retry now bypasses provider backoff only, not the schedule or other holds.

Display truthful states, for example:

- `Waiting for active hours · resumes Monday at 01:00 · America/Chicago`
- `Active hours ended · finishing model response`
- `Finishing cycle 42 after active hours`
- `Loop paused · command 17 still running (12m)`
- `Paused by you · next active window starts at 01:00`

Waiting has no working animation. The dashboard shows the next transition and
any remaining process activity, without pretending the GPU is completely idle.

### Clock behavior

Use a pure, clock-injected schedule evaluator and a maintained Rust timezone
library; no hand-maintained UTC offsets. Windows are start-inclusive/end-exclusive.
Selected days identify the opening day of an overnight window. Reject equal start
and end times; Always allowed is the explicit all-day choice.

Use named zones, detected and shown during setup. For repeated DST times choose
the earlier opening and later closing; for missing times move the boundary to the
first valid instant afterward. Coalesce overlapping/adjacent resolved intervals
and skip any empty interval. Compute actual instants rather than just comparing
local clock minutes. Test spring/fall transitions and non-hour offset changes.

Reevaluate after sleep, wall-clock changes, configuration changes, and restart.
Wall time controls windows and provider reset deadlines; monotonic time controls
timeouts and active run duration. Time spent finishing an in-flight operation or
cycle counts until the hold is acknowledged. After acknowledgement the loop's
active-time budget freezes, even if an existing process continues; that process's
runtime is tracked separately. Daily reopening and reattaching to the same live
run do not reset its timer. Explicitly starting a new timed run does. A provider deadline passing
overnight causes one eligible retry, not a burst of missed retries.

Ollama can retain a model in VRAM after requests stop. Do not automatically unload
a model from a shared server. Initial scheduling stops new work; an explicit
connection-level idle-unload option can follow when shared resource ownership is
available. Existing commands may themselves use the GPU. This is a scheduling
feature, not a guarantee of zero host/GPU activity at the deadline.

## 2. One controller and composable holds

Replace the single pause boolean with distinct user intent and independent holds.
Examples: user wants the loop armed; user paused; schedule closed; operator owns
editing; provider unavailable; budget exhausted; stopping; completed. A hold has
an owner and only that owner/relevant operator action removes it. Derive status
from all reasons, distinguishing requested pause from acknowledged safe pause.

Gate both operation and cycle boundaries. Merely assigning a stop flag is wrong:
it exits the worker and would lose the parked continuation or daily auto-resume.
Neither schedule nor chat release calls an unconditional resume. Existing P,
Ctrl-C, R, timer, and T controls become controller operations with regression
tests. A stop issued while held must preserve work and must not unexpectedly
start new work to satisfy a stop boundary. No later opening undoes that stop.

The controller owns command sessions outside individual cycles. Process polling
and log collection continue while inference waits or an agent is paused. Record
actor, session, turn, command, and request identities instead of a global PID.
Admission gates apply before `Job::new` starts anything and before each advance
within a batch, not just when polling afterward.

Use a short serialized state-operation queue. Never hold its mutex during a model
request, a long command, filesystem scanning, or client wait. Workers return
events/results; controls remain responsive. Long controller operations return a
pending operation ID with status/wait/cancel, not a generic tool refusal or a
single call that blocks for hours. Cancelling a queued operation prevents later
execution; cancelling an active one has explicitly reported effect and limits.

## 3. Interactive chat

Add **Chat** at Home and as an observation tab, without renumbering existing tabs.
Home chat opens without starting the loop. The pane has a composer, streamed
reply, expandable tools/results, and a compact strip showing loop state, chat
model, and editing owner. Switching tabs does not cancel a turn. Distinguish
cancel chat from stop loop, especially while the composer has focus.

Keep chat and loop transcripts separate, with new/resume chat and cold-start
history. A chat starts with a small factual project summary: goal/nudge, current
task, loop state, workspace/branch, and references to checks/history. Fetch more
through tools. Do not stuff hundreds of loop turns into the prompt, or copy the
operator conversation wholesale into the loop. History tools support search,
pagination, timestamps, actor IDs, and artifact references.

Chat has normal agent turns: think, inspect, edit or run tools as needed, then
answer or ask and wait for the human. It has no automatic next-cycle prompt.
Reuse provider recovery, repetition handling, and command sessions while keeping
its transcript/task state distinct from the loop's. An answer is not proof the
whole project is complete. A chat failure retains partial edits and tool evidence.

The default chat model is the project's existing model. An optional project chat
model can be chosen from configured connections, without changing the loop model.
The provider/agent connection plan remains separately tracked; do not make full
multi-provider/helper implementation a hidden prerequisite for initial Ollama chat.
Build reusable route/purpose identifiers now. No implicit cloud fallback.

Inference capacity is separate from editing ownership. Queue requests on the same
Ollama resource by default, allowing the current response to finish. Interactive
chat gets the next available slot without waiting for a complete loop. Reserve a
slot for a request, not a whole conversation or a sleeping command. Future distinct
servers can run concurrently; all internal requests use the same capacity/usage
rules. Never label autonomous work as interactive to escape a policy.

## 4. Editing cooperation

Inspection does not pause the loop. Read responses carry observed content versions
and timestamps; a multi-file live view is not a transactionally consistent snapshot.
Offer pause/stable inspection if needed. No need to copy the entire project just
to answer a status question.

Before chat's first workspace mutation or arbitrary command:

1. Queue an editing turn and add its hold, blocking new loop operations.
2. Capture the in-flight loop response or finish the atomic tool; park remaining
   calls. Let commands that may affect the shared checkout settle. An arbitrary
   command is potentially a writer; command names and model promises cannot prove
   otherwise. Show which command is holding up takeover, with logs and stop control.
3. Once quiescent, preserve a recovery save and grant one fenced editing token.
   Verify read preconditions at mutation time; re-read changed files. Other operator
   clients can inspect or queue, but cannot steal the token.
4. Keep ownership across the whole editing turn, including its checks and repairs.
   Avoid pausing/resuming between every edit. Ordinary tools should not bounce with
   "another tool is running"; return useful pending/dependency state when necessary.
5. At completion, settle owned commands, autosave, record changed paths and actual
   check evidence, and release that turn's hold. The loop resumes automatically
   only if its other conditions still allow it.

This takeover is for workspace access, not all control updates. A nudge, retry,
timer, or normal settings change uses a short revisioned controller operation and
doesn't wait behind an unrelated command. A goal change can be recorded promptly,
then applied to the conversation and pending actions at a safe message boundary.
Distinguish recorded/queued direction from direction the loop has already seen.

The command registry allows both actors to inspect existing jobs. Changing stdin
or stopping a loop-owned command is an explicit operator action and is recorded.
Do not let a completed chat answer drop ownership while its command still writes.
Do not force a legitimate long command to stop merely because chat wants access.
Persistent development servers require their own lifecycle policy later; don't
quietly make them exempt from writer coordination.

If a turn is cancelled or a client disappears, preserve changes, revoke further
writes from its token, and monitor existing processes. A lease timeout is not
proof a process exited. If cancellation settles cleanly, release only its hold;
if outcome/ownership is uncertain, leave a clear recoverable hold and permit an
authorized takeover once safety checks pass. Waiting on a provider before any
write/command need not monopolize editing. After partial edits, a deliberate
checkpoint/release can hand unfinished work back with a note; never silently
interleave another writer between planned dependent changes.

### Resuming the loop after an intervention

The loop may have a recorded response with unexecuted tools when chat takes over.
After changes, do not execute those actions against old assumptions. Give pending
actions explicit not-executed results, retaining completed tool results, then add
one concise handoff after the tool-response block is valid. Prefer discarding the
remaining batch after a material workspace/goal intervention rather than guessing
which old action depends on which changed file. Ask the loop to re-read/replan;
do not wipe its context or begin a mandatory multi-cycle review.

The handoff contains changed files, goal/nudge revisions, commands/check evidence,
any explicit user direction, and unresolved issues. An observation-only chat needs
no handoff. Capture workspace generation as well as content hashes: a check run
while files changed cannot be called current validation just because the final
bytes happen to match. External editors remain outside Chuggin's cooperative lock.

Use normal recovery autosaves for unfinished chat changes and preserve human
staging. No automatic commit for a question or just because chat ended. Explicit
user-requested commits use the existing commit policy; supported finished work
can use the same evidence-bearing completion boundary without claiming the main
goal is done. Do not mark the loop's task complete from unrelated chat work.

## 5. Shared tool surface and permissions

Register typed tools once with schema, capability, side-effect category, and
handler. Generate both internal model tool definitions and MCP definitions from
that registry. Descriptions should be short; larger policy belongs in the
controller, not a repeated prompt essay.

| Capability | Operations |
| --- | --- |
| Understand progress | Project status, current goal/task/nudge, outcomes, checks, snapshots, commits, diff, command status |
| Retrieve history | Search/read transcripts, progress notes, paginated artifacts and command logs |
| Inspect/research | Existing project inventory, file read/search, optional symbols, configured web search and browsing |
| Do work | Edit/write files, run commands/checks, inspect/wait/input/stop owned commands |
| Guide work | Revise goal, add/update/cancel/reopen nudge, deliver a durable operator note |
| Control loop | Pause, resume, stop after cycle, manual retry, timer, hours, one-cycle override, goal-completion setting |
| Select configuration | Choose already configured allowed model/connection and normal project generation/time settings |
| Save/recover | Recovery save, permitted commit, preview/apply recovery with backups and staging preservation |
| Coordinate clients | Open/resume operator session, acquire/release editing turn, operation status/wait/cancel |

Use expected-revision checks for settings, goals, nudges, and recovery previews.
Stale callers receive the new revision and a conflict description, not last-write
wins. Keep tool responses concise with handles for details; don't return hundreds
of raw test lines by default.

Loop tools remain task/work tools plus existing nudge completion and optional goal
completion. They do not get set-goal, set-hours, stop-loop, or permission changes.
Authorization is checked by the executor using an assigned actor identity, even
if the model invents a privileged tool name. Watchdogs retain their read-only role.

Internal chat and MCP have the same maximum operator capabilities and the same
project policy. Routine reversible actions use the user's existing authorization,
without confirmations for every edit or nudge. Credentials, connection trust,
and spending-authority increases remain human Settings actions; models can choose
only approved routes. Destructive recovery or forced takeover from another actor
requires the same concrete preview/explicit authorization whether requested inside
or outside the TUI. Ordinary stopping of managed commands within an authorized
task, including the existing evidence-based watchdog behavior, remains available
without a new blanket approval gate.
The two adapters must not disagree about what is allowed.

A human being present is not itself access control. Conversely, tool separation
is not an OS sandbox: today's arbitrary shell runs as the user's account. Don't
claim it prevents intentional access to controller files, sockets, or secrets.
Exclude runtime/credential material from normal project tools and child environments;
strong isolation would need a separate sandbox design. This feature primarily
prevents accidental control calls and conflicting cooperative agents.

### Goal changes are real controller operations

Current code checks saved state.goal equals config.goal at startup and seeds the
conversation goal only at creation. Simply writing the config would break resume
and leave the model with stale instructions. Implement a journaled goal transaction:

- Record old/new goal, revision, request identity, and explicit user direction.
- Update authoritative state/config consistently and recover partial saves.
- Reopen completion tied to the old goal as appropriate; preserve its historical
  report and do not start an intentionally stopped loop as a side effect.
- Invalidate obsolete pending actions and deliver the new goal at the next valid
  message boundary. Preserve the transcript; historical goals are not rewritten.
- Ask the loop to reconsider its active task; do not erase useful unfinished work.

## 6. MCP and process lifetime

Use the official Rust `rmcp` SDK with local stdio transport. Choose a current
released version during implementation, lock it, and test older supported clients.
Wrap the existing blocking engine on worker threads rather than rewriting every
provider request asynchronously for MCP.

Target architecture: a lazily started per-project engine from the same executable.
TUI and `chuggin mcp --project /path` are clients over user-private local IPC. The
engine owns the state-directory and checkout locks. Identify the real checkout
canonically so symlink/config aliases cannot create two owners; copied projects
get independent runtime identity. Stale sockets alone do not prove owner death.

This is not a separately installed system daemon. First extract the controller
in-process and verify existing behavior; then separate its lifetime for MCP.
Do not ship two independent engines or require users to manage socket files.
Use Unix sockets on Unix and an appropriate local pipe abstraction where supported;
no network listener or remote HTTP in the first version.

Connecting/initializing the adapter can create an idle engine for inspection but
never starts or rearms the loop, including when old state says it used to run.
An already running engine keeps its existing intent. Starting autonomous work is
an explicit operator operation. Opening chat is likewise not Resume project.

Keep quit behavior clear. Normal Quit stops/saves owned work and exits; an optional
explicit **Keep running in background** action can leave an armed schedule alive.
Closing a connected viewer is distinct from stopping the engine. Show connected
operators and offer attach on subsequent `chuggin` launches. Do not silently turn
closing a terminal or an MCP client's exit into new unattended operation. Idle
unarmed engines without clients/jobs can exit. Authorized background engines keep
waiting for their schedule. Final lifetime transitions need integration tests.

MCP connections are not chat identities. Return explicit opaque operator session,
edit-turn, command, and operation handles; require them where state spans requests.
A client must finish/release its editing turn, with disconnect/lease recovery if
it fails. Do not infer completion from lack of an MCP call or assume one stdio
process corresponds to one human conversation.

Settings → External control provides project enablement, capability scope, connected
clients, and a copyable MCP launch configuration. Default disabled until enabled;
using the explicit MCP entry point for the selected project can perform the same
configured opt-in without a separate global install. No credential values in
tool schemas/results. Host/client labels are descriptive, not proof of identity.
Use OS permissions and per-controller client authorization for cooperative access;
do not advertise same-user IPC as a sandbox.

Tools are the compatibility baseline. Also offer read-only resources for status,
goal, history, and logs where client support makes them convenient. Tool results
carry bounded structured data, revision/time, and detail cursors. Long work returns
an operation handle. Don't require experimental protocol task support. Reserve
stdout for MCP framing; diagnostics go to stderr and the controller event store.
No MCP sampling of another model is needed: the external harness provides its model.

Important integration limit: an external harness's own file/shell tools do not
automatically obey Chuggin's editing ownership. Expose an explicit editing session:
acquire it before native edit/shell calls, retain it through any native background
writers, and settle/report those processes before release. Reconcile actual files,
HEAD, staging, and validation on release; invalidate old actions as with internal
chat. A lease expiry or a completed external turn cannot establish that external
commands stopped. The MCP cannot prevent uncooperative direct writes. Its own
edit/command tools are coordinated automatically. Document this in the connection
UI and test the native-tool handoff path, not just server-owned edits.

## 7. Persistence and recovery

Keep loop history and add separate chat sessions with stable message/turn/tool IDs.
Introduce an operation journal and revisioned controller snapshots. Retain existing
recoverable files/Git refs; no second migration of project files or new worktrees.

Journal side-effect identity before execution and completed results afterward.
Duplicate requests with the same id return the known operation/result. IDs include
actor/session and remain usable across reconnects; payload mismatch is an error.
Revision preconditions and operation IDs solve different problems; use both.

If a crash falls between a side effect and its completion record, mark outcome
uncertain and reconcile against file versions, logs, and process identity. Never
blindly repeat a command that may already have modified files or sent a network
request. Do not claim exactly-once arbitrary shell effects. Recovery must handle
pending tool batches without inventing successful tool responses.

Events carry sequence, actor, session, request, command, and project revision.
Use bounded broadcast for live rendering plus durable essential control/tool
events; dropped display deltas cannot drop operation results. Reconnection loads a
snapshot and events/logs after a cursor. Redact credentials consistently across
tools, artifacts, prompts, and MCP. Don't serve raw provider requests as the default
history surface.

## 8. Current implementation seams

| Existing code | Required work |
| --- | --- |
| `run_control.rs` | Composed intent/holds, acknowledged boundary, monotonic active clock |
| `runner.rs::run_controlled/work` | Controller-owned runtime state, parked continuation, revision/handoff delivery |
| `runner.rs` inline tool dispatch and `model.rs::tools` | Shared typed registry and role-checked executor |
| `model.rs::chat_format_once/wait_for_provider_inner` | Shared request admission, actor route, retry/schedule integration |
| `command_jobs.rs` / `command_session.rs` | Controller-owned jobs, gate before start/advance, independent monitoring |
| `project.rs` global active process | Per-command identity, ownership and cancellation |
| `events.rs` single subscriber/println fallback | Actor-scoped broadcast, persisted essential events, quiet MCP stdout |
| `setup.rs`, `menu.rs`, `ui.rs` | Active-hours editor, Chat, external-control setup, authoritative status rendering |
| `nudge.rs`, goal config/state | Common revisioned operations, operation deduplication, safe goal update |
| `workspace.rs` | Reuse recovery saves, input identity and staging-preserving commit/recovery |

The detailed multi-provider/helper plan in `PROVIDERS_AND_AGENTS.md` is still a
plan, not current functionality. Share its resource scheduling and actor concepts
without requiring paid-provider budgets or helper deployment in this feature set.
This plan supersedes the earlier suggestion that Chat merely switches the mode
of the one loop conversation: operator history is independent.

## 9. Implementation order and release boundaries

1. Extract controller, role-aware tools, owned command registry, event broadcast,
   and durable operation/continuation state. Keep current user workflow working.
2. Add active hours and composed pause/retry/timer behavior. This is a useful first
   testable slice before chat exists; it need not await the whole MCP feature.
3. Add persistent internal chat for inspection, then editing takeover, goal/nudge
   controls, saved history, and automatic handoff. Test before exposing external
   writers. Complete ordinary chat editing rather than shipping a permanent
   read-only chat substitute.
4. Separate controller lifetime, expose the same tools through stdio MCP, and add
   in-app connection setup, session handling, and external-tool handoff guidance.

Do not version-bump or release as part of planning. Test behavioral changes with
throwaway fixtures and bounded model probes; don't alter or stop the active Werd
project. Give the user a locally installed candidate to test before publication.

## 10. Required verification

- Injected-clock coverage: opening/closing, overnight days, DST, wake/clock jumps,
  live settings changes, invalid windows, no missed-window catch-up.
- Close during streaming, after tool-call response, in an atomic edit, and between
  validation commands; no new forbidden dispatch, preserved pending continuation.
- Finish-cycle overrun and provider-failure exception; zero inference during
  scheduled rest including watchdog/repetition/helper paths.
- Opening/retry/release cannot clear another hold, revive a stopped/completed
  loop, reset the timer, or replay an already-executed tool.
- Observation-only chat runs without unnecessary loop pause; its own request queue
  is responsive and doesn't mix streams/models with the loop.
- Chat takeover during model output and during long commands; one writer, useful
  pending state, finite commands settle without being killed by elapsed time.
- Two internal/external clients, stale leases/revisions, cancelled queue entries,
  client disappearance with live subprocesses, and explicit ownership transfer.
- Operator changes invalidate stale loop tool batches/completion intents, produce
  correct tool-result ordering, and deliver one concise handoff.
- Goal updates during work and crashes between saves survive restart consistently;
  no lost task/history, no unexpected loop start, no old-goal completion reuse.
- Crash injection around file edits, command spawn/result, operation journal,
  worker/socket lifetime, and reconnect. Unknown outcomes are reconciled.
- Existing staging, unrelated files, recovery refs, current branch and visible
  project semantics survive all controller transitions.
- Chat history cold start, input focus/cancel, narrow terminal, scroll/follow,
  collapsed tool output, truthful waiting/draining/paused status.
- MCP initialization never starts inference; all operator tools have parity with
  internal chat; protocol stdout is clean; older supported clients work.
- Loop can't dispatch operator tools by inventing their names; no secrets enter
  exported status/settings/tool traces. This tests capability routing, not an
  unimplemented shell sandbox.
- All current tests plus formatter/lint checks; targeted live Ollama interaction
  and MCP client smoke tests after mocks. No large benchmark needed just to verify
  schedule/ownership mechanics.

## References checked September 28

- [Official MCP Rust SDK](https://github.com/modelcontextprotocol/rust-sdk): native
  Rust implementation and stdio examples.
- [MCP request/session model](https://modelcontextprotocol.io/specification/2026-07-28/basic/index):
  explicit application handles rather than treating a transport as a conversation.
- [MCP tools](https://modelcontextprotocol.io/specification/2026-07-28/server/tools):
  schemas, results, annotations, and server-side access controls.
- [MCP stdio](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/stdio):
  local transport and protocol-only stdout.
- [Ollama FAQ](https://docs.ollama.com/faq): request concurrency and model residency;
  pausing work doesn't guarantee immediate VRAM release.
