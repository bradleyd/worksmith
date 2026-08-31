#!/usr/bin/env python3
"""Prove each snapshot is really a snapshot.

Two assertions per stage:

- it **passes its own task's check** — the state is correct as far as it goes
- it **fails the next task's check** — the state stops where it should

The second is the one that matters. A snapshot that passes the next check is the
finished solution under a stage's name, and seeding a one-shot trial with it
would be handing the model the answer it is being asked for — the same mistake
`--keep-going` made until the taint rule caught it.

Usage: python3 evals/pool/check_snapshots.py
"""
import shutil
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parent
EXP = HERE / "expenses"
SNAPS = EXP / "snapshots"


def run_check(snapshot: Path, validate: str) -> bool:
    with tempfile.TemporaryDirectory() as d:
        d = Path(d)
        for f in list(snapshot.glob("*.py")) + list((EXP / "files").iterdir()):
            shutil.copy(f, d / f.name)
        r = subprocess.run(validate, shell=True, cwd=d, capture_output=True, text=True)
    return r.returncode == 0


def main() -> int:
    tasks = tomllib.loads((EXP / "fine.toml").read_text())["task"]
    dirs = sorted(SNAPS.iterdir())
    if len(dirs) != len(tasks):
        sys.exit(f"{len(dirs)} snapshots for {len(tasks)} tasks")
    bad = []
    for i, (task, snap) in enumerate(zip(tasks, dirs)):
        if not snap.name.endswith(task["id"]):
            bad.append(f"{snap.name}: expected task {task['id']}")
            continue
        if not run_check(snap, task["validate"]):
            bad.append(f"{snap.name}: does NOT pass its own check")
        if i + 1 < len(tasks) and run_check(snap, tasks[i + 1]["validate"]):
            bad.append(f"{snap.name}: already passes the NEXT check "
                       f"({tasks[i + 1]['id']}) — it is not a snapshot, it is a "
                       "solution, and would hand the model the answer")
    for b in bad:
        print("  " + b, file=sys.stderr)
    print(f"\n{len(dirs)} snapshots · {len(bad)} problems")
    return 1 if bad else 0


if __name__ == "__main__":
    raise SystemExit(main())
