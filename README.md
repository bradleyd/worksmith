# Worksmith

A terminal coding agent for people running small, cheap, or local models.

Codex, Gemini CLI, and pi are thin wrappers around a frontier model that mostly
stays on task. Worksmith bets the other way. The harness does the work of
keeping a weaker model honest. It will not call a task done until a check you
named actually passes. When the model spins, it notices, and sends it back with
the failure output instead of accepting "I'm finished."

```sh
worksmith --until "cargo test" "make the failing test pass"
```

The model stops when the test passes, not when it says so.

## Is this for you?

Probably yes if you run models locally (vLLM, llama.cpp, Ollama) or on cheap
hosted endpoints. Or if you want work gated on a real check instead of a model's
self-assessment, you live in a terminal, or you want to hand background work to
several agents at once without babysitting each.

Probably not if you drive a frontier model that already checks its own work. The
eval below found the loop is dead weight there, spending tokens for no gain. Also
not if you want IDE integration, a GUI, or a hosted service. Worksmith is one
Rust binary that talks to any OpenAI-compatible endpoint, and nothing else.

## The bet, measured

Both numbers come from [`evals/README.md`](evals/README.md), over the same seven
tasks.

**Worth +34 points on a small model.** qwen3.5-9b went from 52% to 86% (11/21 to
18/21) with validation on, at flat cost per solved task: 640 generated tokens
before, 658 after. All ten of the unguided failures had outcome `done`. The
model declared itself finished and was wrong.

**On a capable 27B it changed nothing.** 21/21 either way, for about 18% more
tokens. Guidance turns confidently-wrong into correct. It cannot manufacture
capability a model lacks, and above some line it is pure overhead.

That narrows the pitch on purpose. This earns its keep when the model is weak
enough to need it.

## Install

**Homebrew** (macOS, no Rust toolchain needed):

```sh
brew tap bradleyd/worksmith
brew install bradleyd/worksmith/worksmith
```

