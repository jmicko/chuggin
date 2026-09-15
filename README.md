# Chuggin

A Rust terminal tool for building large projects with small local models through
an indefinite sequence of focused tasks.

**The program loops indefinitely. Model conversations do not.**

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

- **1–4 / Tab:** switch between live activity, model output, checks, and the goal.
- **Arrow keys / mouse wheel / Page Up / Page Down:** scroll without pausing work.
  Scrolling back to the bottom automatically resumes following live output.
- **Home:** oldest retained output. **End / F:** follow live output again.
- **/** searches the current output view; **Esc** clears the filter.
- **?** opens the keyboard guide.
- **Ctrl+C / Q:** finish the current cycle. **Ctrl+C again:** force stop.
  When the run finishes, Enter returns to the splash screen.

The side panel shows request counts, recent acceptance outcomes, context usage,
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

First Ctrl-C finishes the current cycle and returns to the menu. Second Ctrl-C
stops immediately. Subsequent runs resume saved progress with fresh conversations.
During setup, a single Ctrl-C exits.

## Persistent project, fresh stages

1. Discovery picks the next useful gap from the main goal and current evidence.
2. Task shaping specifies a small behavior, writable paths, and acceptance
   criteria. A slice keeps implementation, dependency wiring, and focused tests
   together; oversized proposals are narrowed to one complete behavior.
3. Implementation edits a private Git worktree. Long or stalled conversations
   restart with current files, the task, and the latest check results. Passing
   new tests trigger an early handoff to verification rather than more tinkering.
4. The harness runs the operator's configured checks. For changed Rust code, a
   fresh stage proposes an extra public-API regression test. Chuggin runs that test
   explicitly, retains valid tests, and gives compile errors one fresh test-only
   repair. Remaining noncompiling or timed-out probes are logged and removed. A valid failing test blocks acceptance and carries into repair.
5. A fresh reviewer examines the task, diff, files, and actual results.
6. Passing work is committed; repairable attempts can carry their files into
   the next fresh cycle.

The broad project goal remains fixed. Discovery gets bounded progress evidence,
not prior conversations or unverified reviewer claims. The latest rejected
attempt can be retried from the same accepted baseline; repeated unsuccessful
repairs return to task planning without resurrecting older broken candidates.

Full acceptance requires passing checks, preservation of previously reported
passing Rust test names, scope compliance, and reviewer evidence
for every criterion. Markdown formatting and whitespace differences in copied
criteria do not cause rejection. New reviews reference stable C1/C2 IDs, so they
do not have to reproduce long criterion sentences.

A reviewer may explicitly accept **partial progress** when a task bundled too much:
the delivered subset must be independently useful, pass all checks, preserve all
previously reported passing test names, and add at least one newly passing test.
Unmet criteria remain in the review and progress record. This currently recognizes
Rust-style test output; other runners retain full-acceptance behavior.

## Tools and recovery

Discovery has read-only inspection tools and a bounded Rust API/module index,
including which files appear reachable from default entry points. This is an
orientation aid; compiler and test results remain authoritative. Stage prompts
explicitly favor reusing existing types and connecting useful behavior.

The model can list files, search literal text, read numbered line ranges, replace
a file, make an exact targeted edit, and run configured checks. Long reads include
continuation positions. Unique-match edits prevent accidental broad replacement.
It can request an existing Rust dependency file when a necessary supporting change
was omitted from the plan. The reason and expanded scope are logged and supplied
to review; this does not permit changing Chuggin configuration or Git control files.
When an implementer repeatedly reads without editing, a fresh patch request asks
for literal source replacements. The harness applies valid edits and reruns checks;
a claim that something was fixed never substitutes for changes on disk.

Rust tasks can wire new modules through existing parent modules, src/lib.rs, and
src/main.rs. Their criteria require exercised behavior. Empty Rust test
runs explicitly explain that orphan source files need module declarations.
Cargo.lock may accompany a task when a corresponding Cargo.toml exists.

Structured stages send typed JSON schemas to Ollama and use temperature zero.
Tool-using stages retain native tool calls and temperature 0.4. Model JSON can
be surrounded by prose or code fences. Invalid formatting gets one
fresh formatting-repair request; ambiguous multiple matching answers are rejected.
Repeated JSON fields are allowed; repetitive prose still triggers recovery.
Goal-drafting failures preserve the pitch and offer Retry, Settings, or Back.

## Execution, compiler diagnostics, and symbols

Implementation can use **run_command** with an executable and argument array,
for example `["cargo", "test", "unicode"]` or `["cargo", "fmt"]`. Commands run
in the attempt's workspace, with no implicit shell and no interactive stdin.
The default timeout is 120 seconds; a call can select 1–600 seconds. Chuggin
returns the exit code, timeout status, a bounded output tail, and a log ID.
**read_command_log** retrieves the full log in chunks. Commands and checks share
the live output view and process-group cleanup on completion, timeout, or force stop.
Application runs are bounded foreground runs, not persistent interactive sessions.

**compiler_diagnostics** runs Cargo check for all targets and returns grouped
errors/warnings, source locations, nearby code, and compiler suggestions. It does
not execute tests. Command success does not substitute for the configured final
checks or acceptance review; command-produced file changes still undergo scope review.
Like configured checks, commands execute with the user's OS permissions. A Git
worktree isolates project revisions; it is not an OS sandbox. The model is
instructed to keep commands within its task and leave Git commits/state to Chuggin.

Both discovery and implementation have **lookup_symbol**. It finds Rust types,
functions, private/public methods, and name-based reference candidates, with
paths, line numbers, source snippets, and pagination. It parses Rust syntax, so
comments and string contents do not appear as references. This is not a language
server: it does not resolve types, expand macros, or filter inactive cfg branches.
Discovery remains read-only; execution tools are exposed only during implementation.

## Optional Brave search and page reading

Open **Settings → Brave API key → Enter or replace key**. Entry is masked.
Saving a key enables the web tools; the toggle can disable them without deleting
it. **Test Brave connection** performs a real search and reports errors.

The key is stored separately in ~/.config/chuggin/brave.key (or the corresponding
XDG directory) with owner-only permissions. It is not included in project
configuration, prompts, or request logs. It is sent only to Brave's search API.

Discovery and implementation can use **web_search** for five sourced snippets,
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
48 implementation responses per task. Fresh-context refreshes happen within that
budget. Reaching the budget proceeds to verification and review, not a permanent
pause. Model requests have a ten-minute timeout; checks have individual timeouts.

.chuggin/state.json points at the accepted commit and private workspace. Accepted
branches are named codex/chuggin-.... The original checkout is not overwritten or
automatically merged.
Interrupted candidates with saved tasks are eligible for the same bounded,
freshly verified recovery as rejected attempts.

Each cycle-NNNNNN directory contains:

- Effective configuration, prompt version, discovery inspections, proposed/final
  task, and recovery origin.
- Regression-test proposals, execution logs, and retained/removed probe decisions.
- Exact model requests and raw streamed responses, including server-provided
  token counts, timings, and partial output from interrupted generations.
- Implementation replies, tool results, context refreshes, and response errors.
- Full check logs, verification, review, exact scope/acceptance gates, and outcome.
- The attempt's worktree, including rejected work.

The short state summary rotates; full cycle folders remain. Artifact and build
output cleanup is not yet automatic.

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
and model judgment matter; partial acceptance does not prove arbitrary behavior
correct. Tasks are still bounded by context and a 12KB review diff limit.
Checks run with the user's permissions. Worktrees isolate Git changes, not code
execution.

## Harness design references

The current changes emphasize selective context, useful tool feedback, and
actual verification, informed by [Anthropic's context engineering guidance](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents)
and [tool design guidance](https://www.anthropic.com/engineering/writing-tools-for-agents).
Structured replies follow [Ollama's schema support](https://docs.ollama.com/capabilities/structured-outputs).
Search uses the [Brave Web Search API](https://api-dashboard.search.brave.com/api-reference/web/search/get).

A fresh regression writer is still the same model: it can miss defects or invent
an invalid test. Successful cycles and passing tests do not establish completion
of a broad product goal. Compare prompt-version logs and real accepted diffs
across longer runs before treating these changes as a performance improvement.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
