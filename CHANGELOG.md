# Changelog

## Unreleased

### Removed

- `install.sh`. Build from source with `cargo install --path .`, which installs
  to `~/.cargo/bin`. The script's fast path copied the binary into
  `~/.local/bin`, where old copies outlived upgrades and shadowed newer
  installs; check there for leftovers with `which -a worksmith`.

### Documentation

- A configuration recipes page with complete configs for common setups.
  `cargo test` now parses every TOML example in the README and docs with the
  real config loader, and installs every skill example, so a renamed setting
  fails the build instead of a reader's copy.

## 0.8.0 — 2026-11-07

### Added

- The TUI groups the transcript by turn. Each message you send gets a root row
  quoting your text, with the turn's prose, tool activity, and checkpoints
  indented beneath it. The row shows `working` with the time so far,
  `waiting for you` during a checkpoint or approval, `done` with the elapsed
  time, or `ended` with the loop's reason. Enter in transcript navigation folds
  a turn; search opens a folded turn around its match. Labels quote the user;
  no model call produces them. Timing is the local clock around the live turn
  and is never shown for a turn this process did not run.
- A dropped connection partway through a reply is retried instead of ending
  the turn. Waits double from 2s (2, 4, 8, 16) across 5 attempts, about 30s,
  which covers wifi coming back after a laptop wakes. The partial reply comes
  off the screen before the retry, so the answer is not printed twice.
- An effort level (`/think low`, `thinking = "medium"`) also sends an enforced
  reasoning budget when the provider has a `reasoning-budget-param`: `low`
  1024, `medium` 4096, `high` 8192, `xhigh` 16384, capped at three quarters of
  `max-tokens`; `max` stays uncapped. On vLLM, Qwen 3.8 at `low` reasoned to a
  6,000-token cap with no answer; with the budget it stopped at 1,023 tokens
  and answered.
- A startup warning when Worksmith is launched in a folder that holds several
  repos (not a repo itself, two or more children are). Memory, knowledge and
  decisions are keyed by the launch directory and would be shared by all of
  them; the `knowledge` tool refuses to index there.

### Changed

- Model reasoning is hidden in the transcript by default. Ctrl+T shows it; the
  footer's `↻` count is unchanged. Requests and session records are unaffected.
- `max-steps` is a progress check, not a stop. A 50-step stretch that made
  successful edits inside the project earns another, up to 4 per turn. A
  stretch with no project edits, a mid-turn message from you, or the fourth
  stretch ends the turn. Scratch files outside the project and failed edits do
  not count.
- A session turn has a 150,000 completion-token budget by default (the 99th
  percentile of 758 recorded turns was 57k). `token-budget = 0` turns it off.
  The count now covers the whole turn instead of resetting per validation retry.
- The step-limit stop renders on the checkpoint channel with an alert, and an
  empty Enter continues the turn.
- Turn memory needs real overlap: a memory must share 15% of the shorter
  text's words, with common words ignored. One shared word used to be enough,
  and a 200-word brief pulled in unrelated notes on words like "people".
- Harness checkpoints put the evidence first and the question last, so a long
  tool dump cannot push the question off screen.
- `rust-version = "1.89"` is declared in `Cargo.toml`.

### Fixed

- Pairing checkpoint discussions distinguish Assistant, You, and System with
  bold labels and spacing, plus a border on every wrapped user-message line.
  Speaker separation does not rely on color; user text retains literal markup.
  Expansion, search, copying, and checkpoint behavior are preserved.
- The terminal is restored when Worksmith is killed or panics, instead of
  being left in raw mode with mouse capture on.
- Filing a decision outside a git repository no longer prints git's "not a
  git repository" error over the input box.

### Compatibility and limitations

- No configuration or session migration is required. A turn that used to stop
  every 50 steps can now run to 200; set `max-steps` or `token-budget` to bound
  it tighter.
- Qwen 3.8 accepts only `low`, `medium` and `xhigh`; `/think high` is rejected
  by the server with its own message. A provider without
  `reasoning-budget-param` gets the level alone, as before.
- Building needs Rust 1.89 or newer.

## 0.7.0 — 2026-09-11

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
