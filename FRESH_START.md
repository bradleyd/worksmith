# Fresh-start handoff — Worksmith

Updated after the pairing branch review. Read `AGENTS.md` and
[`NEXT.md`](NEXT.md) first. This handoff recommends the next slice; it is not
authorization to implement every milestone.

## Checkpoint speaker UI ready

`codex/checkpoint-conversation-ui` contains the unreleased speaker-label,
spacing, and user-border fix. Automated checks passed (524 tests, Clippy, PTY,
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

- v0.6.0 is released from `main` at `8220028`; the commit and annotated tag are
  pushed. GitHub published macOS ARM64 and Linux x86-64 binaries with
  `RELEASE_NOTES.md`. Version selection and tagging are complete.
- `~/Projects/homebrew-worksmith` is updated to v0.6.0 and pushed at `d93e1d9`.
  The release archive checksum, formula Ruby syntax, and binary version were
  verified.
- Parent-only MCP, the first M11 worker-worktree slice, shared worker validation
  reports, and dated session storage are shipped. Do not restart their old
  implementation plans.
- Release QA: 500 tests passed, two live probes ignored; all-targets Clippy with
  warnings denied passed. Local build/version checks and both release jobs passed.
  Earlier CLI/REPL/TUI smoke checks covered sessions, search, worker metrics,
  and validation output. These are the baseline results, not a new test run for
  this documentation refresh.

Check current branch/status before editing and preserve any local changes. The
old references to uncommitted MCP planning files and pending release authorization
are obsolete; Git status is the source of truth.

## Next recommended slice: turn grouping and session status

The pairing branch implements guidance, bounded follow-up discussion, expandable
checkpoints/tool entries, and `elapsed_ms` in recorded tool results and the TUI.
Two local MUD runs exercised pairing. Branch review also fixed web extraction of
`<header>`/adjacent scripts and explicit failure for empty readable output.
Read `PAIR_PLAN.md` for validation; these changes are merged and released.

Next, use a fresh branch for turn grouping and working/waiting/finished status.
Discuss the mockups before implementation. Worker trees, tabs, model-role routing,
and workflows remain deferred. Both local and remote worker models can use the
existing provider configuration and spawn controls. Resume still uses configured
pairing state; no persistence of live toggles was added.

Keep UI state separate from session evidence and model context. Continue small
extractions from `tui.rs`; do not require a wholesale rewrite. Scripted long-session
checks exercise UI mechanics but do not establish live-model quality.

## Shipped behavior to preserve

- MCP is parent-only stdio, with compact discovery, selected schemas, scoped
  launch/tool approvals, bounded results, and deterministic request snapshots.
  Activation is not permission. Never replay an uncertain external mutation.
  See `docs/mcp.md` and `MCP_PLAN.md`.
- Workers use clean-commit detached worktrees by default, with retained results,
  explicit guarded apply/discard, and recovery. `--shared` is explicit. Spawn
  remains user initiated; do not introduce autonomous delegation in this slice.
  See `docs/worktrees.md` and `WORKTREE_PLAN.md`.
- Worktrees and approval gates are not an OS sandbox. Sandboxing is tracked in
  https://github.com/bradleyd/worksmith/issues/2. Worker MCP, dirty snapshots,
  non-Git copies, and automatic merging remain deferred.
- Sessions live under `~/.worksmith/sessions/YYYY/MM/DD/<id>/` using UTC creation
  dates. Each contains `transcript.jsonl`, `meta.json`, and supporting artifacts
  such as `checks/` and `mcp-results/`. Resume keeps the original directory.
  Legacy flat sessions remain readable; listing and ID resolution share one
  traversal, and content search uses ripgrep. No SQLite session index, silent
  migration, pruning, or automatic retention. See `docs/sessions.md`.
- Human-facing worker inspection includes complete results and validation
  evidence; model-facing summaries remain bounded. Preserve retained check logs.
- Metrics keep missing/partial usage unpriced and helper spend attached to its
  originating session. Production memory placement remains unchanged; do not
  repeat prior live cache experiments as setup work.

## Working preferences and validation

Keep the Rust core minimal. Follow repository instructions and load the idiomatic
Rust skill before Rust work; last known location:
`~/.worksmith/skills/idiomatic-rust/SKILL.md`. Locate it if unavailable. No subagents
unless asked. Avoid broad refactors and whole-repository formatting churn.

Use scripted models and local fixtures; session/global-memory tests must call
`common::isolate_home()`. Implementation work requires `cargo test`, warning-clean
Clippy, and meaningful PTY checks when UI behavior changes. New tests should fail
when the behavior they guard is deliberately broken. No live provider call is
needed for deterministic request/trace tests; do not use the local B70 for setup
testing.

## Suggested opening prompt

Read FRESH_START.md, NEXT.md, and the current proposal in PAIR_PLAN.md. Discuss
pairing instructions and an expandable session trace before implementation.
Resolve the proposed interaction defaults, question-versus-resume behavior, and
mode persistence. Keep changes focused, preserve existing checks and accounting,
and defer role routing, workflows, tabs, and unrelated refactors.
