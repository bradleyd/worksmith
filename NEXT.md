# What to do next

Updated during the command-output fix on 2026-09-08. `LOOSE_ENDS.md`
keeps the longer forensic notes; this file is the short operational list.

## Current stopping point

`main` has shipped and tagged `v0.5.0`. The recent work landed:

- automatic turn-start memory injection with capped, relevant dynamic memory;
- memory proposal review before durable writes;
- provider presets for first-run `--model openrouter/...` and `openai/...`;
- the first model-call metrics collector and `/metrics` dashboard overlay;
- a manual compaction progress overlay so `/compact` no longer looks frozen;
- current README, quickstart, guide index, and `config.example.toml` updates.

M9 metrics is committed as `c4268f7` and merged into `main`. Current branch:
`codex/command-output-fix`.

Known unrelated local files remain outside this work:

- `evals/results/memory-convention-qwen9b.json`
- `scripts/`

## Checks for this slice

Verified on 2026-09-08: 445 Rust tests, three Python metrics tests,
warning-clean Clippy, the Zola docs build, and `git diff --check` all pass.
A PTY smoke test used a local mock provider to exercise stats/catalog overlays,
keyboard and mouse navigation, filtering, mid-turn skill selection/loading,
repeat-load handling, preview focus, unloading/reloading, streaming, and exit.
Catalog tests also cover loaded markers, external loads, empty filters, and failed
loads. A separate plain-REPL smoke test verified that loading and unloading a skill
change the following model request. The opt-in LAN cache probe also passed; it is
ignored by the normal test suite. Responsive browser tests cover 60×20 through 140×40 terminals, including long
status text and composer isolation. The command-output changes remain uncommitted.

- `cargo test`
- `cargo clippy --all-targets -- -D warnings`
- `python3 -m unittest discover -s evals -p test_metrics.py`
- PTY smoke test for `/metrics`, `/stats`, scrolling, and exit
- `git diff --check`

## 2. M9 metrics, second pass

Merged into main: request-time model/cost/cache records,
shared accounting in `src/metrics.rs`, per-turn history and context attribution,
helper spend, durable worker links, `/stats`, and offline text/JSON reports.
The eval harness reads Worksmith's own combined totals to report dollars per
solved task. The footer accumulates recorded costs across model switches. Usage is normalized
at the provider adapter boundary; cache reads/writes and reasoning have explicit
subset semantics, with coverage for inclusive and disjoint provider counts.

The three accounting review findings are fixed: missing/partial usage stays
unpriced; background helper jobs retain their originating session and late
metrics cannot charge a new footer; eval stats failures no longer skip validation
or overwrite task outcomes. Regression tests cover these cases.

The command-output fix from `COMMAND_OUTPUT_PLAN.md` is implemented on its own
branch: `/stats` and the skill catalog use reference overlays; keyboard/wheel
navigation stays inside them. The skill catalog distinguishes active instructions from discovered catalog
entries. The two-pane browser previews instructions without loading; Tab switches
panes, Enter loads, and `u` unloads without closing the catalog.
`/skill <name>` pins instructions through the
same loader as the model's tool in both frontends. `/skill unload <name>` removes
standing instructions from subsequent requests. The separate 12 KB skill cap has been removed;
active instructions remain subject to the model context window without silent
eviction. The browser now occupies a dedicated screen with wrapped controls and
status, and distinguishes discovered catalog entries from active instructions. Workers inherit independent loaded-skill snapshots. Status messages and errors
for these commands stay out of the streaming transcript. Other commands that
print transcript notices have not been migrated to overlays.

Prompt/cache validation now compares request prefixes, skill lifecycle changes,
wire metadata, memory changes, and compaction. The opt-in synthetic LAN probe
showed ~1.7s first output for repeats versus ~3.3s after changing early memory;
changing tail memory stayed ~1.6s. Cached-token telemetry was unavailable, so this
is not a measured hit rate. The user also confirmed concurrent server load, so
latency comparisons are inconclusive and require an isolated rerun. Production memory placement
is unchanged. See `docs/content/guide/prompt-cache.md` for the reproduction command
and full results. Next: compare task quality with memory near the latest turn
before changing its placement. The skill/browser/cache work remains uncommitted.

