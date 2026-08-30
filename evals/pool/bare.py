#!/usr/bin/env python3
"""Every task in a backlog, one shot each, with no harness at all.

    python3 evals/pool/bare.py omlx/Qwen3.5-4B-MLX-4bit
    python3 evals/pool/bare.py omlx/Qwen3.5-4B-MLX-4bit --backlog medium-inline.toml -n 5

**Why this exists.** Of the first 67 harness-run tasks recorded here, 8 failed —
and 7 of those 8 were ended by a harness policy (the supervisor's repeat-abort,
or the subprocess timeout) rather than by a wrong answer. Both thresholds are
numbers somebody picked. So the chained sweeps largely measure *when worksmith
gives up*, with task granularity as a minor input, and they cannot answer
whether smaller or self-contained tasks are easier for the model itself.

This arm removes every one of those: no tools, no retries, no supervisor, no
validation loop, no timeout. Each task is seeded from the previous task's
snapshot — a known-good starting state — so task 14 can be attempted without
tasks 1-13 having to succeed first, and a failure costs one task instead of
fourteen. Then the model gets one attempt, graded by that task's own check.

It is also cheap enough to repeat: a few hundred generated tokens per task,
seconds each, against one to two hours for a chained sweep. Variance is the
thing that sank the earlier results, and this is the arm that can afford to
average it out.

Pairing this with a harness run over the same tasks separates the two questions
that have been entangled all along: **this** measures whether the task shape
helps the model, and the difference between the two measures what worksmith's
loop adds on top.
"""
import argparse
import json
import re
import shutil
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
from floor import ask, provider_of  # noqa: E402

# USD per million tokens (input, output), for the arms this compares. Local
# models are free to run, which is the entire point of the comparison, so the
# number that matters is what the frontier arm costs to reach the same place.
PRICES = {
    "openrouter/anthropic/claude-sonnet-5": (2.00, 10.00),
    "openrouter/anthropic/claude-sonnet-4.5": (3.00, 15.00),
    "openrouter/qwen/qwen3.5-9b": (0.10, 0.15),
    "openrouter/mistralai/ministral-14b-2512": (0.20, 0.20),
}


def cost_of(model: str, prompt_tok: int, gen_tok: int) -> float:
    """0.0 for anything local — free to run is the claim being tested."""
    pin, pout = PRICES.get(model, (0.0, 0.0))
    return (prompt_tok * pin + gen_tok * pout) / 1e6


PY = re.compile(r"\b(\w+\.py)\b")
FENCE = re.compile(r"```(?:python)?\n(.*?)```", re.S)


def target_file(task: dict) -> str:
    """The file this task is asked to write. Named in every prompt."""
    m = PY.search(task["prompt"])
    return m.group(1) if m else "solution.py"


