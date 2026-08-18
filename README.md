# Worksmith

A minimal terminal coding-agent harness in Rust, built on the bet that the
**harness** — not the model — should do the work of keeping weaker/cheaper
models on task and driving to a *validation*. See [`PLAN.md`](PLAN.md) and
[`worksmith-memory-v1.md`](worksmith-memory-v1.md).

Status: **M1–M3 and M5–M7 done.** Usable
single-agent coding harness with streaming, model-driven tools, JSONL sessions,
and SQLite memory (M1); a validation-driven self-correcting loop, context
compaction, and a four-channel ratatui TUI with a full input editor (M2);
document tools for PDF/DOCX (M3); spawned background workers with fan-out (M6, `/spawn`); and
a rules-based supervisor watching those workers (M7); and memory retrieval +
project knowledge (M5). MCP/plugins (M4) are still ahead.

## What works today

- **Streaming, tool-calling agent loop** against any OpenAI-compatible endpoint
  (vLLM/Qwen, OpenRouter, RunPod, local).
- **Built-in tools:** `read`, `write`, `edit` (exact unique-match, multi-edit,
  atomic), `bash` (timeout + `WORKSMITH_SESSION_ID`), `grep`, `find`, `ls`.
- **Document tools:** `doc` (read/info/convert/extract/create) for PDF/DOCX/…
  via pandoc, poppler, and LibreOffice — clean text/markdown extraction and
  format conversion, with install hints when an engine is missing.
- **Safety guard:** catastrophic `bash` commands (recursive `rm` of `/`/`~`/`.`/`*`,
  fork bombs, `dd`/`mkfs` to devices, `curl … | sh`, recursive `chmod` of `/`)
  are refused and hard-stop the turn. Best-effort, not a sandbox — run untrusted
  work in a container.
- **Fan-out:** one `/spawn` can become several workers. `/spawn create 3
  separate articles on sqlite` asks a cheap planner whether the request divides
  (it answers "one worker" for most tasks); `-n 3` forces the count;
  `--each-files <regex>` runs one worker per matching file with no model call at
  all. There are no template placeholders — your prose is kept verbatim and the
  assignment is appended. A fan-out larger than `agents.max` queues and drains as
  slots free (`/agents drop-queued` calls it off). Set `agents.fanout = "off"` to
  make a bare `/spawn` always one worker.
- **Workers report back:** when a worker finishes, its result, changed files,
  and (if the supervisor stopped it) the reason are injected into the *parent's*
  history — into a running turn via the steering mailbox, or into the session for
  the next one. A fan-out group is held until every member finishes and reported
  as one block, then the parent runs a turn combining them into a single answer
  (`agents.synthesize = false` to skip that turn).
- **Headless workers:** `worksmith spawn [-n N | --each-files <regex>]
  [--worker-model <spec>] "<task>"` fans out, waits, reports each worker, then
  has the session's model combine the results — the non-interactive form of
  `/spawn`, so scripts and evals can exercise the worker layer.
  `--no-synthesis` stops after the drafts, for when the judge needs a model
  that can't be resident at the same time as the workers' (swap between the
  two commands).
- **Sub-workers:** `/spawn <task>` runs a delegated task in a background worker
  (its own session, shared tools/model). When a worker finishes it's announced
  in the transcript with the **files it changed** and its result; `/agents`
  lists live status, `/agents show <id>` shows changed files, the session-file
  path, and the full result, `/agents kill <id>` cancels. Footer shows
  `↑N agents`. Concurrency capped by `agents.max`.
- **Cheap workers, smart parent:** `agents.model` (or `/spawn --model
  <provider/model>`) runs workers on a different model than the session — the
  override carries its own client, so the worker model can live behind another
  provider entirely. Several small models draft in parallel; the session's
  stronger model judges what comes back. `/agents` shows which model each worker
  is on.
- **Supervisor:** each worker's event stream is watched by deterministic rules —
  silence for `agents.stuck-timeout`, the same tool call repeated
  `agents.repeat-threshold` times, an explicit "I'm blocked", or spend past
  `agents.token-budget`. It **nudges** (injects a steering message into the
  running worker) up to `agents.max-nudges` times, then **escalates**: stops the
  worker and reports the partial result with the reason. `/agents nudge <id>
  <message>` steers one by hand. Turn it off with `agents.supervisor = "off"`.
- **Web** (`web` tool): `search` via a configured provider (Brave, Tavily, or a
  self-hosted SearXNG — set `[web]` in config) and `fetch`, which pulls a URL and
  reduces it to readable text. Fetch needs no configuration.
- **Fast mode** (`--fast`, `/fast`, `agent.thinking = "off"`): answer without a
  reasoning pass — the feeling-lucky button. Measured on qwen3.5-9b, same
  question: 101 completion tokens thinking vs 13 without. The bet is that the
  validation loop catches what deliberation would have, which makes it the
  biggest single cost lever in the harness. Nothing is sent unless you ask:
  providers disagree on the field (`reasoning` vs `chat_template_kwargs`) and an
  unrecognized one is a 400, so the dialect is guessed from the endpoint and
  overridable with `thinking-param`.
