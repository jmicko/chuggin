# Visible project workflow

Implementation design, September 27, 2026. Implemented for Chuggin 0.12.0.
Baseline: Chuggin 0.11.0 (`d5c8f2b`). Preserve the persistent conversation, continuous refinement,
retained failing work, provider recovery, nudges, and in-cycle pause.

## Decision

The main agent should work directly in the project directory selected by the
user, on that checkout's current branch. Opening the folder in an editor,
running the project's usual commands, and inspecting Git must show the same
project that Chuggin is developing. Do not automatically create a second main
checkout, switch branches, mirror files between folders, or merge after each cycle.

Git isolation remains useful for optional parallel editing helpers and explicitly
requested experiments. A user may also launch Chuggin in a worktree they created;
that chosen checkout is then the visible project. It is not necessary to add a
second main-workspace mode to Settings in this change.

This is a filesystem/history change, not a new development-stage pipeline.
Failed checks still retain all edits and provide feedback for subsequent repair.

## Evidence from the current implementation

- `create_workspace` in `src/runner.rs` creates `.chuggin/working` on a generated
  `codex/chuggin-working-*` branch. All editing and commands target that directory.
- The original checkout is seeded into the worktree once, then left alone. Tests
  explicitly assert that its files and HEAD remain unchanged.
- A checkpoint currently commits every changed cycle, including unfinished work.
- `working_tree` calls `stage_project`, so even calculating a content identity
  modifies the real Git index. This must not be carried unchanged into a shared
  visible checkout.
- During read-only inspection, Ornith's visible checkout was still at `3531784`
  (September 16), while its developing branch was 271 commits ahead at `1e56216`
  (September 27), with additional uncommitted edits. The original checkout also
  had its own staged changes. These figures are observations, not fixed migration
  inputs; the run can advance after inspection.

## Saving and committing

Separate three concepts in both state and UI:

1. **Current files:** the actual project folder; edits are visible immediately.
2. **Recovery autosave:** a recoverable snapshot at each changed cycle and before
   a user-requested restore. Save unfinished and failing work too. Use a temporary
   Git index and a project-scoped `refs/chuggin/...` reference with reachable
   history; do not modify the user's index or move their branch for an autosave.
3. **Project commit:** a normal commit on the current branch when a task completes
   with the existing configured-check evidence. Use its concrete task summary,
   rather than a generic cycle title. Explicit user commits remain supported.

Autosaves are recovery metadata, not another working directory. They do not
appear in ordinary branch history or a normal branch push. Recovery/backup
export must include their refs and required Git objects; copying only JSON is not
a backup. Preserve autosave history initially; do not introduce silent pruning.

A run ending with an unfinished task saves recovery state and leaves its changes
in the visible folder. Do not fabricate a completed-feature commit just because
a timer expired. If a task produces no file changes, record its completion without
an empty commit. If one task finishes and the model then starts another, commit
the verified task boundary before applying the later task's edits; never label
mixed later work as the earlier completed feature. Until that boundary handling
exists, defer the milestone rather than mislabeling a cycle's whole tree.

Reuse `finish_task` and its factual summary; no extra commit-selection agent or
mandatory review stage. A manual "Commit current changes" action may create a
clearly described work-in-progress commit without claiming the goal is complete.
Commit failures do not discard files, prevent recovery saves, or turn development
back into an acceptance/rejection loop. Show a pending commit and its concrete
failure. Ordinary commits should honor the user's Git identity and commit policy;
do not silently copy the current hooks/signing bypass into normal project commits.

Replace the overloaded `working_ref` concept with explicit branch HEAD, latest
recovery snapshot, and last validated content identity. Associate checks with the
exact captured content, not merely a branch name. Existing checkpoint records
retain their original hashes and meaning.

## Sharing the folder with a human

Reading files needs no special workflow. For a stable inspection, manual edit,
or build, press P and wait for the pause acknowledgement. Already-running commands
can still finish: show their presence clearly rather than implying the operating
system has frozen. No separate "publish into the real project" step is required.

At setup/adoption, clearly state that Chuggin edits this folder and automatically
commits completed tasks on the displayed branch. Capture existing uncommitted
project content as the starting recovery snapshot. Do not silently turn existing
human staging into an agent commit. When human-staged changes would overlap a
commit, keep autosaving and defer the automatic commit, with a visible explanation
and an explicit way to include those changes or let the human commit them first.
Do not stop useful model work solely because a normal commit is deferred.

Snapshotting, content comparison, and diagnostics must never stage, unstage, or
commit unrelated user changes. Limit operations to the configured project scope,
including when it is inside a larger repository. Formal commits need an explicit
index/HEAD concurrency check and must preserve staged paths outside that scope.
Distinguish Chuggin-owned staging left by a failed commit from later human edits.

