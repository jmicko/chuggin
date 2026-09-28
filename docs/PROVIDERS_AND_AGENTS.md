# Connections, provider scheduling, and helper agents

Decision-ready implementation plan, September 23, 2026. This is a design, not
implemented functionality. Baseline: Chuggin 0.10.2 (`cace814`). Preserve the
persistent conversation, retained work, nudges, optional goal completion,
repetition recovery, provider backoff, and full-screen observation interface.
The implementation remains Rust and ships as one executable.

Workspace ownership now follows [Visible project workflow](VISIBLE_PROJECT_WORKFLOW.md):
the parent works in the user’s chosen checkout. Future editing helpers can still
use isolated snapshots and return changes for integration.

## 1. The user experience

One Settings workspace, accessible from Home → Settings and observation tab 5.
Connection setup, including Ollama, happens inside this workspace. No external
wizard, terminal prompts over the dashboard, or required settings flags.

```
Settings

THIS PROJECT
  Main model                 Workshop / ornith-1.5:35b
  Helper agents              Off
  Allowed helper models      Configure…
  Usage limits               Configure…
  Run controls               Timer, timeout, goal completion

ALL PROJECTS
  Connections                Manage…
  Default model              Workshop / ornith-1.5:35b
  Default project settings   Configure…
  Web tools                  Brave

↑↓ navigate   Enter open   Esc back
```

The two headings state scope, rather than requiring users to understand config
precedence. The main screen stays simple; detailed budgets, shared resource groups,
and generation settings live in subpanels. Home's model shortcut opens the same
connection/model picker. Existing scroll position and follow state survive Settings.
Before project setup, This project controls are unavailable while global connection
and default-model controls remain usable.

Connections → Add connection:

1. Select Ollama or Groq. Design a reusable chat-completions adapter now; expose
   arbitrary compatible endpoints after the two supported services are tested.
2. Name the connection. Ollama asks for its address; Groq supplies its address.
3. Groq offers a masked key field and Import from file. Importing the user's
   documented `url:` / `token:` format is handled in code, not by the model.
4. Test connection retrieves models without a generation request. A separate,
   bounded capability test checks streaming, tools, and structured output, with
   its small token allowance visible. It never enters unattended retry/backoff.
5. Save, then choose a global default if this is the first connection.

A default model is a **connection + model pair**. New-project setup copies that
pair and default generation settings into its project policy. Existing projects
stay pinned. "Use current global default" is an explicit project action, not a
continuously changing inheritance relationship. This prevents accidental cloud
usage and the model-picker ambiguity encountered earlier.

A project enables helpers and checks allowed model pairs. "Use main model" is the
simple local choice; adding another machine or Groq is optional. No role hierarchy
or required researcher/coder/reviewer configuration. Initially default to one
active helper; increasing concurrency is a project choice subject to resource caps.

All nested forms remain in the dashboard's event loop. Model-list requests and
connection tests run as cancellable background UI jobs while live events continue
to drain. Show "Current request" and "Next request" when a model change is pending.

## 2. Scope, defaults, and persistence

| Global | Per project | Runtime coordination |
|---|---|---|
| Named connection IDs, endpoints, auth references | Main connection/model | Account/model rate limits |
| Global default model and setup defaults | Allowed helper models and preference order | Inference concurrency per server/resource |
| Optional aggregate account spending ceiling | Project/provider/helper budgets | Usage reservations across Chuggin instances |
| Physical resource/account grouping | Work/output limits and run controls | Queue, cooldown, dispatch and settlement state |

The user's normal usage controls are project-specific. Resource coordination must
still be shared: two projects cannot each assume exclusive access to one GPU or
independently consume the same Groq quota. An optional global spending ceiling is
an additional emergency limit, not a required second budgeting workflow. All
requests count: main model, helpers, goal drafts, diagnostics, retries, and probes.
A helper allowance is part of the project allowance, never extra credit.

