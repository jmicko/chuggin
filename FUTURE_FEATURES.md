# Future feature: optional sequential sub-agents

Recorded September 17, 2026. The user asked to preserve this idea for future work,
not to implement it during the current overnight model comparison.

## Motivation

Keep Chuggin's persistent main conversation and working checkout. Add an optional
way to investigate a bounded question in a separate conversation, then return
concise evidence to the main agent. The benefit on a single GPU is context
isolation and a fresh examination of a problem, not parallel execution speed.

A helper can read many files, inspect logs, search documentation, and try hypotheses
without filling the main conversation with every exploratory step. It may escape
the main agent's mistaken framing, but using the same model does not guarantee
independent reasoning or correct conclusions. Returned claims remain advisory.

Example: “Investigate why paragraph formatting changes after save/load. Reproduce
the failure and identify the cause. Do not modify project files. Return relevant
locations, observed evidence, remaining uncertainty, and a suggested next action.”

## Proposed first version

- Optional investigation/delegation tool; model chooses whether a helper is useful.
- One helper at a time. Pause main-agent inference while it runs, then resume the
  exact main conversation with its result. Same model by default.
- No nested delegation, automatic corporate roles, or compulsory per-cycle agents.
- Fresh helper context containing the bounded question, relevant project constraints,
  workspace location, and selected evidence; not the entire parent transcript.
- Initially inspection and validation only, with no project edits. Determine how to
  enforce this before exposing arbitrary commands: “read-only” prompts alone do not
  prevent a shell command or a test/build from modifying files. Consider isolated
  snapshots for execution while preserving the current uncommitted project state.
- Return a compact findings summary with file/line or log references and uncertainty.
  Preserve the complete helper transcript and artifacts for inspection and follow-up.
- The main agent retains project ownership. Helpers cannot approve/reject checkpoints,
  discard work, redefine the goal, or reset the parent conversation.
- A configurable work budget should bound a helper without discarding its findings
  or stopping the overall project. Avoid reintroducing the old short arbitrary
  stage limits or mandatory review gates.
- Expose active helper task, elapsed time, and returned findings in the existing TUI.
- Reuse repetition recovery and provider backoff. Stop/timer behavior must cover both
  parent and child work; completed edits and conversation must remain resumable.
- Design persistence for interruption during delegation: record helper identity,
  progress and delivery status so restart does not replay tools or duplicate work.

## Costs and uncertainties

Sequential helpers add delegation, repeated reading, and result-integration costs.
Switching conversations can disrupt Ollama prompt-cache reuse; measure prefill
latency rather than assuming fresh context is faster. Helpers may omit crucial
context, repeat work, or produce persuasive mistakes. Small models may be worse
at deciding when/how to delegate than simply continuing the task themselves.

Ollama does support parallel requests when memory permits; its default concurrency
is one, and parallel contexts consume additional memory. A sequential default is
appropriate for the user's 12GB GPU and long contexts. Parallel execution and
helper model overrides can be later options, not prerequisites for usefulness.

## Evaluation before adoption

Finish the current baseline first. Compare the same model and identical starting
project with delegation enabled versus disabled. Repeat across Ornith, locally
runnable Qwen, and a capable cloud model such as GLM. Compare externally checked
functionality, regressions, time to useful checkpoints, tokens, prefill time,
recovery frequency, and time spent investigating versus making changes. More
commits or more passing self-authored tests alone do not establish better results.

Do not replace the continuous-refinement architecture with the old discovery →
shape → implement → accept/reject pipeline. This should complement the durable
conversation, with optional focused investigations and no review veto.

## Current baseline for future context

Chuggin 0.6.2, commit 72e9107: persistent conversation and working checkout; all work
checkpointed even when validation fails; repetition recovery; provider backoff
1/2/4/8/15 minutes, then 15 minutes indefinitely (longer Retry-After honored).
Explicit stop requests and run timers still apply; duration 0 means unlimited.

Experiments are in /home/clark/dev/werd (Ornith) and /home/clark/dev/werd-glm
(GLM 5.3 Flash cloud). /home/clark/dev/werd-qwen is the unused comparison baseline.
The Ornith process started on 0.6.0; GLM started on 0.6.2. Subsequent differences
are provider request recovery/UI, not normal task prompts or tool definitions.
Do not interrupt live runs just to update the executable or add this feature.

References:
- https://docs.ollama.com/faq#how-does-ollama-handle-concurrent-requests
- https://www.anthropic.com/engineering/multi-agent-research-system
