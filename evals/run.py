#!/usr/bin/env python3
"""Worksmith eval harness.

Runs each task in `tasks/*.toml` under modes and reports whether the task's
validation command passes afterward:

  raw     — `worksmith --mode json "<goal>"`         (model stops when it decides)
  memory  — raw, but with the task's [[memory]] rows seeded into project memory
  guided  — `worksmith --mode json --until "<validate>" "<goal>"`
            (the validation-driven loop: re-plan until the check passes)
  workers — `worksmith --mode json spawn -n N "<goal>"`
            (fan out to N workers, parent synthesizes; needs `workers = N` in
            the task, and is *unguided* — workers have no validator, so the
            honest comparison is raw vs workers, not guided vs workers)

All modes are judged by the SAME criterion — the harness runs `validate` in the
scratch dir after the agent finishes — so the comparison is fair. The question
this answers: does the guidance layer make a given model succeed more often?

Usage:
  python3 evals/run.py                      # all tasks, both modes
  python3 evals/run.py --task fix-bug       # one task
  python3 evals/run.py --modes guided       # one mode
  python3 evals/run.py --modes raw,memory --task memory-convention
  python3 evals/run.py --model openrouter/qwen/qwen3-32b
  python3 evals/run.py --timeout 240 --json results.json
"""
import argparse
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import tomllib
import uuid
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
MANIFEST = REPO / "Cargo.toml"
TASKS_DIR = Path(__file__).resolve().parent / "tasks"
FIXTURES_DIR = Path(__file__).resolve().parent / "fixtures"


# The real limit on an attempt is tokens, which mean the same on any hardware.
# Wall-clock time measured the machine: on 2026-11-14 the local 9B (about 19
# tokens/s on a Mac) failed 7 of 10 attempts by timeout, two compaction passes
# had finished at 736s and 828s against a 900s limit, and guided mode, which
# does more work, timed out most. Budgets are about twice the largest
# generation of any passing attempt on record; a task may declare its own.
DEFAULT_TOKEN_BUDGET = {"short": 20_000}
# Only a safety net now, so nothing hangs forever. Generous on purpose.
DEFAULT_SAFETY_TIMEOUT = {"short": 1200, "long": 3600}


def limits(task: dict, timeout_override: int | None = None) -> tuple[int, int]:
    """(token budget, safety timeout in seconds) for one task."""
    length = task.get("length", "short")
    budget = task.get("token_budget") or DEFAULT_TOKEN_BUDGET.get(length)
    if not budget:
        raise ValueError(f"{task['name']}: a {length} task must declare token_budget")
    timeout = timeout_override or task.get("timeout") or DEFAULT_SAFETY_TIMEOUT[length]
    return int(budget), int(timeout)


def default_traces_dir() -> Path:
    """Outside the repo, beside the user's own sessions. Eval traces carry no
    private prompts, but a full run is tens of megabytes."""
    home = Path(os.environ.get("WORKSMITH_HOME") or Path.home() / ".worksmith")
    return home / "eval-traces" / time.strftime("%Y%m%d-%H%M%S")


def snapshot_binary(built: Path, dest_dir: Path) -> Path:
    """A private copy of the binary for this run.

    The build lands in the shared cargo target dir, and every `cargo build` or
    `cargo test` in the repo rewrites that file. Running it directly meant a
    rebuild mid-run silently changed what was being measured: on 2026-11-14 a
    `cargo test` swapped the binary under the last 12 attempts of a 54-attempt
    run, and the comparison it was for lost two of its tasks."""
    dest_dir.mkdir(parents=True, exist_ok=True)
    dest = dest_dir / built.name
    # Copy beside it, then rename into place. Copying straight onto `dest`
    # rewrites the file in place, and when two runs shared a traces dir that
    # rewrote a binary another run was executing: macOS killed every later
    # launch (exit 137). A rename swaps the name; a running copy keeps its file.
    tmp = dest_dir / f".{built.name}.{os.getpid()}.tmp"
    shutil.copy2(built, tmp)
    os.replace(tmp, dest)
    return dest


