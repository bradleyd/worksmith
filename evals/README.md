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
