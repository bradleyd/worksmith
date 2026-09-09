# Fresh-start handoff — Worksmith

Updated 2026-09-09. Start a new conversation from this file. This is a handoff,
not authorization to implement every milestone in one pass.

## Implementation update

The user subsequently authorized and reviewed parent MCP, then authorized M11
worker worktrees, then the shared human-facing worker result view with saved
validation output. These are implemented; the user authorized committing the
accumulated changes and merging to main. Do not restart the planning-only task below. Read `docs/mcp.md`, `WORKTREE_PLAN.md`, and `docs/worktrees.md` for the
current behavior. The original stopping point below is historical context.

The worktree slice uses clean-commit detached checkouts, explicit shared mode,
retained results, guarded apply, and recovery. Dirty snapshots, automatic merging,
worker MCP, and OS sandboxing remain deferred. Sandboxing design is GitHub issue #2.
CLI, plain REPL, and TUI smoke tests use a local scripted model. Intentional cwd
and apply-guard regressions were tested, caught, and restored. No live provider
or B70 was used. No new release tag is authorized: session storage cleanup must
come first (see NEXT.md). Validation logs now live in per-session `.checks`
directories and must be included in that design.

Final QA: 492 tests passed, two live probes ignored; all-targets Clippy is
warning-clean. Build, CLI/REPL/TUI PTY smoke tests (including validation output
and log retention), and `git diff --check` passed. Earlier test totals below
are historical.

## Where we stopped

- On `main`, matching `origin/main` when checked. M9 metrics, command/skill UI,
  and prompt/cache validation are merged and pushed.
- Uncommitted planning files: `MCP_PLAN.md`, `PLAN.md`, `NEXT.md`, and this file.
  Preserve them; do not discard or treat them as unexpected user changes.
- No production changes in the planning pass. Last full verification: 447 Rust
  tests passed, two live probes ignored, Clippy warning-clean. The later handoff
  edits are Markdown only; those checks are a baseline, not new implementation QA.
- Prompt/cache experiments did not justify moving memory. Leave production memory
  placement unchanged; do not repeat those live experiments as setup work.

## Clarified priorities

The user wants MCP to serve Worksmith's guidance/validation thesis, with small
prompts and a clear UI. They also agree M11 is a priority for routing and safety.
We initially described “M11 before MCP” too broadly and corrected it:

1. Parent-only MCP does **not** need worktrees.
2. M11 isolates worker edits/checks and enables reviewed application of results.
3. MCP in workers waits for M11 and the parent MCP foundation.
4. M12 role routing follows as a product priority, not a technical dependency.

Suggested starting scope: **parent-only MCP**. This is a recommendation for the
new task, not a claim that the user explicitly chose it over M11. If the user
instead starts the new task with M11, follow that choice. Do not silently enable
MCP in today's shared-tree workers. Worktrees are not an OS security sandbox.

## First task: make parent-only MCP concrete

Read `AGENTS.md`, this file, and `MCP_PLAN.md` first. Read only relevant sections
of the longer roadmap. Check branch/status before editing; preserve planning work
and use a feature branch such as `codex/mcp-parent-stdio` for implementation.

Before writing production code, inspect these integration boundaries and produce
a short implementation breakdown:

- `src/tools/mod.rs`: Tool trait, registry order, output caps, ToolContext.
- `src/tools/approval.rs`, `src/trust.rs`, `src/config.rs`: server launch trust,
  tool grants, credentials, unattended behavior. Approval caching currently keys
  on reason strings; MCP must not accidentally grant every tool at once.
- `src/agent.rs`, `src/event.rs`, `src/session.rs`: request snapshots, structural
  events, result ownership, approval waits, and cancellation.
- `src/main.rs`, `src/tui.rs`: shared backend, plain REPL, existing skill browser.
- `src/worker.rs`: explicitly keep MCP delegation disabled in this first slice.

Evaluate a pinned released `rmcp` SDK against a deterministic local stdio fixture.
Verify current primary documentation, supported protocol versions, MSRV, process
ownership, cancellation, schema compatibility, and testability. Avoid speculative
trait hierarchies or broad TUI refactors.

Build one narrow vertical slice: trusted configuration → initialized fixture
server → bounded discovery → selected tool activation → scoped approval → call →
bounded result → recorded outcome → clean shutdown. Keep protocol/manager code
outside `tui.rs`. Cover denial, timeout, process crash, and an uncertain mutation
outcome before treating the happy path as releasable.

Then add the browser and recoverable oversized results against that backend.
Follow the release gates in `MCP_PLAN.md`: connectivity alone is not the release.
Each implementation chunk should have meaningful offline tests and an inspectable
diff. Do not commit or merge unless asked.

## Product constraints to preserve

- Compact discovery and selected native tool schemas; activation is not permission.
- Deterministic request snapshots; browsing and connection status do not alter prompts.
- Clear available/active/connected/authorized distinctions, accessible focus markers,
  wrapped controls/status, and no overlap with the composer.
- Explicitly scoped server launch and operation trust. Server annotations cannot
  authorize calls. No automatic installation or broad inherited credentials.
- No replay of an uncertain external mutation. Cancellation does not undo it.
- Validation proves the task outcome; tool success alone does not.
- Defer remote transports/OAuth, marketplaces, server sampling/elicitation, resource
  catalogs, and worker MCP. Do not build a second agent loop.

## Validation and working preferences

Follow repository instructions and load the idiomatic Rust skill before Rust work;
last known location: `~/.worksmith/skills/idiomatic-rust/SKILL.md`. If unavailable,
locate it rather than silently claiming to have used it. No subagents unless asked.
Keep changes focused and avoid whole-repository formatting churn.

Tests use scripted models and a local fixture server, with no live service required.
Session/global-memory tests must use `common::isolate_home()`. Run `cargo test`,
warning-clean Clippy, and meaningful PTY checks for the finished UI. Do not use the
local B70 for live testing; it is occupied. Use OpenRouter only if a live evaluation
is needed and bounded; the fixture integration does not need an LLM call.

## Suggested opening prompt

Read FRESH_START.md and MCP_PLAN.md. Start with the parent-only MCP stdio slice;
M11 remains a separate priority and is required before worker MCP. Inspect the
existing integration points and turn the first vertical slice into a concrete
implementation plan before editing production code. Preserve the uncommitted
planning files, keep the scope narrow, and do not commit automatically.