Global capability tests have their own bounded setup/probe allowance under the
account ceiling, not an accidental charge to the currently open project. Goal
drafting uses a durable draft-project identity and its allowance even before goal
acceptance. Show the chosen model and setup allowance before the first draft;
metered defaults cannot bypass project spending consent during creation.

Suggested records:

- `Connection { id, name, kind, base_url, credential_ref, resource_group,
  account_group, concurrency }`
- `ModelRef { connection_id, model_id }`
- `ProjectPolicy { main, generation, helpers, usage, run_controls }`
- `helpers { enabled, allowed_models, preferred_model, max_active, task_budget }`
- `usage { project_allowance, connection_allowances, helper_allowance }`

Connection IDs remain stable when renamed. Two URLs may share a GPU; two keys may
share a billed account. Detect obvious duplicates, explain reuse, and expose
explicit grouping under Advanced. Do not pretend account identity is reliably
inferable from an opaque API key. Physical resource groups and account/model rate
buckets are distinct: a local Ollama server may route some models to cloud services.
Represent effective resource, limiter and cost metadata at the model-route level
when they differ from connection defaults. An Ollama URL alone does not prove
local compute or free usage.

Storage:

- Global registry/defaults: existing XDG configuration directory, versioned JSON.
- Secrets: restricted per-connection credential files outside projects; owner-only
  permissions where supported, no credentials in subprocess environment or traces.
- Project policy: existing project `chuggin.json`, no embedded credentials.
- Shared coordination: XDG state directory, a short-held OS lock with an atomic
  snapshot and append-only audit records. No lock held during inference, waits, or
  tools. Use one transaction domain for both project and aggregate reservations.
- Helper state/transcripts/results: `.chuggin/agents/<id>/`.

Use unique temporary files, locking, and revision-aware setting updates. The
current fixed `.tmp` + whole-file rewrite is unsafe across simultaneous project
settings windows. Preserve unrelated concurrent edits; show a recoverable conflict
when two editors changed the same field. Credential records are bound to the
intended endpoint; changing hosts must not silently forward an existing key.
Disable/archive a referenced connection rather than silently remapping its users.
In-flight requests retain their snapshot; subsequent requests wait visibly for
configuration or explicit replacement. List affected known projects in Settings.

Usage identity survives ordinary resume/moves. Detect project copies and assign or
explicitly map accounting identity; benchmark clones must not accidentally share
one live job namespace or reset aggregate spending. Host-wide coordination covers
Chuggin instances on this host, not unrelated programs or other controller hosts.

Migration is lazy, idempotent, and atomic on controlled startup/settings entry:
resolve old global/project Ollama URL and model exactly as today, register/reuse
that connection, then pin the effective pair. Preserve legacy readable settings,
unknown project fields, conversation, checkpoints, nudges, waits and completion.
Pin inherited generation/work limits as well, so changing global setup defaults
does not alter any migrated project. Defaults seed projects; they are not live
inheritance. Retain migration backups for recovery.
Do not migrate live benchmark files just by installing a binary. Missing copied
connection IDs open a mapping picker; never guess a provider or paid fallback.

## 3. Provider and request layer

Extract the Ollama-specific networking from `model.rs`; retain agent behavior above
it. Provide Ollama and Groq adapters behind a small normalized interface for model
discovery, streamed generation, capabilities, usage and typed errors.

Persist stable message/tool-call IDs and JSON arguments internally. Adapters encode
Ollama object arguments or chat-completions string arguments, then normalize SSE /
NDJSON fragments, tool IDs/indexes, completion reasons and optional usage. Do not
execute incomplete tool calls after a broken stream. Replay the canonical committed
conversation, not partial assistant tool JSON. Preserve tool-response block order
when inserting helper results, command updates, nudges or recovery instructions.

Model capabilities matter: listed models may be audio-only, guard models, or lack
tools/structured output. Filter the picker for each use and report unsupported
features; do not silently substitute another model. Ollama `num_ctx` is an option;
a cloud context limit is a request capacity, not a server-setting knob. Structured
output support is explicit, needed by project goal drafting and diagnostics.

