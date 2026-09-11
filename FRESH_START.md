# Fresh-start handoff — Worksmith

Updated 2026-09-11 after the pairing/session-trace discussion. Read `AGENTS.md` and
[`NEXT.md`](NEXT.md) first. This handoff recommends the next slice; it is not
authorization to implement every milestone.

## Where we stopped

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

## Next recommended slice: pairing and session trace

Read the September 11 discussion proposal in `PAIR_PLAN.md`. M12 internal role
routing is deferred pending benchmark evidence; workflows and tabs remain later
work. The user authorized the first phase on a new branch. Pairing guidance and
checkpoint discussion handling are implemented on `codex/pairing-guidance`, with
offline validation complete. The trace UI is not implemented.

Next, dogfood the pairing behavior and discuss the working/waiting/finished
terminal mockups before starting the trace. Resume uses current configured
pairing state; persistence of live toggles remains deferred.
Retain mechanical checkpoints, existing worker behavior, and CLI JSON output.
`src/tui/transcript.rs` already exists: extend/extract around the feature instead
of requiring a wholesale `tui.rs` rewrite. No extra model calls for trace labels.

Acceptance includes pairing toggle/compaction/resume coverage, stable selection
while streaming, inspectable failures and decisions, honest available timings,
and meaningful PTY checks. Prompt compliance needs real dogfooding in addition to
scripted tests; a test that proves injection does not prove useful collaboration.

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
