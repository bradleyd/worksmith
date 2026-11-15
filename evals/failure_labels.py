#!/usr/bin/env python3
"""Label what the agent did in each failed turn, then group the failures.

    python3 evals/failure_labels.py --source interactive --source eval
    python3 evals/failure_labels.py --limit 10 --show    # hand-check a sample
    python3 evals/failure_labels.py --report-only        # regroup cached labels

Stage 2 of the failure-signature idea from Self-Harness (arXiv 2606.09498).
Stage 1 (`failures.py`) finds failed turns and reads their cause from the
recorded events. This stage reads each failed turn's trace, plus the user's
next message (often the best label there is), and asks a model to pick the
agent behaviour behind it from a fixed list. Fixed, because failures are
grouped by exact agreement: a free-text label is a group of one.

Turns that died on a request error are skipped. The agent never got to act,
so asking a model what it did wrong would invent an answer.

Labels are cached in the output file and only missing ones are requested, so
a rerun after a crash, or with a larger --limit, costs only the new turns.
Hand-check a sample before trusting the groups: this is exactly the kind of
judgment small models get wrong.
"""
import argparse
import collections
import datetime
import json
import os
import re
import sys
import tomllib
import urllib.error
import urllib.request
from pathlib import Path

import failures as F

# What the agent did. Order is the order the model sees them in.
BEHAVIOURS = {
    "loop_same_action": "repeated the same or nearly the same action without getting new information",
    # Not "never edited": hand-checked turns with 10-13 edits that spent 30-37
    # of 50 steps reading library source were labelled this, rightly.
    "explore_without_acting": "spent most of its steps reading and searching rather than changing things",
    "unverified_assumption": "acted on a guess about an API, file, command or fact without checking it",
    "stale_state": "worked from an old build, binary, file contents or data that had since changed",
    "ignored_error": "a tool reported an error and the following steps did not deal with it",
    "premature_done": "said the work was finished when it was not",
    "kept_going_after_done": "the work was already finished but it kept testing, checking or changing things",
    "reasoning_too_long": "its hidden reasoning used up the reply's token budget, so no action or answer came out "
                          "(not slow or redundant actions, which are other behaviours)",
    "malformed_output": "produced a broken or cut-off tool call or answer",
    "scope_drift": "worked on something other than what was asked",
    "lost_context": "forgot facts or decisions already established earlier in the session",
    "needed_clarification": "the request was ambiguous and the agent needed the user's input to proceed",
    "user_changed_course": "the user stopped or redirected it to change direction, not because it was failing",
    "other": "none of the above fits",
}
CAUSAL = {
    "caused": "this behaviour is why the turn failed",
    "contributed": "it made things worse, but something else was the main reason",
    "environment": "the failure came from the environment, tools or setup, not the agent",
}
SKIP_CAUSES = {"request_transport", "request_rejected", "request_provider"}

SYSTEM = (
    "You review failed turns of an AI coding agent. Each turn is one request from "
    "the user and the steps the agent took. The turn ended with the recorded cause "
    "shown. Decide which single agent behaviour best explains the failure, and "
    "whether it caused the failure.\n\nBehaviours:\n"
    + "\n".join(f"- {k}: {v}" for k, v in BEHAVIOURS.items())
    + "\n\nCausal status:\n"
    + "\n".join(f"- {k}: {v}" for k, v in CAUSAL.items())
    + "\n\nThe user's next message after the turn, when present, is strong evidence "
    "of what went wrong. Choose user_changed_course only when a message from the "
    "user (mid-turn or next) shows a change of direction; a cancelled turn with no "
    "such message says nothing about why, so judge it by what the agent did. "
    "Quote one short line from the trace as evidence. Reply "
    "with JSON only: "
    '{"behaviour": "...", "causal": "...", "evidence": "...", "note": "one sentence"}'
)

SCHEMA = {
    "type": "object",
    "properties": {
        "behaviour": {"type": "string", "enum": list(BEHAVIOURS)},
        "causal": {"type": "string", "enum": list(CAUSAL)},
        "evidence": {"type": "string"},
        "note": {"type": "string"},
    },
    "required": ["behaviour", "causal", "evidence", "note"],
    "additionalProperties": False,
}


# Chat-template markup quoted from a trace. A failure where the model wrote
# its tool call as text carries a literal <tool_call>; when the labeller
# quotes it back, the server's tool-call parser takes it for a real call and
# the request dies with HTTP 500. A lookalike bracket keeps it readable.
MARKUP = re.compile(r"<(/?)(tool_call|tool_response|think|function|parameter)\b", re.I)