def build_info(binp: Path) -> dict:
    """What was measured: the binary's own version line and the checkout it
    came from, with whether that checkout had uncommitted changes."""
    def out(cmd):
        try:
            r = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
            return r.stdout.strip() if r.returncode == 0 else None
        except (OSError, subprocess.TimeoutExpired):
            return None
    status = out(["git", "-C", str(REPO), "status", "--porcelain"])
    return {
        "version": out([str(binp), "--version"]),
        "commit": out(["git", "-C", str(REPO), "rev-parse", "--short", "HEAD"]),
        "dirty": bool(status) if status is not None else None,
    }


def save_trace(workdir: Path, dest: Path, verdict: dict) -> Path | None:
    """Copy an attempt's sessions out of its scratch dir before it is deleted,
    with the check's verdict beside them.

    Rows alone hold totals, which says a run failed but not what the agent did.
    The trace is what failure mining reads, and the verdict is ground truth it
    cannot get from a session: a turn can end `done` and still fail the check."""
    sessions = workdir / ".worksmith-home" / "sessions"
    if not sessions.is_dir():
        return None
    dest.mkdir(parents=True, exist_ok=True)
    shutil.copytree(sessions, dest / "sessions", dirs_exist_ok=True)
    (dest / "verdict.json").write_text(json.dumps(verdict, indent=2))
    return dest


def worksmith_bin() -> str:
    meta = json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--format-version", "1", "--no-deps",
             "--manifest-path", str(MANIFEST)]
        )
    )
    return os.path.join(meta["target_directory"], "debug", "worksmith")


def load_tasks(only: str | None, length: str = "all") -> list[dict]:
    """Tasks to run. `length` splits the suite: a short task finishes in
    seconds to a minute and checks the loop's mechanics; a long one runs for
    many minutes and long context, where compaction and step limits live.
    Measured 2026-11-14 on the 27B: the seven short tasks took 13 minutes in
    all, one long task took 95% of the run."""
    tasks = []
    for f in sorted(TASKS_DIR.glob("*.toml")):
        with open(f, "rb") as fh:
            t = tomllib.load(fh)
        # A comma list, so one invocation (one build, one binary) can cover
        # several tasks; each separate run.py rebuilds from the checkout.
        if only and t["name"] not in only.split(","):
            continue
        if length != "all" and t.get("length", "short") != length:
            continue
        tasks.append(t)
    return tasks


def configured_model() -> str | None:
    """The model a run without --model uses: the project config's, else the
    global one's."""
    for cfg in (REPO / ".worksmith" / "config.toml", active_global_config()):
        if cfg.exists():
            model = tomllib.loads(cfg.read_text()).get("model")
            if model:
                return model
    return None


def active_global_config() -> Path:
    home = os.environ.get("WORKSMITH_HOME")
    if home:
        return Path(home) / "config.toml"
    return Path.home() / ".worksmith" / "config.toml"


def seed_project_memory(path: Path, rows: list[dict]) -> None:
    if not rows:
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(path)
    now = int(time.time())
    try:
        conn.executescript(
            """
            CREATE TABLE IF NOT EXISTS memories (
                id            TEXT PRIMARY KEY,
                scope         TEXT NOT NULL,
                kind          TEXT NOT NULL,
                subject       TEXT NOT NULL,
                content       TEXT NOT NULL,
                importance    INTEGER NOT NULL DEFAULT 50,
                confidence    REAL NOT NULL DEFAULT 1.0,
                created_at    INTEGER NOT NULL,
                updated_at    INTEGER NOT NULL,
                supersedes_id TEXT,
                status        TEXT NOT NULL DEFAULT 'active'
            );
            CREATE INDEX IF NOT EXISTS idx_memories_subject ON memories(subject);
            CREATE INDEX IF NOT EXISTS idx_memories_status  ON memories(status);
            CREATE TABLE IF NOT EXISTS mined_sessions (
                session_id TEXT PRIMARY KEY,
                mined_at   INTEGER NOT NULL,
                candidates INTEGER NOT NULL DEFAULT 0
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
                subject, content, kind, id UNINDEXED, tokenize = 'porter unicode61'
            );
            CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
                INSERT INTO memories_fts(subject, content, kind, id)
                VALUES (new.subject, new.content, new.kind, new.id);
            END;
            CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
                DELETE FROM memories_fts WHERE id = old.id;
            END;
            CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
                DELETE FROM memories_fts WHERE id = old.id;
                INSERT INTO memories_fts(subject, content, kind, id)
                VALUES (new.subject, new.content, new.kind, new.id);
            END;
            """
        )
        for row in rows:
            conn.execute(
                """
                INSERT INTO memories
                    (id, scope, kind, subject, content, importance, confidence,
                     created_at, updated_at, supersedes_id, status)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, 'active')
                """,
                (
                    str(uuid.uuid4()).replace("-", "")[:8],
                    row.get("scope", "project"),
                    row["kind"],
                    row["subject"],
                    row["content"],
                    int(row.get("importance", 80)),
                    float(row.get("confidence", 1.0)),
                    now,
                    now,
                ),
            )
        conn.commit()
    finally:
        conn.close()


