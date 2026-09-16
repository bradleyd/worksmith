# What to do next

Updated 2026-09-16 after the turn-grouping slice.
This is the short operational list;
`PLAN.md` holds the broader roadmap and `LOOSE_ENDS.md` the forensic notes.
Fresh-start handoff: [`FRESH_START.md`](FRESH_START.md).

## Turn grouping built, on a branch

Branch `turn-grouping` groups the transcript by turn with working / waiting /
done / ended status and elapsed time on the root row, folds turns with Enter,
and hides thinking by default. Tests, Clippy, deliberate guard breaks, and a
scripted PTY smoke at 80 and 40 columns passed; see `PAIR_PLAN.md` "Turn
grouping slice" for decisions, the two ordering races it fixed, and limits.
Not yet done: dogfooding on the local 27B with a real task, then merge and
release. Worker trees and tabs remain later work.

## Checkpoint speaker UI merged

The speaker-label, spacing, and user-border fix from
`codex/checkpoint-conversation-ui` is merged into `main` at `7f2deb6`, unreleased. Automated checks passed (524 tests, Clippy, PTY,
and docs); the user confirmed collapse, navigation while waiting, and the skip
message. See `PAIR_PLAN.md` for evidence and the separate skip-policy/model
comparison follow-ups. No new release or Homebrew update is part of this slice.

## v0.7.0 released

[v0.7.0](https://github.com/bradleyd/worksmith/releases/tag/v0.7.0) is published
from `main` at `57101d3`, with macOS ARM64 and Linux x86-64 musl archives.
Both GitHub release jobs passed. The website guides now cover pairing,
expandable entries, tool timings, and pipeline failures; Pages deployment passed.
The Homebrew tap is updated and pushed at `deea370`, using the SHA-256 of the
downloaded macOS archive. Its binary reports 0.7.0 and formula syntax checks pass.

Fresh release verification: 522 tests passed, zero failed, two opt-in live probes
ignored; all-target Clippy passed with warnings denied. The optimized local
binary passed version, checkpoint discussion/expansion PTY, and web-result JSON
smoke checks. Production and preview site checks passed. The user also manually
tested the changes in `mud-test` and reported that they work.

Release notes are in `RELEASE_NOTES.md` and `CHANGELOG.md`. No release steps
remain pending. The v0.6.0 details below are historical context.

## Historical v0.6.0 baseline

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

## Next: dogfood the turn grouping, then release

The pairing branch now implements request guidance, bounded checkpoint discussion,
expandable checkpoints and tool entries, and recorded tool durations in JSON and
the TUI. Two local MUD runs verified discussion before editing and expansion.
The final review also fixes HTML extraction that discarded article content and
makes empty fetches explicit failures. See [`PAIR_PLAN.md`](PAIR_PLAN.md) for
validation and limits.

Turn grouping is built on `turn-grouping`. What remains is the §6 step 5 work:
run one small real task on the local 27B with pairing off and on, check that a
multi-minute turn's elapsed time and waiting state read correctly, and record
any unnecessary interruptions. Then merge, release, and update the website
guide. Worker trees and tabs remain later work. Resume uses configured pairing
state; persisting live mode changes remains deferred.

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
