# Evals

A small, honest measurement harness for the thesis (PLAN.md §0): **does the
guidance layer make a given model succeed more often?**

Each task in `tasks/*.toml` is run in two modes and judged by the same
criterion — the harness runs the task's `validate` command in a scratch dir
after the agent finishes:

- **raw** — `worksmith --mode json "<goal>"` (the model stops when it decides
  it's done)
- **guided** — `worksmith --mode json --until "<validate>" "<goal>"` (the
  validation-driven loop re-plans until the check passes)
- **workers** — `worksmith --mode json spawn -n N "<goal>"` (fan out to N
  workers, parent synthesizes what comes back)

If guidance is doing its job, `guided` should have a higher pass rate than
`raw`, especially on the iterate-until-correct tasks.

`workers` mode is opt-in per task: it only runs where the task declares
`workers = N`, because most of the suite is a single action and fanning that
out measures nothing but overhead. Two things to keep straight when reading
its numbers:

- **It is unguided.** Workers currently run with no validator, so the honest
  comparison is `raw` vs `workers` — both stop when the model says so.
  `guided` vs `workers` moves two variables at once.
- **Its token count includes the workers.** Worker spend never reaches the
  parent's event stream (each runs on its own bus), so `worksmith spawn`
  re-emits the total. Without that a fan-out would score as free.

`--worker-model` runs the workers on a different (usually cheaper) model while
the synthesis stays on `--model`. On one machine, be careful pointing both at
a local server: two resident models can exhaust unified memory. Straddling
(local workers, remote judge) avoids it, as does `--no-synthesis` plus a
second command with the models swapped.

## What config a run uses

Each task runs in a fresh temp directory. Global `~/.worksmith/config.toml`
applies as the base, and the repo's own `.worksmith/config.toml` is copied in as
the project config so the eval uses the same model and provider you develop
against. Set `--model` to override.

Runs pass `--approve-all` and `--trust-project`, because nobody is there to
answer either prompt and the project config in the workdir is one this harness
wrote itself. Real interactive runs prompt for both.

## Comparing thinking levels

`--think LEVEL` runs the suite at a thinking setting: `minimal|low|medium|high`,
a token budget, `on`, or `off`. `--fast` is the same as `--think off`.

```sh
python3 run.py --modes raw --think off  --json off.json
python3 run.py --modes raw --think low  --json low.json
```

Each row records `gen_tokens`, the `reasoning_tokens` inside them, and elapsed
seconds, and the summary prints tokens per *solved* task. That last number is
the one to compare: totals reward doing nothing, and a level that fails more
often looks cheap.

Two things to hold steady while comparing. Effort levels on OpenRouter are a
fraction of `max_tokens` (roughly 20% for `low`, 80% for `high`), so changing
`max-tokens` between runs changes what `low` means. And OpenRouter routes across
backends whose defaults differ, so `--think default` is not one setting.

## Run

```sh
python3 evals/run.py                    # all tasks, both modes
python3 evals/run.py --task fix-bug     # one task
python3 evals/run.py --modes guided     # one mode
python3 evals/run.py --model openrouter/qwen/qwen3-32b
python3 evals/run.py --timeout 240 --json results.json
python3 evals/run.py --modes workers --worker-model openrouter/qwen/qwen3.5-9b
python3 evals/run.py --dry-run              # print commands, run nothing
python3 evals/run.py --keep                 # keep the scratch dir of failures
```

It reuses `<repo>/.worksmith/config.toml` for the provider/model, so make sure
that points at a working endpoint (and `$OPENROUTER_API_KEY` is set).

## Task format (`tasks/*.toml`)

```toml
name = "fix-bug"
description = "..."
goal = "<prompt given to the agent>"
validate = "<shell command; exit 0 = success>"
workers = 3             # optional: run in `workers` mode with N workers

[files]                 # optional: files written into the scratch dir first
"bug.py" = '''
...
'''
```

Keep `validate` dependency-light (bash/grep/python3) so tasks run anywhere.

## Caveats (read before trusting numbers)

- **Small N** — a handful of tasks; treat results as directional, not
  definitive. Add tasks that mirror your real work.
- **Nondeterminism** — models vary run to run; run multiple times before
  concluding a change helped.
- **`guided` sees the check** — that's the point (it validates against it), but
  don't add tasks where the goal text leaks the exact answer.
- This measures *task success*, not code quality.

## Findings so far

**2026-08, newsletter-judge in `workers` mode — it works, and the judge is the
weakest link.** Kimi K3 planning and judging, three deepseek-v4-flash workers
drafting: three complete 1500-word newsletters on logging, CI pipelines, and
message queues, plus a `decision.md` naming a winner with reasons. Every
mechanical rule in the skill's checklist passes. ~$0.05 and about ten minutes.

That is the architecture doing what it claims: cheap models draft in parallel,
a stronger one judges, and a deterministic check gates the result.

**But the judge was wrong both times it was asked.** Run one: "All three drafts
clear the checklist's pass/fail bar … no topic repetition" — two contained a
prose double hyphen and one reused a published topic. Run two: "All three
drafts pass … ~1400-1500 words" — while the checklist requires 1500-2500, so
its own quoted number contradicts the rule it says is satisfied.

**Original finding, run one:** Kimi K3 planning and judging, three
deepseek-v4-flash workers drafting (~$0.05, ~16k generated tokens). The
workers produced three complete 1500-2500 word newsletters and the judge
produced a decision naming a winner with a reasoned rationale. It also wrote:

> "All three drafts clear the checklist's pass/fail bar (all required sections,
> concrete data, working hands-on code, a fair 'when to use it' section, no
> topic repetition, and 1500-2500 words)."

