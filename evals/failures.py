#!/usr/bin/env python3
"""Mine failed turns from recorded sessions and group them by cause.

    python3 evals/failures.py                     # every session under the home
    python3 evals/failures.py --source interactive --since 2026-10-01
    python3 evals/failures.py --json failures.json   # full records, for stage 2

Stage 1 of the failure-signature idea from Self-Harness (arXiv 2606.09498):
before anyone proposes a harness change, know what actually fails, how often,
and on which model. A failed turn is one whose recorded outcome is not `done`,
or that never recorded an outcome at all (a crash, a kill, a dropped
connection). Its *cause* is read from the events the harness wrote, never
guessed, so the grouping is deterministic and needs no model call. Stage 2
labels the agent behaviour behind each failure; that one needs a model.

Turns that ended `done` but were wrong are invisible here: nothing recorded
says so. Eval runs with a validator are the only place that is knowable.
"""
import argparse
import collections
import datetime
import json
import os
import re
import sys
from pathlib import Path

TEMP = re.compile(r"^(/private)?/(var/folders|tmp)/")

# Harness-authored nudges, so the rest can be counted as the user (or a
# supervisor) stepping in. Mirrors the texts agent.rs emits.
HARNESS_NUDGE = re.compile(
    r"^(You've made the same|validation failed|\d+ more steps|Your last response"
    r"|The validation check|You used every step|You were repeating|You have spent)",
    re.I,
)


def home() -> Path:
    return Path(os.environ.get("WORKSMITH_HOME") or Path.home() / ".worksmith")


def source_of(cwd: str) -> str:
    if TEMP.match(cwd):
        return "eval"
    if "/.worksmith/worktrees/" in cwd:
        return "worker"
    return "interactive"


TRANSPORT = re.compile(r"sending chat completion request|reading stream chunk|connection"
                       r"|stream inter|timed out|sent nothing for", re.I)


def cause_of(outcome: str | None, last_error: str | None = None) -> tuple[str, str]:
    """The terminal cause, as a fixed category plus the recorded detail.

    A turn with no outcome usually died on a request error, and which one
    matters: those are harness or environment failures, not agent behaviour,
    so they must not be handed to stage 2 as if the model chose them."""
    if outcome is None:
        if not last_error:
            return "incomplete", "no outcome and no error recorded (crash or kill)"
        first = last_error.split("\n")[0][:200]
        if TRANSPORT.search(last_error):
            return "request_transport", first
        if re.search(r"HTTP 4\d\d", last_error):
            return "request_rejected", first
        if re.search(r"HTTP 5\d\d|upstream error", last_error, re.I):
            return "request_provider", first
        return "incomplete", first
    o = outcome.strip()
    low = o.lower()
    if low == "aborted":
        return "aborted", "cancelled by the user"
    if low.startswith("hit step limit"):
        return "step_limit", o
    if low.startswith("validation failed"):
        return "validation_exhausted", o.split("\n")[0]
    if low.startswith("blocked"):
        return "blocked", o
    if low.startswith("stuck"):
        detail = o.split(":", 1)[1].strip() if ":" in o else o
        d = detail.lower()
        if d.startswith("repeated"):
            return "repeated_call", detail
        if "empty response" in d:
            return "empty_reply", detail
        if "output budget reasoning" in d:
            return "reasoning_overrun", detail
        if "tokens on this turn" in d:
            return "token_budget", detail
        return "stuck_other", detail
    return "other", o.split("\n")[0]


def turns_of(path: Path) -> tuple[dict, list[dict]]:
    """Split one session into user turns, with what each did and how it ended."""
    session = {"path": str(path), "id": None, "cwd": ""}
    turns: list[dict] = []
    cur = None
    with open(path, errors="ignore") as f:
        for line in f:
            try:
                d = json.loads(line)
            except json.JSONDecodeError:
                continue
            data = d.get("data") or {}
            if d.get("type") == "meta":
                session["id"] = data.get("id")
                session["cwd"] = data.get("cwd", "")
                continue
            if d.get("type") != "event":
                continue
            kind = data.get("type")
            if kind == "user_message":
                cur = {
                    "index": len(turns) + 1,
                    "ts": d.get("ts"),
                    "prompt": (data.get("text") or "").strip(),
                    "outcome": None,
                    "model": None,
                    "steps": 0,
                    "tool_calls": collections.Counter(),
                    "edits": 0,
                    "failed_tools": 0,
                    "completion_tokens": 0,
                    "validations": [],
                    "nudges": [],
                    "steering": [],
                    "checkpoints": 0,
                    "compactions": 0,
                    "last_error": None,
                }
                turns.append(cur)
                continue
            if cur is None:
                continue
            # Steps come from `usage`, which every session has; the model only
            # from `model_metrics`, which sessions before 2026-09 never wrote.
            if kind == "model_metrics":
                cur["model"] = data.get("model") or cur["model"]
            elif kind == "usage":
                cur["steps"] += 1
                cur["completion_tokens"] += data.get("completion_tokens") or 0
            elif kind == "tool_call":
                cur["tool_calls"][data.get("name", "?")] += 1
            elif kind == "tool_result":
                if not data.get("ok", True):
                    cur["failed_tools"] += 1
                elif data.get("name") in ("write", "edit"):
                    cur["edits"] += 1
            elif kind == "validation":
                cur["validations"].append(bool(data.get("ok")))
            elif kind == "nudge":
                reason = data.get("reason", "")
                (cur["nudges"] if HARNESS_NUDGE.search(reason) else cur["steering"]).append(reason)
            elif kind == "checkpoint":
                cur["checkpoints"] += 1
            elif kind == "compaction":
                cur["compactions"] += 1
            elif kind == "error":
                cur["last_error"] = data.get("message") or cur["last_error"]
            elif kind == "turn_complete":
                cur["outcome"] = data.get("outcome")
    return session, turns


