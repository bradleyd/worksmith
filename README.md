# Worksmith

A minimal terminal coding-agent harness in Rust, built on the bet that the
**harness** — not the model — should do the work of keeping weaker/cheaper
models on task and driving to a *validation*. See [`PLAN.md`](PLAN.md) and
[`worksmith-memory-v1.md`](worksmith-memory-v1.md).

Status: **M1–M3 and M5–M7 done**, plus skills, a `web` tool, and fast mode.
A usable single-agent coding harness with streaming, model-driven tools, JSONL
sessions and SQLite memory (M1); a validation-driven self-correcting loop,
context compaction, and a four-channel ratatui TUI (M2); document tools for
PDF/DOCX (M3); memory retrieval + project knowledge (M5); background workers
with fan-out, headless `worksmith spawn`, and per-worker models (M6); and a
rules-based supervisor watching them (M7). MCP/plugins (M4) are still ahead.

Two measured results, both in [`evals/README.md`](evals/README.md):
**the loop is worth +34 points on a small model** (qwen3.5-9b: 52% → 86% at
flat cost per solved task, while the same loop is dead weight on a 27B that
self-checks), and **a frontier model asked to check written-down rules
reported a verdict it had not verified** — twice — which the deterministic
checks caught in milliseconds.

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
- **Cheap workers, smart parent:** `agents.model` (or `/spawn --worker-model
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
- **Thinking control** (`--fast` / `--think [budget]`, `/fast` and `/think`,
  `agent.thinking`): fast mode answers without a reasoning pass — the
  feeling-lucky button. Measured on qwen3.5-9b, same question: 101 completion
  tokens thinking vs 13 without. The bet is that the validation loop catches
  what deliberation would have, which makes it the biggest single cost lever in
  the harness.

  `thinking = 2000` is the setting in between. Small models have no sense of a
  budget: given `max-tokens = 8192` and no cap on reasoning, one will spend all
  8192 deliberating and return nothing at all. A budget caps the reasoning
  alone, so the rest is still there for an answer. Only the `reasoning` dialect
  can express it; on a chat-template provider it degrades to plain "on" and
  says so rather than pretending.

  Nothing is sent unless you ask: providers disagree on the field (`reasoning`
  vs `chat_template_kwargs`) and an unrecognized one is a 400. The dialect is
  guessed from the endpoint — a hostname heuristic, so set `thinking-param`
  explicitly behind a proxy — and a 400 on a thinking request names the field
  that was sent and where that choice came from.
- **Project trust**: a project's `.worksmith/config.toml` is *code* — it can set
  `agent.validate` (a shell command the harness runs unattended after every
  turn) and add a provider whose `base-url` points anywhere, sending your
  prompts and file contents there. So it isn't applied just because you `cd`'d
  into the repo: worksmith shows what the file changes, flags the settings that
  run code or move data, and asks once. The answer is remembered by content, so
  a `git pull` that edits the config asks again. Headless runs never prompt —
  they ignore the file and say so, or take `--trust-project`. `/trust` shows the
  current decision and `/trust revoke` reopens it.
- **Steering a running turn**: type while the model works and press Enter — the
  message lands before its next model call, so a correction ("no, use the other
  file") arrives while it still matters instead of after. Anything typed just as
  a turn ends starts the next one rather than disappearing. This is what
  "human-in-the-loop is a feature, not a fallback" has to mean in practice.
- **Checked workers** (`/spawn --until "<check>"`, `[agents] validate`): a
  spawned worker gets the same validation-driven loop as the main agent —
  re-planning until the check passes instead of stopping when the model says it
  is done. It is opt-in rather than inherited from the session: workers share one
  working tree, so a fan-out of N would run the check N times concurrently in the
  same directory. Fine for a read-only check today; the general answer is a tree
  per worker (PLAN M11).
- **Approval gate**: catastrophic commands (`rm -rf /`, `mkfs`, `curl | sh`) are
  refused outright; *outward-facing or irreversible* ones — `git push`, `sudo`,
  `cargo publish`, `curl -X POST`, `kubectl delete`, writes outside the working
  directory — prompt before running (`y` once, `a` for the session, `n` to
  decline). A denial is not fatal: the model is told what was skipped and
  continues. Where nothing can prompt (`--print`, `--mode json`) the answer is
  **no**, because a headless agent that pushes because nobody objected is the
  failure this prevents; `--approve-all` opts out for unattended runs you trust.
  Ordinary work — `cargo test`, `git commit`, `grep` — never prompts, because a
  prompt answered reflexively is worse than no prompt. It raises the cost of an
  accident, not of an attack: real isolation is the sandbox (PLAN M11).
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
  rather than write — `/memory pending` and `/memory approve <id|all>` review
  them. Ids accept any unique prefix (git-style) and Tab completes them.
  `/memory extract` distills the current session into at most a few candidates
  using a classifier biased toward saving nothing.
- **Memory mining** (`/memory mine [n]`): reads *past* sessions of the current
  project and files what they taught as proposals. Asking a small model to
  volunteer memories mid-turn doesn't work — across 1021 recorded sessions the
  `memory` tool was called seven times, every one a search — so the archive is
  read afterwards instead, newest first, skipping sessions already mined and
  ones too slight to be worth a model call. Everything lands in `/memory
  pending`, scoped to the project the session ran in: a lesson from one repo is
  not a global fact.
- **Knowledge** (`.worksmith/knowledge.db`): the project's own docs and source,
  chunked on paragraph boundaries and FTS5-indexed, searched via the `knowledge`
  tool or `/knowledge search`. The index maintains itself — a search indexes on
  demand and re-checks the tree at most once a minute — so the first query works
  with no setup; `/knowledge index` forces a rebuild. It's rebuildable by design and never injected
  into the prompt wholesale — memory is what was *decided*, knowledge is what the
  repo *says*.

## Install

**Homebrew** (macOS, no Rust toolchain needed):

```sh
brew tap bradleyd/worksmith
brew install bradleyd/worksmith/worksmith
```

**Prebuilt binary:** grab the latest `worksmith-<version>-<target>.tar.gz`
from the [releases](https://github.com/bradleyd/worksmith/releases), untar it,
and put `worksmith` on your PATH.

**From source** (needs a Rust toolchain):

```sh
git clone https://github.com/bradleyd/worksmith
cd worksmith
./install.sh          # release build → ~/.local/bin (on PATH)
# ./install.sh --debug for a faster dev build
```

## Quick start

```sh
# 1. Configure a provider (see config.example.toml)
mkdir -p ~/.worksmith
cp config.example.toml ~/.worksmith/config.toml
$EDITOR ~/.worksmith/config.toml     # pick a model; set your endpoint/keys

# 2. Run — full-screen TUI (default in a real terminal)
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

### Running real work

Two setups worth knowing, both configured in `config.example.toml`.

**Hosted, mixing models.** One key, a strong model in the session and cheap
ones doing the legwork:

```sh
export OPENROUTER_API_KEY=...
worksmith --model openrouter/moonshotai/kimi-k3

# in the TUI — three drafters on a cheap model, the session's model judges:
/spawn -n 3 --worker-model openrouter/deepseek/deepseek-v4-flash-0731 \
  "write three candidate newsletter drafts on different topics, then pick one"
```

That exact shape produced three complete newsletters and a reasoned decision,
all passing a written rubric, for about $0.05.

**Self-hosted vLLM.** Serve with tool-calling on, or the agent has no hands:

```sh
vllm serve Qwen/Qwen3.5-9B --enable-auto-tool-choice \
  --tool-call-parser hermes --enable-prefix-caching
worksmith --model vllm/Qwen/Qwen3.5-9B --until "cargo test" "fix the failing test"
```

Small local models are where the validation loop earns its keep — and where
`--fast` matters most, since a thinking model can spend its whole token budget
deliberating and return nothing. When that happens the loop nudges it to answer
and, if it keeps coming back empty, ends the turn as stuck rather than reporting
a silent success.

**Mixing local with hosted** works too, but watch memory: two resident models on
one machine will exhaust unified memory. Straddle instead (local workers,
hosted judge), or use `worksmith spawn --no-synthesis` and swap models between
the two commands.

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
token counts, including `↻` for reasoning tokens (live as they stream) and
`⚠cut` when the last completion was truncated rather than finished.

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
/memory search <query> | /memory extract | /memory mine [n]
/memory pending | /memory approve <id|all>
/mouse [on|off]           wheel scrolling vs. selecting text to copy
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

### Cutting a release

1. Bump `version` in `Cargo.toml`, then in the tap's formula
   (`bradleyd/homebrew-worksmith` → `Formula/worksmith.rb`: the URL's
   `v<version>` tag + version inside the tarball name).
2. Push; tag `v<version>` — the release workflow builds the macOS arm64 and
   Linux x86_64 (musl static) binaries and attaches them to the GitHub release.
3. Fill the formula's `sha256` from the macOS release artifact and push the
   tap. Users then get it with `brew upgrade`.