def clip(text: str, n: int) -> str:
    text = MARKUP.sub(r"‹\1\2", " ".join((text or "").split()))
    return text if len(text) <= n else text[: n - 1] + "…"


def excerpt(f: dict, steps: int = 12) -> str:
    """The failed turn as a reviewer needs it: the ask, the whole turn's shape,
    every time someone stepped in, the last steps, how it ended, and what the
    user said next. Interjections are pinned above the tail because a long
    turn buries them: "you are spinning in circles" sat 398 lines back."""
    path, turn, cause, detail = f["path"], f["turn"], f["cause"], f["detail"]
    with open(path, errors="ignore") as fh:
        lines = [json.loads(l) for l in fh if l.strip()]
    starts = [i for i, d in enumerate(lines)
              if d.get("type") == "event" and (d.get("data") or {}).get("type") == "user_message"]
    begin = starts[turn - 1]
    end = starts[turn] if turn < len(starts) else len(lines)
    prompt = lines[begin]["data"].get("text", "")
    next_user = lines[end]["data"].get("text", "") if turn < len(starts) else ""

    trail = []
    for d in lines[begin + 1:end]:
        data = d.get("data") or {}
        if d.get("type") == "message" and data.get("role") == "assistant":
            if (data.get("content") or "").strip():
                trail.append(f"agent said: {clip(data['content'], 300)}")
        elif d.get("type") != "event":
            continue
        elif data.get("type") == "tool_call":
            trail.append(f"called {data.get('name')}: {clip(data.get('arguments', ''), 160)}")
        elif data.get("type") == "tool_result":
            mark = "ok" if data.get("ok", True) else "FAILED"
            first = clip((data.get("output") or "").split("\n")[0], 160)
            trail.append(f"  -> {mark}: {first}")
        elif data.get("type") == "nudge":
            reason = data.get("reason", "")
            who = "harness" if F.HARNESS_NUDGE.search(reason) else "user, mid-turn"
            trail.append(f"[{who}]: {clip(reason, 240)}")
        elif data.get("type") == "validation":
            trail.append(f"[check {'passed' if data.get('ok') else 'failed'}]")
        elif data.get("type") == "compaction":
            trail.append("[history was compacted here]")
        elif data.get("type") == "error":
            trail.append(f"[error]: {clip(data.get('message', ''), 200)}")

    # Tool results trail their calls, so a step is a call line plus its result;
    # keep the tail in whole lines and say how much was cut.
    shown = trail[-steps * 2:]
    cut = len(trail) - len(shown)
    tools = ", ".join(f"{name} {k}" for name, k in f.get("top_tools", []))
    parts = [
        f"USER REQUEST: {clip(prompt, 600)}",
        f"TURN SHAPE: {f.get('steps', 0)} steps, {f.get('edits', 0)} successful edits, "
        f"{f.get('failed_tools', 0)} failed tool calls, most used: {tools or 'none'}, "
        f"{f.get('compactions', 0)} compactions",
    ]
    stepped_in = [line for line in trail if line.startswith("[user, mid-turn]")]
    nudged = [line for line in trail if line.startswith("[harness]")]
    if stepped_in:
        parts.append("THE USER STEPPED IN DURING THE TURN:")
        parts += [f"  {line}" for line in stepped_in]
    if nudged:
        parts.append(f"HARNESS NUDGES DURING THE TURN ({len(nudged)}):")
        parts += [f"  {line}" for line in dict.fromkeys(nudged)][-5:]
    parts.append("LAST STEPS:")
    if cut:
        parts.append(f"({cut} earlier trace lines omitted)")
    parts += shown
    parts.append(f"TURN ENDED: {cause} ({clip(detail, 200)})")
    if f.get("check"):
        parts.append(f"THE TASK'S CHECK FAILED: `{clip(f['check'], 200)}`")
    parts.append(f"USER'S NEXT MESSAGE: {clip(next_user, 500) if next_user else '(none)'}")
    return "\n".join(parts)


def resolve(model_spec: str | None) -> dict:
    """Endpoint for `provider/model`, read from the same config Worksmith uses."""
    home = F.home()
    cfg = tomllib.loads((home / "config.toml").read_text())
    spec = model_spec or cfg.get("model")
    if not spec or "/" not in spec:
        sys.exit(f"need --model provider/model (config has {spec!r})")
    provider, model = spec.split("/", 1)
    p = (cfg.get("providers") or {}).get(provider)
    if not p:
        sys.exit(f"provider {provider!r} is not in {home / 'config.toml'}")
    key_env = p.get("api-key-env")
    return {
        "spec": spec,
        "url": p["base-url"].rstrip("/") + "/chat/completions",
        "model": model,
        "key": os.environ.get(key_env) if key_env else None,
        "budget_param": p.get("reasoning-budget-param"),
        "chat_template": p.get("thinking-param") == "chat-template",
    }


