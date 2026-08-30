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
sys.path.insert(0, str(Path(__file__).resolve().parent))
from run import MANIFEST, REPO, parse_events, worksmith_bin  # noqa: E402


def server_status(model: str | None) -> dict | None:
    """Counters from a local model server, if it keeps any.

    oMLX exposes `/api/status` unauthenticated. It answers the question tok/s
    cannot: this workload is prefill-dominated — a fresh process per task means
    the same ~2,900-token system prompt is re-sent on every turn of every task —
    so the numbers that matter are the cache hit rate and the prefill/generate
    split, neither of which shows up in tokens per second.

    Returns None for anything that does not answer, which includes every hosted
    provider. A sweep must not fail because a server keeps no statistics.
    """
    if not model:
        return None
    try:
        from floor import provider_of
        base, key, _ = provider_of(model)
    except SystemExit:
        return None
    url = base.rsplit("/v1", 1)[0] + "/api/status"
    r = subprocess.run(["curl", "-s", "--max-time", "5", url,
                        "-H", f"Authorization: Bearer {key}"],
                       capture_output=True, text=True)
    try:
        d = json.loads(r.stdout)
    except json.JSONDecodeError:
        return None
    return d if "total_prompt_tokens" in d else None


def status_delta(before: dict | None, after: dict | None,
                 wall: float) -> dict | None:
    """What the server did during *this backlog*, and nothing else.

    The server's counters are cumulative since it started, so every figure here
    is a subtraction. That matters more than it sounds: `cache_efficiency` and
    `avg_*_tps` as served are lifetime averages, and reporting them beside one
    sweep's pass rates would attribute another run's cache warmth to this one.
    Efficiency is therefore recomputed from the delta, and throughput is derived
    from this backlog's own token counts and wall clock rather than borrowed.

    The lifetime rates are still carried, under names that say so, because they
    are the only clean read on what the hardware does — a per-run rate includes
    process spawn, tool execution and check runs, which is the right number for
    "how long will a sweep take" and the wrong one for "how fast is the GPU".
    """
    if not before or not after:
        return None
    d = {k: after.get(k, 0) - before.get(k, 0)
         for k in ("total_prompt_tokens", "total_completion_tokens",
                   "total_cached_tokens", "total_requests")}
    prompt, cached, gen = (d["total_prompt_tokens"], d["total_cached_tokens"],
                           d["total_completion_tokens"])
    d["cache_efficiency"] = round(100 * cached / prompt, 1) if prompt else None
    d["uncached_prompt_tokens"] = max(prompt - cached, 0)
    # The ratio this workload lives or dies by: a fresh process per task re-sends
    # the whole system prompt every turn, so prompt tokens dwarf generated ones.
    d["prompt_per_completion"] = round(prompt / gen, 1) if gen else None
    d["effective_generation_tps"] = round(gen / wall, 1) if wall else None
    d["lifetime_prefill_tps"] = after.get("avg_prefill_tps") or 0
    d["lifetime_generation_tps"] = after.get("avg_generation_tps") or 0
    # Where the time went, using the hardware's own rates: the uncached prompt
    # tokens are what actually cost prefill, which is the whole point of a cache.
    pre, gtps = d["lifetime_prefill_tps"], d["lifetime_generation_tps"]
    if pre and gtps:
        d["prefill_secs"] = round(d["uncached_prompt_tokens"] / pre, 1)
        d["generate_secs"] = round(gen / gtps, 1)
        tot = d["prefill_secs"] + d["generate_secs"]
        d["prefill_share"] = round(100 * d["prefill_secs"] / tot, 1) if tot else None
    return d


