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

If guidance is doing its job, `guided` should have a higher pass rate than
`raw`, especially on the iterate-until-correct tasks.

## Run

```sh
python3 evals/run.py                    # all tasks, both modes
python3 evals/run.py --task fix-bug     # one task
python3 evals/run.py --modes guided     # one mode
python3 evals/run.py --model openrouter/qwen/qwen3-32b
python3 evals/run.py --timeout 240 --json results.json
```

It reuses `<repo>/.worksmith/config.toml` for the provider/model, so make sure
that points at a working endpoint (and `$OPENROUTER_API_KEY` is set).

## Task format (`tasks/*.toml`)

```toml
name = "fix-bug"
description = "..."
goal = "<prompt given to the agent>"
validate = "<shell command; exit 0 = success>"

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
