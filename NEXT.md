# What to do next

Updated 2026-11-08 after the v0.8.0 release.
This is the short operational list;
`PLAN.md` holds the broader roadmap and `LOOSE_ENDS.md` the forensic notes.
Fresh-start handoff: [`FRESH_START.md`](FRESH_START.md).

## v0.8.0 released

[v0.8.0](https://github.com/bradleyd/worksmith/releases/tag/v0.8.0) is published
from `main` at `b3401ad`, with macOS ARM64 and Linux x86-64 musl archives. The
Homebrew tap is updated and pushed at `54a0ea5`, using the SHA-256 of the
downloaded macOS archive (`2479d6f0…086e`); that binary reports
`worksmith 0.8.0 (b3401adbf)`.

It ships the turn grouping and checkpoint speaker work that was waiting on
`main`, plus a round driven by a two-day 27B session building a POS app:
`max-steps` as a progress check (only successful in-project edits count, a
mid-turn steer stops the next extension, 4 stretches at most), a 150k default
turn token budget, a one-key continue at the step limit, effort levels backed
by an enforced `thinking_token_budget`, mid-stream connection drops retried
with 2/4/8/16s backoff, a 15% word-overlap gate on turn memory, and a warning
plus a `knowledge` refusal when launched in a folder of repos. Full list in
`CHANGELOG.md`; release text in `RELEASE_NOTES.md`.

**The tag moved once.** The first `v0.8.0` push was at `1ca2ad1`. Its macOS job
was cancelled before any step ran: GitHub retired the `macos-14` runner
("The configuration 'macos-14-arm64-us-default' is not supported"), so the
release went out with only the Linux archive. `b3401ad` switches the job to
`macos-latest`; the partial release and the tag were deleted and `v0.8.0` was
re-pushed at `b3401ad`, where both jobs passed. The Linux job still pins
`ubuntu-22.04`, which will retire the same way.

Release verification: 549 tests passed, zero failed, two opt-in live probes
ignored; all-target Clippy passed with warnings denied; `cargo fmt --check`
clean. Live checks against the vLLM server (Qwen 3.8 27B): the
`thinking_token_budget` cut reasoning at exactly 300, 1,000 and 2,500 tokens,
and `/think low` at `max-tokens` 8192 stopped at 1,023 and answered. The
website guides were updated after the release (`be9cf80`), with a tested
recipes page (`755c828`).

## Historical v0.7.0 baseline

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

## Next

- **Failure signatures and eval rounds (branch `failure-signatures`).**
  `evals/failures.py` finds failed turns and reads their cause from recorded
  events; `evals/failure_labels.py` has a model label the behaviour, three
  votes each (labels flipped run to run at temperature 0 on one sample).
  `evals/run.py` now keeps every attempt's trace and verdict, runs a private
  copy of the binary (a `cargo test` once swapped it under a live run), limits
  attempts by token budget with wall-clock only as a safety net, and splits
  tasks into short and long. Two long coding tasks were added as the first
  variants of families (`unfamiliar-library`, `compaction-refactor`), so later
  variants can be held out as Self-Harness (arXiv 2606.09498) requires.

  Round 2 results (pass counts of 6, fixed build):

  | Task | 27B vLLM | 9B OpenRouter, thinking | 9B OpenRouter, `--fast` | 9B local oMLX |
  |---|---|---|---|---|
  | unfamiliar-library | 6 | 0 | 5 | 6 |
  | compaction-refactor | 3 | not run | 0 | 2 |
  | docx-styling | 3 | not run | 3 | 2 |

  The OpenRouter 9B's 0/6 was the provider ignoring the reasoning budget (24
  of 587 calls past 2,000, up to 8,329); oMLX enforces it exactly and the same
  model went 6/6. Settings depend on provider and model, not model alone. The
  local 9B's oMLX losses were mostly the 900s clock (7 of 10), which is why the
  limits are now tokens.

  Harness changes found, each tied to a measured failure:
  - Done: no rescue of tool calls from cut-off or brainstorming replies (16
    of the OpenRouter 9B's 17 "stuck" failures); warn once when a provider
    ignores the reasoning budget.
  - Next: cap the reasoning budget to the output room actually left (27B
    compaction replies squeezed to 256-1,448 tokens, all reasoning).
  - Decide: a hard refusal ends the turn; returning an error instead would
    have saved 3 of the 27B's 6 docx-styling attempts (`cd /tmp/x && rm -rf *`).
  - Try: a missing-argument error that shows a filled-in example (the
    `--fast` 9B sent the same argument-less edit five times, 6 of 6).
  - Later: per-model settings keyed by `provider/model` (#2), and harder
    variants of unfamiliar-library (the 27B passed 6/6, so it cannot tell
    harness changes apart there).
- **Mark mid-turn steering as a redirect: tested, rejected (2026-11-14).** The
  worry was that a mid-turn message, as the latest user message, gets
  re-answered every step. All recorded sessions held 55 mid-turn messages;
  of the 252 replies with text that followed them, 23 (9%) opened with an
  acknowledgement, and the normal shape is one acknowledgement then work. Only
  the POS "spinning in circles" message repeated (5 of 7 replies), from a model
  already circling deep into a compacted session. Not a pattern, so nothing
  built and no probe run.
- **Unpin the Linux release runner** before `ubuntu-22.04` is retired.

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
- **Remote MCP (HTTP transport + OAuth):** not supported; only local stdio
  servers are. Asked for `https://mcp.privateemail.com/mcp`, which needs OAuth
  via `id.service.spaceship.com` (dynamic client registration and PKCE). Today's
  route is the third-party `mcp-remote` bridge as a stdio command, with
  `HOME = "HOME"` in its `env` so it finds its saved token. Native support means
  `rmcp`'s HTTP client transport plus a login, token store and refresh under
  the existing trust and approval gates.
- **MCP coprocesses orphaned on abnormal exit:** the SIGTERM/SIGINT/panic
  handlers (`install_abnormal_exit_handlers`, `src/tui.rs`) restore the terminal
  and call `process::exit`, which skips `Drop` and `shutdown`. Each MCP server
  runs in its own process group (`src/mcp/connection.rs`), so it keeps running.
  Approved fix, not yet built: a process-global registry of MCP pgids, added at
  connection start and removed on close, and the handler `kill(-pgid, SIGKILL)`s
  each before exiting. `kill -9` and OOM stay uncatchable. See
  `docs/decisions/0002-scope-of-the-abnormal-exit-fix.md`.

## Validation habit

Use scripted models and isolated test homes. Run `cargo test` and warning-clean
Clippy for implementation changes, plus meaningful PTY checks for affected UI.
Test the test: deliberately break the behavior a new guard claims to verify and
confirm it fails. Tool success or a vacuous validation is not proof of completion.
