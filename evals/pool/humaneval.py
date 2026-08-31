#!/usr/bin/env python3
"""HumanEval, three arms, to test the harness on somebody else's problems.

    python3 evals/pool/humaneval.py --model omlx/Qwen3.5-4B-MLX-4bit --limit 60

The expenses fixture has an obvious objection: the same person wrote the tasks
and the checks. This removes it. The problems and the tests are OpenAI's, the
tests are the grader *and* the `--until` check, and nothing here was authored to
make a point.

Three arms, because two would leave the interesting confound open:

- **bare** — the function stub, one shot, no tools. What the model can do alone.
- **bare+test** — the stub *and* the test text, still one shot. The harness's
  check is information as well as enforcement, and this arm holds the
  information constant so the difference from the next arm is enforcement only.
  The original worksmith eval sidestepped this because its goals already named
  their checks; here the test is genuinely extra, so it has to be controlled.
- **harness** — worksmith with the stub seeded and `--until python3 test.py`.

bare+test versus harness is the number that matters. bare versus bare+test says
how much of any gain was just showing the model the test.
"""
import argparse
import json
import random
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(HERE.parent))
from floor import ask, provider_of  # noqa: E402
from run import parse_events, worksmith_bin  # noqa: E402

FENCE = re.compile(r"```(?:python)?\n(.*?)```", re.S)


def workdir_for(p: dict, seed_stub: bool) -> Path:
    """A scratch dir holding solution.py and test.py, graded identically in
    every arm so the arms are comparable."""
    d = Path(tempfile.mkdtemp(prefix="he-"))
    (d / "solution.py").write_text(p["prompt"] if seed_stub else "")
    (d / "test.py").write_text(
        f"from solution import {p['entry_point']}\n{p['test']}\n"
        f"check({p['entry_point']})\nprint('ok')\n"
    )
    return d


def grade(d: Path, code: str) -> bool:
    (d / "solution.py").write_text(code)
    r = subprocess.run([sys.executable, "test.py"], cwd=d,
                       capture_output=True, text=True, timeout=30)
    return r.returncode == 0


def one_shot(p: dict, base, key, name, show_test: bool, max_tokens: int) -> bool:
    prompt = (
        "Complete this Python function. Reply with the whole function, "
        "including the signature, as one Python code block and nothing else."
        f"\n\n{p['prompt']}"
    )
    if show_test:
        prompt += f"\n\nIt must satisfy these tests:\n\n{p['test']}"
    text, _usage, finish = ask(base, key, name, prompt, max_tokens, False)
    if finish == "length":
        return None  # truncated, not wrong
    m = FENCE.search(text)
    d = workdir_for(p, seed_stub=False)
    try:
        return grade(d, m.group(1) if m else text)
    except subprocess.TimeoutExpired:
        return False
    finally:
        shutil.rmtree(d, ignore_errors=True)


def harnessed(p: dict, binp: str, model: str, timeout: int) -> tuple[bool, int]:
    d = workdir_for(p, seed_stub=True)
    try:
        r = subprocess.run(
            [binp, "--mode", "json", "--approve-all", "--trust-project",
             "--model", model, "--fast", "--until", "python3 test.py",
             f"Implement the function in solution.py. Keep the existing "
             f"signature. `python3 test.py` must pass."],
            cwd=d, capture_output=True, text=True, timeout=timeout)
        tok = parse_events(r.stdout).get("gen_tokens", 0)
        ok = subprocess.run([sys.executable, "test.py"], cwd=d,
                            capture_output=True, text=True, timeout=30).returncode == 0
        return ok, tok
    except subprocess.TimeoutExpired:
        return False, 0
    finally:
        shutil.rmtree(d, ignore_errors=True)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--model", required=True)
    ap.add_argument("--data", type=Path,
                    default=Path("/private/tmp/claude-501/"
                                 "-Users-bradleydsmith-Projects-worksmith/"
                                 "dcea7351-86e8-40b7-bdde-63cfb2c05ad8/HumanEval.jsonl"))
    ap.add_argument("--limit", type=int, default=60)
    ap.add_argument("--timeout", type=int, default=300)
    ap.add_argument("--max-tokens", type=int, default=3000)
    ap.add_argument("--arms", default="bare,bare+test,harness")
    ap.add_argument("--json")
    args = ap.parse_args()

    problems = [json.loads(l) for l in args.data.read_text().splitlines() if l.strip()]
    # Fixed seed: every arm and every re-run must see the same problems, or the
    # comparison is between samples rather than between arms.
    random.Random(20260830).shuffle(problems)
    problems = problems[: args.limit]
    arms = [a.strip() for a in args.arms.split(",") if a.strip()]

    base, key, name = provider_of(args.model)
    binp = worksmith_bin()
    res = {a: {"pass": 0, "graded": 0, "tok": 0} for a in arms}
    rows = []
    for i, p in enumerate(problems, 1):
        row = {"task_id": p["task_id"]}
        for a in arms:
            if a == "harness":
                ok, tok = harnessed(p, binp, args.model, args.timeout)
                res[a]["tok"] += tok
            else:
                ok = one_shot(p, base, key, name, a == "bare+test", args.max_tokens)
            if ok is None:
                row[a] = "trunc"
                continue
            res[a]["graded"] += 1
            res[a]["pass"] += ok
            row[a] = ok
        rows.append(row)
        marks = " ".join(f"{a}={row.get(a)}" for a in arms)
        print(f"  {i:>3}/{len(problems)} {p['task_id']:<14} {marks}", file=sys.stderr)
        if args.json:
            Path(args.json).write_text(json.dumps(
                {"model": args.model, "arms": res, "rows": rows}, indent=2))

    print(f"\n{args.model} · HumanEval, {len(problems)} problems")
    for a in arms:
        r = res[a]
        pct = 100 * r["pass"] / r["graded"] if r["graded"] else 0
        extra = f"  gen_tok={r['tok']:,}" if r["tok"] else ""
        print(f"  {a:<10} {r['pass']}/{r['graded']} = {pct:.0f}%{extra}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