def toml_dump(data: dict) -> str:
    """Enough TOML to write a config back out: tables, strings, numbers,
    booleans and flat arrays, which is all a Worksmith config holds. tomllib
    reads but the stdlib cannot write, and this is too small to add a
    dependency for."""
    def value(v):
        if isinstance(v, bool):
            return "true" if v else "false"
        if isinstance(v, (int, float)):
            return repr(v)
        if isinstance(v, str):
            return json.dumps(v)
        if isinstance(v, list):
            return "[" + ", ".join(value(x) for x in v) + "]"
        raise TypeError(f"cannot write {type(v).__name__} to TOML")

    def key(k):
        return k if k.replace("-", "").replace("_", "").isalnum() else json.dumps(k)

    lines: list[str] = []

    def table(path, d):
        scalars = {k: v for k, v in d.items() if not isinstance(v, dict)}
        if path and scalars:
            lines.append("\n[" + ".".join(key(p) for p in path) + "]")
        lines.extend(f"{key(k)} = {value(v)}" for k, v in scalars.items())
        for k, v in d.items():
            if isinstance(v, dict):
                table(path + [k], v)

    table([], data)
    return "\n".join(lines).lstrip("\n") + "\n"


def force_context(config: Path, model: str | None, tokens: int) -> None:
    """Shrink the context window for the model under test, so a long task hits
    compaction in minutes. Per-model `context` wins over `[agent]
    context-limit`, and a global config can name one for this model, so both
    are set in the project config, which overrides the global field by field."""
    data = tomllib.loads(config.read_text()) if config.exists() else {}
    data.setdefault("agent", {})["context-limit"] = tokens
    if model:
        data.setdefault("models", {}).setdefault(model, {})["context"] = tokens
    config.write_text(toml_dump(data))


def set_token_budget(config: Path, tokens: int) -> None:
    """Worksmith's own per-turn limit, so an attempt ends the same way on any
    hardware: `stuck: spent N tokens on this turn without finishing`."""
    data = tomllib.loads(config.read_text()) if config.exists() else {}
    data.setdefault("agent", {})["token-budget"] = tokens
    config.write_text(toml_dump(data))


def setup_workdir(task: dict, seed_memory: bool = False, model: str | None = None,
                  token_budget: int | None = None) -> Path:
    d = Path(tempfile.mkdtemp(prefix=f"wseval-{task['name']}-"))
    # Copy the active global config into an isolated home, so evals can use the
    # same provider/model without reading or writing the user's real memory DB.
    home = d / ".worksmith-home"
    home.mkdir(parents=True, exist_ok=True)
    global_cfg = active_global_config()
    if global_cfg.exists():
        shutil.copy(global_cfg, home / "config.toml")

    # Reuse the repo's project config so the eval uses the same project defaults.
    ws = d / ".worksmith"
    ws.mkdir(parents=True, exist_ok=True)
    repo_cfg = REPO / ".worksmith" / "config.toml"
    if repo_cfg.exists():
        shutil.copy(repo_cfg, ws / "config.toml")
    for rel, content in (task.get("files") or {}).items():
        p = d / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(content)
    # Binary/prepared fixtures copied from evals/fixtures/.
    for fx in task.get("fixtures") or []:
        src = FIXTURES_DIR / fx
        dst = d / fx
        dst.parent.mkdir(parents=True, exist_ok=True)
        if src.is_dir():
            # A whole project. `fixture_root = "<name>"` puts its contents at the
            # workdir root, so the agent starts inside the project.
            shutil.copytree(src, d if task.get("fixture_root") == fx else dst,
                            dirs_exist_ok=True)
        else:
            shutil.copy(src, dst)
    if task.get("context"):
        force_context(ws / "config.toml", model, int(task["context"]))
    if token_budget:
        set_token_budget(ws / "config.toml", token_budget)
    if seed_memory:
        seed_project_memory(ws / "memory.db", task.get("memory") or [])
    return d


