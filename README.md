# Chuggin

A Rust terminal tool for advancing software, document, and data projects with local models.
It keeps working, checking results, and refining the same project.

**Save the work. Test it. Keep improving it.**

## Install

Chuggin is currently tested on Linux. Install a recent stable Rust toolchain and
Git, and have an Ollama server with a tool-capable chat model available.

Chuggin uses your configured Git `user.name` and `user.email` for commits,
including repository overrides of global settings. If either is missing,
interactive setup or resume asks for it and saves it in the project's Git
configuration before starting work. Noninteractive runs stop with setup
instructions. Use a GitHub verified email or your GitHub noreply email for
GitHub attribution; GitHub login is not required for local commits.

~~~sh
cargo install chuggin --locked
~~~

Or install from a checkout of this repository:

~~~sh
cargo install --path . --locked
~~~

This installs one Rust executable into Cargo's bin directory (normally
~/.cargo/bin). Make sure that directory is on PATH. No Python runtime is used.

## Start

~~~sh
cd your-project
chuggin
~~~

The arrow-key menu highlights **Set up this project** or **Resume project**.
Model selection, Settings, the accepted goal, Progress, and Quit are also in
the menu. No flags are needed for everyday use.

## Full-screen observation

The interactive interface uses the terminal's alternate screen, with a large
splash screen, in-place settings and goal drafting, and a live run dashboard.
The dashboard shows the current task and stage, model output as it arrives,
tool actions, check output, recovery messages, and recent outcomes.

- **1–6 / Tab:** switch between live activity, model output, checks, the goal, local settings, and chat.
- **Arrow keys / mouse wheel / Page Up / Page Down:** scroll without pausing work.
  Scrolling back to the bottom automatically resumes following live output.