Tool definitions consume input tokens too. Give focused helpers a compact permitted
tool set, and count schema overhead when calculating usable context. A model's
128K context advertisement does not make a 20K request fit an 8K TPM account. The
small live probe used one tool; test the actual helper and parent schemas separately
before claiming production compatibility.

Use cancellable I/O (for example async reqwest with an explicit cancellation token)
for streaming. Today's blocking line reader only sees stop flags between received
lines, so a silent socket cannot promptly notice cancellation. A new provider
adapter must not reproduce that behavior or claim closing a local stream stops
remote generation or billing.

At dispatch, take one coherent snapshot of connection revision, model, effective
policy and generation settings. Existing requests finish under their original
settings. Subsequent attempts use the latest user-approved settings, keeping the
committed conversation; log the change so it is diagnosable. Revoking a route or
budget blocks new dispatches, not a fiction that already transmitted tokens are free.
Helpers retain their selected route when the main model changes. A helper route
changes only through explicit project policy/routing changes; removing permission
suspends its next request with partial work intact.

### Recovery and local inference-engine changes

A dropped Ollama connection during engine development is not evidence of a bad
task, repetitive model reasoning, or a failed edit. Keep separate states for:
connection failure, transient HTTP error, quota cooldown, authentication/config
repair, context/request-too-large, model-output repetition, and actual tool failure.

Transient failures back off indefinitely while the user wants the run active:
1/2/4/8/15 minutes with bounded jitter, then a 15-minute maximum when no server
reset is supplied. Honor a longer Retry-After/reset. Shared cooldown means many
helpers do not all probe the same unavailable service. Model switches and repaired
credentials wake the appropriate configuration wait; a manual Retry now control
allows recovery testing without bypassing budget/rate checks.

Do not retry an intrinsically oversized request unchanged forever. Return a clear
capacity condition, reduce context with preserved notes/evidence, or select an
explicitly allowed route. Do not apply generic 400/401 errors to the reasoning
repetition counter. A suspended provider leaves jobs and partial findings durable;
the parent can work elsewhere only within its configured permissions.

## 4. Scheduler and spending

Schedule **requests**, not whole helper lifetimes. Holding the local GPU slot while
a helper reads files, runs tests, waits for a sibling, or backs off would deadlock
or waste the server. Default one in-flight request per local resource. Separate
servers/cloud pools may progress simultaneously. Requests have purpose IDs;
watchdog calls need fair priority without starving main/helper work.

Before sending, under the coordination lock:

1. Check connection/model permission, cooldown, capacity, and project/account limits.
2. Reserve an allowance for the full encoded input (including tools/history) and
   maximum permitted output. Use provider-appropriate counts where available;
   otherwise explicitly conservative estimates. Never silently treat unknown price
   or missing token usage as zero.
3. Persist request identity, reservation, resource lease, owner and dispatch state.
4. Release the lock, then send. Update heartbeat while the request is alive.
5. On response, reconcile usage, release the physical slot, update rate buckets and
   retain a durable settled-attempt record. Requests-per-day and tokens-per-minute
   are different counters and reset schedules.

Allow local, user-declared free-account, and metered usage profiles. Adding a key
alone does not enable helper spending. Free-account mode uses explicit token /
request allowances; it does not prove the provider cannot bill that credential.
Metered use requires an explicit project allowance, supported price information,
and bounded maximum output. Offer token limits alongside estimated dollar limits.
Prices/tokens and provider-side billing can differ: do not advertise a guaranteed
invoice ceiling based on a client estimate. Provider account caps complement it.

For crashes or missing final usage, release stale physical leases only when owner
liveness is known; retain conservative **billing-uncertain** reservations. A retry
is a new potentially billable attempt. There is no general exactly-once remote
inference guarantee. Do not release the budget just because the connection closed.
Remote work may also continue briefly after local cancellation; expose uncertainty.