On resume from a pause, reconcile changed files and HEAD with the last observed
state and append a short factual update to the conversation. Pending edits based
on changed inputs must re-read them, not overwrite them blindly. A human commit
on the same branch can be adopted; a branch switch, detached HEAD, or unresolved
Git operation needs an explicit workspace decision before new mutations.
Use an OS lock tied to the checkout identity so a second config cannot start a
second Chuggin writer against the same folder.

This remains a cooperative single-writer workflow. Arbitrary external processes
can write files without consulting Chuggin's lock. Do not claim snapshot isolation
for tests running while files change, or perfect protection against concurrent
external shell commands. Capture/check consistency at safe boundaries, retain
changed-workspace results as unverified, and recommend pause for manual changes.

The current model-controlled whole-project `restore_checkpoint` is too broad for
the visible shared folder. Keep recovery available as an explicit user action
with a diff preview and a fresh autosave first; models can continue making targeted
repairs with ordinary tools. Restoration must preserve history and unrelated files,
not hard-reset the branch. Update the tool schema and startup instructions once,
and handle historical pending restore calls without executing an old broad restore.

## Existing-project migration

Migration is a dedicated one-time TUI operation before starting a worker. Merely
opening a project, reading its status, or installing a binary must not migrate it.
An active or paused worker still owns the project; do not migrate underneath it.

1. Inspect the visible checkout and the actual saved working checkout, including
   both HEADs, ancestry, index contents, dirty files, and untracked collisions.
2. Save a recovery journal and backups of both sets of uncommitted work, their
   index states, configuration, and state. Retain old Git refs and the old workspace.
   Preserve ignored files in place; do not add ignored secrets or build outputs to
   Git snapshots or overwrite collisions while moving project content.
3. Show where the current developing work is and what will change in the visible
   folder. For a clean, compatible checkout, bring forward the existing history
   without rewriting commit IDs, authors, dates, or messages. Carry over dirty
   developing files as well as committed changes.
4. If the original folder has independent edits or diverged commits, reconcile
   both versions in a separate preparation area. Do not blindly copy one over the
   other, assume original dirty edits are already included, or discard either side.
   Present actual conflicts and retain both inputs until resolved.
5. Verify the prepared result before applying it. Record each migration phase so
   interruption can resume or recover without applying a patch twice. Change the
   workspace/state schema only once the visible files and history are verified.
6. Preserve goal, nudges, provider waits, task IDs, conversations, completion
   history, and logs. Append one workspace-change message so old absolute paths in
   the conversation cannot silently send new tools to the retired checkout.
   Archive old workspace material safely; active tools must not keep writing there.

Do not rewrite historical benchmark backups or old logs. Do not automatically
remove the retained workspace in the same operation that adopts the new layout.
Reconcile legacy schema recovery first: projects that still use old candidate
workspaces must not accidentally migrate from their stale original checkout.

## Interface

Keep the existing observation UI. Show the actual working directory and branch,
last recovery save, last normal commit, and whether current files have matching
check results. Prefer labels such as "Autosaved · task in progress", "Committed:
Add document search", and "Checks apply to an earlier revision".

Progress should provide recovery history and commit details without requiring Git
branch knowledge. The user can run the normal project commands from the displayed
folder. Do not hard-code a Rust launch/build action or require a project type.

## Implementation order and verification

1. Extract workspace/recovery operations from the runner. Implement snapshotting
   without changing the user's index; split saved-content and branch-commit state.
2. Implement direct-folder execution and commit boundaries with isolated fixtures.
   Keep the old reader/migration path until compatibility tests pass.
3. Implement restartable, previewable legacy migration. Test on copies of actual
   Werd layouts, never by modifying a live benchmark as a test fixture.
4. Wire the TUI, pause/resume reconciliation, restore UI, and documentation. Keep
   future helper isolation compatible with this visible parent checkout.
5. Test and install; migrate a real project only after the run has stopped and the
   prepared migration result is reviewable.

Regression coverage must include visible edits before commit, ordinary branch
history advancing, failing work retained without fake milestone commits, unrelated
and partially staged changes preserved, staged human edits blocking only commit
promotion, failed hooks/signing, check/file races, human commits/branch switches,
same-checkout concurrent starts, detached HEAD, initial/unborn repositories,
force-stop/restart, restore with external edits, and migration interruptions.
Migration cases include dirty changes on both sides, deletes/renames, binaries,
untracked and ignored collisions, legacy schemas, and unchanged historical records.
Use a non-code fixture as well as code fixtures; none of these rules is Rust-specific.

Do not implement provider adapters, helpers, project types, or new task acceptance
rules as part of this change.
