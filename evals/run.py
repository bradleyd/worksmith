#!/usr/bin/env python3
"""Worksmith eval harness.

Runs each task in `tasks/*.toml` under two modes and reports whether the task's
validation command passes afterward:

  raw     — `worksmith --mode json "<goal>"`         (model stops when it decides)
  guided  — `worksmith --mode json --until "<validate>" "<goal>"`
            (the validation-driven loop: re-plan until the check passes)

Both are judged by the SAME criterion — the harness runs `validate` in the
scratch dir after the agent finishes — so the comparison is fair. The question
this answers: does the guidance layer make a given model succeed more often?

Usage:
  python3 evals/run.py                      # all tasks, both modes
  python3 evals/run.py --task fix-bug       # one task
  python3 evals/run.py --modes guided       # one mode
  python3 evals/run.py --model openrouter/qwen/qwen3-32b
  python3 evals/run.py --timeout 240 --json results.json
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
MANIFEST = REPO / "Cargo.toml"
TASKS_DIR = Path(__file__).resolve().parent / "tasks"


def worksmith_bin() -> str:
    meta = json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--format-version", "1", "--no-deps",
             "--manifest-path", str(MANIFEST)]
        )
    )
    return os.path.join(meta["target_directory"], "debug", "worksmith")


def load_tasks(only: str | None) -> list[dict]:
    tasks = []
    for f in sorted(TASKS_DIR.glob("*.toml")):
        with open(f, "rb") as fh:
            t = tomllib.load(fh)
        if only and t["name"] != only:
            continue
        tasks.append(t)
    return tasks


def setup_workdir(task: dict) -> Path:
    d = Path(tempfile.mkdtemp(prefix=f"wseval-{task['name']}-"))
    # Reuse the repo's provider config so the eval uses the same model.
    ws = d / ".worksmith"
    ws.mkdir(parents=True, exist_ok=True)
    repo_cfg = REPO / ".worksmith" / "config.toml"
    if repo_cfg.exists():
        shutil.copy(repo_cfg, ws / "config.toml")
    for rel, content in (task.get("files") or {}).items():
        p = d / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(content)
    return d


def parse_events(stdout: str) -> dict:
    model_calls = tool_calls = gen_tokens = ctx_peak = 0
    outcome = None
    for line in stdout.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            e = json.loads(line)
        except json.JSONDecodeError:
            continue
        t = e.get("type")
        if t == "usage":
            model_calls += 1
            gen_tokens += e.get("completion_tokens", 0)
            ctx_peak = max(ctx_peak, e.get("prompt_tokens", 0))
        elif t == "tool_call":
            tool_calls += 1
        elif t == "turn_complete":
            outcome = e.get("outcome")
    return {"model_calls": model_calls, "tool_calls": tool_calls,
            "gen_tokens": gen_tokens, "ctx_peak": ctx_peak, "outcome": outcome}


def validate(workdir: Path, cmd: str) -> bool:
    r = subprocess.run(["bash", "-lc", cmd], cwd=workdir,
                       capture_output=True, text=True)
    return r.returncode == 0


def run_one(binp: str, task: dict, mode: str, model: str | None, timeout: int) -> dict:
    workdir = setup_workdir(task)
    cmd = [binp, "--mode", "json"]
    if mode == "guided":
        cmd += ["--until", task["validate"]]
    if model:
        cmd += ["--model", model]
    cmd.append(task["goal"])

    row = {"task": task["name"], "mode": mode, "passed": False,
           "model_calls": 0, "tool_calls": 0, "gen_tokens": 0, "outcome": None,
           "error": None}
    try:
        r = subprocess.run(cmd, cwd=workdir, capture_output=True, text=True,
                           timeout=timeout)
        row.update(parse_events(r.stdout))
        row["passed"] = validate(workdir, task["validate"])
        if r.returncode != 0 and not row["passed"]:
            row["error"] = (r.stderr or "").strip()[:200]
    except subprocess.TimeoutExpired:
        row["error"] = f"timeout after {timeout}s"
    finally:
        shutil.rmtree(workdir, ignore_errors=True)
    return row


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--task")
    ap.add_argument("--modes", default="raw,guided")
    ap.add_argument("--model")
    ap.add_argument("--timeout", type=int, default=240)
    ap.add_argument("--json")
    args = ap.parse_args()

    modes = [m.strip() for m in args.modes.split(",") if m.strip()]
    tasks = load_tasks(args.task)
    if not tasks:
        print("no matching tasks", file=sys.stderr)
        return 1

    print("building worksmith…", file=sys.stderr)
    subprocess.run(["cargo", "build", "--quiet", "--manifest-path", str(MANIFEST)], check=True)
    binp = worksmith_bin()

    rows = []
    for task in tasks:
        for mode in modes:
            print(f"running {task['name']} [{mode}] …", file=sys.stderr)
            row = run_one(binp, task, mode, args.model, args.timeout)
            rows.append(row)
            mark = "PASS" if row["passed"] else "FAIL"
            extra = f" ({row['error']})" if row["error"] else ""
            print(f"  {mark}  calls={row['tool_calls']} gen_tok={row['gen_tokens']} "
                  f"outcome={row['outcome']}{extra}", file=sys.stderr)

    print("\n=== results ===")
    print(f"{'task':<16} {'mode':<8} {'pass':<5} {'model_calls':>11} "
          f"{'tool_calls':>10} {'gen_tokens':>10} {'outcome'}")
    for r in rows:
        print(f"{r['task']:<16} {r['mode']:<8} {('yes' if r['passed'] else 'no'):<5} "
              f"{r['model_calls']:>11} {r['tool_calls']:>10} {r['gen_tokens']:>10} "
              f"{r['outcome'] or ''}")

    print("\n=== summary (pass rate) ===")
    for mode in modes:
        mr = [r for r in rows if r["mode"] == mode]
        passed = sum(1 for r in mr if r["passed"])
        gen = sum(r["gen_tokens"] for r in mr)
        print(f"  {mode:<8} {passed}/{len(mr)} passed   total gen_tokens={gen}")

    if args.json:
        Path(args.json).write_text(json.dumps(rows, indent=2))
        print(f"\nwrote {args.json}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
