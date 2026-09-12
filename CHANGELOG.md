# Changelog

## 0.7.0 — Unreleased

### Added

- Pairing guidance in model requests and bounded follow-up discussion before
  continuing work; questions remain distinct from decisions.
- Expandable checkpoint discussions and tool call/result entries, with search,
  copying, colored diffs, and stable selection during streaming.
- Optional `elapsed_ms` in recorded/JSON tool results and completed TUI tool rows.

### Fixed

- Calls batched after a blocking checkpoint wait for a fresh model request that
  can incorporate the user's answer.
- Checkpoints no longer duplicate their question as raw tool JSON in the TUI.
- Bash and command validators preserve pipeline failures with `pipefail`.
- HTML extraction preserves headers and prose after skipped scripts and keeps
  UTF-8 offsets valid. Empty readable output is reported as a fetch error.

### Compatibility and limitations

- No new configuration or session migration is required; old tool results may
  lack durations. Measured timings include time waiting inside tool dispatch.
- `pipefail` changes shell exit-status behavior, including upstream SIGPIPE when
  consumers such as `head` close a pipe early. Explicit `|| true` still applies.
- Pairing is guidance plus bounded checkpoints. Resume uses configured pairing
  state; live toggles are not persisted. Turn grouping and worker trees are deferred.

## 0.6.0 — 2026-09-10

### Added

- Session and worker metrics with recorded cost, timing, context, and usage evidence.
- Interactive skill management and improved command overlays.
- Prompt-cache and memory-placement regression checks with documented measurements.
- Parent-only stdio MCP with explicit trust and approval gates, bounded discovery,
  retained results, and protection against replaying uncertain calls.
- Isolated worker Git worktrees with retained results, explicit diff/apply/discard,
  conservative conflict checks, and interruption recovery.
- Shared readable worker reports in the TUI, plain REPL, and CLI, including actual
  validation output and saved logs for successful and failed checks.
- Date-organized session directories with supporting files kept together.
- `worksmith sessions list` and ripgrep-backed `sessions search`, with inclusive
  date/range and project filters; no model or session database index required.

### Fixed

- UTF-8 continuation reads of retained MCP results now accept arbitrary offsets.
- Explicit worker diffs are colored and fully expanded in the TUI.
- Worker subprocess cleanup stops descendants on cancellation and completion.
- Session lookup, resume, memory mining, metrics, worker links, and supporting
  artifact paths understand both dated and legacy session storage.

### Compatibility and limitations

- Existing flat sessions remain readable and are neither moved nor deleted.
- Worktrees require a clean Git checkout; `--shared` explicitly opts out.
- MCP subprocesses and workers are not OS-sandboxed. Use MCP at your own risk.
- Session retention/pruning and OS-enforced sandboxing remain future work.
