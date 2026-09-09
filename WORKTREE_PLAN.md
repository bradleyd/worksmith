# M11: worker worktrees, simple first

Initial implementation completed in the working tree, 2026-09-09. The user
authorized implementation after reviewing this plan, then commit and merge.
Usage: [`docs/worktrees.md`](docs/worktrees.md).

## Outcome

Two workers can edit the same filename and run checks in separate working trees.
Neither result changes the parent's checkout until explicitly applied. Failed,
cancelled, and conflicting results remain available for inspection.

This is edit isolation and review, not process sandboxing. Worktrees share Git
objects and repository configuration; shell commands, absolute paths, symlinks,
external services, and shared build directories can still affect other state.
OS-enforced containment is tracked in
[issue #2](https://github.com/bradleyd/worksmith/issues/2).

## First-version decisions

- Git repositories with an existing commit only. Require a clean index and working
  tree, including non-ignored untracked files, when accepting an isolated spawn.
  Fail clearly on dirty trees, unresolved merges, submodules, or unsupported sparse
  checkouts. Never stash, commit the user's work, or silently share their directory.
- Capture the full HEAD object ID once per spawn request. All workers in a fan-out,
  including queued workers, use that same base. Later parent edits do not change it.
- Use detached worktrees. No worker branches, branch naming rules, merge commits,
  new Git library, or generic workspace-provider trait.
- Do not copy ignored dependencies, build output, secrets, or arbitrary local
  configuration. Explain missing prerequisites when worker validation fails.
- Isolated execution is the default for `/spawn` and headless `worksmith spawn`.
  Keep existing non-Git use possible through an explicit `--shared` choice whose
  notice states that workers edit the parent's directory. No automatic fallback.
- MCP remains disabled in workers. Worktrees alone do not authorize delegation.
- Applying results is explicit and applies a complete reviewed result. No automatic
  merge, hunk selection, conflict resolution, or model-triggered apply in this slice.

Requiring a clean start deliberately narrows the earlier roadmap's dirty-snapshot
proposal. Dirty tracked/untracked snapshots are a later increment, not a hidden
claim of this first release. This limitation must appear in help and spawn errors.

## Small Rust implementation

Follow `~/.worksmith/skills/idiomatic-rust/SKILL.md` and repository conventions.

Add one concrete `src/workspace.rs` module that owns worktree metadata and Git
operations. Use owned `PathBuf`/`String` fields, borrowed `&Path` parameters,
explicit `Result` errors with context, and enums for workspace/result state.
Keep visibility narrow. Do not introduce a backend trait, generic lifecycle
framework, extra shared mutex, unsafe code, or a new dependency without need.

Use the installed Git CLI through structured process arguments, never shell
interpolation. Run subprocess work asynchronously with cancellation and bounded
output/time. Existing spawn entry points are synchronous: prepare inside the
worker's async task, expose preparation in worker status, and count preparation
against the existing concurrency cap. Return setup failures through normal worker
reports rather than silently dropping queued tasks.

`WorkerManager` owns the worktree lifecycle. One workspace value holds the
canonical repository root, original relative cwd, base object ID, unique workspace
path, and durable worker/session identity. Use session UUIDs for on-disk ownership;
`w1` is only a display identifier and repeats across sessions.

Retain the existing execution status enum; review state is separate because a
failed worker can still produce useful changes. Add only the result states needed
by review/apply/discard. Avoid combinations of booleans for these transitions.

## Execution and context

1. Validate eligibility and capture the base before accepting the group. If the
   caller is in a repository subdirectory, preserve that relative cwd in workers.
2. Record ownership metadata before creating the detached worktree beneath
   `WORKSMITH_HOME/worktrees/<parent-session>/<worker-session>/`. Refuse a storage
   location inside the source checkout. Persist setup failures for recovery.
3. Fork the agent with the worker cwd explicitly supplied. Audit the cloned
   `ToolContext`: file/shell tools, knowledge lookup, workspace instructions and
   validator must resolve against the worker tree. Preserve the already-resolved
   model settings, trust decision, approval policy, and loaded-skill snapshot;
   do not reload global/project config from an arbitrary new directory.
4. Keep project memory identity and parent-session links tied to the original
   project. Read inherited memory context once; worker proposals remain subject
   to parent review. Do not create a separate durable project-memory identity for
   each temporary checkout. Verify shared tool registry entries do not retain
   mutable parent workspace paths or write through parent-owned indexes.
5. Run the worker and its explicitly configured check there. Keep current validator
   selection semantics; do not also change inheritance from the parent's `--until`.
6. After the turn and owned tool/check processes stop, capture the result. Obtain
   changed paths from Git/file state, not just tool events, so shell edits count.
   Record validation command, cwd, result, and the captured result identity together.
   Later edits invalidate any claim that a check validated the updated result.

Git checks/builds can still use a shared Cargo target or external cache. Document
this; do not promise complete validation-side-effect isolation or introduce a
package-manager-specific cache isolation system in M11.

## Review and apply

Reuse `/agents` in both interactive frontends:

- `/agents diff <id>` shows the retained result, paths, and validation outcome.
- `/agents apply <id>` explicitly accepts the captured result into the parent cwd.
- `/agents discard <id>` removes a stopped worker workspace with an explicit
  confirmation if it contains unapplied changes.

Provide equivalent headless review commands using durable session/worker identity
so results from `worksmith spawn` remain usable after that process exits. Reuse
one backend; do not build a new TUI browser for this release. Existing worker
reports say "changes awaiting review", not "files updated" in the parent.

Capture a Git-compatible patch against the recorded base, including staged and
unstaged tracked changes and non-ignored new files. A private temporary index may
be used to represent the result; never stage in the parent's index. Include binary
contents and executable modes. Treat renames as delete/add if that simplifies the
implementation. Reject symlink, submodule, or unsupported special-file changes
from automatic apply, preserving them for manual inspection. Do not silently omit
unsupported changes or pretend truncated patches are complete.

Before apply, verify workspace ownership, recorded base, and result identity. The
parent must still be on the recorded HEAD. For each affected path, both parent
index and working content/type/mode must match the base; added paths must still be
absent. Unrelated parent edits may remain. Refuse the entire apply if any affected
path changed, including when Git could merge its hunks. Preserve the worker result.

Pause parent model/tool execution while applying and serialize applies through the
manager. Validate paths and reject escapes or symlink ancestors. Use Git's normal
checked patch application without `--3way`, `--reject`, index updates, or force.
Recheck preconditions immediately before applying; report success only after
verifying the resulting content. Git working-tree writes are not a filesystem
transaction against arbitrary external editors: document that limitation, retain
the patch on failures, and never roll back by overwriting concurrent user edits.

Parent changes remain unstaged. No auto-commit. Disjoint worker results can be
applied sequentially; overlapping results stop for review. An applied worker's
check does not prove that the combined parent tree passes: run the appropriate
parent validation separately and keep that evidence distinct.

## Retention and recovery

Keep completed, failed, and cancelled worktrees until explicit discard. Apply does
not delete the source result. Do not perform destructive cleanup in `Drop`.

Cancellation first stops the worker and waits for owned tool/check cleanup, then
records the retained workspace. `/new` and quit must stop/join workers or retain
clear ownership; do not remove a tree while a worker still uses it. Audit existing
bash/validator subprocess cleanup as part of this step. Detached hostile processes
remain outside the M11 guarantee.

Persist a small versioned workspace record alongside existing sessions; no new
database or daemon. On restart, list retained/interrupted workspaces without
resuming execution automatically. Cross-check ownership against Git's worktree
registration before removal. Never run broad prune or recursive deletion on a
path supplied by worker output. Failed cleanup reports the exact retained path.
Crash during apply is an uncertain result: inspect destination versus the retained
patch before allowing any retry, rather than claiming the apply never happened.

## Implementation order and acceptance gates

1. **Owned worktree creation and execution.** Implement the concrete module and
   worker cwd propagation with a single worker. Test setup failure, cancellation,
   relative cwd, and a dirty-start refusal. Then use the existing fan-out queue
   with one captured base. No review UI refactor.
2. **Retained results and explicit apply.** Capture complete diffs and validation
   evidence; add shared diff/apply/discard operations and concise reports. Test
   parent-edit preservation before calling the feature usable.
3. **Recovery and frontend parity.** Persist ownership, recover interrupted
   results, and cover shutdown/session transitions. Exercise TUI, REPL, and
   headless spawn/review before enabling the new default.

Use temporary Git repositories and scripted model clients; no network or live
models. Session tests call `common::isolate_home()`.

Required scenarios:

- Two workers write the same file with different content; both results survive,
  parent content/index remain unchanged, and each check reads its own version.
- Queued fan-out workers retain the original base after the parent changes.
- Tool writes, shell writes, validation cwd, subdirectory cwd, and workspace reads
  all resolve correctly. A test that only checks directory creation is insufficient.
- Review includes new/deleted files, binary content, executable bits, and staged
  worker edits. Unsupported paths fail visibly rather than disappearing.
- Applying a reviewed result preserves unrelated parent edits and staged content;
  overlapping content, staged edits, new-file collisions, or moved HEAD reject the
  entire operation. Applying a changed or already-applied result is refused.
- Cancellation, setup failure, `/new`, quit, restart, and interrupted apply preserve
  inspectable results. Cleanup never removes another session's or user's worktree.
- Shared mode is explicit; failed isolation never silently runs in the parent cwd.
- Break cwd propagation and conflict checks deliberately to prove their tests fail.

Run `cargo test`, `cargo clippy --all-targets -- -D warnings`, focused formatting,
`git diff --check`, and a PTY smoke test for the finished implementation. Keep
changes local to the integration points; no broad worker/TUI redesign.

## Deferred

Dirty-tree snapshot mirroring, non-Git scratch copies, submodules/sparse checkouts,
automatic merging, per-hunk application, automatic retention policies, worker MCP,
role routing, and OS sandboxing. Add these only against demonstrated need and an
explicit follow-up design. The first release is complete when isolated edits,
review, guarded apply, and retention work—not when every repository shape works.

## Implementation verification

The complete repository suite passed 487 tests (two live probes ignored).
Clippy is warning-clean. Offline CLI, plain REPL, and TUI PTY smoke tests passed
spawn, diff, apply, and lifecycle checks. Deliberately breaking worker cwd
propagation and the pre-apply parent guard made their regression tests fail; both
mutations were restored before final verification. No dependencies were added
for worktrees. Later worker-result UX changes retain successful validation output
and use the same human-facing report in TUI, REPL, and CLI.