def ask(endpoint: dict, evidence: str, budget: int, structured: bool = True) -> dict:
    body = {
        "model": endpoint["model"],
        "messages": [{"role": "system", "content": SYSTEM},
                     {"role": "user", "content": evidence}],
        "max_tokens": budget + 1024,
        # Zero: the same evidence has to get the same label, or it is not a
        # signature. Measured at 0.2: two of three labels changed on a rerun.
        "temperature": 0,
    }
    if endpoint["chat_template"]:
        body["chat_template_kwargs"] = {"enable_thinking": True}
    if endpoint["budget_param"]:
        body[endpoint["budget_param"]] = budget
    if structured:
        body["response_format"] = {"type": "json_schema",
                                   "json_schema": {"name": "label", "schema": SCHEMA}}
    req = urllib.request.Request(endpoint["url"], data=json.dumps(body).encode(),
                                 headers={"content-type": "application/json"})
    if endpoint["key"]:
        req.add_header("authorization", f"Bearer {endpoint['key']}")
    # A label takes 10-20s on a free server; three minutes means it is stuck
    # or busy, and waiting longer only hides that.
    with urllib.request.urlopen(req, timeout=180) as r:
        reply = json.load(r)
    return parse(reply["choices"][0]["message"].get("content") or "")


def parse(text: str) -> dict:
    """The label, checked against the fixed lists whatever the server did."""
    start, end = text.find("{"), text.rfind("}")
    if start < 0 or end < start:
        raise ValueError(f"no JSON in reply: {clip(text, 200)}")
    label = json.loads(text[start:end + 1])
    if label.get("behaviour") not in BEHAVIOURS:
        raise ValueError(f"behaviour not in the list: {label.get('behaviour')!r}")
    if label.get("causal") not in CAUSAL:
        raise ValueError(f"causal not in the list: {label.get('causal')!r}")
    return {k: str(label.get(k, "")) for k in ("behaviour", "causal", "evidence", "note")}


def key_of(f: dict) -> str:
    return f"{f['session']}#{f['turn']}"


def one_vote(ask_fn, evidence: str) -> dict | None:
    """One label, with one retry: a malformed reply is often a one-off."""
    for _ in range(2):
        try:
            return ask_fn(evidence)
        except (ValueError, json.JSONDecodeError):
            continue
    return None


def decide(votes: list[dict]) -> dict:
    """The majority label, or `ambiguous` when no behaviour has two votes.

    One sample per failure was not a signature: on the local 27B, two of three
    labels changed between identical runs at temperature 0. A label is kept
    only when most votes agree, and the disagreement is reported rather than
    resolved by picking one."""
    counts = collections.Counter(v["behaviour"] for v in votes)
    top, k = counts.most_common(1)[0] if counts else ("", 0)
    if k < 2:
        return {"behaviour": "ambiguous", "causal": "", "evidence": "",
                "note": "votes: " + ", ".join(v["behaviour"] for v in votes) or "no valid votes"}
    agreeing = [v for v in votes if v["behaviour"] == top]
    causal = collections.Counter(v["causal"] for v in agreeing).most_common(1)[0][0]
    pick = next(v for v in agreeing if v["causal"] == causal)
    return {**pick, "causal": causal}


def label_all(failures: list[dict], cache: dict, ask_fn, limit: int | None,
              votes: int = 3, log=print) -> dict:
    todo = [f for f in failures if f["cause"] not in SKIP_CAUSES and key_of(f) not in cache]
    if limit is not None:
        todo = todo[:limit]
    for n, f in enumerate(todo, 1):
        log(f"  [{n}/{len(todo)}] {key_of(f)[:14]} {f['cause']:20s} ...", end="", flush=True)
        evidence = excerpt(f)
        try:
            got = [one_vote(ask_fn, evidence) for _ in range(votes)]
        except (urllib.error.URLError, TimeoutError) as e:
            log(f"\nstopping: the model server failed ({e}); labels so far are kept")
            break
        valid = [v for v in got if v]
        label = decide(valid)
        cache[key_of(f)] = {**label, "votes": [v["behaviour"] for v in valid],
                            "cause": f["cause"], "model": f["model"],
                            "source": f["source"], "session": f["session"],
                            "turn": f["turn"], "date": f["date"],
                            "prompt": clip(f["prompt"], 160)}
        log(f" {label['behaviour']} ({label['causal'] or '-'}) votes {'/'.join(cache[key_of(f)]['votes'])}")
    return cache