def load(path: Path) -> dict:
    b = tomllib.loads(path.read_text())
    b["dir"] = path.parent
    b["file"] = path.name
    # Remember each task's predecessor before any filtering, so --task can find
    # the right snapshot by name instead of by position in a shortened list.
    for i, t in enumerate(b["task"]):
        t["_prev"] = b["task"][i - 1]["id"] if i else None
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
             model: str | None, timeout: int, fast: bool = False,
             think: str | None = None, trace_dir: Path | None = None) -> dict:
    cmd = [binp, "--mode", "json", "--approve-all", "--trust-project"]
    if model:
        cmd += ["--model", model]
    # A model that always reasons will spend its whole budget thinking and
    # return empty content, which reaches the harness as "stuck: the model
    # returned an empty response" and reads like a capability failure. Measured:
    # qwen3.5-9b answered "reply with exactly: hello" with 46 reasoning tokens,
    # no content, finish_reason=length. Its whole sweep scored that way.
    if fast:
        cmd.append("--fast")
    elif think:
        cmd += ["--think", think]
    if task.get("validate"):
        cmd += ["--until", task["validate"]]
    cmd.append(prompt_for(task, produced))

    before = snapshot(workdir)
    row = {"id": task["id"], "passed": False, "outcome": None, "gen_tokens": 0,
           "tool_calls": 0, "model_calls": 0, "reasoning_tokens": 0,
           "elapsed": 0.0, "wrote": [], "error": None, "confidently_wrong": False,
           "timed_out": False, "by_tool": {}, "tool_errors": {}}
    stream = ""
    t0, w0 = time.monotonic(), time.time()
    try:
        r = subprocess.run(cmd, cwd=workdir, capture_output=True, text=True,
                           timeout=timeout)
        stream = r.stdout
        row.update(parse_events(stream))
        if r.returncode != 0:
            row["error"] = (r.stderr or "").strip()[:200]
    except subprocess.TimeoutExpired as e:
        # TimeoutExpired carries everything the process printed before the kill,
        # and dropping it makes a *slow* task indistinguishable from a dead one:
        # both report gen_tok=0, outcome=None. Three runs were called provider
        # stalls on that evidence when the model had in fact read the spec and
        # written a thousand tokens of implementation before the clock ran out.
        partial = e.stdout or ""
        if isinstance(partial, bytes):
            partial = partial.decode("utf-8", "replace")
        stream = partial
        row.update(parse_events(partial))
        row["timed_out"] = True
        row["error"] = (f"timeout after {timeout}s "
                        f"(had run {row['model_calls']} model calls, "
                        f"{row['tool_calls']} tool calls, "
                        f"{row['gen_tokens']} gen tokens)")
    # Monotonic, to match the clock `subprocess.run(timeout=)` enforces. Wall
    # clock does not: the machine slept mid-sweep on 2026-08-30 and a task
    # reported 3,155s elapsed against an 1,800s timeout that had correctly not
    # fired. A number that large silently implies the harness is broken.
    row["elapsed"] = round(time.monotonic() - t0, 1)
    # The gap between the two clocks IS the sleep. A run that straddles one has
    # dropped connections and stale server state in it, and its failures cannot
    # be read as the model's.
    slept = (time.time() - w0) - (time.monotonic() - t0)
    if slept > 60:
        row["slept_secs"] = round(slept, 1)
        print(f"  !! machine slept ~{slept / 60:.0f}m during {task['id']} — "
              "this run is not clean", file=sys.stderr)

    if trace_dir is not None:
        # Keep the raw event stream. worksmith already emits every model call,
        # tool call, tool result, validation failure with its output, and nudge;
        # this function reduces all of it to a handful of counters and then the
        # process is gone.
        #
        # That cost three wrong diagnoses in one day — whole-file rewriting, the
        # supervisor being the right place, the outer loop being the right layer
        # — and each one needed a fresh run with fresh instrumentation to settle,
        # because the evidence from the previous run no longer existed. Keeping
        # it makes a past run re-analysable instead of re-runnable, which for a
        # 900-second task is the difference between an answer and an afternoon.
        trace_dir.mkdir(parents=True, exist_ok=True)
        n = len(list(trace_dir.glob(f"{task['id']}*.jsonl")))
        (trace_dir / f"{task['id']}{'' if not n else f'-{n}'}.jsonl").write_text(
            locals().get("stream") or "")

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
    row["confidently_wrong"] = (row["outcome"] == "done" and not row["passed"]
                                and not row["timed_out"])
    return row


