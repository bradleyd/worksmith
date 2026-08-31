#!/usr/bin/env python3
"""Read one task's event stream as a timeline of what the loop actually did.

    python3 evals/pool/trace.py traces/pa-reject.jsonl

Answers the questions that counters cannot, and that cost three wrong
diagnoses in one day:

- which tool did it reach for, with what, and did it work
- **did the result change from the last time it ran the same thing** — the
  difference between checking your work after an edit, which is correct, and
  thrashing, which is not, and the current stuck detector cannot tell them apart
- what did the validation check say, and did that change either
- what actually ended the turn

Repeated calls are marked `=` when the result is byte-identical to the previous
run of that same call after normalising, and `~` when it differs. A column of
`=` is a loop; alternating `~` is a model making progress.
"""
import json
import re
import sys
from pathlib import Path

TMP = re.compile(r"(/private)?/(var/folders|tmp)/[^\s\"']+")
LINE = re.compile(r"\bline \d+")
ADDR = re.compile(r"0x[0-9a-fA-F]+")


def normalise(text: str) -> str:
    """Mirrors supervisor::normalise_check. A traceback carries the scratch dir
    and a line number that moves whenever the model edits above it, so a raw
    comparison stops matching exactly when the model is editing."""
    for pat, sub in ((TMP, "<tmp>"), (LINE, "line <n>"), (ADDR, "<addr>")):
        text = pat.sub(sub, text)
    return " ".join(text.split())


def one_line(s: str, n: int = 68) -> str:
    s = " ".join((s or "").split())
    return s if len(s) <= n else s[: n - 1] + "…"


def main() -> int:
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    events = []
    for line in Path(sys.argv[1]).read_text().splitlines():
        line = line.strip()
        if line:
            try:
                events.append(json.loads(line))
            except json.JSONDecodeError:
                pass

    step = 0
    pending: dict[str, str] = {}      # call id -> "name args"
    last_result: dict[str, str] = {}  # call signature -> normalised output
    last_check = None
    calls = repeats = 0

    for e in events:
        t = e.get("type")
        if t == "model_call_started":
            step += 1
        elif t == "tool_call":
            sig = f"{e.get('name')}::{e.get('arguments')}"
            pending[e.get("id")] = sig
            calls += 1
            print(f"{step:>3}  {e.get('name'):<7} {one_line(e.get('arguments'))}")
        elif t == "tool_result":
            sig = pending.pop(e.get("id"), "?")
            out = normalise(e.get("output", ""))
            mark = " "
            if sig in last_result:
                mark = "=" if last_result[sig] == out else "~"
                repeats += mark == "="
            last_result[sig] = out
            ok = "ok " if e.get("ok", True) else "ERR"
            print(f"     {mark} {ok} {one_line(e.get('output'), 62)}")
        elif t == "validation":
            sig = normalise(e.get("detail", ""))
            mark = "=" if sig == last_check else "~"
            last_check = sig
            state = "PASS" if e.get("ok") else "FAIL"
            print(f"{step:>3}  CHECK {state} {mark} {one_line(e.get('detail'), 56)}")
        elif t == "nudge":
            print(f"{step:>3}  NUDGE  {one_line(e.get('reason'))}")
        elif t == "turn_complete":
            print(f"\nended: {e.get('outcome')}")

    print(f"{step} model calls, {calls} tool calls, "
          f"{repeats} of them returning a byte-identical result to a previous "
          f"run of the same call")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
