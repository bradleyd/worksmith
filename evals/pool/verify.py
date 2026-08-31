#!/usr/bin/env python3
"""Run every backlog's checks against a reference implementation.

A backlog is an answer key, and an answer key nobody graded is a way to spend a
day discovering that the 14B was right and the check was wrong. This proves two
things before any model runs:

- every per-task check passes for a correct solution (no check is impossible)
- the dependency graph has no cycle and every `needs` resolves

It does NOT prove a check is strict enough. Nothing can; that is what the runs
are for.

Usage: python3 evals/pool/verify.py [task-dir ...]
"""
import shutil
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parent


def order(tasks):
    """Dependency order, and a hard failure on a cycle or a dangling `needs`."""
    ids = {t["id"] for t in tasks}
    for t in tasks:
        for n in t.get("needs", []):
            if n not in ids:
                sys.exit(f"{t['id']}: needs unknown task {n!r}")
    done, out = set(), []
    while len(out) < len(tasks):
        ready = [t for t in tasks if t["id"] not in done
                 and all(n in done for n in t.get("needs", []))]
        if not ready:
            sys.exit(f"cycle among: {sorted(ids - done)}")
        for t in ready:
            done.add(t["id"])
            out.append(t)
    return out


def check_backlog(path: Path, ref: Path, fixtures: Path) -> bool:
    backlog = tomllib.loads(path.read_text())
    tasks = order(backlog["task"])
    with tempfile.TemporaryDirectory() as d:
        d = Path(d)
        for src in list(ref.glob("*.py")) + list(fixtures.iterdir()):
            shutil.copy(src, d / src.name)
        # Not every fixture has a spec file: the newsletter tasks carry their
        # own criteria, which is the point of that fixture.
        spec = path.parent / backlog.get("spec", "SPEC.md")
        if spec.exists():
            shutil.copy(spec, d)
        ok = True
        for t in tasks:
            r = subprocess.run(t["validate"], shell=True, cwd=d,
                               capture_output=True, text=True)
            if r.returncode != 0:
                ok = False
                print(f"  FAIL {t['id']}\n{r.stdout}{r.stderr}".rstrip())
        r = subprocess.run(backlog["validate"], shell=True, cwd=d,
                           capture_output=True, text=True)
        if r.returncode != 0:
            ok = False
            print(f"  FAIL end-to-end\n{r.stdout}{r.stderr}".rstrip())
    print(f"{'ok  ' if ok else 'FAIL'} {path.parent.name}/{path.name} "
          f"({len(tasks)} tasks)")
    return ok


def main() -> int:
    dirs = [Path(a) for a in sys.argv[1:]] or [
        p for p in sorted(HERE.iterdir()) if (p / "reference").is_dir()
    ]
    good = True
    for d in dirs:
        for backlog in sorted(d.glob("*.toml")):
            good &= check_backlog(backlog, d / "reference", d / "files")
    return 0 if good else 1


if __name__ == "__main__":
    sys.exit(main())
