# Beam harness experiments — 2026-10-24

Short live runs used `beam:cloud` through the native Ollama API, with disposable
projects and separate rootless Podman installations. Existing user projects were
left untouched. This is an investigation of concrete failures, not a model
benchmark or evidence that unattended long runs are solved.

## Observations and changes

The baseline private-computer run made 42 main-model requests across six cycles.
Ten of its 44 tool calls failed because the model supplied malformed JSON-encoded
argument arrays, particularly for multiline shell programs. The command tool now
accepts an explicit `script` string alongside its existing `argv` alternative.
Exactly one is allowed; invalid arguments are never guessed or executed. The
parser is shared by the working loop, chat and MCP. Repetition detection treats
equivalent script and explicit shell-array calls consistently.

A fresh private run with the revised tool made 47 main-model requests and 54 tool
calls over eight cycles, with no malformed-argv errors. Four calls still failed:
three omitted a file path and one attempted an edit while finite work was running.
File-path and task-list errors now identify the argument to repair and whether
anything changed. These counts describe different stochastic runs, not a
controlled estimate of the tool's effect.

The revised run chose to build a local Kanban web app. Its HTTP tests could not
reach the app because inherited proxy settings routed container loopback through
the public-only proxy. Running the same generated suite with the old environment
timed out during startup and ran no tests. With loopback proxy bypass enabled,
all 12 tests passed in 0.236 seconds. Tool-launched commands now receive both
`NO_PROXY` and `no_proxy` for localhost and loopback addresses. The container still
has no direct network; host/LAN destinations remain blocked by the public proxy.

The prompt/tool audit also found contradictions:

- The repetition assessor for unassigned conversations inherited advice to
  complete tasks and select new project work. It now evaluates repetition without
  requiring development, file changes, testing or task completion.
- Some private file-tool descriptions still referred to project-relative paths
  and project restrictions. Their descriptions now match private-computer access.
- Managed private-command snapshots advertised a host Podman-client PID. They now
  direct the model to its command ID instead of presenting that PID as a guest PID.

Both unassigned private runs independently chose Python software projects. The
audited unassigned instructions did not require Python, tests or software creation.
That choice alone does not establish a leftover Python prompt. No speculative
personality or creativity instructions were added.

Desktop image caching now uses a hash of embedded build assets. New installations
cannot accidentally reuse stale startup assets from an earlier build of the same
application version; existing private installations are preserved.

## Goal and helper coverage

A nine-row CSV analysis exercised a non-code goal: preserve input, identify three
invalid rows, report six valid rows totaling 50 minutes, and document reproducible
verification. A clean revised run completed the goal in two cycles, with 14 main
requests, 18 tool calls and no tool errors. Independent checks confirmed the
per-team counts/totals and unchanged input bytes.

A second analysis run explicitly required a read-only helper investigation. The
goal completed in four cycles. Two helpers used six responses each: the first
returned retained evidence on budget exhaustion, and the second completed its
report with no concrete analysis errors found. The main model received both
outcomes and continued. This exercises helper recovery and completion; it does
not show that models spontaneously delegate well.

An initial baseline analysis run had an experiment flaw: its stdout log was
redirected into the project under test. The harness correctly detected those
concurrent changes. That run produced correct analysis but is excluded from any
before/after productivity comparison. Subsequent stdout logs were outside their
projects.

## Verification and limits

The ordinary suite passed 443 tests. A separate real rootless Podman integration
test passed, covering private files, desktop screenshots/pointer controls,
explicit shell scripts, background local HTTP service, command-ID termination,
and blocked proxy access to host/local-network destinations. Formatting, Clippy
and the locked release build also passed.

Raw request/response traces, generated artifacts and test logs are kept in a
private local experiment archive, not committed to the repository. No inference
credentials were copied into fixtures or containers. These bounded runs support
the specific fixes above; a longer unattended run is still needed to assess
long-context repetition and sustained progress. Beam's text-only image rejection
also means this experiment did not validate model vision.