def parse_events(stdout: str) -> dict:
    model_calls = tool_calls = gen_tokens = ctx_peak = reasoning_tokens = 0
    model_ms = first_output_ms = prompt_tps = decode_tps = metrics_calls = first_outputs = 0
    outcome = None
    session_id = None
    memory_ids: list[str] = []
    context_breakdown: dict = {}
    by_tool: dict[str, int] = {}
    tool_errors: dict[str, int] = {}
    for line in stdout.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            e = json.loads(line)
        except json.JSONDecodeError:
            continue
        t = e.get("type")
        if t == "session_started":
            session_id = e.get("id")
        elif t == "usage":
            model_calls += 1
            gen_tokens += e.get("completion_tokens", 0)
            # Reasoning is part of completion_tokens; splitting it out is what
            # tells you whether a thinking level cost anything.
            reasoning_tokens += e.get("reasoning_tokens", 0)
            ctx_peak = max(ctx_peak, e.get("prompt_tokens", 0))
        elif t == "model_metrics":
            metrics_calls += 1
            model_ms += e.get("total_ms", 0)
            prompt_tps += e.get("prompt_tokens_per_second", 0.0)
            decode_tps += e.get("completion_tokens_per_second", 0.0)
            if e.get("purpose") != "helper":
                ctx_peak = max(ctx_peak, e.get("prompt_tokens", 0))
                context_breakdown = e.get("context_breakdown") or context_breakdown
            if e.get("first_output_ms") is not None:
                first_outputs += 1
                first_output_ms += e["first_output_ms"]
        elif t == "tool_call":
            tool_calls += 1
            # By name, because "which tool" is the question behind the
            # whole-file rewrite problem: worksmith already has an anchored
            # `edit`, so the interesting number is whether the model reaches
            # for it or for `write`, and whether `edit` is failing when it does.
            by_tool[e.get("name") or "?"] = by_tool.get(e.get("name") or "?", 0) + 1
        elif t == "tool_result" and not e.get("ok", True):
            n = e.get("name") or "?"
            tool_errors[n] = tool_errors.get(n, 0) + 1
        elif t == "memory_used":
            memory_ids.extend(e.get("ids") or [])
        elif t == "turn_complete":
            outcome = e.get("outcome")
    avg_first = round(first_output_ms / first_outputs) if first_outputs else 0
    avg_prompt_tps = round(prompt_tps / metrics_calls, 1) if metrics_calls else 0.0
    avg_decode_tps = round(decode_tps / metrics_calls, 1) if metrics_calls else 0.0
    return {"model_calls": model_calls, "tool_calls": tool_calls,
            "gen_tokens": gen_tokens, "reasoning_tokens": reasoning_tokens,
            "ctx_peak": ctx_peak, "model_ms": model_ms,
            "first_output_ms": avg_first, "prompt_tps": avg_prompt_tps,
            "decode_tps": avg_decode_tps, "context_breakdown": context_breakdown,
            "outcome": outcome, "session_id": session_id,
            "memory_ids": memory_ids, "memory_used": bool(memory_ids),
            "by_tool": by_tool, "tool_errors": tool_errors}


def validate(workdir: Path, cmd: str) -> bool:
    r = subprocess.run(["bash", "-lc", cmd], cwd=workdir,
                       capture_output=True, text=True)
    return r.returncode == 0


