# Changelog

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