A budget crossing suspends further work on that allowance and preserves results;
it does not fail the entire project, enable a paid fallback, or permit the model to
raise a cap. Soft stop, force stop and request cancellation override queue waits.
Groq's documented 10–15 minute spend-tracking delay is why admission control must
not rely solely on its billing dashboard.

## 5. Commands: concurrency without pretending stale results are current

Borrow the distinction verified in Codex: an invocation can finish while the
process it started is still alive. A background process is not a global tool lock.
The stale-status bug was fixed in 0.10.2; this is a separate policy improvement.

- A process monitor updates status/output/exit independently of model requests.
- Registry ownership includes project, agent and command ID. Replace the single
  global `ACTIVE_CHECK` PID before allowing parallel processes.
- File reads, searches, notes, task updates and nudge bookkeeping need no blanket
  live-command restriction. Serialize actual file-tool mutations briefly.
- Allow the main agent to edit while its commands run, and run independent commands
  within configured process capacity. Report active commands and dependencies;
  do not classify arbitrary shell invocations as harmless by command name.
- Capture check input identity and mutation generation. If project content changes
  during validation, keep the output but label it **earlier/changed workspace**;
  it is not current verification. Cover shell/external edits as well as file tools,
  and handle changes that revert to identical bytes. Unknown provenance is
  unverified, not a pass. Isolated snapshot verification offers stronger evidence.
- True conflicts still need coordination: restore/reset/merge/checkpoint requires
  relevant writers to settle. Queue such operations with concrete dependency IDs
  and reasons rather than wasting repeated model turns on generic refusals.
- Completion records require resolved owned work and a saved checkpoint. A pending
  helper result is not automatically accepted or silently discarded. The parent
  resolves/integrates/cancels it before final project completion.
- Finite commands differ from explicitly declared persistent services. Initially
  retain existing finite-command semantics. Add service lifecycle support as a
  separate step; otherwise a dev server would make checkpoint draining wait forever.
- Do not copy Codex's process eviction cap: queue capacity-limited starts; never
  silently kill a legitimate long calculation to free an LRU slot.

Concurrent execution is not a sandbox. A workspace and a prompt do not prevent
arbitrary shell access. Isolated copies protect file integration, not the rest of
the host. Any stronger execution boundary must be explicit and actually enforced.

## 6. Helpers as durable tasks

Expose a small lifecycle: `spawn_agent`, `agent_status` / wait, `send_agent_message`,
`cancel_agent`. No compulsory delegation stage or recursive spawning initially.
Helpers use `report_result(summary, evidence, remaining_work)` to complete their
bounded assignment; they do not inherit the main project's endless scheduler.
The parent uses `apply_agent_changes` to integrate a returned change bundle.
The model chooses a bounded question or artifact and may request an approved model;
user policy owns providers, permissions, budgets and concurrency.

A fresh helper receives the relevant overall goal/nudge, assigned task, constraints,
workspace snapshot reference and selected evidence. Do not fork the full parent
transcript by default. Findings are advisory and must include uncertainty and
artifact references. This helps with current small-model loops and Groq capacity.

Persist spawn identity using parent session/turn/tool-call ID before starting work.
Repeated delivery of the same call returns the same job. Suggested states:
queued → running ↔ waiting-provider / waiting-budget / waiting-user →
completed / failed / cancelled, with a separate interrupted/resumable state.
Durable result delivery is idempotent and occurs between valid tool-response blocks.
Do not let a completed helper wake or restart a paused parent by itself.

First slice: helpers use enforceable read/search/web tools for investigation and
research, with no arbitrary shell and no parent edits. The full feature then adds
editable snapshots and command execution: include current uncommitted project
content, preserve helper changes and transcripts, and return a change bundle with
base hashes. Capture at a quiescent boundary or detect mutation and retry capture;
copying during a writer's activity can create an inconsistent snapshot. Record
included/omitted inputs: ignored datasets can matter in non-code projects, while
caches and secrets should not be copied automatically. Parent applies bundles
serially, using a three-way merge where possible;
conflicts return to the parent with both versions intact. No checkout/reset of the
parent to a stale helper base. The same mechanism handles documents/data projects,
not just Rust or Git-specific feature work.