def run_one(binp: str, task: dict, mode: str, model: str | None, timeout: int,
            keep: bool = False, worker_model: str | None = None,
            fast: bool = False, think: str | None = None,
            trace_to: Path | None = None, build: dict | None = None,
            token_budget: int | None = None) -> dict:
    workdir = setup_workdir(task, seed_memory=mode == "memory",
                            model=model or configured_model(), token_budget=token_budget)
    # Unattended and trusted: the approval gate has nobody to ask here, and a
    # refusal would score as a task failure rather than the safety behaviour it
    # is. Real interactive runs prompt instead.
    # --trust-project: the workdir is a fresh temp dir each run, and trust is
    # keyed by path, so the config this harness just wrote there would be
    # untrusted and silently ignored. It is our own file.
    cmd = [binp, "--mode", "json", "--approve-all", "--trust-project"]
    if model:
        cmd += ["--model", model]
    if fast:
        cmd.append("--fast")
    elif think:
        cmd += ["--think", think]
    if mode == "workers":
        # `spawn` is a subcommand, so its args come after it. The task declares
        # how many workers it decomposes into; a task that doesn't decompose
        # isn't run in this mode at all (see main).
        cmd += ["spawn", "-n", str(task["workers"])]
        if worker_model:
            cmd += ["--worker-model", worker_model]
    elif mode == "guided":
        cmd += ["--until", task["validate"]]
    cmd.append(task["goal"])

    # `thinking` labels the row: "off" (--fast), a level, or "default".
    thinking = "off" if fast else (think or "default")
    row = {"task": task["name"], "mode": mode, "fast": fast, "thinking": thinking,
           "passed": False, "model_calls": 0, "tool_calls": 0, "gen_tokens": 0,
           "reasoning_tokens": 0, "outcome": None, "elapsed": 0.0, "error": None,
           "ctx_peak": 0, "model_ms": 0, "first_output_ms": 0,
           "prompt_tps": 0.0, "decode_tps": 0.0, "context_breakdown": {},
           "memory_used": False, "memory_ids": [], "timed_out": False,
           "token_budget": token_budget}
    t0 = time.time()
    try:
        env = os.environ.copy()
        env["WORKSMITH_HOME"] = str(workdir / ".worksmith-home")
        r = subprocess.run(cmd, cwd=workdir, capture_output=True, text=True,
                           timeout=timeout, env=env)
        row.update(parse_events(r.stdout))
        if row.get("session_id"):
            try:
                stats = subprocess.run([binp, "stats", row["session_id"], "--json"],
                                       cwd=workdir, capture_output=True, text=True,
                                       timeout=30, env=env)
                if stats.returncode == 0:
                    row.update(metrics_fields(json.loads(stats.stdout)))
                else:
                    row["metrics_error"] = (stats.stderr or "stats unavailable")[-500:]
            except (subprocess.TimeoutExpired, OSError, ValueError, KeyError, TypeError) as exc:
                row["metrics_error"] = f"stats unavailable: {exc}"[-500:]
        row["passed"] = validate(workdir, task["validate"])
        # Keep stderr even on success: it carries the planner's account of how
        # it split the work, which is the only explanation of a surprising
        # fan-out. Discarding it on exit 0 hid that for four runs.
        stderr = (r.stderr or "").strip()
        if stderr:
            row["stderr"] = stderr[-1500:]
        if r.returncode != 0 and not row["passed"]:
            row["error"] = stderr[:200]
    except subprocess.TimeoutExpired:
        row["error"] = f"timeout after {timeout}s"
        row["timed_out"] = True
    finally:
        row["elapsed"] = round(time.time() - t0, 1)
        if trace_to is not None:
            saved = save_trace(workdir, trace_to, {
                "task": task["name"], "mode": mode, "model": model,
                "thinking": thinking, "passed": row["passed"],
                "outcome": row.get("outcome"), "session_id": row.get("session_id"),
                "validate": task["validate"], "error": row.get("error"),
                "build": build, "token_budget": token_budget,
                "timed_out": row["timed_out"],
            })
            if saved:
                row["trace"] = str(saved)
        # Keeping the workdir is the difference between diagnosing a failure and
        # guessing at it from token counts.
        if keep and not row["passed"]:
            row["workdir"] = str(workdir)
            print(f"  kept {workdir}", file=sys.stderr)
        else:
            shutil.rmtree(workdir, ignore_errors=True)
    return row


