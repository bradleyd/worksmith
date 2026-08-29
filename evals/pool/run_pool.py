#!/usr/bin/env python3
"""Run a backlog: small tasks, dispatched by dependency, one fresh agent each.

    python3 evals/pool/run_pool.py evals/pool/expenses/fine.toml
    python3 evals/pool/run_pool.py evals/pool/expenses/*.toml --model ... --json out.json

**Phase 1 needs no Rust.** A disposable worker is just another `worksmith`
process: separate invocation, fresh context, its own `--until`. So the whole
pool — readiness gate, per-task acceptance, blocked dependents — runs here, and
the number that decides the experiment (POOL_PLAN.md §8) can be had before a
line of `worker.rs` changes. The Rust manager is what makes the pool a *feature*
inside worksmith; it is not what makes it measurable.

The three things being measured, per POOL_PLAN.md §4:

- **end-to-end pass** — the backlog's own `validate`, identical across
  granularities, so it cannot tell them apart
- **per-task pass rate**
- **confidently wrong** — the task reported outcome `done` and its check failed
  anyway. The eval's sharpest finding was that all ten raw failures on the 9B
  looked like successes from inside; if small tasks do not convert that into
  caught-and-retried, granularity is not the lever.
"""
import argparse
import json
import shutil
import subprocess
import sys
import tempfile
import time
import tomllib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from run import MANIFEST, REPO, parse_events, worksmith_bin  # noqa: E402


def load(path: Path) -> dict:
    b = tomllib.loads(path.read_text())
    b["dir"] = path.parent
    b["file"] = path.name
    ids = [t["id"] for t in b["task"]]
    if len(set(ids)) != len(ids):
        sys.exit(f"{path}: duplicate task id")
    for t in b["task"]:
        for n in t.get("needs", []):
            if n not in ids:
                sys.exit(f"{path}: {t['id']} needs unknown task {n!r}")
    return b


def setup(backlog: dict) -> Path:
    """One scratch dir for the whole backlog — tasks share a cwd and hand work
    to each other through files, which is the point (POOL_PLAN.md §5)."""
    d = Path(tempfile.mkdtemp(prefix=f"wspool-{backlog['name']}-"))
    ws = d / ".worksmith"
    ws.mkdir(parents=True)
    cfg = REPO / ".worksmith" / "config.toml"
    if cfg.exists():
        shutil.copy(cfg, ws / "config.toml")
    for name in backlog.get("files", []):
        src = backlog["dir"] / "files" / name
        if not src.exists():
            src = backlog["dir"] / name
        shutil.copy(src, d / name)
    return d


def snapshot(d: Path) -> dict:
    """Name -> mtime+size for source files, so a task's output can be named
    rather than guessed at."""
    return {
        p.name: (p.stat().st_mtime_ns, p.stat().st_size)
        for p in d.glob("*.py")
        if p.name != "check.py"
    }


def prompt_for(task: dict, produced: dict) -> str:
    """The task, plus one line per finished dependency naming what it wrote.

    Paths, never payloads (POOL_PLAN.md §5): the file is in the cwd, and a task
    that needs its contents can read it. Passing the text instead would grow
    every prompt with the length of the backlog, which is the exact failure the
    small-task bet is trying to avoid.
    """
    body = task["prompt"].strip()
    lines = [
        f"- `{n}` wrote {', '.join(produced[n])}"
        for n in task.get("needs", [])
        if produced.get(n)
    ]
    if lines:
        body += "\n\nAlready done, in this directory:\n" + "\n".join(lines)
    return body


def run_task(binp: str, task: dict, workdir: Path, produced: dict,
             model: str | None, timeout: int) -> dict:
    cmd = [binp, "--mode", "json", "--approve-all", "--trust-project"]
    if model:
        cmd += ["--model", model]
    if task.get("validate"):
        cmd += ["--until", task["validate"]]
    cmd.append(prompt_for(task, produced))

    before = snapshot(workdir)
    row = {"id": task["id"], "passed": False, "outcome": None, "gen_tokens": 0,
           "tool_calls": 0, "model_calls": 0, "reasoning_tokens": 0,
           "elapsed": 0.0, "wrote": [], "error": None, "confidently_wrong": False}
    t0 = time.time()
    try:
        r = subprocess.run(cmd, cwd=workdir, capture_output=True, text=True,
                           timeout=timeout)
        row.update(parse_events(r.stdout))
        if r.returncode != 0:
            row["error"] = (r.stderr or "").strip()[:200]
    except subprocess.TimeoutExpired:
        row["error"] = f"timeout after {timeout}s"
    row["elapsed"] = round(time.time() - t0, 1)

    after = snapshot(workdir)
    row["wrote"] = sorted(n for n in after if before.get(n) != after.get(n))

    if task.get("validate"):
        v = subprocess.run(["bash", "-lc", task["validate"]], cwd=workdir,
                           capture_output=True, text=True)
        row["passed"] = v.returncode == 0
        if not row["passed"]:
            row["check_output"] = (v.stdout + v.stderr).strip()[-400:]
    else:
        row["passed"] = row["error"] is None

    # The metric the experiment turns on: the model said it was finished and it
    # was not. Everything else here is context for this number.
    row["confidently_wrong"] = row["outcome"] == "done" and not row["passed"]
    return row


