# Worker worktrees

Workers now use separate detached Git worktrees by default. Their edits and checks
run there; the parent checkout changes only when you explicitly apply a result.
This is not an OS sandbox. Absolute paths, shared Git metadata, build caches, and
external services can still be affected. See the [sandboxing design issue](https://github.com/bradleyd/worksmith/issues/2).

## Start workers

Commit pending work first. The index and working tree must be clean, including
non-ignored untracked files. Ignore local runtime databases/build artifacts through
your normal project configuration; Worksmith does not stash or commit for you.
Submodules, sparse checkouts, assume-unchanged/skip-worktree index entries, and
repositories without a commit are not supported
by isolated execution in this version. Ignored dependencies and credentials are
not copied into worker worktrees.

```text
/spawn -n 2 --until "cargo test" implement independent alternatives for the parser
/agents
/agents show w1
/agents diff w1
/agents apply w1
```

All workers in one request, including queued workers, start at the same captured
commit. A repository subdirectory remains the worker's execution subdirectory.
Check commands run there too. A worker check passing does not validate the combined
parent result: run parent tests after applying changes.

Use `/spawn --shared ...` only when you explicitly want the previous behavior:
workers edit the parent's directory concurrently. This also supports non-Git
projects. Failed worktree setup never falls back to shared execution.

## Review, apply, and discard

`/agents diff <id>` shows a complete captured patch and validation evidence.
`/agents apply <id>` applies the whole result, leaving changes unstaged. Parent HEAD
must still match the starting commit, and every affected file must still match
its original content and mode in both the index and working tree. Unrelated parent
edits/staging are preserved. Overlapping results, new-file collisions, or changed
worker files stop the apply. There is no automatic merge or force-apply option.

Stop the parent turn before applying. Git writes are not an atomic transaction
against external editors: avoid editing affected files during application. An
interrupted or unverifiable apply is marked `Applying` and requires manual
inspection of the retained patch and parent files; it is not automatically retried.

Binary files and executable bits are included. Symlinks, submodule changes, and
special-file changes cannot be automatically applied. Oversized patches fail
explicitly at the 16 MiB command-output limit instead of being silently truncated.

Worktrees remain after completion, failure, cancellation, and successful apply.
`/agents discard <id>` removes an applied or empty result. For unapplied or
interrupted work, inspect it first, then repeat with `--yes` to confirm deletion.
Shutdown and `/new` cancel and join current workers; they do not delete results.

## After restart or from scripts

Short IDs such as `w1` belong to the current manager. Retained results use the
worker session UUID shown in reports:

```text
/agents retained
/agents diff <worker-session-uuid>
```

The equivalent commands work without a model or interactive session:

```sh
worksmith spawn -n 2 --no-synthesis 'implement independent alternatives'
worksmith agents retained
worksmith agents diff <worker-session-uuid>
worksmith agents apply <worker-session-uuid>
worksmith agents discard <worker-session-uuid> --yes
```

Run review commands from the original project. Records and worktrees live under
`WORKSMITH_HOME/worktrees/<parent-session>/<worker-session>/` (normally beneath
`~/.worksmith`). A workspace lock prevents review/mutation while its worker is
active. After a crash, `diff` captures the interrupted workspace with validation
marked unknown. No worker automatically resumes and no background sweep deletes
retained work.

Worker MCP remains disabled. Dirty working-state snapshots, automatic merging,
non-Git scratch copies, and process sandboxing are separate future work.

## Worker results and validation output

`/agents show w1` displays the complete worker summary and the latest validation
command, outcome, exit code, and output excerpt. The plain REPL uses the same
format; headless `worksmith spawn` prints it when each worker finishes. The model's
summary is labeled separately from the actual validation evidence.

Every completed check retains its output, including successful checks. Long output
shows a labeled tail of at most 4,000 bytes plus a validation log path. Logs live
in the dated session directory at `checks/<attempt-id>.log` (legacy sessions keep
`<session-id>.checks/<attempt-id>.log`); stdout is
followed by stderr, not interleaved. The existing subprocess limit of 16 MiB per
stream still applies; exceeding it is an execution error. `/agents tail w1` shows
validation events during the run. After restart, `worksmith agents diff <uuid>`
shows the retained check excerpt and log path. Old records explicitly report when
output was not recorded. Dated session storage is implemented; retention/pruning remains separate future work.