Snapshot helpers own their own command sessions. The parent retains goal/nudge and
checkpoint ownership; a helper cannot finish the whole project, change settings,
create recursive helpers or discard another agent's work. Useful partial results
survive a helper limit; limits are not acceptance vetoes.

Agents view (tab 6 once enabled): task, model/connection, queued/running/waiting,
time, tokens/estimated cost, findings/change bundle, and cancel control. All events
carry agent/request IDs so streams and usage never mix. Live can show compact helper
updates; selecting a helper opens its own transcript and commands.

First Ctrl+C: stop spawning/new cycles, let the parent's current cycle finish and
checkpoint as today, and suspend queued/waiting helpers. In-flight helper work finishes its
current request/tool batch and is persisted; do not wait for an entire helper task
or a quota reset. Second Ctrl+C: cancel network work and terminate owned process
groups, preserving artifacts and uncertain billing. Never kill unrelated jobs or
reuse a PID without validating process identity. Resume keeps job ownership and
never replays tool side effects merely because a response was interrupted.

## 7. Repetition improvements informed by the Ornith run

The 363-request orientation loop included identical prose across separate replies
but varying file offsets. Keep the existing stream detector, and add bounded
cross-turn narrative/evidence tracking. Unchanged files or long research alone are
not failure. Correlate repeated prose, already-read ranges, unchanged findings and
absence of a selected task; exclude provider errors and command waiting.

First intervene with a concise evidence-based observation and request for a
specific unresolved question or next task. Repeated confirmed stalls may use one
budgeted fresh helper when enabled, with a cooldown. Preserve the useful parent
conversation and work; do not automatically clear it, forbid further research, or
spawn repeated diagnoses indefinitely. Show interventions and provider-wait time
separately in the UI and logs.

## 8. Implementation slices and acceptance gates

1. Typed settings, connection registry, secrets, migration and integrated UI.
   Preserve old Ollama behavior; pin defaults; live forms must not drop stream events.
2. Normalized requests plus Groq adapter and durable shared scheduler/usage ledger.
   No helper dispatch until preflight reservations and redaction tests pass.
3. Agent-scoped events, process registry, independent process monitoring and narrower
   command restrictions. Check provenance and cancellation tests before parallelism.
4. Durable helper lifecycle, focused read/research helpers, routing and Agents view.
   Then isolated editable workspaces and conflict-preserving integration.
5. Cross-turn repetition assistance, operational metrics and controlled benchmarks.
   This can be developed independently but must use the same scheduler for probes.

Required tests include:

- Every legacy URL/model override combination resolves identically after migration;
  global defaults cannot retarget old projects; concurrent settings saves do not
  lose updates; copied projects map connections/accounting explicitly.
- Groq SSE split at arbitrary byte boundaries, interleaved tool arguments and Unicode;
  tool results with correct IDs; structured output; missing usage; unsupported models;
  no side effects from incomplete/truncated responses.
- Two Chuggin processes cannot oversubscribe a budget or GPU slot; crash injection
  before/after reservation, send and settlement; stale lease recovery; uncertain
  billing; independent account/model rate buckets; no paid fallback.
- Provider disconnection/restart before headers, mid-text, mid-tool JSON, after final
  response, and while paused. Same committed tool side effect must not execute twice.
- Tests lasting longer than inference, editing during tests, changed-and-reverted
  inputs, shell writes, parallel commands, output after process exit, promptless
  waits, soft/force stop, and status truth without another model poll.
- Duplicate spawn delivery, interrupted creation, completed helper before wait,
  parent paused at delivery, same-GPU sequential operation, separate-server overlap,
  merge conflict, parent changing a base file, hidden/untracked/binary files, and
  helpers unable to modify project policy/credentials through their permitted tools.