def dispatch(backlog: dict, binp: str, model: str | None, timeout: int,
             keep: bool) -> dict:
    """The readiness gate. A task runs when every id in `needs` has passed.

    Deliberately sequential. Tasks share one directory and a backlog does not
    say which file a task writes, so two ready tasks can collide — in
    `medium.toml`, `parse-amount` and `format-cents` are both roots and both
    write `money.py`. Running them at once would clobber one with the other and
    score it as a model failure.

    Making that safe needs each task to declare what it writes (PLAN.md M11's
    collision, from the other end), and it would buy only wall clock, which this
    project spends freely (POOL_PLAN.md §4). Work, then correct, then fast: this
    is the correct one, and it is not the slow part anyway — `fine.toml` is a
    22-deep chain with no parallelism available to leave on the table.
    """
    workdir = setup(backlog)
    tasks = {t["id"]: t for t in backlog["task"]}
    done: set[str] = set()
    produced: dict[str, list[str]] = {}
    rows: list[dict] = []
    blocked: list[str] = []

    remaining = list(tasks)
    while remaining:
        ready = [i for i in remaining
                 if all(n in done for n in tasks[i].get("needs", []))]
        if not ready:
            # Everything left waits on something that failed. Running these
            # would produce a second failure that looks independent and is not.
            blocked = list(remaining)
            for i in blocked:
                print(f"  BLOCK {i}", file=sys.stderr)
            break
        tid = ready[0]
        remaining.remove(tid)
        row = run_task(binp, tasks[tid], workdir, produced, model, timeout)
        rows.append(row)
        if row["passed"]:
            done.add(tid)
            produced[tid] = row["wrote"]
        mark = "PASS " if row["passed"] else "FAIL "
        flag = " CONFIDENTLY-WRONG" if row["confidently_wrong"] else ""
        wrote = f" wrote={','.join(row['wrote'])}" if row["wrote"] else ""
        print(f"  {mark}{tid:<16} {row['elapsed']:>6.1f}s "
              f"gen_tok={row['gen_tokens']:<6} outcome={row['outcome']}"
              f"{wrote}{flag}", file=sys.stderr)

    e2e = subprocess.run(["bash", "-lc", backlog["validate"]], cwd=workdir,
                         capture_output=True, text=True)
    solved = sum(1 for r in rows if r["passed"])
    total_tok = sum(r["gen_tokens"] for r in rows)
    result = {
        "backlog": backlog["name"],
        "granularity": backlog.get("granularity"),
        "tasks": len(tasks),
        "ran": len(rows),
        "solved": solved,
        "blocked": blocked,
        "confidently_wrong": sum(1 for r in rows if r["confidently_wrong"]),
        "end_to_end": e2e.returncode == 0,
        "gen_tokens": total_tok,
        # The eval reports cost per *solved* task, not total: a loop that spends
        # more and succeeds more is not more expensive per unit of work.
        "gen_tokens_per_solved": round(total_tok / solved) if solved else None,
        "wall_clock": round(sum(r["elapsed"] for r in rows), 1),
        "task_rows": rows,
    }
    if not result["end_to_end"]:
        result["end_to_end_output"] = (e2e.stdout + e2e.stderr).strip()[-600:]

    if keep and not result["end_to_end"]:
        result["workdir"] = str(workdir)
        print(f"  kept {workdir}", file=sys.stderr)
    else:
        shutil.rmtree(workdir, ignore_errors=True)
    return result


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("backlogs", nargs="+", type=Path)
    ap.add_argument("--model")
    ap.add_argument("--timeout", type=int, default=240, help="per task, seconds")
    ap.add_argument("--repeat", type=int, default=1)
    ap.add_argument("--json")
    ap.add_argument("--keep", action="store_true",
                    help="keep the scratch dir when the end-to-end check fails")
    ap.add_argument("--bin", help="run this instead of worksmith. The dispatcher "
                    "is the thing under test here as much as the model is, and a "
                    "stub exercises ordering, blocking and the metrics without "
                    "spending a model run on it (see test_dispatch.py).")
    args = ap.parse_args()

    if args.bin:
        binp = args.bin
    else:
        print("building worksmith…", file=sys.stderr)
        subprocess.run(["cargo", "build", "--quiet", "--manifest-path", str(MANIFEST)],
                       check=True)
        binp = worksmith_bin()

    results = []
    for path in args.backlogs:
        backlog = load(path)
        for i in range(args.repeat):
            tag = backlog["name"] + (f" {i+1}/{args.repeat}" if args.repeat > 1 else "")
            print(f"\n=== {tag} ({len(backlog['task'])} tasks) ===", file=sys.stderr)
            r = dispatch(backlog, binp, args.model, args.timeout, args.keep)
            r["run"] = i
            results.append(r)
            if args.json:
                Path(args.json).write_text(json.dumps(results, indent=2))

    print(f"\n{'backlog':<20} {'tasks':>7} {'e2e':>5} {'conf-wrong':>11} "
          f"{'tok/solved':>11} {'wall':>8}")
    for r in results:
        e2e = "pass" if r["end_to_end"] else "FAIL"
        per = r["gen_tokens_per_solved"]
        print(f"{r['backlog']:<20} {r['solved']:>3}/{r['tasks']:<3} {e2e:>5} "
              f"{r['confidently_wrong']:>11} {per if per else '—':>11} "
              f"{r['wall_clock']:>7}s")
    return 0 if all(r["end_to_end"] for r in results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
