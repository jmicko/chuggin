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

## Shared Loop, Finish, and Chat modes (September 20)

Nudges shipped locally in 0.9.0: one durable temporary user priority, next-call
injection into the existing conversation, explicit evidence-bearing completion,
reopen/cancel/replace history, and no extra acceptance gate. They are separate
from task completion and never complete the overall project goal.

September 21 refinement: no separate Finish mode or urgency prompt. The user
can enable "Allow goal completion" (default off). This exposes finish_project,
whose description says to use it only when the overall goal is fully achieved
and verified. Otherwise the loop continues unchanged. A call saves a checkpoint
and completion report, then pauses durably. Explicit resume reopens work; there
is no additional review veto. This is implemented in 0.10.0.

Next, add Chat mode: schedule work in response to user messages, allow multiple
tool calls and commands within a turn, and wait after an answer or question.
Support read-only questions about results as well as explicitly requested edits.
Mode transitions should be persisted, take effect at safe boundaries, and be
clearly visible. Keep the stable prompt prefix and use a short mode update in
the conversation instead of separate agent implementations or long explanations.
Chat remains a proposed next step, not implemented in 0.10.0.

## Multiple providers and helper agents — September 23 plan

This extends the earlier sequential-helper proposal: allow concurrent helpers on
separate inference resources, with a conservative one-request default per local
server. Actual Ollama concurrency depends on server configuration and memory.

Build in this order:
1. Provider profiles and scheduler: existing Ollama adapter plus a direct Groq
   adapter using its OpenAI-compatible chat-completions protocol. Normalize stream
   deltas, tool-call argument fragments, usage, finish reasons and provider errors.
   Preserve existing model behavior and restart-compatible conversation storage.
2. Named profiles in a Providers TUI: add Ollama / Groq / compatible endpoint;
   enter URL or masked key; test connection; list available models; choose allowed
   main/helper uses. A model selection is provider + model, not a bare model name.
   Keep shared secrets in restricted home configuration, never project logs or
   request artifacts. Changing model/provider affects subsequent calls only.
3. User-controlled helper pool: checkboxes per profile/model, resource concurrency,
   optional fallback priority. Main model may request a helper task and allowed
   profile; it cannot authorize new providers, raise limits or opt into paid usage.
   Do not silently move a local task to a cloud endpoint on failure.
4. Durable spawn_agent / agent_status / cancel_agent tools. Fresh task-specific
   context, parent goal/nudge constraints, selected evidence; no whole-transcript
   cloning. Start with inspection/research and patch proposals. Arbitrary commands
   and edits require isolated snapshots (including uncommitted parent work), not
   a merely "read-only" prompt. Parent integrates patches serially and resolves
   conflicts. No recursive spawning initially; no compulsory roles or review veto.
5. Observation Agents view: task, provider/model, queued/running/waiting/completed,
   elapsed time, usage/estimated spend, findings, cancel control. Persist jobs and
   delivered results so restart cannot replay edits or charge duplicate helper jobs.

Budget and backoff design:
- Helpers disabled on a newly added profile until the user opts in. Paid profiles
  require an explicit budget; local profiles still have concurrency/task limits.
- Account for ALL provider requests (parent/helpers/watchdog/retries). Shared,
  transactionally reserved allowances across projects/processes, not per-agent
  counters. Reserve conservative input + maximum output before dispatch; reconcile
  reported usage afterward. Unknown outcome retains its reservation until resolved.
- Use per-helper token/work budgets plus per-provider daily/monthly estimated
  spending limits and global concurrency. Persist them across restart. Token and
  request caps supplement monetary estimates; estimates are not a billing guarantee.
- Enforce user boundaries in the scheduler. When a budget runs out, suspend that
  provider's queued work and retain partial findings; parent can continue locally.
  Never silently recharge, raise caps, or use an unapproved paid fallback.
- Honor Retry-After and reset/remaining headers at provider/account level; 429
  should pause the shared queue, not make every helper retry independently. Use
  jitter with bounded exponential backoff when no reset is supplied. Detect a
  request intrinsically larger than the token limit instead of retrying forever.
- Separate authentication/configuration errors from transient outages. Keep work
  resumable while clearly showing "needs configuration".
- Groq account limits must be verified from its console/response headers. Public
  free limits can be much smaller than model context windows. Use small helper
  prompts and bounded retrieval. An API key alone does not certify zero billing.
- Groq spend tracking currently documents a 10–15 minute delay, so provider-side
  spend limits complement rather than replace local request reservations.

Test with mocked streaming, tool fragments, 429/reset headers, exhausted budgets,
concurrent reservations and restart recovery before a bounded live Groq probe.
Do not log credentials. Live testing requires the user's key configured privately;
no live Groq request was made during the September 23 audit.

Sources checked September 23:
- https://console.groq.com/docs/rate-limits
- https://console.groq.com/docs/spend-limits
- https://console.groq.com/docs/api-reference
- https://docs.ollama.com/faq