def metrics_fields(stats: dict) -> dict:
    """Headline accounting comes from Worksmith, including helpers and workers."""
    total = stats["combined"]
    return {"metrics": stats, "model_calls": total["calls"],
            "tool_calls": total["tool_calls"], "gen_tokens": total["completion_tokens"],
            "reasoning_tokens": total["reasoning_tokens"], "model_ms": total["model_ms"],
            "prompt_tokens": total["prompt_tokens"], "cached_tokens": total["cached_tokens"],
            "known_cost_usd": total["known_cost_usd"], "unpriced_calls": total["unpriced_calls"]}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--task", help="one task name, or several separated by commas")
    ap.add_argument("--length", choices=["short", "long", "all"], default="all",
                    help="short: seconds-to-a-minute mechanics checks; long: "
                         "long-context runs (tasks declare `length`, default short)")
    ap.add_argument("--modes", default="raw,guided")
    ap.add_argument("--model")
    ap.add_argument("--timeout", type=int, default=None,
                    help="override every task's wall-clock safety net (seconds); "
                         "the real limit is each task's token budget")
    ap.add_argument("--repeat", type=int, default=1,
                    help="runs per task/mode (averages over model nondeterminism)")
    ap.add_argument("--json")
    ap.add_argument("--think", metavar="LEVEL",
                    help="thinking level to test: minimal|low|medium|high, a token "
                         "budget, on, or off. Mutually exclusive with --fast, which "
                         "is the same as --think off.")
    ap.add_argument("--fast", action="store_true",
                    help="run with thinking off (--fast); pair with --modes to ask "
                         "whether the loop can substitute for the model's deliberation")
    ap.add_argument("--worker-model",
                    help="model the spawned workers run on (workers mode); the "
                         "synthesis still uses --model")
    ap.add_argument("--dry-run", action="store_true",
                    help="print the command for each run without executing it")
    ap.add_argument("--keep", action="store_true",
                    help="keep the scratch dir of any run that fails, for inspection")
    ap.add_argument("--traces", type=Path, default=None,
                    help="where each attempt's sessions and verdict are kept "
                         "(default: $WORKSMITH_HOME/eval-traces/<timestamp>)")
    ap.add_argument("--no-traces", action="store_true",
                    help="discard the sessions as before")
    args = ap.parse_args()
    traces = None if args.no_traces else (args.traces or default_traces_dir())

    modes = [m.strip() for m in args.modes.split(",") if m.strip()]
    tasks = load_tasks(args.task, args.length)
    if not tasks:
        print("no matching tasks", file=sys.stderr)
        return 1

    if traces and not args.dry_run:
        print(f"keeping traces in {traces}  (mine with: python3 evals/failures.py "
              f"--root {traces})", file=sys.stderr)
    print("building worksmith…", file=sys.stderr)
    subprocess.run(["cargo", "build", "--quiet", "--manifest-path", str(MANIFEST)], check=True)
    if args.dry_run:
        binp, build = worksmith_bin(), None
    else:
        # Run a private copy, never the shared build: see snapshot_binary.
        binp = str(snapshot_binary(
            Path(worksmith_bin()),
            traces if traces else Path(tempfile.mkdtemp(prefix="wseval-bin-"))))
        build = build_info(Path(binp))
        print(f"measuring {build['version']} at {build['commit']}"
              f"{' (uncommitted changes)' if build['dirty'] else ''}; binary {binp}",
              file=sys.stderr)

    rows = []
    for task in tasks:
        for mode in modes:
            if mode == "workers" and not task.get("workers"):
                print(f"skipping {task['name']} [workers]: no `workers = N` in the task "
                      "(it doesn't decompose)", file=sys.stderr)
                continue
            if mode == "memory" and not task.get("memory"):
                print(f"skipping {task['name']} [memory]: no [[memory]] rows in the task",
                      file=sys.stderr)
                continue
            for i in range(args.repeat):
                tag = f"{task['name']} [{mode}]" + (f" {i+1}/{args.repeat}" if args.repeat > 1 else "")
                if args.dry_run:
                    wd = setup_workdir(task, seed_memory=mode == "memory")
                    # Unattended and trusted: nobody is here to approve, and
                    # a refusal would score as a task failure rather than the
                    # safety behaviour it is. --trust-project because the
                    # workdir is a fresh temp dir each run and trust is keyed
                    # by path, so the config this harness just wrote there
                    # would be untrusted and silently ignored. It is our file.
                    cmd = [binp, "--mode", "json", "--approve-all",
                           "--trust-project"]
                    if args.model:
                        cmd += ["--model", args.model]
                    if args.fast:
                        cmd.append("--fast")
                    elif args.think:
                        cmd += ["--think", args.think]
                    if mode == "workers":
                        cmd += ["spawn", "-n", str(task["workers"])]
                        if args.worker_model:
                            cmd += ["--worker-model", args.worker_model]
                    elif mode == "guided":
                        cmd += ["--until", "<validate>"]
                    cmd.append("<goal>")
                    shutil.rmtree(wd, ignore_errors=True)
                    print(f"  {tag}: {' '.join(cmd)}", file=sys.stderr)
                    continue
                print(f"running {tag} …", file=sys.stderr)
                # A task may declare its own budget. newsletter-judge writes
                # three full drafts plus a review in one turn; 240s kills it
                # mid-stream and reports it as a failure of the harness rather
                # than of the clock.
                token_budget, budget = limits(task, args.timeout)
                trace_to = (traces / f"{task['name']}-{mode}-{i + 1}") if traces else None
                row = run_one(binp, task, mode, args.model, budget, args.keep,
                              args.worker_model, args.fast, args.think, trace_to, build,
                              token_budget)
                row["run"] = i
                rows.append(row)
                mark = "PASS" if row["passed"] else ("TIME" if row.get("timed_out") else "FAIL")
                extra = f" ({row['error']})" if row["error"] else ""
                reason = (f" reason_tok={row['reasoning_tokens']}"
                          if row["reasoning_tokens"] else "")
                memory = " memory=used" if row.get("memory_used") else ""
                print(f"  {mark}  {row['elapsed']}s calls={row['tool_calls']} "
                      f"gen_tok={row['gen_tokens']}{reason} ctx={row['ctx_peak']} "
                      f"decode/s={row['decode_tps']} "
                      f"outcome={row['outcome']}{memory}{extra}",
                      file=sys.stderr)
                # Write partial results after each run so a killed run isn't lost.
                if args.json:
                    Path(args.json).write_text(json.dumps(rows, indent=2))

    # Aggregate per (task, mode).
    def agg(task_name, mode):
        rs = [r for r in rows if r["task"] == task_name and r["mode"] == mode]
        n = len(rs)
        passed = sum(1 for r in rs if r["passed"])
        avg_calls = sum(r["tool_calls"] for r in rs) / n if n else 0
        avg_tok = sum(r["gen_tokens"] for r in rs) / n if n else 0
        return passed, n, avg_calls, avg_tok

    print("\n=== results (pass rate per task) ===")
    header = f"{'task':<16}"
    for mode in modes:
        header += f" {mode + ' pass':>12} {mode + ' tok':>10}"
    print(header)
    for task in tasks:
        line = f"{task['name']:<16}"
        for mode in modes:
            passed, n, _calls, tok = agg(task["name"], mode)
            if n == 0:
                line += f" {'—':>12} {'—':>10}"
            else:
                line += f" {f'{passed}/{n}':>12} {tok:>10.0f}"
        print(line)

    print("\n=== summary ===")
    for mode in modes:
        mr = [r for r in rows if r["mode"] == mode]
        if not mr:
            continue
        passed = sum(1 for r in mr if r["passed"])
        timed_out = sum(1 for r in mr if r.get("timed_out"))
        gen = sum(r["gen_tokens"] for r in mr)
        reason = sum(r["reasoning_tokens"] for r in mr)
        secs = sum(r["elapsed"] for r in mr)
        # Cost per *solved* task is the number that decides whether a thinking
        # level is worth it; totals reward doing nothing.
        per = round(gen / passed) if passed else "-"
        print(f"  {mode:<8} {passed}/{len(mr)} passed, {len(mr) - passed - timed_out} failed, "
              f"{timed_out} timed out   gen_tokens={gen} "
              f"(reasoning={reason}, {per}/solved)   {round(secs)}s")
        if timed_out:
            print(f"           {timed_out} hit the wall-clock safety net, not the token budget: "
                  "that measures the machine, so read them apart from failures")
        known = sum(r.get("known_cost_usd", 0.0) for r in mr)
        complete = all("metrics" in r and not r.get("unpriced_calls")
                       and not r["metrics"].get("warnings") for r in mr)
        dollars = f"${known / passed:.6f}/solved" if complete and passed else "$/solved unavailable"
        print(f"           recorded cost=${known:.6f}   {dollars}"
              + (" (incomplete pricing or metrics)" if not complete else ""))

    if args.json:
        Path(args.json).write_text(json.dumps(rows, indent=2))
        print(f"\nwrote {args.json}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