That was false. Two drafts contained a double hyphen — the skill's Critical
Style Rule #1 — and one reused SQLite, already published as Dispatch #3. The
task's own deterministic validator caught all three violations in
milliseconds.

A second lesson landed immediately after, at my own expense: the first version
of this task's validator produced **six false failures out of seven**. It
flagged `rsync --delete` and `-- SQL comment` inside fenced code blocks as
prose-style violations, and called a logging issue a repeat of the
observability issue because the word appeared in its opening paragraph. The
drafts had done nothing wrong. A check that looks authoritative and measures
the wrong thing is worse than no check, because it is believed. Prose rules now
strip code blocks, and topic reuse is judged from the title.

The model being over-trusted here was not a small one. It was a $3/$15-per-Mtok
frontier model doing exactly the job it was asked to do, and it reported a
verdict it had not verified. This is PLAN §0 in miniature: **the model's
judgment is a proposal, the check is the gate** — and it argues that a reviewer
stage should run deterministic rules *first* and use model judgment only for
what rules cannot express (voice, argument quality, whether the alternative is
genuinely simpler).



**2026-08, qwen3.5-9b (OpenRouter), 7 tasks ×3, raw vs `--until` guided:**

```
task                 raw pass    raw tok  guided pass guided tok
create-file               3/3         97          3/3         99
fix-bug                   3/3        190          3/3        219
implement-fib             0/3        238          3/3        397
refactor                  0/3        226          3/3        430
merge-intervals           3/3        429          3/3        492
recipe-constraints        2/3        982          3/3       1112
docx-styling              0/3        185          0/3       1198

raw     11/21 = 52%   gen_tok=7040   tok/solve=640
guided  18/21 = 86%   gen_tok=11841  tok/solve=658
```

**The guidance layer works on a small model: +34 points.** Three details
matter more than the headline:

- **All 10 raw failures had outcome `done`** — not stuck, not out of steps. The
  model declared itself finished and was wrong. That is the thesis (§0/§7a)
  as data: "I'm done" is a proposal, the check is the gate.
- **Cost per *solved* task is flat: 640 → 658 tokens (+3%).** The +68% raw token
  increase is spent almost entirely on work that then succeeds. This is the
  number to publish, not total tokens.
- **`guided` gained no information, only enforcement.** The two tasks that
  flipped (`implement-fib`, `refactor`) both validate with `python3 test.py`,
  and both goals already tell the model to make `test.py` pass. Guided knew
  nothing raw didn't.

The limit is real too: `docx-styling` failed 0/3 in **both** modes, with guided
burning 6× the tokens iterating. Guidance converts "confidently wrong" into
"correct"; it cannot manufacture capability the model lacks.

**2026-08, qwen3.8-27b (OpenRouter), same suite:** raw 21/21, guided
effectively 21/21 (the one guided loss was a since-fixed truncation bug),
guided ~+18% tokens. **No pass-rate gain** — on this suite the 27B self-loops
and self-checks, so the loop only forces what it already does, and the extra
tokens are pure overhead.

Taken together the two runs give the shape of the differentiator: **dead weight
on a capable model, decisive on a weak one, at flat cost per unit of delivered
work.** Which argues for making guidance cheap and default-on for small models
rather than universal.

Not yet measured: the worker/supervisor layer (M6/M7) — `run.py` has no mode
that spawns workers, because there is no non-interactive `worksmith spawn`.
Sub-workers also currently run with **no validator at all** (`worker.rs` passes
`None`), so the loop measured here doesn't exist for them yet.