- TUI nested forms, cancel/back, narrow windows, Unicode, secret paste/import,
  ongoing stream rendering, cold-start history, and clear current/next settings.

Use mocks for quota errors and failure injection rather than intentionally burning
an account's allowance. Then bounded live Groq and Ollama checks with fresh throwaway
projects; do not restart or mutate the paused Werd benchmark. Ollama may legitimately
fail while its inference engine is being changed. Compare tokens/time/work achieved
and recovery overhead, not only task or commit counts.

## 9. Verified references and probe

Public Codex source pinned to `e6da6fd7c7fd557a7b23ddea2916a075ca47bf82`
(September 23). This is reference code, not a claim about every deployed Codex app.

- [Invocation locks](https://github.com/openai/codex/blob/e6da6fd7c7fd557a7b23ddea2916a075ca47bf82/codex-rs/core/src/tools/parallel.rs#L191-L214)
  and [parallel exec](https://github.com/openai/codex/blob/e6da6fd7c7fd557a7b23ddea2916a075ca47bf82/codex-rs/core/src/tools/handlers/unified_exec/exec_command.rs#L142-L144).
- [Process storage/yield](https://github.com/openai/codex/blob/e6da6fd7c7fd557a7b23ddea2916a075ca47bf82/codex-rs/core/src/unified_exec/process_manager.rs#L598-L651)
  and [exit watcher](https://github.com/openai/codex/blob/e6da6fd7c7fd557a7b23ddea2916a075ca47bf82/codex-rs/core/src/unified_exec/async_watcher.rs#L163-L198).
- [Patch handler](https://github.com/openai/codex/blob/e6da6fd7c7fd557a7b23ddea2916a075ca47bf82/codex-rs/core/src/tools/handlers/apply_patch.rs#L342-L425)
  has no blanket running-process prohibition. Check provenance above is our proposed
  design, not a verified Codex feature.
- [Child configuration](https://github.com/openai/codex/blob/e6da6fd7c7fd557a7b23ddea2916a075ca47bf82/codex-rs/core/src/agent/child_config.rs#L102-L193),
  [spawn persistence](https://github.com/openai/codex/blob/e6da6fd7c7fd557a7b23ddea2916a075ca47bf82/codex-rs/core/src/agent/control/spawn.rs#L780-L832),
  and [completion delivery](https://github.com/openai/codex/blob/e6da6fd7c7fd557a7b23ddea2916a075ca47bf82/codex-rs/core/src/agent/control/completion.rs#L88-L114).
  Spawn does not automatically isolate a checkout; our snapshot policy is separate.
- [Codex token accounting](https://github.com/openai/codex/blob/e6da6fd7c7fd557a7b23ddea2916a075ca47bf82/codex-rs/core/src/rollout_budget.rs#L18-L66)
  is not a substitute for our pre-dispatch monetary reservations.
- [Groq compatibility](https://console.groq.com/docs/openai),
  [tool calling](https://console.groq.com/docs/tool-use/local-tool-calling),
  [rate limits](https://console.groq.com/docs/rate-limits),
  [spend limits](https://console.groq.com/docs/spend-limits), and
  [Ollama concurrency](https://docs.ollama.com/faq).

A bounded live Groq probe on September 23 used the user-authorized credential file
programmatically. No token entered model context, logs, source or shell arguments.
One model-list request and three small requests to `qwen/qwen3.8-27b` succeeded:
streamed function call, tool-result continuation, strict JSON-schema goal draft.
Reported usage: 453 total tokens (411 input, 42 output). Response headers for this
account/model indicated 8,000 TPM and 1,000 RPD. The three responses took roughly
0.45 / 0.24 / 0.26 seconds end-to-end. This proves basic protocol compatibility,
not coding quality, billing tier, sustained speed, or tested exhaustion recovery.
No deliberate 429 or long-running load test was performed. The account limits
must be read from current headers/settings rather than hard-coded from this probe.
