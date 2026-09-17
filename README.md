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

- **1–5 / Tab:** switch between live activity, model output, checks, the goal, and local settings.
- **Arrow keys / mouse wheel / Page Up / Page Down:** scroll without pausing work.
  Scrolling back to the bottom automatically resumes following live output.
- **Home:** oldest retained output. **End / F:** follow live output again.
- **/** searches the current output view; **Esc** clears the filter.
- **?** opens the keyboard guide.
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
apply immediately against elapsed time since this run started. Shortening the
duration below elapsed time requests a stop after the current cycle; extending
it allows additional time. Resuming a stopped run starts a new timer.

## One conversation, continuous refinement

Chuggin separates **saving progress** from **verifying correctness**. One durable
working branch and worktree hold the developing project. Failing checks, model
errors, and cycle boundaries do not discard its edits. A checkpoint is a saved
revision, not an approval.

The agent uses **one conversation across tasks, cycles, and restarts**. Its system
instructions, main goal, and tool definitions stay stable. New task directions,
tool results, and check feedback are appended to the same history. This keeps
useful reasoning available and gives Ollama the opportunity to reuse a cached
prompt prefix instead of processing a different conversation for each activity.
Actual cache reuse depends on the server, model, and available context.
Tool definitions are fixed during a run; changes such as enabling web tools are
applied when the next run starts.

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
There is no mandatory separate planning agent or reviewer, and no review veto.
An ordinary prose response can end a work interval without marking the task done.

At each cycle boundary Chuggin runs configured checks, even if the agent already
ran them, saves a checkpoint when there are changes, and records the results.
The last passing revision advances only when all configured checks pass and the
project files remain unchanged during those checks. Commands that modify files
leave a saved but unverified checkpoint for another validation pass. The next
cycle continues from those files and the same conversation.

Chuggin does not rebuild an attempt from an older passing snapshot. If an approach
needs to be undone, the agent can make a targeted correction or explicitly use
`restore_checkpoint` with an ancestor commit and a reason. Restoration first saves
the current work in Git, then restores the requested project files. It does not
erase the prior history or automatically certify the restored files.

Conversation recovery happens for reported context pressure or repeated request
failures, not simply because a task or cycle ended or a text-size estimate was
exceeded. Before a handoff, Chuggin archives the full conversation. It preserves
the stable instructions and goal, then supplies the current task, checkpoint,
progress notes, and latest feedback. A token-pressure handoff also retains recent
messages. This is a deterministic handoff, without an extra summary-model request,
and never resets project files. The interface distinguishes checkpoints,
unresolved failures, and the last revision whose configured checks passed.
A passing check is evidence about that check, not proof of project completeness.

The agent can use `save_progress_note` for a short factual handoff: what changed,
what it checked, outstanding problems, and a useful next action. Recent actions
and failures are recorded too. These notes work with any language or artifact
format and remain advisory; current files and observed validation take precedence.

## Tools and recovery

The agent has the same inspection, editing, execution, and task tools throughout
the working conversation, plus a bounded inventory of project files. Rust
declarations and module reachability supplement the inventory when present. Other
formats use file reading and text search. Instructions describe project outcomes
and evidence rather than assuming a language or test framework.
Setup asks for a validation command appropriate to the project; it only suggests
`cargo test` when a Cargo manifest exists. Documents and data can use linters,
consistency checkers, or custom scripts. A validation command is still required.

The model can list files, search literal text, read numbered line ranges, replace
a file, make an exact targeted edit, and run configured checks. Long reads include
continuation positions. Unique-match edits prevent accidental broad replacement.
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

## Execution, compiler diagnostics, and symbols

The working agent can use **run_command** with an executable and argument array,
for example `["cargo", "test", "unicode"]` or `["cargo", "fmt"]`. Commands run
in the persistent workspace, with no implicit shell and no interactive stdin.
The default timeout is 120 seconds; a call can select 1–600 seconds. Chuggin
returns the exit code, timeout status, a bounded output tail, and a log ID.
**read_command_log** retrieves the full log in chunks, including logs from earlier
cycles. Use the complete returned ID, such as `cycle-000003/command-2-0.log`, so
continued conversations retrieve the original evidence even after command numbers
repeat. Legacy IDs without a cycle prefix refer only to the current cycle.
Commands and checks share
the live output view and process-group cleanup on completion, timeout, or force stop.
Application runs are bounded foreground runs, not persistent interactive sessions.

**compiler_diagnostics** is a Rust helper that requires a Cargo manifest. It runs
Cargo check for all targets and returns grouped errors/warnings, source locations,
nearby code, and compiler suggestions. It does
not execute tests. Command success does not substitute for the configured final
checks. Command-produced edits remain part of the persistent project and are
saved at the next checkpoint.
Like configured checks, commands execute with the user's OS permissions. A Git
worktree isolates project revisions; it is not an OS sandbox. The model is
instructed to keep commands within its task and leave Git commits/state to Chuggin.

**lookup_symbol** inspects Rust files. It finds types,
functions, private/public methods, and name-based reference candidates, with
paths, line numbers, source snippets, and pagination. It parses Rust syntax, so
comments and string contents do not appear as references. This is not a language
server: it does not resolve types, expand macros, or filter inactive cfg branches.
These helper definitions remain available to keep the tool schema stable; they
are useful only for the formats they support. They are not required validation
steps for other projects.

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

Defaults: 32,768 context tokens, 4,096 output tokens, thinking disabled, and up to
48 work steps per cycle, with bounded request recovery within each step.
Conversations are not reset at an estimated byte threshold. After repeated failed
request recovery or actual context pressure, work can continue in a refreshed
conversation with the current files, main goal, task, and recorded failures.
Reaching the step budget proceeds to checks and a saved checkpoint.
It does not discard unfinished edits or pause the project. Model requests default
to a 30-minute total timeout; set it to 0 for no request deadline. Connection
establishment remains bounded to ten seconds.
Checks have their own individual timeouts. A timeout or connection failure retries
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
the cooldown. Changing the selected model/server releases its old wait. A soft stop
or the run timer ends a provider wait promptly, then runs checks and saves work;
time waiting counts toward the run duration. Each provider failure and chosen
retry delay is recorded in the cycle's `provider-error-NNN.json` files.

`.chuggin/state.json` records `working_ref`, `working_branch`, `working_workspace`,
`last_checks_passed_ref`, the current task, feedback, and recent outcomes.
New projects use `.chuggin/working` on a `codex/chuggin-working-...` branch.
The initial worktree includes the original checkout's current project files,
including uncommitted edits, deletions, and untracked files that Git does not ignore.
The original checkout is not overwritten or automatically merged. The persistent
worktree is the developing project; checkpoint commits can contain failing or
unfinished work. `.chuggin/conversation.json` preserves the working conversation
across tasks, cycles, and restarts.

Existing projects adopt their latest compatible candidate with actual changes,
skipping newer attempts that stopped before editing. They adopt that work in place and
back up their previous state. Historical cycle folders, logs, and commits remain
available for diagnosis. The adopted workspace retains uncommitted files too.

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

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