Review and dogfood this change before M12. Historical sessions cannot
recover missing prices or worker links; cache-discount billing remains outside
this pass. Failed or interrupted requests with no returned usage are not billed
by this accounting.

Why: the last long dogfood session made the performance problem visible but not
actionable. The TUI now has a `/metrics` overlay and records request timing,
token counts, context size, and an estimated context breakdown. This pass turns that into a diagnostic instrument.

Implemented scope:

- per-turn rows, not only aggregate latest/average numbers;
- session totals for model calls, tool calls, generated tokens, reasoning
  tokens, elapsed model time, and compactions;
- cache data when providers expose it, especially
  `prompt_tokens_details.cached_tokens`;
- cost by model using `[models]` prices, with local providers explicitly free
  unless priced;
- worker metrics included as worker metrics, not mixed into the parent context
  percentage;
- `/stats` or an equivalent non-overlay dump for plain mode and logs;
- JSONL events rich enough that eval scripts stop rebuilding a shadow metrics
  system outside Worksmith.

Success criteria:

- after a long session, the user can answer "what grew the context?", "how fast
  is the provider from Worksmith's point of view?", and "which turns cost the
  most?" without reading server logs;
- the memory teach/test eval can report the same headline numbers from
  Worksmith's own events;
- the metrics display stays out of the transcript and does not block transcript
  scrolling.

## 3. M12 per-role model routing

Do this after the metrics second pass, because routing needs measurement.

The harness already makes model calls that are not the user's main turn:
compaction, memory extraction/classification, fan-out planning, and synthesis or
judging. Today those mostly inherit the session model. That is expensive,
rough on local VRAM, and hard to compare.

First useful shape:

```toml
[roles]
planner = "openrouter/qwen/qwen3.5-9b"
classifier = "openrouter/qwen/qwen3.5-9b"
compactor = "vllm/Qwen/Qwen3.5-9B"
judge = "openrouter/qwen/qwen3.8-27b"
```

Keep this mechanical: call-site role -> configured model -> existing
`client_for` path. Avoid task-kind auto-classification for now.

## 4. Session store cleanup

The real session store has been polluted by test and eval runs, and
`most_recent_for_cwd` still crawls a flat directory. This matters more now that
`/history`, `/metrics <session-id>`, `--resume`, workers, and evals all depend
on sessions.

Cheap first cut:

- make tests and evals default to an isolated `WORKSMITH_HOME`;
- keep real user sessions out of temp/eval storage;
- add enough indexing or directory structure that resume does not parse every
  session file.

## 5. M11 worker worktrees

This is the next large safety/usefulness project, but it is larger than the
metrics and routing work.

Each worker should be able to run in its own git worktree or scratch overlay,
then report a diff for the parent to accept. That fixes fan-out collisions,
makes worker undo real, and lets write-heavy validation run without leaving
failed attempts in the user's live tree.

Do not call it a full security sandbox. It is filesystem isolation and review
semantics first.

## 6. TUI command/run-loop refactor

Keep going here only when a feature forces it. `CommandContext` already pulled
several command families out of the giant TUI file. The remaining command and
run-loop extractions are valuable, but mechanical refactoring is lower leverage
than metrics, routing, and session cleanup right now.

When returning to it, keep each extraction behavior-preserving and test the
branch ordering carefully: worker completions, parent synthesis, approvals,
checkpoints, memory mining, compaction, and turn completion race through the
same loop.

## Habit to keep

Test the test. Nearly every expensive false turn came from a check that passed
for the wrong reason: a scaffold check that accepted implemented code, a
playcheck with vacuous assertions, `spawn --until` accepting a flag but not
running the check, and guard tests that matched the wrong branch. Before
trusting a new validation, break the thing it claims to guard and watch it fail.
