#!/usr/bin/env python3
"""Drive the dispatcher with stub agents, so its own logic is tested without
spending a model run on it.

Three stubs, three questions:

- **perfect** — writes the reference solution. Does a clean backlog run
  end to end, in dependency order, with every check passing?
- **idle** — writes nothing and reports `done`. Does a failure block its
  dependents instead of running them on a broken dependency, and is the
  "declared success, check disagrees" case counted?
- **late** — writes the reference only from the third task on. Does an early
  failure stop the run even though later work would have succeeded?

Usage: python3 evals/pool/test_dispatch.py
"""
import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
REF = HERE / "expenses" / "reference"

STUB = '''#!/usr/bin/env python3
import json, os, shutil, sys, time
from pathlib import Path
ref, mode, state = Path(os.environ["STUB_REF"]), os.environ["STUB_MODE"], Path(os.environ["STUB_STATE"])
n = int(state.read_text()) if state.exists() else 0
state.write_text(str(n + 1))
if mode == "perfect" or (mode == "late" and n >= 2):
    for f in ref.glob("*.py"):
        shutil.copy(f, Path.cwd() / f.name)
print(json.dumps({"type": "usage", "completion_tokens": 100, "prompt_tokens": 500}))
print(json.dumps({"type": "tool_call", "name": "write"}))
print(json.dumps({"type": "turn_complete", "outcome": "done"}))
if mode == "slow":
    sys.stdout.flush()
    time.sleep(30)
'''

FAILURES = []


def check(label, got, want):
    if got != want:
        FAILURES.append(f"{label}: want {want!r}, got {got!r}")


def run(mode: str, backlog: str, *extra: str) -> dict:
    with tempfile.TemporaryDirectory() as d:
        d = Path(d)
        stub = d / "stub.py"
        stub.write_text(STUB)
        stub.chmod(stub.stat().st_mode | stat.S_IEXEC)
        out = d / "out.json"
        env = {**os.environ, "STUB_REF": str(REF), "STUB_MODE": mode,
               "STUB_STATE": str(d / "n")}
        r = subprocess.run(
            [sys.executable, str(HERE / "run_pool.py"),
             str(HERE / "expenses" / backlog), "--bin", str(stub),
             "--json", str(out), *extra],
            capture_output=True, text=True, env=env,
        )
        if not out.exists():
            FAILURES.append(f"{mode}/{backlog}: no results\n{r.stderr[-800:]}")
            return {}
        return json.loads(out.read_text())[0]


# A perfect agent: every task passes, in order, and the shared check agrees.
for backlog, n in (("coarse.toml", 3), ("medium.toml", 8), ("fine.toml", 22)):
    res = run("perfect", backlog)
    check(f"perfect/{backlog} solved", res.get("solved"), n)
    check(f"perfect/{backlog} end-to-end", res.get("end_to_end"), True)
    check(f"perfect/{backlog} blocked", res.get("blocked"), [])
    check(f"perfect/{backlog} confidently-wrong", res.get("confidently_wrong"), 0)

# Ran in dependency order: nothing appears before something it needs.
res = run("perfect", "medium.toml")
order = [r["id"] for r in res["task_rows"]]
check("dependency order", order.index("parse-line") > order.index("parse-amount"), True)
check("dependency order", order.index("cli") > order.index("format-report"), True)
# Output is attributed to the task that produced it, so a dependent can be told.
check("wrote attributed", "money.py" in res["task_rows"][0]["wrote"], True)

# An agent that declares success and writes nothing: one task runs, fails its
# own check, and everything downstream is blocked rather than attempted.
res = run("idle", "fine.toml")
check("idle ran", res.get("ran"), 1)
check("idle solved", res.get("solved"), 0)
check("idle blocked", len(res.get("blocked", [])), 21)
check("idle confidently-wrong", res.get("confidently_wrong"), 1)
check("idle end-to-end", res.get("end_to_end"), False)

# Work that would have succeeded later does not rescue an early failure.
res = run("late", "coarse.toml")
check("late ran", res.get("ran"), 1)
check("late blocked", len(res.get("blocked", [])), 2)

# --keep-going: a failure stops being a wall, so a pass rate covers the whole
# backlog instead of reporting time-to-first-obstacle.
res = run("idle", "fine.toml", "--keep-going")
check("keep-going attempted all", res.get("ran"), 22)
check("keep-going blocked none", res.get("blocked"), [])
check("keep-going solved none", res.get("solved"), 0)
check("keep-going repaired once", res.get("repaired"), ["fc-whole"])
# The reference implements the whole backlog, so every later check would pass
# for free. Scoring them would invent 21 successes for an agent that wrote
# nothing — the exact number this rule exists to refuse.
check("keep-going scores only the untainted", res.get("scored"), 1)
check("keep-going taints the rest", len(res.get("tainted", [])), 21)
# Every dependency was handed over by the harness, so the shared check would be
# grading the reference solution. Reporting it as a pass would be a lie.
check("keep-going voids end-to-end", res.get("end_to_end"), None)

# An agent that starts working from the third task on: the first two are
# repaired past, the rest are its own.
res = run("late", "fine.toml", "--keep-going")
check("late keep-going attempted all", res.get("ran"), 22)
check("late keep-going repaired the first", res.get("repaired"), ["fc-whole"])
check("late keep-going scores only up to the repair", res.get("scored"), 1)

# A task killed by the clock must still report the work it did. Dropping the
# partial output makes a slow task look identical to a dead one, which is how
# three sweeps got misdiagnosed as provider stalls.
res = run("slow", "coarse.toml", "--timeout", "2")
row = (res.get("task_rows") or [{}])[0]
check("timeout is flagged", row.get("timed_out"), True)
check("timeout keeps token count", row.get("gen_tokens"), 100)
check("timeout keeps tool calls", row.get("tool_calls"), 1)
check("timeout is not confidently-wrong", row.get("confidently_wrong"), False)

if FAILURES:
    print("\n".join(FAILURES), file=sys.stderr)
    print(f"\n{len(FAILURES)} failed", file=sys.stderr)
    sys.exit(1)
print("ok — dispatcher orders, blocks, attributes output, and counts confidently-wrong")