- **Home:** oldest retained output. **End / F:** follow live output again.
- **/** searches the current output view; **Esc** clears the filter.
- **?** opens the keyboard guide.
- **P:** pause inside the current cycle, then press P again to continue where it left off.
  An active model response or tool operation finishes before the pause takes effect.
- **T:** retry a waiting provider now. While paused, queues the retry for resume.
- **Ctrl+C / Q:** finish the current cycle. **Ctrl+C again:** force stop.
- **R:** cancel a pending stop or resume directly after the run finishes.
  When the run finishes, Enter returns to the splash screen.

The side panel shows request counts, recent checkpoints and findings, context usage,
and generation speed. Token counts and speed come from Ollama after each completed
response, not estimates during streaming. CPU, RAM, and process memory describe
the local Linux computer; they do not measure the remote server's GPU.

The display redraws at up to ten frames per second and keeps 2,000 output lines.
Its bounded event stream cannot block the agent; full diagnostic logs remain on
disk. Narrow windows hide the side panel, and resizing does not stop the run.
Shell settings and the previous terminal screen are restored on exit.
Terminal window titles start with the project folder, such as
`werd-ornith-35b · Chuggin`, to distinguish runs in the taskbar. Compatible terminals
restore the previous title when Chuggin exits.

Updating the executable does not change an already-running process. The new UI
appears the next time you launch Chuggin. Explicit `chuggin run` commands retain plain
output for scripts and redirected logs.

New installations default to **http://localhost:11434**, with no model preselected.
Setup connects to your server and asks you to choose from its installed models.
For a remote Ollama server, enter its URL during setup or in Settings.
Existing saved settings take precedence over these defaults. Brave is optional
and disabled until configured.

Setup expands your elevator pitch into a project goal. Accept it or request
changes; drafts persist across revisions and restarts. Choose a validation
command, then Resume. Empty projects get an initial Git commit. For existing
files without commits, setup offers to commit project files while respecting
Git ignores and excluding Chuggin's state.

First Ctrl-C requests a stop after the current cycle. Press R to cancel that
request, or press R on the finished screen to continue from saved progress.
Second Ctrl-C stops immediately. Subsequent runs resume the persistent working project.
During setup, a single Ctrl-C exits.

For shorter interruptions, press **P** in the observation view. **PAUSING** means
the current model response or tool operation is finishing; **PAUSED IN CYCLE**
means no further model requests or tools will start until you press P again.
This preserves the current cycle, conversation, and pending tool calls without
starting a new session. Already-started commands may still finish; pausing does
not suspend operating-system processes. The run timer stops while the cycle is
held. Pressing P again while a pause is pending cancels it. Ctrl+C or Q releases
the held continuation and saves without launching new work. Other holds, such as
active hours or operator editing, remain independent of the manual pause.

Choose **Run duration** from the home menu to set a project-specific duration
in hours (fractional hours work; 0 means unlimited). The observation view shows
the time remaining. When the limit is reached, Chuggin finishes the current cycle,
saves its outcome, and stops before starting another. This is a soft limit, so
the run can exceed the chosen duration by the remaining cycle time. Resuming
starts a new timer. The setting persists as `run_duration_seconds` in `chuggin.json`
and also applies to command-line runs.

## Live project settings

Tab **5 Settings** in the observation view edits the exact model name, request
timeout in minutes (0 means unlimited), and run duration in hours (0 means
unlimited). Use arrows to select, Enter to edit/save, Ctrl+U to clear, and Esc
to cancel. Changes persist only in this project’s `chuggin.json`.

Model and request-timeout changes are snapshotted at the next model call, including
a retry; the active call finishes with its original settings. The panel shows the
current/last request model separately from the next selected model. Timer edits
apply immediately against active elapsed time since this run started, excluding
time held by P. Shortening the duration below elapsed time requests a stop after
the current cycle; extending it allows additional time. Resuming a stopped run
starts a new timer.

**Investigation helpers** in tab 5 opens the same project settings as
**Settings → This project · Investigation helpers**. Enable helpers, choose their
model, and set their response allowance there. These permissions remain in human
Settings; Chat and MCP cannot turn on helpers or authorize another provider.

## Investigation helpers

Helpers are optional and disabled by default. The main model can ask one to
investigate a focused question with fresh context, then use its findings in the
existing working conversation. Helpers can read project files, search, inspect
saved evidence and logs, and use enabled web tools. They cannot edit files, run
commands, launch other helpers, or decide whether work is complete. Their reports
are advisory, and main work remains saved even when an investigation is inconclusive.

Choose **Use project model** to use the project's connection, or explicitly
select an Ollama or Groq model. Ollama uses the project's server; Groq uses the
configured shared key and request/token budgets. Investigations run sequentially
with main work. Separate-provider parallel jobs are not enabled by this feature.
Helper traffic passes through the same provider recovery and quota handling as
other requests. It does not bypass Groq waits or silently switch providers.

The default allowance is 12 model responses per investigation, adjustable in
Settings. Reaching it preserves available findings and returns control to the
main model. Pause and stop apply at safe operation boundaries. The live view
shows investigation activity; inputs, tool evidence, and results persist under
`.chuggin/agents/`. Saved results can be retrieved without repeating an investigation.

Model and allowance changes apply to the next investigation; an existing job
keeps its chosen route and allowance. Shared helper settings are defaults for new
projects. Existing projects stay disabled until you enable them explicitly.

## Active hours

Open **Settings → This project · Active hours** (also available in tab 5).
Choose Always allowed, custom hours, or the shared default under Shared settings.
Set opening and closing times, weekdays, and a named timezone. Overnight windows
belong to the day they open; daylight-saving changes follow that timezone.
Existing projects remain Always allowed until you opt in.

At closing time choose either:

- **Finish current call:** finish the model response or tool action, then pause
  before the next operation. Tools returned by that response wait for reopening.
- **Finish current cycle:** continue through checks and saving, then pause before
  another cycle. This can run well past the closing time. Provider backoff outside
  hours parks the unfinished cycle instead of retrying throughout the day.

These hours control **only the autonomous loop**. Chat and inspection remain
available. Already-started processes may finish, and Ollama may retain its model
in memory; the schedule does not unload a shared server's models.

You can resume outside hours: choose **Run until the next scheduled stop**,
**Run one cycle now**, or **Wait for active hours**. This leaves your saved schedule
unchanged. A scheduled reopening never cancels a manual pause or another session's
editing hold. Time spent paused does not consume the run-duration timer.

Keep Chuggin open for scheduled work; it does not install a system scheduler or
wake a sleeping computer. Closing the last viewer unexpectedly requests a pause
after a one-minute reconnect grace period and the current operation. Reconnecting
does not silently clear that manual pause.

## Project chat

Open **Chat with this project** from Home, or tab **6 Chat** during a run.
Opening Chat does not start the loop. Ask about progress, request an edit, change
the overall goal, or set a temporary nudge. The chat model can inspect files,
search saved history, use configured web tools, run commands and checks, save or
restore checkpoints, and control the loop. These operator controls are not given
to the autonomous model.

**Enter** sends; **Esc / Ctrl+C** cancel the chat reply; **Ctrl+N** starts a new
chat; **F2** expands tool details. Tab returns to the observation views. The latest
chat loads after restarting Chuggin. Chat and loop histories are separate and
persist under the project's state directory. Older chat can be retrieved through
the history tools even when it no longer fits in the model's working context.

Chat uses the project model unless you set **This project · Chat model** in
Settings. Requests sharing an Ollama endpoint are serialized; an interactive
request gets the next free slot within its controller. The current response is
allowed to finish. Connection-wide fairness across independent controllers is
best effort; this is not a server-wide scheduler.

Inspection leaves the loop running. Before an edit or command, Chat waits for a
safe boundary and takes exclusive editing ownership. It keeps that ownership
through its checks and repairs. The loop receives a concise change notice and
reconsiders any stale, unexecuted tools before continuing. Existing commands are
observable and can be explicitly stopped; a long command is not killed just to
let Chat edit. Human Git staging and unfinished files are preserved.

## External AI access (MCP)

**Settings → This project · External AI access (MCP)** shows a project-specific
configuration to copy into another AI application's MCP settings. The server is
part of the same Rust binary, using the official Rust MCP SDK and local stdio:

~~~json
{
  "mcpServers": {
    "chuggin": {
      "command": "chuggin",
      "args": ["mcp", "--project", "/absolute/path/to/project"]
    }
  }
}
~~~

The project argument may instead name its configuration file. Connecting is an
explicit opt-in for that project and never starts the loop. TUI, scripted runs,
and MCP share one local controller and checkout lock. Local IPC currently requires
Unix; Linux is the tested platform. No separate daemon installation is needed.

The external model calls `open_operator_session`, then uses that session ID on
the same operator tools as Chat. Mutating calls include a unique `operation_id`;
retries reuse that ID and the same arguments. `operation_status` retrieves a
recorded outcome. Long commands return handles for inspection, input, and stop.
Status is also exposed as a read-only MCP resource. Credentials and connection
trust remain in human Settings rather than operator tool results.

If an external harness uses its **own** edit or shell tools, it must acquire
`begin_edit`, wait until granted, and retain ownership until all its writers
finish; then call `end_edit`. Chuggin cannot coordinate tools that bypass this
agreement. This is cooperative coordination, not an operating-system sandbox.

Interrupted actions are recorded as uncertain and are never automatically
replayed. Interrupted editing retains a recovery hold. Review files and logs,
confirm any external writers have stopped, then use the recovery tools to release
that hold. Restoring a snapshot requires a current preview and retains a backup;
restoration refuses to overwrite human staging.

## One conversation, continuous refinement

Chuggin works directly in **your project folder, on its current branch**. Open
that folder in your editor or run the project's normal commands to see the actual
work. New runs do not create a hidden developing checkout.

Recovery saves and normal commits serve different purposes. Each changed cycle
creates a recovery snapshot without moving your branch or changing your Git
staging. Completed tasks whose configured checks pass can create normal commits
with descriptive task summaries. Failing checks, model errors, and cycle boundaries
do not discard edits. Unfinished work stays visible and recoverable.

The agent uses **one conversation across tasks, cycles, and restarts**. Its system
instructions, main goal, and tool definitions stay stable. New task directions,
tool results, and check feedback are appended to the same history. This keeps
useful reasoning available and gives Ollama the opportunity to reuse a cached
prompt prefix instead of processing a different conversation for each activity.
Actual cache reuse depends on the server, model, and available context.
Tool availability follows project capabilities and enabled settings. Enabling or
disabling helpers updates their availability on the next main-model request.

Within that conversation the agent can:

- Inspect the current files and choose a useful next result with `set_task`.
- Plan, edit, run checks, and repair in any order that the evidence warrants.
- Examine its changes for missed problems using the same tools and history.
- Call `finish_task` with a factual summary when it believes the task is done.

`set_task` records the task's objective, desired outcomes, and likely relevant
files. These are planning hints, not file allowlists or rigid acceptance rules.
`finish_task` expresses completion intent. Chuggin marks the task complete only
when the configured checks pass; failures are appended to the conversation and
the task remains unfinished for repair. **Every result still preserves the files.**
Tasks have persistent IDs and completed-task records. Completion is tied to the
active task; repeating an old completion does not end another work interval.
The agent receives guidance to select useful next work. Research and other tasks
can complete without file edits; backend, document, and non-code work are valid.
There is no mandatory separate planning agent or reviewer, and no review veto.
An ordinary prose response can end a work interval without marking the task done.

At each cycle boundary Chuggin runs configured checks, even if the agent already
ran them, creates a recovery save when there are changes, and records the results.
The last passing revision advances only when all configured checks pass and the
project files remain unchanged during those checks. Commands that modify files
leave a saved but unverified checkpoint for another validation pass. The next
cycle continues from those files and the same conversation.

Chuggin does not rebuild an attempt from an older passing snapshot. The agent
can make targeted corrections. Whole-project recovery is a user action under
**Progress → Browse recovery saves / restore files**. Preview the changes before
restoring; Chuggin saves the current files first and retains Git history. Restored
files need validation again.

**Progress → Commit current changes** lets you explicitly commit unfinished work,
including staged and unstaged project changes. Automatic task commits defer when
you have staged project changes, preserving your staging while useful work and
recovery saves continue. Normal commits honor your Git hooks and signing settings;
a commit failure is reported without discarding the work. Pause with P before
making manual edits or running a build that needs stable inputs. Running local
commands can still finish. On resume, changes outside recorded model actions are
reported and affected files must be re-read before file-tool edits.

A branch switch or unfinished Git merge/rebase stops further agent mutations.
Return to the original branch, or explicitly select **Progress → Use current
branch** after finishing the Git operation. A checkout lock prevents two Chuggin
instances using different configurations from writing into the same checkout.

Conversation recovery happens for reported context pressure, repeated request
failures, or sustained repeated tool actions, not simply because a task or cycle ended or a text-size estimate was
exceeded. Before a handoff, Chuggin archives the full conversation. It preserves
the stable instructions and goal, then supplies the current task, checkpoint,
progress notes, and latest feedback. A token-pressure handoff also retains recent
messages. This is a deterministic handoff, without an extra summary-model request,
and never resets project files. The interface distinguishes checkpoints,
unresolved failures, and the last revision whose configured checks passed.
A passing check is evidence about that check, not proof of project completeness.

The agent can use `save_progress_note` for a factual handoff: what changed,
what it checked, outstanding problems, and a useful next action. Recent actions
and failures are recorded too. These notes work with any language or artifact
format and remain advisory; current files and observed validation take precedence.
Notes are stored in full, with replacement or append support. Handoffs include a
6KB excerpt; `read_progress_note` retrieves the full note through byte pagination.
Task evidence distinguishes current validation from incidental tool errors and
older task notes. It records the file state checked, so a past passing result
does not describe files changed afterward. The working model can search and read
saved conversation history after a handoff instead of repeating earlier research.

## Tools and recovery

The agent has the same inspection, editing, execution, and task tools throughout
the working conversation, plus a bounded inventory of project files. Rust
declarations and module reachability supplement the inventory when present. Other
formats use file reading and text search. Instructions describe project outcomes
and evidence rather than assuming a language or test framework.
Setup asks for a validation command appropriate to the project; it only suggests
`cargo test` when a Cargo manifest exists. Documents and data can use linters,
consistency checkers, or custom scripts. A validation command is still required.

The model can list files, search literal text within selected paths, read numbered line ranges, replace
a file, make an exact targeted edit, and run configured checks. Long reads include
continuation positions. Unique-match edits prevent accidental broad replacement.
File size alone does not block reads or edits. Very long lines can be retrieved
exactly using `read_file` byte pagination. Edits and writes replace files atomically,
preserve existing permissions, and report `changed: false` if the bytes are identical.
Structured tool results expose fields directly instead of nesting escaped JSON.
Searches include surrounding lines, pagination, and an explicit completeness
indicator. Command status can wait for new output or completion instead of rapidly
polling the same tail. Plans can be updated while commands run; edits and new
execution still wait until commands using the project have finished.
Edits can address any relevant project file, including supporting work omitted from
the original plan. Chuggin configuration, runtime state, and Git control files remain
protected.
Investigation and repairs use the same native tools and conversation. Reading
several files does not trigger a separate patch-generation stage. If the agent
tries to finish while validation is failing, it receives the failure evidence
and continues in place; claims never substitute for changes on disk.

Validation is project-defined. There is no mandatory Rust regression-writing
stage, test-name gate, or language-specific acceptance rule. The agent can add and
run suitable tests and investigate missing checks using the normal tools.

The working conversation uses native tool calls and temperature 0.4. Task metadata
is recorded through tools; ordinary work does not require a staged JSON report.
Setup goal drafting still uses structured output and formatting repair.
Repeated JSON fields and fenced code are allowed. Sustained repeated words,
phrases, or sentence blocks in prose interrupt the stream. Chuggin retains the
completed conversation and retries up to twice, removing the repetitive tail.
Only ordinary prose before the repetition may be reused; interrupted tool calls,
thinking, and partial JSON are never replayed as completed actions. Structured
responses are regenerated whole. Generation-limit interruptions also request a
shorter complete response. Each failure and continuation request is logged.
Goal-drafting failures preserve the pitch and offer Retry, Settings, or Back.

A separate action detector catches repetition across responses and restarts:
identical no-op edits, reads, searches, and invalid completions. Four repetitions
of the same action or a short repeating sequence trigger feedback. If that same
pattern persists, Chuggin archives the conversation and resumes with the goal,
current task, completed-task information, notes, and check feedback, excluding
the repetitive history. It neither pauses the run nor rolls back files.
Distinct investigative reads and changed results are allowed. Commands reset this
exact-action detector because identical output does not establish identical side effects.
Recovery events appear in live output and `action-recovery-N.json` cycle artifacts;
the detector's observations and cumulative intervention count persist in the conversation.

Version 0.7.1 adds a separate **command repetition assessment**. It tracks model-issued
commands, configured-check requests, and compiler diagnostics against the observed
project files, task ID, and outcome status. Eight repeated executions with unchanged
observed files produce a brief question about what another execution should establish.
After sixteen, a fresh conversation with the selected model assesses the evidence.
These are assessment thresholds, not command limits: commands are not blocked or
silently replaced with cached results. Checks run by Chuggin at cycle boundaries
do not count toward this detector.

The diagnostic runs sequentially and can read files, inspect the inventory, search,
and retrieve command logs or progress notes. It cannot execute commands, edit files,
complete tasks, or reject checkpoints. It sees the goal, active task, recent commands
and inspections, project snapshot, and check feedback instead of the entire repetitive
conversation. It reports **productive**, **stalled**, or **uncertain**, with a reason,
next action, and expected new evidence. Its opinion is advisory. For a reported stall,
Chuggin archives the repetitive conversation and resumes from saved work with that
advice. Productive or uncertain assessments keep the main conversation intact.
An inconclusive or failed assessment also returns to normal work; it does not pause
the project. Provider waits retain the existing backoff and stop/timer behavior.

Each assessment has up to five inspection rounds followed by a final reporting round,
up to eight read-only tool requests per reply, and at most 2,048 output tokens per
response. Existing transport/provider retries still apply. Continued repetition is reassessed
after another sixteen executions, or sixty-four when judged productive. Changed
project files, task identity, command arguments, or outcome status reset the streak.
Optional `run_command.reason` explains deliberate trials or polling. Identical files
and exit codes alone cannot establish unchanged external state or ignored artifacts;
the diagnostic must consider that uncertainty. It can still make mistakes.

Successful repeated command output gets a shorter, explicitly labeled excerpt with
the full log ID. Full output remains on disk and retrievable. **Diagnose** appears
in the observation view, and `command-repetition-*` and `command-diagnostic-*` cycle
artifacts record the evidence, read-only actions, report, or failure. Recovery state
survives cycles and restarts. Healthy runs do not pay for a diagnostic on every cycle.

## Reading the observation view

**Live** combines model text, command summaries, tool notices and checkpoints.
**Model** shows only streamed model text; **Checks** shows commands and validation.
On a cold start, Live and Model restore recent model text from the saved
conversation; Live also restores tool names and file/task hints. A New session
divider separates this history from new activity. This display uses the last 200
saved messages within the normal bounded scrollback; older conversation archives
and full command logs remain on disk. It does not replay actions, reconstruct old
command-output panels, or add anything to model context or session counters.
Command output is collapsed by default. Recognized Rust, pytest, Jest/Vitest and TAP
summaries show passed/total counts, failures and skipped tests. Multiple Rust suites
are aggregated without counting individual test lines twice. Other commands show
exit status and output-line counts instead of guessed test totals. A few error or
slow-test messages remain visible even when collapsed.

Press **E** in Live or Checks to expand/collapse command output. The expanded view
retains a bounded recent excerpt and shows the full log path; full diagnostic logs
are unchanged on disk. Searching also searches retained command details. Summaries
are computed locally, without extra model requests, and never alter what the agent
sees or how verification is evaluated. Repeated identical stage headings are omitted.

When output is quiet, a small moving ASCII indicator appears at the bottom of the output
box. Its arrow travels behind each bracket and reverses direction. It labels waiting for a response, a running command, or provider waiting;
it indicates an active harness, not a completion percentage or proof of progress.
It stops when the run is paused.

## Execution, compiler diagnostics, and symbols

The working agent can use **run_command** with an executable and argument array,
for example `["cargo", "test", "unicode"]` or `["cargo", "fmt"]`. Commands run
in the persistent workspace, with no implicit shell.
If the model supplies a valid JSON-encoded argument array, Chuggin decodes it and
reports `arguments_normalized: true`. Arbitrary command strings are not interpreted.
Starting with **0.8.0**, commands return within about one second. If unfinished,
they return `running: true` and a `command_id`. **command_status** inspects or waits
briefly for that same process; **command_input** sends stdin text (including an
explicit newline when needed) or closes stdin; **stop_command** terminates the
process group with a recorded reason. Input reports how many bytes were accepted,
so callers can retry only the remainder if a pipe is full. These are pipe-backed
sessions, not full PTYs. Completed responses include the exit status, bounded
output tail, and full log ID. The agent can inspect files while a command runs;
new commands, project edits, restoration and task completion wait until it finishes.
This prevents duplicate builds and checks racing with edits.

A fresh, read-only **watchdog** inspects long-running processes. The default first
review is after 120 seconds, configurable in observation Settings as **Command
first review (seconds)**. It applies to newly started commands. For compatibility,
existing check/tool `timeout_seconds` values now request an earlier first review
(the smaller of that value and `command_review_seconds`), not an automatic kill.
The watchdog sees purpose, current task, recent output, elapsed time, time since
output, and Linux process-tree counters where available. It can inspect source
and full logs, but cannot edit or run additional commands. Quiet output, high CPU,
and runtime alone are not grounds for termination.

A productive or uncertain verdict lets the existing process continue, with another
review in five minutes. A stalled verdict terminates it and returns the reason and
suggested repair to the working agent. If the watchdog is unavailable or cannot
produce a valid report, the process keeps running and review is retried later.
Watchdog requests have a two-minute per-request deadline and no provider retry
loop; at most six requests are made per review. A command that finishes during
inspection keeps its actual exit result. Reviews run sequentially between working
model calls or while waiting for a command; they do not interrupt an in-flight
model request or require parallel Ollama inference.

Both model-requested commands and final checkpoint checks use this mechanism.
Unfinished commands are awaited before checkpoint verification; running checks
never count as passing. Sessions belong to the current cycle and are not reattached
on restart. Errors or normal shutdown clean up owned processes; force stop kills
the active process group. Files and command logs are retained. Session metadata
is saved beside logs as `*.session.json`; `watchdog-*/decision.json` and diagnostic
artifacts record each assessment. The observation sidebar shows command elapsed
time and time until its next review.
**read_command_log** retrieves the full log in chunks, including logs from earlier
cycles. Use the complete returned ID, such as `cycle-000003/command-2-0.log`, so
continued conversations retrieve the original evidence even after command numbers
repeat. Legacy IDs without a cycle prefix refer only to the current cycle.
Commands and checks share
the live output view and process-group cleanup on completion or explicit termination.
Long-running applications can be inspected during a cycle, but are not detached services.

**compiler_diagnostics** is a Rust helper that requires a Cargo manifest. It runs
Cargo check for all targets and returns grouped errors/warnings, source locations,
nearby code, and compiler suggestions. It does
not execute tests. Command success does not substitute for the configured final
checks. Command-produced edits remain part of the persistent project and are
saved at the next checkpoint.
Like configured checks, commands execute with the user's OS permissions. The
visible-folder workflow is cooperative and does not isolate arbitrary external
processes. The model is instructed to keep commands within its task and leave Git
commits/state to Chuggin.

**lookup_symbol** inspects Rust files. It finds types,
functions, private/public methods, and name-based reference candidates, with
paths, line numbers, source snippets, and pagination. It parses Rust syntax, so
comments and string contents do not appear as references. This is not a language
server: it does not resolve types, expand macros, or filter inactive cfg branches.
The loop advertises these Rust tools when the project has a Cargo manifest;
other projects use the general file, search, and command tools. They are not
required validation steps for other projects.

## Optional Brave search and page reading

Open **Settings → Brave API key → Enter or replace key**. Entry is masked.
Saving a key enables the web tools; the toggle can disable them without deleting
it. **Test Brave connection** performs a real search and reports errors.

The key is stored separately in ~/.config/chuggin/brave.key (or the corresponding
XDG directory) with owner-only permissions. It is not included in project
configuration, prompts, or request logs. It is sent only to Brave's search API.

The agent can use **web_search** for five sourced snippets,
and **read_web_page** for numbered, paginated public HTTPS text and links.
Responses are bounded and cached within a cycle. Page reading does not execute
JavaScript; private/local addresses and binary downloads are rejected. External
content is labelled untrusted and cannot authorize tools or change the goal.
Search queries and returned evidence are logged, so prompts direct agents to use
public API names/errors and never private code or secrets. Brave billing and
quotas apply to searches; keys are optional and web tools default to disabled.

## Settings and history

Shared settings: ~/.config/chuggin/settings.json, or
$XDG_CONFIG_HOME/chuggin/settings.json. Project-local chuggin.json contains the accepted
goal, repository, checks, and state location. Project overrides take precedence;
shared settings are reloaded when starting a run.

On the home screen, **Choose model** changes the current project's model using
its configured Ollama server. Before project setup it changes the shared default.
**Shared settings** changes defaults; existing project overrides still take precedence.

Defaults: 32,768 context tokens, 4,096 output tokens, thinking disabled, and up to
48 work steps per cycle, with bounded request recovery within each step.
Conversations are not reset at an estimated byte threshold. After repeated failed
request recovery or actual context pressure, work can continue in a refreshed
conversation with the current files, main goal, task, and recorded failures.
Reaching the step budget proceeds to checks and a saved checkpoint.
It does not discard unfinished edits or pause the project. Model requests default
to a 30-minute total timeout; set it to 0 for no request deadline. Connection
establishment remains bounded to ten seconds.
Command reviews are separate from model-request timeouts. A timeout or connection failure retries
the same model request once, retaining completed edits and validation instead of
restarting earlier stages. If the retry fails, the existing files are still
retained for continued work.
Request settings and failed attempts are recorded alongside the response traces.

Cloud provider limits use a separate recovery path. Legacy session/weekly limits,
HTTP 429 responses, and temporary HTTP 5xx outages preserve the exact working
conversation and retry after waiting. `Retry-After` seconds or HTTP dates take
precedence; without that header, delays increase from 1 to 2, 4, 8, then 15 minutes.
The dashboard shows **Waiting for provider** and a countdown. Provider errors do
not trigger conversation compaction or count as reasoning failures. Limit errors
inside an otherwise successful Ollama stream are handled too.

Credit/payment exhaustion and authentication/access errors use the same waiting
policy, even without a supplied reset time. After reaching the 15-minute backoff
cap, Chuggin keeps retrying every 15 minutes indefinitely; there is no attempt limit.
An explicit longer `Retry-After` is still honored. Work and conversation stay intact
while you resolve account issues, credits reset, or you select another model.
Chuggin never purchases credits or changes your billing plan. Set Run duration to
0 for an unlimited run; explicit stop requests and configured run timers still apply.
An unrecognized quota message returned with HTTP 429
still receives the bounded-frequency retry policy; a reset time cannot be inferred
reliably from every provider's prose.

Pending waits persist in `.chuggin/provider-wait.json`, so restarting does not bypass
the cooldown. Once you have fixed the connection or account issue, press **T** in
the observation view to skip the current wait and try now, including a wait restored
after restarting. This requests one retry; another provider failure returns to the
normal backoff policy. T does not unpause a held cycle; the retry waits for P to resume.
Changing the selected model/server releases its old wait. A soft stop
or the run timer ends a provider wait promptly, then runs checks and saves work;
time waiting counts toward the run duration unless you explicitly pause with P.
Each provider failure and chosen retry delay is recorded in the cycle's
`provider-error-NNN.json` files.

`.chuggin/state.json` records the visible `working_workspace`, current branch,
`branch_head` (normal Git history), `working_ref` (latest recovery save),
`last_validated_tree`, last passing recovery reference, current task, completion
records, pending commit explanation, and recent outcomes. Full evidence remains
in cycle artifacts. `.chuggin/conversation.json` retains the conversation across
tasks, cycles, and restarts.

Recovery snapshots use project-scoped `refs/chuggin/autosaves/...` references.
They preserve failing and unfinished work without appearing in ordinary branch
history or a normal branch push. Backups must include the Git objects and recovery
refs plus Chuggin's state; a normal clone alone is not a complete recovery backup.
For an explicitly chosen Git worktree, the shared Git directory is also required.
Snapshots exclude ignored files and Chuggin's runtime/configuration paths. Existing
ignored files remain in your working folder. Runtime paths are also added to
Git’s local exclude file, keeping logs out of ordinary staging without editing
your project’s tracked ignore rules.

**Upgrading an existing project:** choose Resume to prepare a migration preview.
The old developing workspace and the visible folder are reconciled in a separate
preparation directory. Nothing is copied over the visible folder until you apply
that preview. If both folders changed a file, choose **Review conflicting files**
to compare versions and select the whole file from Chuggin or your original folder. Both
originals remain backed up; choices only change the preview. The screen explains
why upgrading from the older separate-folder workflow needs this review and
recommends keeping Chuggin’s latest work. **Ask AI to recommend a version** uses
your configured project model for a bounded, read-only review. It explains its
choice for you to approve, or asks for closer review when neither whole file is
a safe choice. Provider errors return to the menu; no file choice is applied.
Once conflicts are resolved, **Move files and resume** applies the result.
Combining parts manually is also possible in the displayed preview folder.
**Start review over (if files changed)** re-reads both folders and starts a fresh
file comparison. Use it after editing either folder during the review, or to redo
your file choices. Previous choices remain backed up but are not reused.
Restarting the review leaves project files and development history unchanged.
Existing commits keep their IDs, authors, dates, and messages. Both input
snapshots, staging backups, the old workspace, conversation, and diagnostic history
remain available. A running or paused worker must stop before migration.

If the original folder has changes selected for a future Git commit, the menu
explains that selection separately. You can clear the old selection while keeping
the updated files, or explicitly preserve it. Its backup remains available.
For maintenance/automation, `chuggin migrate`
prints the preview and `chuggin migrate --apply` applies it; unattended runs never
silently migrate. Interrupted application is journaled and can resume; intervening
file or staging changes stop automatic recovery rather than being overwritten.
Version 0.12 uses state schema 4; older binaries cannot resume that state. Once
migration succeeds, later launches resume directly; this is a one-time move per
project, not a step repeated on every launch or version update.

Migration also records its actual changes relative to the agent's previous files
in `.chuggin/migration-handoff.json`. If those files changed, the first work
interval receives a one-time note listing affected files, including original
versions kept in place of Chuggin's work, additions, removals, and merges. The note
asks for focused inspection and appropriate checks, followed by ordinary work.
It survives an interrupted interval and conversation recovery, then retires at
the next completed cycle boundary. Concrete unresolved findings remain ordinary
follow-up work. Keeping the agent's files unchanged adds no extra review request.

Each cycle-NNNNNN directory records the evidence for that interval:

- Effective configuration, prompt version, and task information.
- Exact model requests and raw streamed responses, including server-provided
  token counts, timings, and interrupted output.
- Tool results, progress notes, context recovery, and request errors.
- Full check logs, validation results, completion requests, and checkpoint outcome.

The working conversation and current files survive task and cycle boundaries. The short
state summary rotates; full cycle folders remain. Artifact and build output
cleanup is not yet automatic.

## Build and test

All implementation and tests are Rust. Installation is one executable.

~~~sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release
install -m 755 target/release/chuggin ~/.local/bin/chuggin
~~~

Git and the target project's build/test tools must be on PATH. Ollama can run on
another machine. Advanced automation commands are available through help.

This is an experimental harness, not proof of product completeness. Check quality
and model judgment matter; a saved checkpoint does not prove arbitrary behavior
correct. Requests have no byte-based context cutoff; the configured context and output token
limits are sent to Ollama. Large requests can still exceed the model's context capacity.
Checks run with the user's permissions. Worktrees isolate Git changes, not code
execution.

## Harness design references

The current design emphasizes persistent work, useful tool feedback, and
observable verification, informed by [Anthropic's context engineering guidance](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents)
and [tool design guidance](https://www.anthropic.com/engineering/writing-tools-for-agents).
Structured goal drafts follow [Ollama's schema support](https://docs.ollama.com/capabilities/structured-outputs).
Search uses the [Brave Web Search API](https://api-dashboard.search.brave.com/api-reference/web/search/get).

The working model can still miss defects or invent a concern. Saved checkpoints
and passing tests do not establish completion of a broad product goal. Compare
actual changes, unresolved findings, and validation results across longer runs
before treating harness changes as a performance improvement.

## Nudges

Press **N** in the observation view, including while paused, to give the model a
persistent temporary priority within the project goal. Enter saves it; Esc closes
the editor. Ctrl+U clears the draft, Ctrl+D cancels the active nudge, and Ctrl+R
reopens the latest closed nudge. One nudge is active at a time; replacing one
preserves its history. Adding a nudge while paused does not resume work.

The next model call receives the update without discarding conversation history
or interrupting a request or command already underway. A nudge can span multiple
tasks. The model calls `finish_nudge` with a summary and evidence, then returns to
the overall goal. This records the model's completion claim, not independent
verification. The Goal tab shows requests, completion reports, and history; reopen
a nudge if more work is needed. State lives in `.chuggin/nudges.json` separately
from worker state so concurrent UI and model updates cannot overwrite one another.

## Allowing goal completion

In the observation view, open **5 Settings**, select **Allow goal completion**,
and set it to **on**. It defaults to off and persists for this project. The next
model call receives a tool to mark the overall goal complete when fully achieved
and verified. There is no urgency prompt, reduced budget, or new work stage.
Without that tool call, the loop continues normally.

Completion saves the working tree and the model's summary and evidence, then
pauses with **Model reports project complete**. Configured check results are
recorded alongside the claim; the claim itself is not independent verification.
The saved completion prevents automatic continuation after restarting. Press
**R** from the paused observation view to explicitly reopen work, or turn the
setting off and resume to return to indefinite looping. Reports remain in the
cycle artifacts. Turning the setting off also rejects outstanding completion
calls; running commands must finish before the model can report completion.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).

### Groq free-tier inference

Choose **Choose model → Groq** from Home. Add the API key under
**Settings → Shared settings → Groq connection and limits**. The model list comes
from your Groq account. Project models use names such as
`groq/openai/gpt-oss-120b`; the `groq/` prefix selects Groq, while existing model
names continue to use Ollama. Chat can select a Groq model independently with the
same prefix. Credentials live in the private global `groq.key` file, never in
project configurations or model request artifacts. Connections use Groq's official
HTTPS endpoint and do not follow redirects.

The default shared budget is 24 requests/minute, 900 requests/day, 7,200
tokens/minute, and 180,000 tokens/day. These defaults leave headroom below the
published free limits for GPT-OSS and Qwen as of September 2026. Verify your actual
account allowances on [Groq's Limits page](https://console.groq.com/settings/limits).
Limits and the 1,024-token maximum response cap are editable in the same menu;
optional separate input/output minute limits are supported too.

Every inference call—including chat, recovery and watchdog requests—must reserve
estimated input plus maximum output before sending. The budget is shared across
projects and models on this machine, locked across processes, and persisted across
restarts. Actual reported usage replaces reservations; interrupted or unreported
requests retain their reservations. Daily accounting uses a conservative rolling
24-hour window. Minute token capacity refills continuously; Chuggin waits for the
next request to fit instead of always waiting a full minute. The dashboard labels
this **Waiting for Groq budget**, separately from provider-error retries.
Successful response headers can tighten admission, and an unexpected
429 also establishes a shared cooldown. Manual retry rechecks this budget rather
than bypassing it. A watchdog skips unavailable inference instead of waiting
indefinitely while reviewing a command. Other agents can pause/stop normally during
quota waits. Model-list connection tests do not invoke inference.

Free-tier throughput is mostly constrained by **tokens**, not request counts.
Once the daily budget is used, the loop waits for capacity to return; this is
expected and survives restarting Chuggin. Waiting cannot make an oversized request
fit. Groq requests therefore keep the system instructions and initial goal, drop
older complete exchanges when necessary, and explicitly shorten large tool results.
The latest exchange stays paired, and pre-shortening conversations are archived.
If the required context still cannot fit, Chuggin reports the problem without sending
it. This smaller working context applies only to Groq.

Token counting is an estimate with headroom and upward calibration from actual
usage, not a provider tokenizer guarantee. Other apps/machines using your Groq
organization are only reflected in provider headers, so 429 recovery remains a
fallback. Cached tokens are conservatively counted locally. The local limits are
not an account-wide billing cap and do not automatically enable paid usage.
