#!/usr/bin/env python3
"""Put every arm in one table: accuracy, consistency, and cost.

    python3 evals/pool/compare.py results/*.json

Reads both shapes — `bare.py`'s and `run_pool.py`'s — and reports each arm the
same way, because the claim being tested spans them: *is a cheap model plus this
harness as dependable as the expensive one, and what does that cost?*

**Consistency is a separate column from accuracy on purpose.** A task that
passes every attempt can be built on. A task that passes two of three is a coin
flip wearing a percentage, and a mean hides which one you have. That distinction
is not academic here: this suite produced 7/8 and then 2/8 on identical inputs
earlier, and a single run would have reported either as fact.
"""
import json
import sys
from collections import defaultdict
from pathlib import Path

# USD per million tokens, mirroring bare.py. Local models are free to run.
PRICES = {
    "openrouter/anthropic/claude-sonnet-5": (2.00, 10.00),
    "openrouter/qwen/qwen3.5-9b": (0.10, 0.15),
    "openrouter/mistralai/ministral-14b-2512": (0.20, 0.20),
}


def load(path: Path) -> dict | None:
    d = json.loads(path.read_text())
    if isinstance(d, dict) and "tasks" in d:          # bare.py
        return {"arm": f"{d['model']} · bare",
                "passes": {t["id"]: (t["passes"], t["graded"]) for t in d["tasks"]},
                "usd": d.get("usd", 0.0), "wall": None}
    if isinstance(d, list) and d and "task_rows" in d[0]:  # run_pool.py
        per = defaultdict(lambda: [0, 0])
        usd = wall = 0.0
        model = None
        for run in d:
            wall += run.get("wall_clock") or 0
            for r in run["task_rows"]:
                per[r["id"]][1] += 1
                per[r["id"]][0] += bool(r["passed"])
        return {"arm": f"{path.stem} · harness",
                "passes": {k: tuple(v) for k, v in per.items()},
                "usd": usd, "wall": wall}
    return None


def main() -> int:
    arms = [a for a in (load(Path(p)) for p in sys.argv[1:]) if a]
    if not arms:
        sys.exit("no readable result files")
    print(f"{'arm':<44} {'passed':>10} {'always':>7} {'flaky':>6} {'never':>6} "
          f"{'cost':>9} {'wall':>8}")
    for a in arms:
        p = a["passes"]
        got = sum(x for x, _ in p.values())
        tot = sum(y for _, y in p.values())
        always = sum(1 for x, y in p.values() if y and x == y)
        flaky = sum(1 for x, y in p.values() if 0 < x < y)
        never = sum(1 for x, y in p.values() if y and x == 0)
        pct = f"{got}/{tot} {100*got/tot:.0f}%" if tot else "-"
        cost = f"${a['usd']:.4f}" if a["usd"] else "free"
        wall = f"{a['wall']/60:.0f}m" if a["wall"] else "-"
        print(f"{a['arm']:<44} {pct:>10} {always:>7} {flaky:>6} {never:>6} "
              f"{cost:>9} {wall:>8}")
    print("\nalways/flaky/never count TASKS, not attempts: a task that passes "
          "every attempt,\nsome attempts, or none. Accuracy and dependability "
          "are different claims.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