def session_files(root: Path) -> list[Path]:
    return sorted(root.glob("**/*.jsonl"))


def mine(root: Path, since: datetime.date | None, sources: set[str] | None,
         project: str | None) -> tuple[list[dict], collections.Counter]:
    failures = []
    totals = collections.Counter()
    for path in session_files(root):
        session, turns = turns_of(path)
        src = source_of(session["cwd"])
        if sources and src not in sources:
            continue
        if project and project not in session["cwd"]:
            continue
        for i, t in enumerate(turns):
            when = datetime.datetime.fromtimestamp(t["ts"] or 0).date()
            if since and when < since:
                continue
            last = i == len(turns) - 1
            # A session's last turn with no outcome may simply still be
            # running; only an earlier one is certainly cut off.
            if t["outcome"] is None and last:
                totals["open"] += 1
                continue
            totals["turns"] += 1
            if t["outcome"] == "done":
                continue
            cause, detail = cause_of(t["outcome"], t["last_error"])
            failures.append({
                "session": session["id"] or path.parent.name,
                "path": str(path),
                "turn": t["index"],
                "date": when.isoformat(),
                "source": src,
                "project": session["cwd"],
                "model": t["model"] or "unknown (before 2026-09)",
                "cause": cause,
                "detail": detail,
                "prompt": t["prompt"][:300],
                "steps": t["steps"],
                "edits": t["edits"],
                "failed_tools": t["failed_tools"],
                "top_tools": t["tool_calls"].most_common(3),
                "completion_tokens": t["completion_tokens"],
                "validations": t["validations"],
                "nudges": len(t["nudges"]),
                "steering": t["steering"],
                "checkpoints": t["checkpoints"],
                "compactions": t["compactions"],
            })
    return failures, totals


def report(failures: list[dict], totals: collections.Counter) -> str:
    out = []
    n = totals["turns"]
    out.append(f"{n} finished turns, {len(failures)} failed "
               f"({100 * len(failures) / max(n, 1):.0f}%); "
               f"{totals['open']} last turns without an outcome skipped as possibly live")
    by_cause = collections.Counter(f["cause"] for f in failures)
    out.append("\nby cause")
    for cause, k in by_cause.most_common():
        group = [f for f in failures if f["cause"] == cause]
        steps = sorted(f["steps"] for f in group)
        no_edit = sum(1 for f in group if f["edits"] == 0)
        steered = sum(1 for f in group if f["steering"])
        dates = sorted(f["date"] for f in group)
        span = dates[0] if dates[0] == dates[-1] else f"{dates[0]}..{dates[-1]}"
        out.append(f"  {k:4d}  {cause:20s} median steps {steps[len(steps) // 2]:3d}  "
                   f"no edits {no_edit:3d}/{k:<3d} stepped in {steered:3d}/{k:<3d} {span}")
    by_model = collections.defaultdict(collections.Counter)
    for f in failures:
        by_model[f["model"]][f["cause"]] += 1
    out.append("\nby model")
    for model, c in sorted(by_model.items(), key=lambda kv: -sum(kv[1].values())):
        top = ", ".join(f"{cause} {k}" for cause, k in c.most_common(4))
        out.append(f"  {sum(c.values()):4d}  {model}: {top}")
    by_source = collections.Counter(f["source"] for f in failures)
    out.append("\nby source  " + ", ".join(f"{s} {k}" for s, k in by_source.most_common()))
    return "\n".join(out)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--root", type=Path, default=home() / "sessions",
                    help="sessions directory (default: $WORKSMITH_HOME/sessions)")
    ap.add_argument("--since", type=datetime.date.fromisoformat,
                    help="only turns on or after this date (YYYY-MM-DD)")
    ap.add_argument("--source", action="append",
                    choices=["interactive", "worker", "eval"],
                    help="only sessions from this source (repeatable)")
    ap.add_argument("--project", help="only sessions whose directory contains this")
    ap.add_argument("--json", type=Path, help="write every failure record here")
    args = ap.parse_args()

    failures, totals = mine(args.root, args.since,
                            set(args.source) if args.source else None, args.project)
    print(report(failures, totals))
    if args.json:
        args.json.write_text(json.dumps(failures, indent=2))
        print(f"\nwrote {len(failures)} records to {args.json}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
