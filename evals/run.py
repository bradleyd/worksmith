#!/usr/bin/env python3
"""Worksmith eval harness.

Runs each task in `tasks/*.toml` under two modes and reports whether the task's
validation command passes afterward:

  raw     — `worksmith --mode json "<goal>"`         (model stops when it decides)
  guided  — `worksmith --mode json --until "<validate>" "<goal>"`
            (the validation-driven loop: re-plan until the check passes)
  workers — `worksmith --mode json spawn -n N "<goal>"`
            (fan out to N workers, parent synthesizes; needs `workers = N` in
            the task, and is *unguided* — workers have no validator, so the
            honest comparison is raw vs workers, not guided vs workers)

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
import time
import tomllib
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
MANIFEST = REPO / "Cargo.toml"
TASKS_DIR = Path(__file__).resolve().parent / "tasks"
FIXTURES_DIR = Path(__file__).resolve().parent / "fixtures"


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
    # Binary/prepared fixtures copied from evals/fixtures/.
    for fx in task.get("fixtures") or []:
        src = FIXTURES_DIR / fx
        dst = d / fx
        dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy(src, dst)
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


def run_one(binp: str, task: dict, mode: str, model: str | None, timeout: int,
            keep: bool = False, worker_model: str | None = None,
            fast: bool = False) -> dict:
    workdir = setup_workdir(task)
    # Unattended and trusted: the approval gate has nobody to ask here, and a
    # refusal would score as a task failure rather than the safety behaviour it
    # is. Real interactive runs prompt instead.
    cmd = [binp, "--mode", "json", "--approve-all"]
    if model:
        cmd += ["--model", model]
    if fast:
        cmd.append("--fast")
    if mode == "workers":
        # `spawn` is a subcommand, so its args come after it. The task declares
        # how many workers it decomposes into; a task that doesn't decompose
        # isn't run in this mode at all (see main).
        cmd += ["spawn", "-n", str(task["workers"])]
        if worker_model:
            cmd += ["--worker-model", worker_model]
    elif mode == "guided":
        cmd += ["--until", task["validate"]]
    cmd.append(task["goal"])

    row = {"task": task["name"], "mode": mode, "fast": fast, "passed": False,
           "model_calls": 0, "tool_calls": 0, "gen_tokens": 0, "outcome": None,
           "elapsed": 0.0, "error": None}
    t0 = time.time()
    try:
        r = subprocess.run(cmd, cwd=workdir, capture_output=True, text=True,
                           timeout=timeout)
        row.update(parse_events(r.stdout))
        row["passed"] = validate(workdir, task["validate"])
        # Keep stderr even on success: it carries the planner's account of how
        # it split the work, which is the only explanation of a surprising
        # fan-out. Discarding it on exit 0 hid that for four runs.
        stderr = (r.stderr or "").strip()
        if stderr:
            row["stderr"] = stderr[-1500:]
        if r.returncode != 0 and not row["passed"]:
            row["error"] = stderr[:200]
    except subprocess.TimeoutExpired:
        row["error"] = f"timeout after {timeout}s"
    finally:
        row["elapsed"] = round(time.time() - t0, 1)
        # Keeping the workdir is the difference between diagnosing a failure and
        # guessing at it from token counts.
        if keep and not row["passed"]:
            row["workdir"] = str(workdir)
            print(f"  kept {workdir}", file=sys.stderr)
        else:
            shutil.rmtree(workdir, ignore_errors=True)
    return row


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--task")
    ap.add_argument("--modes", default="raw,guided")
    ap.add_argument("--model")
    ap.add_argument("--timeout", type=int, default=240)
    ap.add_argument("--repeat", type=int, default=1,
                    help="runs per task/mode (averages over model nondeterminism)")
    ap.add_argument("--json")
    ap.add_argument("--fast", action="store_true",
                    help="run with thinking off (--fast); pair with --modes to ask "
                         "whether the loop can substitute for the model's deliberation")
    ap.add_argument("--worker-model",
                    help="model the spawned workers run on (workers mode); the "
                         "synthesis still uses --model")
    ap.add_argument("--dry-run", action="store_true",
                    help="print the command for each run without executing it")
    ap.add_argument("--keep", action="store_true",
                    help="keep the scratch dir of any run that fails, for inspection")
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
            if mode == "workers" and not task.get("workers"):
                print(f"skipping {task['name']} [workers]: no `workers = N` in the task "
                      "(it doesn't decompose)", file=sys.stderr)
                continue
            for i in range(args.repeat):
                tag = f"{task['name']} [{mode}]" + (f" {i+1}/{args.repeat}" if args.repeat > 1 else "")
                if args.dry_run:
                    wd = setup_workdir(task)
                    # Unattended and trusted: the approval gate has nobody to ask here, and a
    # refusal would score as a task failure rather than the safety behaviour it
    # is. Real interactive runs prompt instead.
    cmd = [binp, "--mode", "json", "--approve-all"]
                    if args.model:
                        cmd += ["--model", args.model]
                    if args.fast:
                        cmd.append("--fast")
                    if mode == "workers":
                        cmd += ["spawn", "-n", str(task["workers"])]
                        if args.worker_model:
                            cmd += ["--worker-model", args.worker_model]
                    elif mode == "guided":
                        cmd += ["--until", "<validate>"]
                    cmd.append("<goal>")
                    shutil.rmtree(wd, ignore_errors=True)
                    print(f"  {tag}: {' '.join(cmd)}", file=sys.stderr)
                    continue
                print(f"running {tag} …", file=sys.stderr)
                row = run_one(binp, task, mode, args.model, args.timeout, args.keep,
                              args.worker_model, args.fast)
                row["run"] = i
                rows.append(row)
                mark = "PASS" if row["passed"] else "FAIL"
                extra = f" ({row['error']})" if row["error"] else ""
                print(f"  {mark}  {row['elapsed']}s calls={row['tool_calls']} "
                      f"gen_tok={row['gen_tokens']} outcome={row['outcome']}{extra}",
                      file=sys.stderr)
                # Write partial results after each run so a killed run isn't lost.
                if args.json:
                    Path(args.json).write_text(json.dumps(rows, indent=2))

    # Aggregate per (task, mode).
    def agg(task_name, mode):
        rs = [r for r in rows if r["task"] == task_name and r["mode"] == mode]
        n = len(rs)
        passed = sum(1 for r in rs if r["passed"])
        avg_calls = sum(r["tool_calls"] for r in rs) / n if n else 0
        avg_tok = sum(r["gen_tokens"] for r in rs) / n if n else 0
        return passed, n, avg_calls, avg_tok

    print("\n=== results (pass rate per task) ===")
    header = f"{'task':<16}"
    for mode in modes:
        header += f" {mode + ' pass':>12} {mode + ' tok':>10}"
    print(header)
    for task in tasks:
        line = f"{task['name']:<16}"
        for mode in modes:
            passed, n, _calls, tok = agg(task["name"], mode)
            if n == 0:
                line += f" {'—':>12} {'—':>10}"
            else:
                line += f" {f'{passed}/{n}':>12} {tok:>10.0f}"
        print(line)

    print("\n=== summary ===")
    for mode in modes:
        mr = [r for r in rows if r["mode"] == mode]
        if not mr:
            continue
        passed = sum(1 for r in mr if r["passed"])
        gen = sum(r["gen_tokens"] for r in mr)
        print(f"  {mode:<8} {passed}/{len(mr)} passed   total gen_tokens={gen}")

    if args.json:
        Path(args.json).write_text(json.dumps(rows, indent=2))
        print(f"\nwrote {args.json}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