def report(all_labels: list[dict]) -> str:
    ambiguous = [l for l in all_labels if l["behaviour"] == "ambiguous"]
    labels = [l for l in all_labels if l["behaviour"] != "ambiguous"]
    unanimous = sum(1 for l in labels if len(set(l.get("votes", []))) == 1)
    out = [f"{len(all_labels)} failures voted on: {len(labels)} labeled "
           f"({unanimous} unanimous), {len(ambiguous)} ambiguous (no behaviour had two votes)"]
    groups = collections.Counter((l["cause"], l["behaviour"]) for l in labels)
    out.append("\nby cause + behaviour (the failure signature)")
    for (cause, behaviour), k in groups.most_common():
        caused = sum(1 for l in labels if (l["cause"], l["behaviour"]) == (cause, behaviour)
                     and l["causal"] == "caused")
        out.append(f"  {k:4d}  {cause:20s} {behaviour:24s} caused {caused}/{k}")
    out.append("\nby behaviour, across causes")
    for b, k in collections.Counter(l["behaviour"] for l in labels).most_common():
        models = collections.Counter(l["model"] for l in labels if l["behaviour"] == b)
        out.append(f"  {k:4d}  {b:24s} " + ", ".join(f"{m} {c}" for m, c in models.most_common(3)))
    out.append("\nexamples (largest groups first)")
    for (cause, behaviour), _ in groups.most_common(5):
        ex = next(l for l in labels if (l["cause"], l["behaviour"]) == (cause, behaviour))
        out.append(f"  {cause} / {behaviour}: {ex['session'][:8]} turn {ex['turn']} ({ex['date']})")
        out.append(f"    asked: {ex['prompt']}")
        out.append(f"    evidence: {clip(ex['evidence'], 200)}")
    return "\n".join(out)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--root", type=Path, default=F.home() / "sessions")
    ap.add_argument("--since", type=datetime.date.fromisoformat)
    ap.add_argument("--source", action="append", choices=["interactive", "worker", "eval"])
    ap.add_argument("--project")
    ap.add_argument("--model", help="provider/model from your config (default: its `model`)")
    ap.add_argument("--budget", type=int, default=1024, help="reasoning tokens per label")
    ap.add_argument("--limit", type=int, help="label at most this many new failures")
    ap.add_argument("--votes", type=int, default=3, help="labels asked per failure (majority wins)")
    # Labels quote session prompts, which are private; keep them beside the
    # sessions they came from, not in the repo.
    ap.add_argument("--out", type=Path, default=F.home() / "failure-labels.json",
                    help="label cache (default: $WORKSMITH_HOME/failure-labels.json)")
    ap.add_argument("--show", action="store_true", help="print each label's evidence for hand-checking")
    ap.add_argument("--report-only", action="store_true", help="regroup cached labels, no model calls")
    args = ap.parse_args()

    cache = json.loads(args.out.read_text()) if args.out.exists() else {}
    failures, _ = F.mine(args.root, args.since,
                         set(args.source) if args.source else None, args.project)
    if not args.report_only:
        endpoint = resolve(args.model)
        structured = [True]

        def ask_fn(evidence):
            try:
                return ask(endpoint, evidence, args.budget, structured[0])
            except urllib.error.HTTPError as e:
                if structured[0] and e.code == 400:
                    structured[0] = False  # server without JSON-schema support
                    return ask(endpoint, evidence, args.budget, False)
                raise

        skipped = sum(1 for f in failures if f["cause"] in SKIP_CAUSES)
        print(f"labeling with {endpoint['spec']}; {skipped} request failures skipped "
              f"(the agent never got to act)")
        try:
            label_all(failures, cache, ask_fn, args.limit, args.votes)
        finally:
            args.out.parent.mkdir(parents=True, exist_ok=True)
            args.out.write_text(json.dumps(cache, indent=2))

    wanted = {key_of(f) for f in failures}
    labels = [l for k, l in cache.items() if k in wanted]
    if args.show:
        by_key = {key_of(f): f for f in failures}
        for k, l in cache.items():
            if k in by_key:
                f = by_key[k]
                print(f"\n=== {k} {f['cause']} -> {l['behaviour']} ({l['causal']})")
                print(excerpt(f))
                print(f"--- evidence: {l['evidence']}\n--- note: {l['note']}")
    print(report(labels))
    return 0


if __name__ == "__main__":
    sys.exit(main())