def reference_ok(backlog: dict) -> bool:
    """Does the reference still pass the backlog's own end-to-end check?

    Checked rather than assumed because it has already been wrong once: an
    agent run under `--approve-all` wrote its own broken `format_cents` over
    `reference/money.py`. Not because worksmith fails to confine writes:
    `approve_write_outside_cwd` gates exactly that. Because every run here passes
    `--approve-all`, so the gate approved itself. The corrupted key then failed a
    dispatcher test, which is the lucky version; the unlucky version is
    `--keep-going` splicing broken code into a run and scoring the result.
    """
    ref = backlog["dir"] / "reference"
    with tempfile.TemporaryDirectory() as d:
        d = Path(d)
        for f in list(ref.glob("*.py")) + list((backlog["dir"] / "files").iterdir()):
            shutil.copy(f, d / f.name)
        r = subprocess.run(backlog["validate"], shell=True, cwd=d,
                           capture_output=True, text=True)
    return r.returncode == 0


def repair(workdir: Path, backlog: dict) -> list[str]:
    """Splice the reference solution in, so a failed task stops being a wall.

    Only used by --keep-going, and it copies the *whole* reference because a
    failed task rarely leaves exactly one file wrong.

    That bluntness is why every task after a repair is marked `tainted` and left
    out of the pass rate. The reference implements the entire backlog, so once
    it is in place a later task's check passes whether or not the model did
    anything — a stub agent that writes nothing scores 21 of 22 without the
    taint rule. Grading tainted tasks would not be a generous measurement, it
    would be a fabricated one.
    """
    ref = backlog["dir"] / "reference"
    if not ref.is_dir():
        return []
    if not reference_ok(backlog):
        sys.exit(f"reference for {backlog['name']} does not pass its own "
                 "end-to-end check — refusing to splice it in. Run "
                 "`python3 evals/pool/verify.py`.")
    names = []
    for f in sorted(ref.glob("*.py")):
        shutil.copy(f, workdir / f.name)
        names.append(f.name)
    return names


def seed_for(backlog: dict, tasks: list, i: int) -> Path | None:
    """The snapshot a task starts from. Same rule as `bare.py`, deliberately:
    the two arms are only comparable if they start from identical states.

    Snapshots are named by position in the *whole* backlog, so `--task` looks
    the directory up by name rather than trusting the filtered list's index. A
    filtered run that seeded task 8 from position 1 would start it in an empty
    directory and score the model on work it was never given.
    """
    snaps = backlog["dir"] / "snapshots"
    task = tasks[i]
    if "seed" in task:
        return (snaps / task["seed"]) if task["seed"] else None
    prev = task.get("_prev")
    if prev is None:
        return None
    d = next((x for x in snaps.glob(f"*-{prev}") if x.is_dir()), None)
    return d


def run_independent(backlog: dict, binp: str, model: str | None, timeout: int,
                    fast: bool, think: str | None,
                    trace_dir: Path | None = None) -> dict:
    """Every task on its own, seeded from a snapshot — the harness arm of the
    comparison with `bare.py`.

    The chained mode answers "how far does a backlog get before something
    blocks", which is a different question and the one that produced results
    decided by a single coin flip at task 8. This answers "what fraction of
    these tasks does the harness solve", per task, with no task's fate
    depending on another's — which is the only shape that can be set beside a
    one-shot bare number and subtracted.
    """
    tasks = backlog["task"]
    rows, solved, attempted = [], 0, 0
    for i, task in enumerate(tasks):
        if not task.get("validate"):
            continue
        seed = seed_for(backlog, tasks, i)
        workdir = setup(backlog)
        if seed:
            for f in seed.glob("*.py"):
                shutil.copy(f, workdir / f.name)
        row = run_task(binp, task, workdir, {}, model, timeout, fast, think,
                       trace_dir)
        shutil.rmtree(workdir, ignore_errors=True)
        rows.append(row)
        attempted += 1
        solved += row["passed"]
        mark = "PASS " if row["passed"] else ("TIME " if row["timed_out"] else "FAIL ")
        print(f"  {mark}{task['id']:<16} {row['elapsed']:>6.1f}s "
              f"gen_tok={row['gen_tokens']:<6} "
              f"tools={row['by_tool']} errs={row['tool_errors']} "
              f"outcome={row['outcome']}", file=sys.stderr)
    return {"backlog": backlog["name"], "granularity": backlog.get("granularity"),
            "mode": "independent", "tasks": len(tasks), "ran": attempted,
            "scored": attempted, "solved": solved, "blocked": [], "tainted": [],
            "repaired": [], "timed_out": [r["id"] for r in rows if r["timed_out"]],
            "slept_during": [r["id"] for r in rows if r.get("slept_secs")],
            "confidently_wrong": sum(1 for r in rows if r["confidently_wrong"]),
            "end_to_end": None,
            "gen_tokens": sum(r["gen_tokens"] for r in rows),
            "gen_tokens_per_solved": (round(sum(r["gen_tokens"] for r in rows) / solved)
                                      if solved else None),
            "wall_clock": round(sum(r["elapsed"] for r in rows), 1),
            "server": None, "task_rows": rows}