def build_prompt(task: dict, seed: Path | None, target: str,
                 spec: str | None = None) -> str:
    parts = [task["prompt"].strip()]
    # A task that says "see SPEC.md section 1" is under-specified without it,
    # and the harness runs have the file sitting in the working directory. Not
    # supplying it here would measure this script's omission rather than the
    # task's shape — and nine of fine.toml's twenty-two tasks point at it.
    if spec and "SPEC" in task["prompt"]:
        parts.append(f"--- SPEC.md ---\n{spec.rstrip()}")
    if seed:
        files = sorted(p for p in seed.glob("*.py"))
        if files:
            parts.append("The directory currently contains these files:")
            for f in files:
                parts.append(f"--- {f.name} ---\n{f.read_text().rstrip()}")
    parts.append(f"Reply with the complete contents of {target} after your "
                 "change, as one Python code block and nothing else.")
    return "\n\n".join(parts)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("model")
    ap.add_argument("--backlog", default="fine.toml")
    ap.add_argument("-n", type=int, default=3, help="attempts per task")
    # 3000 was not enough once SPEC.md is in the prompt: the model answers at
    # length and both attempts at pa-reject were cut off mid-function. A
    # truncated answer is reported as truncated rather than wrong, but it is
    # still a wasted call.
    ap.add_argument("--max-tokens", type=int, default=6000)
    ap.add_argument("--think", action="store_true")
    ap.add_argument("--json")
    ap.add_argument("--task", help="only this task id")
    args = ap.parse_args()

    exp = HERE / "expenses"
    backlog = tomllib.loads((exp / args.backlog).read_text())
    tasks = backlog["task"]
    if args.task:
        tasks = [t for t in tasks if t["id"] == args.task] or sys.exit("no such task")
    snaps = sorted((exp / "snapshots").iterdir()) if (exp / "snapshots").is_dir() else []
    by_id = {d.name.split("-", 1)[1]: d for d in snaps}

    spec_path = exp / backlog.get("spec", "SPEC.md")
    spec = spec_path.read_text() if spec_path.exists() else None
    base, key, name = provider_of(args.model)
    rows, solved, attempted = [], 0, 0
    tok_in = tok_out = 0
    for i, task in enumerate(tasks):
        if not task.get("validate"):
            continue
        # A task may name its starting state explicitly (`seed = "08-pa-reject"`),
        # which is what lets the coarser backlogs run here at all: their tasks do
        # not line up one-to-one with fine.toml's stages, so "the previous task's
        # snapshot" is only the right default within fine.toml itself.
        # `seed = ""` means start from an empty directory.
        if "seed" in task:
            seed = (exp / "snapshots" / task["seed"]) if task["seed"] else None
            if seed is not None and not seed.is_dir():
                sys.exit(f"{task['id']}: no snapshot {task['seed']!r}")
        else:
            prev = tasks[i - 1]["id"] if i else None
            seed = by_id.get(prev) if prev else None
            if prev and seed is None:
                print(f"  SKIP {task['id']}: no snapshot for {prev}", file=sys.stderr)
                continue
        target = target_file(task)
        prompt = build_prompt(task, seed, target, spec)
        passes = truncated = 0
        for _ in range(args.n):
            text, usage, finish = ask(base, key, name, prompt, args.max_tokens,
                                      args.think)
            tok_in += usage.get("prompt_tokens", 0) or 0
            tok_out += usage.get("completion_tokens", 0) or 0
            if finish == "length":
                truncated += 1
                continue
            m = FENCE.search(text)
            code = m.group(1) if m else text
            with tempfile.TemporaryDirectory() as d:
                d = Path(d)
                if seed:
                    for f in seed.glob("*.py"):
                        shutil.copy(f, d / f.name)
                for f in (exp / "files").iterdir():
                    shutil.copy(f, d / f.name)
                (d / target).write_text(code)
                r = subprocess.run(task["validate"], shell=True, cwd=d,
                                   capture_output=True, text=True)
            passes += r.returncode == 0
        graded = args.n - truncated
        attempted += graded
        solved += passes
        rows.append({"id": task["id"], "passes": passes, "graded": graded,
                     "truncated": truncated,
                     # Consistency, not just the mean: a task that passes every
                     # attempt is usable in a pipeline; one that passes three of
                     # five is a coin flip wearing a percentage.
                     "reliable": graded > 0 and passes == graded,
                     "flaky": 0 < passes < graded})
        flag = f"  ({truncated} truncated)" if truncated else ""
        print(f"  {task['id']:<16} {passes}/{graded}{flag}", file=sys.stderr)

    pct = 100 * solved / attempted if attempted else 0
    rel = sum(1 for r in rows if r["reliable"])
    flaky = sum(1 for r in rows if r["flaky"])
    never = sum(1 for r in rows if r["graded"] and r["passes"] == 0)
    cost = cost_of(args.model, tok_in, tok_out)
    print(f"\n{args.model} · {args.backlog} · bare, {args.n} attempts each")
    print(f"{solved}/{attempted} attempts passed ({pct:.0f}%) "
          f"across {len(rows)} tasks")
    print(f"tasks: {rel} always pass · {flaky} flaky · {never} never pass")
    print(f"tokens: {tok_in:,} in / {tok_out:,} out"
          + (f" · ${cost:.4f}" if cost else " · local, no API cost"))
    if args.json:
        Path(args.json).write_text(json.dumps(
            {"model": args.model, "backlog": args.backlog, "n": args.n,
             "solved": solved, "attempted": attempted,
             "always_pass": rel, "flaky": flaky, "never_pass": never,
             "prompt_tokens": tok_in, "completion_tokens": tok_out,
             "usd": round(cost, 4), "tasks": rows}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