- **Skills** — the [Agent Skills](https://agentskills.io) format as published, so
  a `SKILL.md` you wrote for Claude Code, Codex, or Cursor works here unchanged
  (and vice versa). Found in `<project>/skills/`, `~/.claude/skills/`,
  `~/.worksmith/skills/`, and the project-local versions of both, nearest
  winning. Only each skill's one-line description sits in the prompt; the model
  calls the `skill` tool to load the rest, and reads `references/` itself.
  `/skill` lists them, `/skill <name>` loads one.
- **Typed event stream** → `--mode json` and JSONL session files.
- **Sessions** under `~/.worksmith/sessions/` with `--resume`/`--continue`.
  `WORKSMITH_HOME` relocates the whole global directory (config, sessions,
  global memory) — useful for throwaway runs and used by the test suite.
- **Config** (`~/.worksmith/config.toml` + project override) and `AGENTS.md` /
  `CLAUDE.md` discovery.
- **Memory** (global + project SQLite, supersede semantics): FTS5 search ranked
  by text match, exact-subject hit, importance, recency, and a project boost.
  The agent reaches it through the `memory` tool (`search` / `remember`), with
  write-time dedup so restatements don't grow the store. Workers *propose*
  rather than write — `/memory pending` and `/memory approve <id>` review them.
  `/memory extract` distills the current session into at most a few candidates
  using a classifier biased toward saving nothing.
- **Knowledge** (`.worksmith/knowledge.db`): the project's own docs and source,
  chunked on paragraph boundaries and FTS5-indexed, searched via the `knowledge`
  tool or `/knowledge search`. The index maintains itself — a search indexes on
  demand and re-checks the tree at most once a minute — so the first query works
  with no setup; `/knowledge index` forces a rebuild. It's rebuildable by design and never injected
  into the prompt wholesale — memory is what was *decided*, knowledge is what the
  repo *says*.

## Quick start

```sh
# 1. Configure a provider (see config.example.toml)
mkdir -p ~/.worksmith
cp config.example.toml ~/.worksmith/config.toml
$EDITOR ~/.worksmith/config.toml     # set your vLLM/Qwen base-url

# 2. Build
cargo build --release

# 3. Run — full-screen TUI (default in a real terminal)
worksmith

# force the plain line REPL instead of the TUI
worksmith --plain

# one-shot, pipe-friendly
worksmith --print "summarize src/main.rs"

# machine-readable event stream
worksmith --mode json "list the rust files"

# validation-driven: keep working until a check passes (the thesis)
worksmith --until "cargo test" "make the failing test pass"
```

### Validation loop (§7a)

With `--until "<command>"` (or `agent.validate` in config, or `/validate <cmd>`
in the REPL), a turn isn't "done" when the model stops talking — it's done when
the command exits 0. On failure, the command's output is fed back as a re-plan
directive and the model tries again (bounded by `agent.max-retries`). The loop
also detects when the model repeats identical tool calls and nudges, then
escalates. This is what keeps weaker models on task.

### TUI

The default interactive mode is a full-screen ratatui interface that renders
four visually distinct channels — **you**, the **assistant**, **tool** activity,
and the model's **thinking** — with a footer showing the model, context %, and
token counts.

Edits from `edit`/`write` render as colored unified diffs so you can see exactly
what changed.

The composer is multi-line and paste-safe (bracketed paste drops a whole
snippet in at the cursor instead of sending it line-by-line), with input history.

Keys: `Enter` send · `Alt+Enter` newline · `Ctrl+G` edit in `$EDITOR` · `↑`/`↓`
input history · `←`/`→`/`Home`/`End` move cursor · `Ctrl+W` delete word · `Tab`
autocomplete (`/command` and `@path`;
repeat to cycle) · `Esc` abort a running turn (or clear input) · `Ctrl+C` quit ·
`Ctrl+O` expand/collapse long tool output & diffs · `Ctrl+T` show/hide thinking
· scroll with the mouse wheel,
`PgUp`/`PgDn`, `Ctrl+U`/`Ctrl+D`, `↑`/`↓`, `Home`/`End`. Commands: `/new`
`/compact` `/memory` `/validate <cmd|off>` `/quit`, and `@path` to include a
file. (Model cycling, vim keybindings, and themes are planned follow-ups.)

### Plain REPL commands (`--plain`)

The line REPL has the same commands as the TUI:

```
/help                     show commands
/quit                     exit
/new                      start a new session
/compact                  summarize the session now
/memory [list|global|project|show <id>|forget <id>|add <scope> <kind> <subject> <content...>]
/memory search <query> | /memory extract | /memory pending | /memory approve <id>
/knowledge [index|search <query>|status]
/spawn [-n N | --each-files <regex>] <task>
/agents [list|show <id>|kill <id>|nudge <id> <msg>|drop-queued]
/validate <cmd|off>       success check for a turn
@path                     include a file's contents in your message
```

Two differences from the TUI, both from having no event loop at the prompt:
a `/spawn` that needs the planner blocks until it returns, and worker results
are reported (and added to the session) at the next prompt rather than the
moment they finish — with no automatic synthesis turn.

Ctrl+C aborts the current turn; Ctrl+D exits.

## Development

```sh
cargo test        # unit + streaming/tool-call integration tests
cargo clippy
```

Tests point `WORKSMITH_HOME` at a per-process scratch directory
(`tests/common/mod.rs`), so a run never touches your real sessions or memory.