def dispatch(backlog: dict, binp: str, model: str | None, timeout: int,
             keep: bool, keep_going: bool = False, fast: bool = False,
             think: str | None = None, trace_dir: Path | None = None) -> dict:
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
    status_before = server_status(model)
    tasks = {t["id"]: t for t in backlog["task"]}
    done: set[str] = set()
    produced: dict[str, list[str]] = {}
    rows: list[dict] = []
    blocked: list[str] = []
    tainted = False

    repaired: list[str] = []
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
        row = run_task(binp, tasks[tid], workdir, produced, model, timeout,
                       fast, think, trace_dir)
        row["tainted"] = tainted
        rows.append(row)
        if row["passed"]:
            done.add(tid)
            produced[tid] = row["wrote"]
        elif keep_going:
            # Measuring, not working. Blocking is right when the output is the
            # code; it is wrong when the output is a pass rate, because one hard
            # task early in a chain reports as twenty failures while saying
            # nothing about the other nineteen. Splice in the reference and let
            # the rest of the backlog be attempted on a correct dependency —
            # scored, but marked (see `repair`).
            patched = repair(workdir, backlog)
            if patched:
                repaired.append(tid)
                done.add(tid)
                produced[tid] = patched
                tainted = True
                print(f"  REPAIR {tid} — reference spliced in; everything after "
                      "this is unscored", file=sys.stderr)
        mark = "PASS " if row["passed"] else ("TIME " if row["timed_out"] else "FAIL ")
        flag = " CONFIDENTLY-WRONG" if row["confidently_wrong"] else ""
        wrote = f" wrote={','.join(row['wrote'])}" if row["wrote"] else ""
        print(f"  {mark}{tid:<16} {row['elapsed']:>6.1f}s "
              f"gen_tok={row['gen_tokens']:<6} outcome={row['outcome']}"
              f"{wrote}{flag}", file=sys.stderr)

    # An end-to-end pass means nothing once the reference has been spliced in.
    e2e = subprocess.run(["bash", "-lc", backlog["validate"]], cwd=workdir,
                         capture_output=True, text=True)
    wall = round(sum(r["elapsed"] for r in rows), 1)
    server = status_delta(status_before, server_status(model), wall)
    scored = [r for r in rows if not r.get("tainted")]
    solved = sum(1 for r in scored if r["passed"])
    total_tok = sum(r["gen_tokens"] for r in scored)
    result = {
        "backlog": backlog["name"],
        "granularity": backlog.get("granularity"),
        "tasks": len(tasks),
        "ran": len(rows),
        "scored": len(scored),
        "solved": solved,
        "tainted": [r["id"] for r in rows if r.get("tainted")],
        "blocked": blocked,
        "repaired": repaired,
        "confidently_wrong": sum(1 for r in scored if r["confidently_wrong"]),
        "end_to_end": None if repaired else e2e.returncode == 0,
        "gen_tokens": total_tok,
        # The eval reports cost per *solved* task, not total: a loop that spends
        # more and succeeds more is not more expensive per unit of work.
        "gen_tokens_per_solved": round(total_tok / solved) if solved else None,
        "timed_out": [r["id"] for r in rows if r["timed_out"]],
        "slept_during": [r["id"] for r in rows if r.get("slept_secs")],
        "wall_clock": wall,
        "server": server,
        "task_rows": rows,
    }
    if server and server.get("cache_efficiency") is not None:
        print(f"  server: cache {server['cache_efficiency']}% · "
              f"prompt {server['total_prompt_tokens']:,} tok / completion "
              f"{server['total_completion_tokens']:,} tok "
              f"({server['prompt_per_completion']}:1)"
              + (f" · prefill {server['prefill_share']}% of compute"
                 if server.get("prefill_share") is not None else ""),
              file=sys.stderr)
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
    # Must stay above worksmith's own stream-idle timeout
    # (llm::DEFAULT_STREAM_IDLE_SECS, 600s), or this harness kills the process
    # at the same moment worksmith is about to report the stall itself — and a
    # provider that accepted the request and went quiet then scores as
    # `outcome=None gen_tok=0`, which reads as the model failing. Two 9B tasks
    # died that way at exactly 600.0s with both timeouts set to 600.
    ap.add_argument("--timeout", type=int, default=900,
                    help="per task, seconds. Keep it above worksmith's "
                         "stream-idle timeout (600s) so a stalled provider is "
                         "reported by worksmith rather than killed here.")
    ap.add_argument("--task", action="append", metavar="ID",
                    help="run only this task (repeatable). Implies "
                         "--independent, since a single task out of a chain has "
                         "to start from its snapshot. This is the fast feedback "
                         "loop: a change aimed at one task can be judged in "
                         "minutes instead of the 111 a full sweep takes.")
    ap.add_argument("--independent", action="store_true",
                    help="run every task on its own, seeded from its snapshot, "
                         "instead of as a blocking chain. This is the arm that "
                         "compares with bare.py: same tasks, same starting "
                         "states, harness the only difference.")
    ap.add_argument("--keep-going", action="store_true",
                    help="on failure, splice in the reference solution and carry "
                         "on, so every task is attempted. Gives a per-task pass "
                         "rate over the whole backlog instead of time-to-first-"
                         "wall. Voids the end-to-end result, which is then null.")
    ap.add_argument("--fast", action="store_true",
                    help="thinking off. Needed for a model that always reasons: "
                         "it otherwise spends the budget thinking and returns "
                         "empty content, which reads as a capability failure.")
    ap.add_argument("--think", metavar="LEVEL")
    ap.add_argument("--repeat", type=int, default=1)
    ap.add_argument("--json")
    ap.add_argument("--trace", metavar="DIR", type=Path,
                    help="save each task's raw event stream here. worksmith "
                         "already emits every call, result and check failure; "
                         "without this the run reduces it to counters and the "
                         "evidence is gone when a diagnosis turns out wrong.")
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
            if args.task:
                keep = set(args.task)
                unknown = keep - {t["id"] for t in backlog["task"]}
                if unknown:
                    sys.exit(f"no such task: {', '.join(sorted(unknown))}")
                backlog = {**backlog,
                           "task": [t for t in backlog["task"] if t["id"] in keep]}
            if args.independent or args.task:
                r = run_independent(backlog, binp, args.model, args.timeout,
                                    args.fast, args.think, args.trace)
            else:
                r = dispatch(backlog, binp, args.model, args.timeout, args.keep,
                             args.keep_going, args.fast, args.think, args.trace)
            r["run"] = i
            results.append(r)
            if args.json:
                Path(args.json).write_text(json.dumps(results, indent=2))

    print(f"\n{'backlog':<20} {'tasks':>7} {'e2e':>5} {'conf-wrong':>11} "
          f"{'tok/solved':>11} {'wall':>8}")
    for r in results:
        e2e = "—" if r["end_to_end"] is None else ("pass" if r["end_to_end"] else "FAIL")
        per = r["gen_tokens_per_solved"]
        denom = r["scored"] if r["tainted"] else r["tasks"]
        print(f"{r['backlog']:<20} {r['solved']:>3}/{denom:<3} {e2e:>5} "
              f"{r['confidently_wrong']:>11} {per if per else '—':>11} "
              f"{r['wall_clock']:>7}s")
    return 0 if all(r["end_to_end"] is not False for r in results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