**Prebuilt binary:** grab the latest `worksmith-<version>-<target>.tar.gz` from
the [releases](https://github.com/bradleyd/worksmith/releases), untar it, and
put `worksmith` on your PATH.

Pick one location and stick to it. `install.sh` writes to `~/.local/bin` and
`cargo install --path .` writes to `~/.cargo/bin`; if both exist, whichever comes
first on your PATH wins, and you can spend a while debugging a bug you already
fixed. `which -a worksmith` shows the duplicates.

**From source** (needs a Rust toolchain):

```sh
git clone https://github.com/bradleyd/worksmith
cd worksmith
./install.sh          # release build → ~/.local/bin (on PATH)
# ./install.sh --debug for a faster dev build
```

## Quick start

```sh
# First run creates ~/.worksmith and leaves an annotated config.example.toml
# there. Copy it to config.toml, then set `model` and its [providers.*] section.
# If it starts unconfigured, worksmith prints both paths.
worksmith                                  # full-screen TUI

# The point of the thing. Work until a check passes.
worksmith --until "cargo test" "make the failing test pass"

worksmith --print "summarize src/main.rs"  # one-shot, pipe-friendly
worksmith --mode json "list the rust files" # machine-readable event stream
worksmith --plain                          # line REPL instead of the TUI
```

Inside, `/help` lists the commands. `Esc` or `jj` switches to reading the
transcript, where `/` searches and `y` yanks. `Ctrl+C` quits.

## Running real work

Two setups worth knowing, both configured in `config.example.toml`.

**Hosted, mixing models.** One key, a strong model in the session and cheap
ones doing the legwork:

```sh
export OPENROUTER_API_KEY=...
worksmith --model openrouter/moonshotai/kimi-k3

# In the TUI. Three drafters on a cheap model, the session's model judges.
/spawn -n 3 --worker-model openrouter/deepseek/deepseek-v4-flash-0731 \
  "write three candidate newsletter drafts on different topics, then pick one"
```

That exact shape produced three complete newsletters and a reasoned decision,
all passing a written rubric, for about $0.05.

**Finding out what a local server accepts.** Most are FastAPI-based and publish
their request schema, which settles the question better than trying a field and
watching what happens (a server that ignores a field looks the same as one that
honors it):

```sh
curl -s http://127.0.0.1:8000/openapi.json \
  | python3 -c "import json,sys; \
    print(*json.load(sys.stdin)['components']['schemas']['ChatCompletionRequest']['properties'])"
```

Servers disagree about names as well as support: oMLX calls the reasoning budget
`thinking_budget`, vLLM calls it `thinking_token_budget`. That is why
`reasoning-budget-param` is something you set rather than something worksmith
guesses.

**Self-hosted vLLM.** Serve with tool-calling on, or the agent has no hands:

```sh
vllm serve Qwen/Qwen3.5-9B --enable-auto-tool-choice \
  --tool-call-parser hermes --enable-prefix-caching
worksmith --model vllm/Qwen/Qwen3.5-9B --until "cargo test" "fix the failing test"
```

Small local models are where the validation loop earns its keep. They are also
where `--fast` matters most, because a thinking model can spend its whole token
budget deliberating and return nothing at all. When that happens the loop nudges
it to answer. If it keeps coming back empty, the turn ends as stuck instead of
reporting a silent success.

**Mixing local with hosted** works too, but watch memory. Two resident models on
one machine will exhaust unified memory. Straddle instead, with local workers
and a hosted judge, or run `worksmith spawn --no-synthesis` and swap models
between the two commands.

## Status

Everything in the next section works today. MCP and a real sandbox are still
ahead. [`PLAN.md`](PLAN.md) §10a records what is being built next, and why in
that order. Design notes live in [`PLAN.md`](PLAN.md) and
[`worksmith-memory-v1.md`](worksmith-memory-v1.md).

## What works today

- **Streaming, tool-calling agent loop** against any OpenAI-compatible endpoint
  (vLLM/Qwen, OpenRouter, RunPod, local).
- **Built-in tools:** `read`, `write`, `edit` (exact unique-match, multi-edit,
  atomic), `bash` (timeout + `WORKSMITH_SESSION_ID`), `grep`, `find`, `ls`.
- **Document tools:** `doc` (read/info/convert/extract/create) for PDF/DOCX/…
  via pandoc, poppler, and LibreOffice. Clean text/markdown extraction and
  format conversion, with install hints when an engine is missing.
- **Safety guard:** catastrophic `bash` commands (recursive `rm` of `/`/`~`/`.`/`*`,
  fork bombs, `dd`/`mkfs` to devices, `curl … | sh`, recursive `chmod` of `/`)
  are refused outright and hard-stop the turn. Outward-facing ones prompt (see
  the approval gate below). None of it is a sandbox. It raises the cost of an
  accident, not the cost of an attack (PLAN §10a item 4).
- **Fan-out:** one `/spawn` can become several workers. `/spawn create 3
  separate articles on sqlite` asks a cheap planner whether the request divides
  (it answers "one worker" for most tasks); `-n 3` forces the count;
  `--each-files <regex>` runs one worker per matching file with no model call at
  all. There are no template placeholders. Your prose is kept verbatim and the
  assignment is appended. A fan-out larger than `agents.max` queues and drains as
  slots free (`/agents drop-queued` calls it off). Set `agents.fanout = "off"` to
  make a bare `/spawn` always one worker.
- **Workers report back:** when a worker finishes, its result, changed files,
  and (if the supervisor stopped it) the reason are injected into the *parent's*
  history, either into a running turn via the steering mailbox or into the
  session for the next one. A fan-out group is held until every member finishes and reported
  as one block, then the parent runs a turn combining them into a single answer
  (`agents.synthesize = false` to skip that turn).
- **Headless workers:** `worksmith spawn [-n N | --each-files <regex>]
  [--worker-model <spec>] "<task>"` fans out, waits, reports each worker, then
  has the session's model combine the results. This is the non-interactive form
  of `/spawn`, so scripts and evals can exercise the worker layer.
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
  <provider/model>`) runs workers on a different model than the session. The
  override carries its own client, so the worker model can live behind another
  provider entirely. Several small models draft in parallel; the session's
  stronger model judges what comes back. `/agents` shows which model each worker
  is on.
- **Supervisor:** each worker's event stream is watched by deterministic rules.
  Silence for `agents.stuck-timeout`, the same tool call repeated
  `agents.repeat-threshold` times, an explicit "I'm blocked", or spend past
  `agents.token-budget`. It **nudges** (injects a steering message into the
  running worker) up to `agents.max-nudges` times, then **escalates**: stops the
  worker and reports the partial result with the reason. `/agents nudge <id>
  <message>` steers one by hand. Turn it off with `agents.supervisor = "off"`.
- **Web** (`web` tool): `search` via a configured provider (Brave, Tavily, or a
  self-hosted SearXNG, set `[web]` in config) and `fetch`, which pulls a URL and
  reduces it to readable text. Fetch needs no configuration.
- **Per-model settings** (`[models."provider/model"]`): prices in USD per
  million tokens, which turn the footer's token counts into a running session
  cost, plus the sampling that model asks for (`temperature`, `top-p`,
  `top-k`). Sampling lives here because the right numbers are the model's own:
  Qwen wants 0.6 with thinking on and 0.7 with it off, and those are Qwen's
  numbers, not universal. A model with no entry shows no cost rather than
  $0.00, since unknown is not free.
- **Provider routing** (`/route`, `[providers.*] sort`): `throughput` for the
  fastest tokens/sec, `latency` for the fastest first token, `price` for the
  cheapest. OpenRouter only; other servers ignore it. Deliberately *not* folded
  into `--fast`: `sort` changes which provider serves you, and their endpoints
  differ in quantization and price, so a speed button that silently swaps your
  backend would be a surprise rather than a feature.
- **Thinking control** (`--fast` / `--think [budget]`, `/fast` and `/think`,
  `agent.thinking`): fast mode answers without a reasoning pass. The
  feeling-lucky button. Measured on qwen3.5-9b, same question: 101 completion
  tokens thinking vs 13 without. The bet is that the validation loop catches
  what deliberation would have, which makes it the biggest single cost lever in
  the harness.

  `--think low|medium|high` (also `minimal`, `xhigh`, `max`, or
  `thinking = "low"`) asks in the providers' own vocabulary, which OpenRouter
  and vLLM both take natively. Servers disagree about which levels exist — one
  vLLM build accepts only `xhigh`, `medium` and `low` — so the word is passed
  through and the provider objects if it does not know it.

  `thinking = 2000` is the setting in between. Small models have no sense of a
  budget: given `max-tokens = 8192` and no cap on reasoning, one will spend all
  8192 deliberating and return nothing at all. A budget caps the reasoning
  alone, so the rest is still there for an answer. OpenRouter and OpenAI take it
  as `reasoning.max_tokens`. vLLM has its own, `thinking_token_budget`, which it
  enforces server-side by forcing the reasoning to end, and you opt in with
  `reasoning-budget-param` because the other chat-template servers (llama.cpp,
  LM Studio, Ollama) have no such field. Where a budget genuinely cannot be
  expressed it degrades to plain "on" and says so rather than pretending.

  Nothing is sent unless you ask: providers disagree on the field (`reasoning`
  vs `chat_template_kwargs`) and an unrecognized one is a 400. The dialect is
  guessed from the endpoint, which is a hostname heuristic, so set
  `thinking-param` explicitly behind a proxy. A 400 on a thinking request names
  the field that was sent and where that choice came from.
- **Project trust**: a project's `.worksmith/config.toml` is code. It can set
  `agent.validate`, a shell command the harness runs unattended after every
  turn, and it can add a provider whose `base-url` points anywhere, sending your
  prompts and file contents there. So it is not applied just because you `cd`'d
  into the repo. Worksmith shows what the file changes, flags the settings that
  run code or move data, and asks once. The answer is remembered by content, so
  a `git pull` that edits the config asks again. Headless runs never prompt.
  They ignore the file and say so, or take `--trust-project`. `/trust` shows the
  current decision and `/trust revoke` reopens it.
- **Normal mode** (`jj`, or Esc on an empty composer): read the transcript with
  the keyboard. `j`/`k`, `g`/`G`, `Ctrl+U`/`Ctrl+D`, `/` to search with `n`/`N`
  to cycle matches, and `y` to yank the message under the cursor (the whole
  message, not the wrapped line) to the system clipboard via OSC 52, which works
  over SSH. `i`, `Enter` or `Esc` returns to typing. The mode exists to reclaim
  the alphabet, since `j` and `/` cannot coexist with a composer that eats every
  character. Nothing is mode-only, so a mode you never enter cannot trap you.
  `jj` is configurable (`[tui] insert-escape`, `""` to disable). The window is
  short and the first key is inserted immediately, so a lone `j` followed by a
  pause is only a letter.
- **A picker overlay**: `/help` opens a floating, filterable list of commands
  with descriptions. Type to narrow, ↑↓ to move, Enter to put it in the composer
  (not to run it, so you can look at a command before you fire it), Esc to leave
  with whatever you had typed intact. One component, because everything awkward
  here is picking an opaque thing. Models, sessions and worker ids are next. `/help keys` still prints the full reference.
- **A history you can read afterwards** (`/history`, `/history <session-id>`):
  the session file records what the loop *did*, not only what was said — tool
  calls and their results, model-call boundaries, nudges, validations,
  compactions, warnings, and how the turn ended, each with the time it
  happened. Diagnosing a worker that died otherwise meant reading the model
  server's own logs and correlating timestamps by hand. Per-token deltas are
  left out; they would multiply the file by the length of every answer.
- **Watching a worker** (`/agents tail <id>`): a worker's events go to its own
  bus and never reach the parent's transcript, so `/agents` could report status
  but never what a worker was doing. Each worker now keeps a bounded log of its
  tool calls, output, nudges and validation results, and `tail` streams it into
  the conversation live. It reads by cursor, so following never re-prints what
  you already saw, and it reports a count when a busy worker outruns the cap.
- **Steering a running turn**: type while the model works and press Enter. The
  message lands before its next model call, so telling it to use the other file
  arrives while that still matters instead of after. Anything typed just as a
  turn ends starts the next one rather than disappearing. Human-in-the-loop has
  to mean something in practice, and this is the part that does.
- **Checked workers** (`/spawn --until "<check>"`, `[agents] validate`): a
  spawned worker gets the same validation-driven loop as the main agent. It
  re-plans until the check passes instead of stopping when the model says it is
  done. Opt-in rather than inherited from the session, because workers share one
  working tree, and a fan-out of five would run the check five times at once in
  the same directory. Fine for a read-only check today. The general answer is a
  tree per worker (PLAN M11).
- **Approval gate**: catastrophic commands (`rm -rf /`, `mkfs`, `curl | sh`) are
  refused outright. Outward-facing or irreversible ones prompt before running:
  `git push`, `sudo`, `cargo publish`, `curl -X POST`, `kubectl delete`, and
  writes outside the working directory, whether they come from `write`, `edit`
  or `doc`. Answer `y` once, `a` for the session, or
  `n` to decline. A denial is not fatal. The model is told what was skipped and
  carries on. Where nothing can prompt (`--print`, `--mode json`) the answer is
  no, because a headless agent that pushes because nobody objected is the exact
  failure this prevents. `--approve-all` opts out for unattended runs you trust.
  Ordinary work never prompts, so `cargo test`, `git commit` and `grep` stay
  quiet. A prompt answered reflexively is worse than no prompt at all. None of
  this is isolation. It raises the cost of an accident, not the cost of an
  attack (PLAN M11).
- **Skills**: the [Agent Skills](https://agentskills.io) format as published, so
  a `SKILL.md` you wrote for Claude Code, Codex, or Cursor works here unchanged
  (and vice versa). Found in `<project>/skills/`, `~/.claude/skills/`,
  `~/.worksmith/skills/`, and the project-local versions of both, nearest
  winning. Only each skill's one-line description sits in the prompt; the model
  calls the `skill` tool to load the rest, and reads `references/` itself.
  `/skill` lists them, `/skill <name>` loads one.
- **Typed event stream** → `--mode json` and JSONL session files.
- **Sessions** under `~/.worksmith/sessions/` with `--resume`/`--continue`.
  `WORKSMITH_HOME` relocates the whole global directory (config, sessions,
  global memory), which is useful for throwaway runs and used by the test suite.
- **Config** (`~/.worksmith/config.toml` + project override) and `AGENTS.md` /
  `CLAUDE.md` discovery.
- **Memory** (global + project SQLite, supersede semantics): FTS5 search ranked
  by text match, exact-subject hit, importance, recency, and a project boost.
  The agent reaches it through the `memory` tool (`search` / `remember`), with
  write-time dedup so restatements don't grow the store. Workers *propose*
  rather than write, and `/memory pending` and `/memory approve <id|all>` review
  them. Ids accept any unique prefix (git-style) and Tab completes them.
  `/memory extract` distills the current session into at most a few candidates
  using a classifier biased toward saving nothing.
- **Memory mining** (`/memory mine [n]`): reads *past* sessions of the current
  project and files what they taught as proposals. Asking a small model to
  volunteer memories mid-turn does not work. Across 1021 recorded sessions the
  `memory` tool was called seven times, every one of them a search. So the
  archive is read afterwards instead, newest first, skipping sessions already
  mined and ones too slight to be worth a model call. Everything lands in
  `/memory pending`, scoped to the project the session ran in. A lesson from one
  repo is not a global fact.
- **Knowledge** (`.worksmith/knowledge.db`): the project's own docs and source,
  chunked on paragraph boundaries and FTS5-indexed, searched via the `knowledge`
  tool or `/knowledge search`. The index maintains itself. A search indexes on
  demand and re-checks the tree at most once a minute, so the first query works
  with no setup, and `/knowledge index` forces a rebuild. It is rebuildable by
  design and never injected into the prompt wholesale. Memory is what was
  decided; knowledge is what the repo says.

## The validation loop

With `--until "<command>"` (or `agent.validate` in config, or `/validate <cmd>`
in the REPL), a turn is not done when the model stops talking. It is done when
the command exits 0. On failure, the command's output is fed back as a re-plan
directive and the model tries again, bounded by `agent.max-retries`. The loop
also spots the model repeating identical tool calls, nudges it, then escalates.
That is what keeps weaker models on task.

## TUI

The default interactive mode is a full-screen ratatui interface that renders
four visually distinct channels: **you**, the **assistant**, **tool** activity,
and the model's **thinking**. The footer shows the model, context %, and token
counts, including `↻` for reasoning tokens as they stream and `⚠cut` when the
last completion was truncated rather than finished.

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

## Plain REPL commands (`--plain`)

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
/agents [list|tail <id>|show <id>|kill <id>|nudge <id> <msg>|drop-queued]
/validate <cmd|off>       success check for a turn
@path                     include a file's contents in your message
```

Two differences from the TUI, both from having no event loop at the prompt:
a `/spawn` that needs the planner blocks until it returns, and worker results
are reported (and added to the session) at the next prompt rather than the
moment they finish, with no automatic synthesis turn.

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
2. Push, then tag `v<version>`. The release workflow builds the macOS arm64 and
   Linux x86_64 (musl static) binaries and attaches them to the GitHub release.
3. Fill the formula's `sha256` from the macOS release artifact and push the
   tap. Users then get it with `brew upgrade`.
