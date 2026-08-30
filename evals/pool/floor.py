#!/usr/bin/env python3
"""Is this model above the floor for the smallest task in a backlog?

    python3 evals/pool/floor.py omlx/Qwen3.5-2B-6bit
    python3 evals/pool/floor.py omlx/Qwen3.5-9B-OptiQ-4bit --task pa-reject -n 5

Sends the task straight to the provider — no worksmith, no tools, no validation
loop, no system prompt — and checks the answer against the task's own check. One
shot per attempt, several attempts, because a small model at temperature 0.7
swings run to run: the 2B failed `fc-whole` in 53s and passed the identical task
in 16s on the next attempt.

**Run this before a sweep, not after.** A 22-task backlog on a local model is an
hour; this is under a minute, and it separates the two questions a bad sweep
otherwise conflates:

- **0 of N here, and the sweep also fails** → the model cannot do the work. The
  harness is not the story and neither is task size. (Measured: the 2B scores
  0/5 on `fc-whole`, the smallest task in the suite, at an 84-token prompt.)
- **0 of N here, and the sweep passes** → that is the thesis working. The loop
  turned a model that cannot do it one-shot into one that gets there by being
  made to check. Worth reporting loudly.
- **N of N here** → the model is comfortably above the floor, and a sweep
  failure is about task size, context, or the harness.
"""
import argparse
import json
import pathlib
import re
import subprocess
import sys
import tempfile
import tomllib

REPO = pathlib.Path(__file__).resolve().parents[2]


def provider_of(model: str) -> tuple[str, str, str]:
    """`omlx/Qwen3.5-2B-6bit` -> (base_url, api key, bare model id)."""
    provider, _, name = model.partition("/")
    import os
    for cfg in (REPO / ".worksmith" / "config.toml",
                pathlib.Path.home() / ".worksmith" / "config.toml"):
        if not cfg.exists():
            continue
        p = tomllib.loads(cfg.read_text()).get("providers", {}).get(provider)
        if p:
            key = os.environ.get(p.get("api-key-env", ""), "")
            return p["base-url"], key, name
    sys.exit(f"no provider `{provider}` in either config")


def ask(base: str, key: str, model: str, prompt: str, max_tokens: int,
        think: bool) -> tuple[str, dict, str]:
    body = {"model": model, "max_tokens": max_tokens,
            "messages": [{"role": "user", "content": prompt}]}
    if not think:
        # Two dialects, because providers disagree and the wrong one is silently
        # ignored rather than refused. Measured on the same trivial prompt:
        #
        #   omlx      + chat_template_kwargs  ->   1 completion token
        #   omlx      without it              -> 387 completion tokens
        #   OpenRouter + chat_template_kwargs -> 186 tokens, 159 of them reasoning
        #   OpenRouter + reasoning.enabled    ->   2 tokens, 0 reasoning
        #
        # Sending only Qwen's spelling meant the hosted arm of the quantisation
        # control reasoned while the local arm did not, so it compared thinking
        # against no-thinking with quantisation along for the ride. worksmith
        # itself gets this right (`ThinkingDialect`, openai.rs:356) — this script
        # was the one guessing. Both are harmless where unrecognised.
        body["chat_template_kwargs"] = {"enable_thinking": False}
        body["reasoning"] = {"enabled": False}
    body = json.dumps(body)
    r = subprocess.run(["curl", "-s", f"{base}/chat/completions",
                        "-H", f"Authorization: Bearer {key}",
                        "-H", "Content-Type: application/json", "-d", body],
                       capture_output=True, text=True)
    try:
        d = json.loads(r.stdout)
    except json.JSONDecodeError:
        return "", {}, "unparseable"
    if "error" in d:
        sys.exit(f"provider error: {d['error']}")
    c = d["choices"][0]
    return (c["message"].get("content") or ""), d.get("usage", {}), (c.get("finish_reason") or "")


def extract(text: str) -> str:
    m = re.search(r"```(?:python)?\n(.*?)```", text, re.S)
    return m.group(1) if m else text


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("model")
    ap.add_argument("--backlog", type=pathlib.Path,
                    default=REPO / "evals" / "pool" / "expenses" / "fine.toml")
    ap.add_argument("--task", help="task id (default: the first one)")
    ap.add_argument("-n", type=int, default=5)
    ap.add_argument("--max-tokens", type=int, default=3000,
                    help="800 was too low: the 9B was cut off mid-function and "
                         "the truncated source scored as a wrong answer.")
    ap.add_argument("--think", action="store_true",
                    help="leave the model's reasoning on (off by default, to "
                         "match how the sweep runs it)")
    args = ap.parse_args()

    backlog = tomllib.loads(args.backlog.read_text())
    tasks = backlog["task"]
    task = next((t for t in tasks if t["id"] == args.task), None) if args.task else tasks[0]
    if task is None:
        sys.exit(f"no task {args.task!r} in {args.backlog.name}")
    if not task.get("validate"):
        sys.exit(f"task {task['id']} has no check to grade against")

    base, key, name = provider_of(args.model)
    prompt = (task["prompt"].strip() +
              "\nReply with only the Python code, no explanation.")

    passes = truncated = 0
    for i in range(args.n):
        text, usage, finish = ask(base, key, name, prompt, args.max_tokens,
                                  args.think)
        if finish == "length":
            # Not a wrong answer — an answer we cut off. Scoring it as a failure
            # is how a harness bug gets published as a model result.
            truncated += 1
            print(f"  {i+1}: TRUNC prompt_tok={usage.get('prompt_tokens', '?'):<5} "
                  f"gen_tok={usage.get('completion_tokens', '?'):<5} "
                  f"hit the {args.max_tokens}-token cap; raise --max-tokens")
            continue
        with tempfile.TemporaryDirectory() as d:
            # The check imports the module the task was asked to write, so the
            # answer is graded by exactly the same command the sweep would use.
            pathlib.Path(d, "money.py").write_text(extract(text))
            r = subprocess.run(["bash", "-lc", task["validate"]], cwd=d,
                               capture_output=True, text=True)
        ok = r.returncode == 0
        passes += ok
        why = "" if ok else (r.stdout + r.stderr).strip().splitlines()[-1][:64]
        print(f"  {i+1}: {'PASS' if ok else 'FAIL'}  "
              f"prompt_tok={usage.get('prompt_tokens', '?'):<5} "
              f"gen_tok={usage.get('completion_tokens', '?'):<5} {why}")

    graded = args.n - truncated
    note = f" ({truncated} truncated, not graded)" if truncated else ""
    print(f"\n{args.model} on `{task['id']}`, bare: {passes}/{graded}{note}")
    if truncated == args.n:
        print("Every attempt was cut off. This says nothing about the model — "
              "raise --max-tokens and run it again.")
        return 1
    if passes == 0:
        print("Below the floor one-shot. A sweep that also fails says nothing "
              "about task size; a sweep that passes is the loop earning its keep.")
    return 0 if passes else 1


if __name__ == "__main__":
    raise SystemExit(main())
