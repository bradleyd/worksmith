# What to do next

Updated after the 0.5.0 release and the documentation catch-up on
2026-09-05. `LOOSE_ENDS.md` keeps the longer forensic notes; this file is the
short operational list.

## Current stopping point

`main` has shipped and tagged `v0.5.0`. The recent work landed:

- automatic turn-start memory injection with capped, relevant dynamic memory;
- memory proposal review before durable writes;
- provider presets for first-run `--model openrouter/...` and `openai/...`;
- the first model-call metrics collector and `/metrics` dashboard overlay;
- a manual compaction progress overlay so `/compact` no longer looks frozen;
- current README, quickstart, guide index, and `config.example.toml` updates.

The current branch is for planning-doc cleanup only. Keep it to `NEXT.md` and
`LOOSE_ENDS.md`.

Known untracked local files that are not part of this branch:

- `evals/results/memory-convention-qwen9b.json`
- `scripts/`

## Checks for this slice

This is a markdown-only branch, so do not run the full Rust suite just to prove
the prose changed.

- `git diff --check`
- scan for stale branch names, old release references, and placeholders
- read the first screen of both files and make sure the next action is clear

## 1. Finish the planning-doc refresh

Bring `NEXT.md` and the top of `LOOSE_ENDS.md` up to date with the shipped
0.5.0 state. Preserve old incident notes where they still explain failures, but
the first page should say what is actually next.

Commit command when ready:

```bash
git add NEXT.md LOOSE_ENDS.md
git diff --cached --check
git commit -m "Refresh planning notes after 0.5.0"
```

## 2. M9 metrics, second pass

This is the next best feature work.

Why: the last long dogfood session made the performance problem visible but not
actionable. The TUI now has a `/metrics` overlay and records request timing,
token counts, context size, and an estimated context breakdown. The next pass
should turn that into a diagnostic instrument.

Target shape:

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
