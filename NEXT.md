# What to do next

Updated 2026-09-11 after the pairing/session-trace discussion.
This is the short operational list;
`PLAN.md` holds the broader roadmap and `LOOSE_ENDS.md` the forensic notes.
Fresh-start handoff: [`FRESH_START.md`](FRESH_START.md).

## Released baseline

[v0.6.0](https://github.com/bradleyd/worksmith/releases/tag/v0.6.0) is published
from `main` at `8220028`. Both macOS ARM64 and Linux x86-64 release assets are
available. The Homebrew tap is updated and pushed at `d93e1d9`.

Shipped work includes:

- M9 metrics: request-time model/cost/cache records, per-turn history, context
  attribution, helper spend, durable worker links, `/stats`, and offline reports.
- Command overlays and interactive skill management shared with the existing
  skill loader; workers inherit independent loaded-skill snapshots.
- Parent-only stdio MCP with trust/approval gates, bounded discovery and results,
  and recoverable oversized results. See [`docs/mcp.md`](docs/mcp.md).
- The first M11 slice: clean-commit detached worker worktrees, retained results,
  guarded apply/discard, and recovery. See [`docs/worktrees.md`](docs/worktrees.md).
- Shared human-facing worker results with validation evidence, retained logs,
  and readable diffs across the frontends.
- Dated JSONL sessions under `YYYY/MM/DD/<id>/`, grouped supporting files,
  shared ID lookup, and ripgrep search with UTC date/range and project filters.
  Legacy flat sessions remain readable in place. See [`docs/sessions.md`](docs/sessions.md).

Release verification: 500 tests passed, two live probes ignored;
`cargo clippy --locked --all-targets -- -D warnings` passed. Local build/version
verification and both GitHub release jobs passed. Earlier CLI/REPL/TUI smoke
checks covered dated sessions, search, worker metrics, and validation output.
These are release results, not claims of new validation for this Markdown refresh.

## Next: pairing behavior and an expandable session trace

The next milestone is making the main session easier to follow and participate
in. The first pairing slice is implemented on `codex/pairing-guidance`, with
offline validation complete; see [`PAIR_PLAN.md`](PAIR_PLAN.md). Trace rendering remains
proposed. This milestone supersedes M12 as the next recommended work.

Two bounded slices:

1. Make `/pair on` explicitly describe collaboration in outgoing model requests,
   retaining mechanical checkpoints and keeping questions distinct from consent
   to resume. Verify toggling, compaction, and resume behavior.
2. Group the main transcript by user turn with expandable tool/check details,
   available timings, and prominent decisions. Use actual events, without model
   calls to invent activity summaries. Extend the existing transcript module and
   extract only what this feature requires.

Two local MUD runs verified checkpoint expansion and follow-up discussion before
edits (see `PAIR_PLAN.md`). Individual expandable tool entries are the current
slice. Next, discuss the working/waiting/finished
trace mockups before adding turn grouping. Resume currently uses configured
pairing state; persisted live mode changes remain deferred.

## Deferred work and constraints

- **M12 internal role routing:** benchmark tooling first. Compare helper models
  with role-specific cost, latency, failures, and downstream task success before
  adding production configuration. Cheaper compaction/extraction/planning is an
  unproven benefit; existing `[agents].model` already selects worker models.
- **Workflows:** retain the goal of a TOML job replacing shell loops for dependent
  staged work, drawing on agent-line. A dependency graph can execute serially;
  concurrent workers and model swapping are separate choices. Reconcile older
  linear-only examples and worktree handoff before implementation.
- **Tabs and broad TUI refactoring:** deferred. The trace belongs in the main
  session window. Extract code as this feature needs it.
- **System-prompt replacement:** deferred. The pairing instruction is a bounded
  mode behavior, not a general prompt editor or a lightweight inference mode.

- **OS sandboxing:** tracked in [issue #2](https://github.com/bradleyd/worksmith/issues/2).
  Worktrees isolate edits for review; they do not restrict process authority.
  MCP remains use-at-your-own-risk. Approval gates are not a sandbox.
- **Worker integration:** worker MCP, dirty snapshots, non-Git copies, and
  automatic merging remain deferred. Isolated spawn requires a clean checkout;
  `--shared` is an explicit alternative. Spawning is user initiated.
- **Session retention:** pruning and automatic retention need a separate design.
  No silent deletion, legacy migration, or SQLite session index is enabled.
- **Prompt/cache changes:** production memory placement stays unchanged. The
  prior experiments did not establish a consistent latency or completed-task
  quality benefit. The next gate is completed tool-loop fixtures with observed
  or fixed provider routing, then cross-turn/compaction/resume coverage if
  placement moves. See [`docs/content/guide/prompt-cache-results.md`](docs/content/guide/prompt-cache-results.md).
  Do not repeat live experiments as setup work.
- **Metrics:** missing/partial usage remains unpriced. Historical sessions cannot
  recover missing prices or worker links; cache-discount billing remains deferred.
- **TUI extraction:** continue only when a feature forces it. Preserve ordering
  around worker completion, synthesis, approvals, checkpoints, mining, compaction,
  and turn completion.

## Validation habit

Use scripted models and isolated test homes. Run `cargo test` and warning-clean
Clippy for implementation changes, plus meaningful PTY checks for affected UI.
Test the test: deliberately break the behavior a new guard claims to verify and
confirm it fails. Tool success or a vacuous validation is not proof of completion.
