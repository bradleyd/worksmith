# Plan: Worksmith — Minimal Rust Coding Agent CLI

Worksmith is a pi-style terminal coding harness in Rust. Its bet is on the
**harness** — the guidance layer that keeps weaker/cheaper models on task and
drives them to a validation (see §0). Minimal core, aggressive extensibility.
It also ships first-class document tools (.docx, PDF) that pi can't easily offer
from TypeScript — a useful capability, but not the differentiator.

## 0. Thesis (the differentiator)

Codex/Gemini/pi are thin harnesses that assume a strong frontier model which
mostly stays on task. Worksmith bets the other way: **the harness does the
work of making weaker/cheaper models succeed** — keeping them on task,
noticing when they spin, correcting the approach, and refusing to call a task
"done" until something actually *validates*. That's a harness advantage, not
a model advantage, which is why it composes with local/RunPod/cheap models
(see provider priorities in §3).

Two commitments follow:

- **Validation-driven loop (§7a).** The loop terminates on a passing check,
  not on the model *claiming* it finished. Self-correction (stuck detection →
  nudge/re-plan → escalate) runs over the event stream for the single agent
  first, and the supervisor is that same mechanism applied to many workers.
- **Productivity companion, not full autonomy.** Everyone is racing toward
  hands-off agents; our angle is a collaborator. For a fuzzy task, the
  harness *co-designs* the plan with the human up front — "how shall we
  validate this? want an editor worker, then a reviewer?" — offered as a
  menu, not assumed. Human-in-the-loop is a feature, not a fallback.

## 1. Philosophy (inherited from pi)

- Minimal core: 4 agent tools (`read`, `write`, `edit`, `bash`) + optional
  `grep`/`find`/`ls` (or just let `bash` do it — decide early, pi ships them).
- No plan mode, no sub-agents, no to-dos, no permission popups in the
  core. Those become plugins/skills. MCP *is* in core (see §6) — with a
  compiled-plugin model, MCP is how we get dynamic, cross-ecosystem
  extensibility without an npm.
- Everything is a file: sessions are JSONL trees, config is TOML, skills and
  prompt templates are Markdown. No hidden state.
- Four modes from day one: interactive TUI, `--print` (one-shot), `--mode json`
  (event stream), and a small library/SDK surface. The JSONL event stream is
  what makes embedding/integration possible, so design it first, not last.

## 2. What made Codex CLI & Gemini CLI good (and we keep)

| Lesson | Source | Adoption |
|---|---|---|
| Single fast static binary, zero runtime | Codex | Rust → free. Ship via GitHub releases + `cargo-binstall` + brew tap |
| Approval/sandbox modes (`suggest` / `auto-edit` / `full-auto`) | Codex | Ship 3 simple permission modes in core (not popups — yes/no per mode, `--ask` flag for the middle ground) |
| `AGENTS.md` convention, walked up the tree | Codex | Yes, exact same discovery (also accept `CLAUDE.md` for compat) |
| Non-interactive `exec` mode for CI | Codex | That's `--print`; make sure it's excellent and pipe-friendly |
| Cheap fast default model + easy model switching | Gemini | Model cycling in TUI (Ctrl+P), `provider/model` patterns |
| `@file` mentions and `!shell` inline in the prompt | Gemini | In editor + `@files` on CLI (pi has both, replicate) |
| Transparent context: what's loaded, token/cost counters | Gemini | Startup header + footer with tokens/cost/context % |
| Sandbox options (Docker/Landlock/Seatbelt) | Both | Phase 2. v1: no sandbox, document "run in a container" (pi's stance) |

## 3. Architecture

### Relationship to rustopedia/
We already have a Rust agent built for Rust-specific dev work
(`Projects/rustopedia/`: llm.rs, session.rs, retry_loop.rs, tools/, memory.rs,
planner.rs, chroma/qdrant RAG). This project is the **general-purpose**
harness. Strategy:
- **Reuse, don't re-derive:** port the parts of rustopedia that are
general — `llm.rs` (provider calls), `retry_loop.rs`, `session.rs`,
`config.rs`, and the tool-dispatch pattern in `tools/`. It already uses
reqwest/clap/tokio/serde, same picks as this plan.
- **Leave behind:** the Rust-specific bits (syn/quote/proc-macro2 parsing,
chroma/qdrant RAG, `planner.rs`, `intents.rs`) stay in rustopedia.
- **End state for rustopedia:** its Rust-development knowledge becomes a
**skill** (SKILL.md + tool notes for cargo/syn-based checks) that runs on
the new general harness. One binary, rustopedia's brain as content.
  (M6 — only if the new harness proves itself first.)

### Workspace layout

```
worksmith/
├── crates/
│   ├── core/                  # agent loop, session, compaction, tool registry
│   ├── llm/                   # provider clients (Anthropic, OpenAI, Gemini + OpenAI-compat)
│   ├── tui/                   # terminal UI
│   ├── tools/                 # built-in tools: read/write/edit/bash/grep
│   └── doc-tools/             # docx + pdf tools (thin wrappers over CLIs)
├── plugins/                   # example plugin binaries
└── skills/                    # bundled markdown skills
```

Key dependencies:
- `clap` — CLI
- `tokio` — async runtime
- `toml` — config files (TOML, not JSON — it's Rust, and hand-edited config
  wants comments and sections)
- `reqwest` + `eventsource`/SSE — provider streaming (or evaluate `rig`, the
  Rust LLM framework, which already handles tool-calling across providers;
  risk: it adds opinions. Decision gate at spike 1)
- `ratatui` + `crossterm` — TUI (pi wrote its own; ratatui gets us 90% faster)
- `serde` + `serde_json` — data structures. Rule of thumb: TOML for anything
  a human edits (config, models), JSONL for anything that crosses a process
  boundary or grows over time (sessions, event streams, RPC)
- `tokio::process` — bash tool + document engine shelling out
- `notify` — optional file watching

### Agent loop
Same as pi: model call → tool calls → results → repeat, until no tool calls.
Emit every step as a typed JSON event (`message_delta`, `tool_call`,
`tool_result`, `usage`, `compaction`) — this is the `--mode json` output and
the event bus the TUI subscribes to.

### Sessions
JSONL files, each entry `{id, parentId, type, data}` → tree structure,
in-place branching, `/tree` navigation. Auto-save per cwd under
`~/.worksmith/sessions/`. Compaction: summarize old messages, keep recent, full
history stays in the file.

### Providers
Order of priority (drives M0/M1):
1. **OpenAI** — first-class (API + thinking/reasoning).
2. **OpenAI-compatible** — the workhorse. Same client, different base URL:
   **OpenRouter**, **RunPod**, **vLLM** (local/self-hosted), plus anything
   else that speaks the API (llama.cpp, etc.). Configured in `models.toml`:

   ```toml
   [providers.openrouter]
   type = "openai-compat"
   base-url = "https://openrouter.ai/api/v1"
   api-key-env = "OPENROUTER_API_KEY"

   [providers.runpod]
   type = "openai-compat"
   base-url = "https://api.runpod.net/v1"        # or pod-specific endpoint
   api-key-env = "RUNPOD_API_KEY"

   [providers.local]
   type = "openai-compat"
   base-url = "http://localhost:8000/v1"         # vLLM
   ```

   Model lists: fetch from `/v1/models` where available (works on OpenRouter,
   RunPod, vLLM), or hardcode in `models.toml`. No auth needed for vLLM.
3. **Anthropic** — first-class, incl. prompt caching + thinking levels.

API keys from env; no OAuth/subscriptions in v1 (pi's big moat, skip it).

### Reference repos (cloned, read-only, in `reference/`)
Both are TypeScript — nothing is portable to Rust, but both are the right
books to steal **terminal behavior** from. Consult these, not memory:
- `reference/pi/` — the minimal-UX standard. `packages/tui/` is worth a real
  read before M2: `editor-component.ts`, `keybindings.ts`, `kill-ring.ts` +
  `undo-stack.ts` (editor feel), `stdin-buffer.ts` (paste safety),
  `terminal-image.ts` (image paste/drag), `fuzzy.ts` (@file refs),
  `layout-node.ts` (streaming reflow). `packages/coding-agent/` for event
  flow + session format. (Full pi docs also live locally under the
  homebrew install if the clone lags the version.)
- `reference/gemini-cli/` — the UX-richness checklist. `packages/cli/src/ui/`
  (ink/React, not portable) — steal *what* it renders: tool approval
  display, @mention/shell completion, footer status layout, streaming
  presentation, `/help` organization.

Rule for both: replicate behavior, never copy code or file structure.

### Config
TOML, two files, project overrides global (field-level merge):
- `~/.worksmith/config.toml` — model, thinking level, permission mode, doc
  engines, memory sync remote, shortcuts
- `.worksmith/config.toml` — project overrides (gated by the project-trust flow)

Sessions and the JSON event stream stay JSON/JSONL — machine formats, not
hand-edited.

## 4. Built-in tools (core)

1. `read` — files + images (sent to model if vision-capable). Offset/limit.
2. `write` — create/overwrite, mkdir -p semantics.
3. `edit` — exact-match replacements, multiple disjoint edits per call
   (this is the tool that makes the agent *good*; nail the semantics,
   including the "unique match" rules, before anything else).
4. `bash` — with timeout, env passthrough (`WORKSMITH_SESSION_ID` etc. like pi).
5. Optional: `grep`, `find`, `ls` — pi ships these; cheap to add, models use
   them a lot. Include.

## 5. Document tools (a capability, not the moat)

A genuinely useful workflow advantage — handling .docx/PDF well is something pi
can't easily do from TypeScript — but the differentiator is the guidance layer
(§0), not these wrappers. Anyone could shell out to pdftotext/pandoc; almost
no one does the validation-driven, self-correcting loop.

**Not Rust crates.** Document handling shells out to well-proven CLI tools.
The agent harness is a Rust binary; docx/pdf work is delegated to the
battle-tested engines we already use daily. No PDF parser to maintain,
no engine-swap risk, best fidelity available.

### Engines (all installable via brew/apt, detected at startup)

| Job | Primary tool | Fallback |
|---|---|---|
| PDF → text (page-structured) | `pdftotext -layout` (poppler) | `mutool draw -F text` (mupdf) |
| PDF → markdown/images | `pdftotext` + `pdfimages` | `mutool` |
| PDF info/meta | `pdfinfo` | `mutool info` |
| PDF merge/split/edit | `qpdf` | `mutool` |
| PDF → images (OCR/vision) | `pdftoppm` | `mutool draw` |
| DOCX → md/html/text | `pandoc` | `docx2txt` |
| DOCX → PDF | `soffice --headless --convert-to pdf` (LibreOffice) | `pandoc` (needs LaTeX; worse fidelity) |
| md/txt → DOCX | `pandoc` | `soffice` |
| DOCX targeted edit | pandoc round-trip, or python `python-docx` one-liner via `bash` | docx2txt+manual |

Optional: bundle tiny pure-Rust readers (`pdf_oxide` / `office_oxide`) as a
`--pure-rust` feature so the tool works with zero external deps. Nice to
have, not v1.

### Implementation: thin built-in tools, `tokio::process` underneath
One `doc` tool with subcommands (or a few small tools), each just a
command-wrapper with parsed output:
- `doc read path [--pages a-b]` — auto-detect extension, run the right
  engine, return clean text/markdown. **This alone is 80% of the value.**
- `doc extract path --images/--tables outdir`
- `doc create out.{docx,pdf} < markdown or --from input`
- `doc convert in out` — table lookup of engine per (from, to) pair
- `doc info path`

Engine discovery: `which` at startup, warn if missing, print install hint.
Overridable per-engine in config:

```toml
# ~/.worksmith/config.toml
[doc.engines]
pdf-text = ["pdftotext", "mutool"]      # tried in order
docx-pdf = "soffice"
```

A bundled skill file (`.worksmith/skills/docs/SKILL.md`) teaches the model when
to use `doc` vs raw `bash` (e.g. OCR = `pdftoppm` → `read` the PNG if the
model has vision).

Since the model also has `bash`, the CLI tools are usable directly — the
`doc` wrapper just gives better parsing, page selection, and error messages.

## 6. Extensibility (no npm, but dynamic via MCP)

Four tiers, cheapest first:

**Tier 1 — Skills (Markdown, free)** *(done)*
We implement the **Agent Skills spec as published** rather than a private
format: Anthropic released it Dec 2025 and ~32 tools read the same `SKILL.md`
(VS Code, Codex, Cursor, Gemini CLI, Goose, Copilot). Required frontmatter is
`name` + `description`; `scripts/`, `references/`, `assets/` are optional.
Search order, nearest wins: `<project>/skills/`, `~/.claude/skills/`,
`~/.worksmith/skills/`, `<project>/.claude/skills/`,
`<project>/.worksmith/skills/`. `WORKSMITH_HOME` relocates both home paths so a
run can be made reproducible. Progressive disclosure as the spec intends: only
name+description reach the prompt; the `skill` tool fetches a body; the model
reads `references/` itself. Shadowing and malformed skills are reported, not
silent. Prompt templates (`/name` with `{{var}}`) are still unbuilt.

**Tier 2 — External CLI tools (the pi-philosophy answer)**
A tool that is just an executable with a good `--help`/README. The skill
file describes it; `bash` runs it. This covers 90% of what pi npm packages
do, with zero plugin API to maintain. (pi's own docs recommend exactly this.)

**Tier 3 — MCP servers (the dynamic layer, in core)**
Because our compiled plugins are heavier to write/distribute than pi's npm
extensions, MCP fills the gap: a huge existing ecosystem of ready-made
tool servers, fully dynamic (add/remove per project without rebuilding
anything).
- Built-in MCP **client** (evaluate the `rmcp` Rust crate first, fall back
  to hand-rolled — stdio transport is trivial JSON-RPC over a process,
  which we already speak for plugins). Transports: stdio first; HTTP/SSE next.
- Config in TOML:

  ```toml
  # .worksmith/config.toml (project) — servers here are project-scoped
  [mcp.servers.github]
  command = "npx"
  args = ["-y", "@modelcontextprotocol/server-github"]
  env = { GITHUB_TOKEN = "${GITHUB_TOKEN}" }
  ```

- Servers from `~/.worksmith/config.toml` (global) + `.worksmith/config.toml`
  (project, trust-gated) + `--mcp name=command` flag.
- Tools exposed to the model as `mcp__<server>__<tool>`; listable/health
  via `/mcp`, shown in the startup header. Lazy-start servers on first
  tool use to keep startup fast.
- Accept claude-desktop-style `mcpServers` JSON import so existing configs
  just work.

**Tier 4 — Compiled plugins (our native extension format)**
A plugin is a small binary that speaks line-delimited JSON-RPC over
stdin/stdout (same protocol shape as pi's `--mode rpc`). The host:
- launches it, calls `init` → plugin returns `{tools, commands, events}`
- routes tool calls, subscribes to events, renders plugin UI as plain
  markdown/ANSI (no rich component API in v1). Division of labor: MCP =
"consume existing tool servers", plugins = "extend the agent itself"
(custom commands, event hooks, status lines).

Install via `worksmith install github.com/user/plugin` (downloads a release
binary) or a local path. No cargo, no compilation on the user's machine,
no dylib ABI hell, works on any platform, and a plugin can be written in
Rust, Go, Zig, whatever. Tradeoff vs pi: plugins can't inject TUI components
or intercept every hook in v1 — accept that; richness comes from the
event/tool surface, not UI widgets.

Deliberately NOT doing: dynamic-lib loading (`libloading`) and WASM plugins.
Both look clever, both create maintenance traps for a minimal project.

## 7a. Validation-driven loop (core — the thesis, §0)

This is what makes weaker models useful. It lives in the **single-agent loop
from M1/M2**, before workers exist; the supervisor (§7) is the multi-worker
generalization of the exact same machinery.

### Co-designing the goal (companion behavior)
When a task is fuzzy, the harness doesn't guess — it asks, then offers a
menu. Example:
```
/spawn "fetch latest articles on over-engineering, distill the ones about observability"
→ Worksmith: How should we validate this?
   1. Draft only (editor worker) — fastest
   2. Editor → reviewer worker (reviewer checks relevance + citations)  [recommended]
   3. Editor → reviewer → your approval before save
   Validation check: reviewer confirms ≥N sources, each on-topic, deduped.
```
The user picks; the harness records the chosen validation predicate and
worker pipeline for the run. Skippable (`--yes`/config) for users who want it
to just go, but on by default — this is the companion angle, not autonomy.

### The loop itself
- **Terminate on validation, not on "I'm done."** A task carries a success
  predicate: the project's test/build command, a user-supplied check
  (`--until "cargo test passes"`), a reviewer worker's verdict, or "parses/
  compiles/output matches." The model saying it finished is a *proposal*; the
  check is the gate.
- **Stuck detection** (deterministic first): no progress event for N seconds,
  repeated identical tool calls, token spend climbing with no `tool_result`
  change, explicit "blocked."
- **Correction:** nudge (steer the input), or re-plan (ask the model to
  revise its approach) on stall. Bounded retries with timeouts before
  escalation.
- **Escalation:** stop, return partial + reason, optionally re-spawn refined.

### Reuse agent-line (crate dependency)
`agent-line` (already built) has the shape of this: **retries, timeouts, a
validation path, and worker-linking (chaining sub-workers).** Decision: **depend
on the crate**, don't re-derive it. It's sync/threaded while we're
async/tokio, so we bridge — run agent-line runners on a blocking thread pool
(`tokio::task::spawn_blocking`) and surface their outcomes as events on our
JSONL bus. Mapping: `Runner`/`Outcome::Next` → validation predicate +
re-plan/escalate outcomes; its retry/timeout policy → per-task config; its
worker chaining → the editor→reviewer pipelines above. If a specific piece
fights the async model in practice, re-derive just that piece — but start
from the crate.

## 7. Spawned agents (background work, à la agent-line)

Pi's answer is "use tmux, no sub-agents." We want it built-in, because
it composes with everything else we're building (sessions, JSONL events,
memory, doc tools).

### Mechanism: self-spawn over the JSONL event stream
A spawned agent is just a headless instance of ourselves:
```
worksmith --mode json --name "task:foo" --no-session-prompt "<task>"
```
- Child process, `tokio::process`, streams its JSONL events to the parent.
- Parent TUI subscribes to the same event bus it uses for the main session
  → zero new rendering code; a spawned agent renders as a collapsible
  list entry with live status (tool being run, last line, cost).
- Each child gets its own session file (same project dir → shares project
  memory and AGENTS.md, isolated transcript). Killable; on finish, the
  child's final assistant message + session path is injected into the
  parent as a tool result.
- Same cwd by default; `--cwd` to point a worker elsewhere.

### UX
- `/spawn "refactor the parser and add tests"` → launches, returns to
  editor immediately. Footer shows `↑2 agents running`.
- `/agents` → panel: list, tail one (Esc back), kill, show result.
- `worksmith spawn "..."` non-interactive; `worksmith agents` lists running
  workers from a pidfile in the session dir (survives TUI quit).
- Concurrency cap in config (`agents.max = 4`), per-agent model override
  (`/spawn --model local/qwen3 "..."` — cheap models for grunt work on
  vLLM/RunPod, the strong model in the foreground).

### Supervisor (the factory foreman, once workers land)
The supervisor is the validation-driven loop (§7a) scaled to many workers —
a **factory foreman watching the assembly line**. It doesn't do the work; it
watches for trouble and can **pull the andon cord** (halt the whole line) or
pull a single worker off the floor (stop/re-spawn just that one). It's a
lightweight watcher over the worker event streams — **not** a second agent in
the loop of every worker, which would double cost. The parent (or a dedicated
cheap-model observer) reacts to the JSONL events workers already emit:
- **Stuck detection (cheap, deterministic first):** no progress event for N
  seconds, repeated identical tool calls, ballooning token spend with no
  `tool_result` diff, or an explicit "I'm blocked" from the worker → flag.
- **Nudge:** inject a steering message into the worker's input (same
  mechanism as interactive steering) — "you've run the same grep 4×; try a
  different approach" or re-state the goal. Bounded number of nudges before
  escalation.
- **Escalation:** stop the worker, return partial result + reason to the
  parent, optionally re-spawn with a refined task. Config: `agents.supervisor
  = "off" | "rules" | "model"`, `agents.stuck-timeout`, `agents.max-nudges`.
- **Why it's cheap to build here:** the supervisor consumes the *same event
  bus* the TUI and parent already subscribe to (§ Mechanism). Rules-based
  detection is free; the optional model observer runs on a cheap model
  (local/RunPod), watching, not doing. Reuses steering + kill, which spawn
  already implements.

Deliberately staged after spawn works — a supervisor with nothing to
supervise is premature. Its own milestone (M7).

### Relationship to agent-line
agent-line is a **crate dependency** (see §7a "Reuse agent-line"). It's
sync/threaded and we're async/tokio, so we run its runners via
`spawn_blocking` and map their outcomes onto our JSONL event bus. Two things
it gives us directly:
1. **Workflow definition:** a later `workflows/` feature (TOML or SKILL.md
   with steps + outcomes, on top of agent-line's `Runner`/`Outcome::Next`)
   that the harness executes as a chain of spawned agents. M8+.
2. **Fan-out pattern** *(done)*: one `/spawn` → N workers. Shipped as
   `-n N` (exact count), `--each-files <regex>` (one per matching file — a
   regex, matching the `find` tool's idiom rather than a glob), and a default
   `auto` mode where a cheap planner call decides whether the request divides at
   all. Deliberately **no task template/placeholder syntax**: the task stays
   prose and the assignment is appended, because `{}` interpolation is shell
   idiom in a harness where you're writing to a model. Over-cap fan-outs queue.
   Results are collected: a fan-out is a *group*, held until every member
   finishes, then reported to the parent as one block and (by default)
   synthesized into a single answer by one more parent turn.

## 8. Memory (and sharing it)

Pi's answer to memory is just AGENTS.md. We go further: durable memory is
**distilled state in SQLite**, not a pile of markdown files. The full design
lives in `worksmith-memory-v1.md`; this section is the summary and how it
plugs into the harness.

Why SQLite over loose markdown: memory needs supersede-don't-overwrite,
dedup-by-subject, importance/confidence ranking, and hybrid retrieval. Those
are unimplementable on scattered `.md` files without reinventing a database
badly — and markdown that feels tidy at file #5 is unmanageable sprawl at
file #200. SQLite gives all of it in a single, inspectable file.

Division of labor:
- **`AGENTS.md` / `CLAUDE.md`** — human-authored project instructions. Stays
  markdown, walked up the tree, loaded every session (unchanged from §2).
- **Memory** — distilled decisions/constraints/preferences/facts/lessons.
  SQLite (`memory.db`).
- **Knowledge** — rebuildable index of source material (repo files, docs).
  SQLite (`knowledge.db`), post-v1.

### Two databases, same schema, different scope
```text
~/.worksmith/memory.db          # global: user prefs, cross-project conventions
repo/.worksmith/memory.db       # project: decisions, constraints, gotchas
repo/.worksmith/knowledge.db    # project knowledge index (optional, post-v1)
```
Session/worker scratch is **not** durable — it lives in process/session state
and disappears when the session or worker exits, unless explicitly promoted.

### How it works
- The five memory types (decision, constraint, preference, fact, lesson),
  the schema, importance/confidence, supersede semantics, and the write
  policy are all specified in `worksmith-memory-v1.md`. Follow that doc.
- **Retrieval, not a full dump:** on each task, query both DBs (exact-subject
  lookup + FTS5), rank/merge, and inject a small `<MEMORY>` section into the
  prompt. Never load the whole database into context.
- **Conservative writes (v1):** persist only on explicit user request, a
  clear project decision/constraint, or high-confidence end-of-task
  extraction (0–3 candidates per task). Bad durable memory is worse than
  missing memory.
- `/memory` command surface: `list`, `search`, `show <id>`, `forget <id>`,
  `global`, `project` (see memory doc §25).

### Sharing (git, without checking in a binary)
The `.db` is authoritative but **gitignored** — binary blobs don't diff or
merge. Sharing works through a generated markdown view:
1. **Same machine, all projects:** global scope, free.
2. **Across machines / teammates:** `worksmith memory export` writes a
   reviewable, diffable `.worksmith/memory.md` (or per-scope files) from the
   DB; that markdown is what's committed and reviewed in PRs. `worksmith
   memory import` merges it back (dedup + supersede rules apply, so a
   conflicting edit becomes a supersede, not a clobber). Global memory can
   sync the same way against a private git repo. No server, no accounts.
3. **Between agent instances / workers:** same cwd → same project `memory.db`
   opened in WAL mode with a shared busy timeout. No protocol needed.

Deliberately NOT doing in v1: vector/semantic retrieval (Stage 4 in the
memory doc), the events table, the LLM classifier, broadcast/broker, cloud
sync service, memory encryption. Keep the v1 core small and deterministic —
it's the surface open-source contributors build against.

## 9. TUI (pi-parity checklist)

- Editor: multi-line, `@file` fuzzy ref, tab path completion, `!cmd` /
  `!!cmd` bash injection, Ctrl+G external editor, image paste.
- Footer: cwd, session, tokens ↑/↓, cost, context %, model.
- Startup header: loaded AGENTS.md, skills, templates, plugins.
- Commands: `/model` `/login` (keys only in v1) `/resume` `/new`
  `/compact` `/session` `/tree` `/settings` `/memory` `/export` `/quit`.
- Shortcuts: Ctrl+L model, Ctrl+P cycle, Shift+Tab thinking level,
  Ctrl+O collapse tool output, Esc abort, Esc-Esc `/tree`.
- ~~Message queue: Enter = steering, Alt+Enter = follow-up.~~ **Done
  (2026-08-20):** Enter mid-turn steers the running turn (the agent drains its
  mailbox at the top of every step). Alt+Enter stays newline — it was already
  bound and retraining that costs more than the follow-up variant is worth;
  a message that arrives too late to be drained starts the next turn instead,
  which covers the same need. Before this, mid-turn input was cleared from the
  composer and discarded.
- ~~Agent panel: `/agents` list/tail/kill, footer running-count (§7).~~ **Done
  (2026-08-20)** — `tail` was the missing piece: worker events go to their own
  bus, so the parent could show status but not activity. Each worker keeps a
  bounded log; `tail` follows it by cursor. A dedicated *tab* for this is still
  M8; this is the 80% without the layout work.
- Streaming: token-by-token, tool calls rendered inline, collapsible.
- **Role-distinct rendering (four channels).** The user must be able to tell
  at a glance which stream they're looking at. Each gets its own visual
  treatment (color + prefix/gutter), consistent everywhere:
  - **User message** — their input, echoed as a distinct block.
  - **Assistant response** — the model's prose answer (primary text).
  - **Tool calls / results** — dim, gutter-marked (`⚙`), collapsible.
  - **Thinking** — reasoning-field content (§ llm reasoning support), visibly
    de-emphasized (dim/italic) and separable from the answer; toggle to
    hide/show. This is the "watch the weak model think" affordance pi has.
  (The M1 REPL already ships a minimal version: dim thinking, dim `⚙` tool
  lines, plain assistant text. M2 makes it a proper styled layout.)
- **Tabbed layout (later, vim-style).** Split the screen into tabs: a main
  window (the foreground conversation) plus additional tabs for workers/agents
  activity (§7), a **metrics tab** (M9 depth view), `/tree`, etc. Tab-switching
  keys (vim-like). Depends on the agent/worker milestones landing first — see
  M8. The main-view footer stays a quick-glance summary; depth lives in tabs.
- **Scrolling.** Mouse wheel + PageUp/Down + Ctrl+U/D (half-page) + Up/Down +
  Home/End, with follow-tail on new output. (Shipped in M2.)
- **Vim keybindings — navigation done (2026-08-20).** Esc on an empty composer
  enters a normal mode: `j/k`, `g/G`, `Ctrl+U/D`, `/` search with `n/N`, and `y`
  to yank the item under the cursor (OSC 52, so it works over SSH). Modal
  editing in the composer is deliberately *not* done: the mode earns its place
  by shipping search and yank — things the always-insert model cannot do —
  rather than by adding focus for its own sake, and every insert-mode key still
  works, so the mode cannot trap you.

  Click-to-focus was considered and dropped: it needs mouse capture, which is
  what kills terminal text selection, and the preference here is keyboard over
  mouse. `/mouse on` remains for wheel scrolling.
- **A note on menus (2026-08-20):** which-key-style leader trees were considered
and rejected — they solve a modal editor's problem (dozens of chords to
memorize), and this app has one text field with Tab completion. The part worth
stealing is showing what is available *with descriptions* rather than requiring
recall, which is what the picker does. Telescope's shape, not which-key's.

- **Themes (later).** Selectable color themes (e.g. a `[theme]` config section
  or `/theme`), plus respecting light/dark terminals. The four-channel palette
  (user/assistant/tool/thinking) should be theme-driven rather than hard-coded.
- **Command & path autocomplete (partly done 2026-08-20).** `/help` is a
  filterable popup list with descriptions, backed by one `COMMANDS` table that
  also feeds Tab completion — they used to be separate hand-maintained lists.
  The overlay component is reusable; `/model`, `/resume` and worker pickers are
  the next users. Still to do: the popup appearing *as you type* `/` in the
  composer, and `@path` fuzzy picking. Original entry: complete `/`
  slash-commands (with a popup list + descriptions) and `@path` file references
  (fuzzy, tab to accept). Steal the behavior from `reference/gemini-cli`'s
  @mention/shell completion and `reference/pi`'s `fuzzy.ts`.

## 10. Milestones

**M0 — Spike (1–2 days)**: `llm` crate against Anthropic streaming + one
tool call round-trip. Decide rig vs hand-rolled. Build `doc read` wrappers
around `pandoc`/`pdftotext`/`soffice`, run them on your 5 ugliest real
files to confirm output quality is good enough to feed a model.

**M1 — Harness that works (core loop)**: read/write/edit/bash, `--print`
mode, JSONL event stream, sessions with continue/resume, AGENTS.md loading.
Port llm/retry/session/tool-dispatch from rustopedia where it saves time.
Usable daily for coding. *Gate: it must feel as good as pi on a real repo
before anything below this.*

**M2 — TUI polish + validation loop (§7a)**: everything in §9, compaction,
model cycling, cost tracking. Plus the single-agent validation-driven loop —
success predicates (`--until`, test/build gate), stuck detection, nudge/
re-plan, bounded retries via the agent-line dependency. This is the thesis
(§0) landing in the core, before workers. Read `reference/pi/packages/tui/`
editor pieces first; the editor feel (undo, kill ring, paste, @-refs) is
where terminal CLIs live or die.

**M3 — Document tools GA**: full §5 surface + skill file. A high-value
capability for real .docx/PDF workflows — but positioning stays clear: the
differentiator is the guidance layer (§0), this is a useful add-on.

**M4 — Extensibility** *(web tool landed early)*: a `web` tool (search via
Brave/Tavily/SearXNG + URL fetch with HTML→text) ships now because agents need
current docs; the rest is still ahead — MCP client in core (stdio), `rmcp` decision,
plugin JSON-RPC protocol, `worksmith install/list`, tier-2 skill distribution,
prompt templates.

**M5 — Memory** *(done, minus export/sync)*: SQLite `memory.db` (global +
project), CRUD, supersede, exact-subject + FTS5 hybrid retrieval, `/memory`
(incl. `search`, `pending`/`approve`, `extract`), a `memory` tool the model
calls, write-time dedup, and worker proposals. Plus **knowledge**
(`knowledge.db`): the repo's text chunked + FTS5-indexed behind a `knowledge`
tool and `/knowledge`. Not done: `worksmith memory export/import/sync` via git,
auto-extraction at compaction (today `/memory extract` is explicit), and
semantic/vector retrieval (`worksmith-memory-v1.md` §30 stage 4).

**M6 — Spawned agents** *(done)*: `/spawn`, `/agents` panel, fan-out (`-n`, `--each-files`, planner
`auto`) with a queue past `agents.max`, `worksmith spawn` for headless runs,
and per-worker model override
(`agents.model` / `/spawn --model`) for the cheap-workers/smart-parent split. Worker results now feed back into the
parent's history (via the steering mailbox mid-turn, or the session between
turns), with fan-out groups reported as one block and synthesized into a single
answer. Not done: `propose_memory` from workers, `worksmith spawn`, per-agent
model override.
(Unblocked by the M1 JSONL stream — that's why the stream is designed first.)

**M7 — Supervisor** *(done, minus the model observer)*: watches worker event
streams; rules-based stuck detection (idle timeout, repeated tool calls,
explicit "I'm blocked", runaway spend), bounded nudges via a steering mailbox on
the agent, escalate/stop with the reason reported to the parent.
`agents.supervisor = off | rules | model` — `model` (the cheap-model observer)
and re-spawn-with-a-refined-task are not built yet; `model` behaves as `rules`.
(§7 — depends on M6.)

**M8 — Nice to have**: vim-style tabbed TUI layout (main window + worker/agent
tabs + `/tree`, §9), `/tree` branching UI, `--mode rpc`, sandbox modes,
subscription logins (Anthropic/OpenAI OAuth), brew/binstall distribution,
agent-line-style workflow definitions, and porting rustopedia's Rust-development brain in as a skill so one binary
covers both general and Rust-specific work.

On workflow definitions specifically: chaining workers (draft → review → revise)
should be a **file, not TUI syntax**, and a *separate artifact from skills* —
the Agent Skills spec deliberately covers none of orchestration, determinism, or
validation, and forking a format 30 tools agree on to smuggle in a state machine
would cost the interop and gain nothing. Shape: `workflows/<name>.toml`, steps
modeled on agent-line's `Outcome` (`Continue | Done | Next(id) | Retry | Fail`),
each step optionally fanning out (`workers = N`), naming its own `model`, and
gating on `validate` with `on-fail = { next, max-retries }`. A pipeline you'd reuse is one you'd rather
store than retype, and a pipe-style `/spawn "a" | "b"` is shell idiom in a place
where you write prose to a model. The reviewer stage in particular needs more
structure than a freeform skill gives — it has to produce a *decision*, not
prose, so the loop can act on it deterministically: a fixed verdict shape
(pass/fail + reason + optional line refs), a named rubric, and a retry budget.
Call it a workflow, a contract, a review spec — the point is the shape is fixed
even when the prompt isn't.

Two rules learned before building any of it:
- **Stages pass paths, not payloads.** Workers already share a cwd, so stage 2
  reads the file stage 1 wrote. Re-sending a 2,000-word draft through four
  stages is 4× the context for no gain.
- **Don't add a knob the default can infer.** When the parent judges, a
  per-worker reviewer is duplicated work — the parent reads everything anyway.
  Worker-level validation earns its keep only when the check is cheap and
  objective (a shell command), or when the parent's context can't hold every
  worker's output. Otherwise it's N extra model calls to reach a judgment that
  was going to happen regardless.

**M9 — Metrics & cost tracking**: two tiers of visibility.
- **Quick-glance (footer, main view):** only the few numbers you watch live —
  model, context %, tokens ↑/↓, session cost, gen tok/s. Can land early
  (footer already shows some). Keep it minimal; the main view is for the work.
- **Metrics tab (depth):** a dedicated tab (rides on the tabbed layout, §9 /
  M8, which depends on M6) for everything else — per-turn token/cost history,
  cache hit-rate (parse `prompt_tokens_details.cached_tokens`), cost breakdown
  by turn, time-to-first-token + tok/s trends, turn / tool-call / step counts,
  retry & nudge counts, per-tool usage, validation pass-rate, and session
  totals. A `/stats` command dumps the same for `--plain`/non-tab contexts.
- **Cost** needs a per-model price table in config (`[models.<id>] input/output
  price`); local vLLM = free. Cost per *completed* task — not per turn — is the
  number that decides whether frugal mode (M10) is working.
Prompt caching itself: automatic server-side today (vLLM `--enable-prefix-
caching`, OpenAI/OpenRouter auto-cache) because we keep a stable message prefix;
explicit Anthropic `cache_control` breakpoints land with the Anthropic client
(§3). Keeping the system-prompt/`<MEMORY>` prefix stable maximizes cache hits.

**M10 — Frugal mode (low-token operation)**: a mode that does the same work for
materially fewer tokens, selected by `--frugal` / `mode = "frugal"`. This is
positioning, not just thrift: the competing harnesses spend freely (DeepSeek's
`dsh` reportedly burned ~20M tokens on a single task), and "cheap models, small
context, real work" is the opposite bet — the one that makes local/vLLM
deployment and long unattended runs viable. Cost per *completed* task is the
number to publish, so this depends on M9's accounting to prove anything.

Known spend, roughly in order of waste:
- **The `<MEMORY>` block is injected every turn** regardless of relevance —
  currently the top 20 rows by importance. Now that `memory.search()` does
  hybrid retrieval, frugal mode should inject only what matches the turn (or
  nothing, and let the model call the `memory` tool). Note the tension with
  prompt caching below.
- **Tool results dominate.** `MAX_TOOL_RESULT_BYTES` is 24k; frugal should cut
  it hard, prefer `grep` hits over whole-file `read`s, and return smaller
  knowledge chunks.
- **Reasoning tokens** — disable/limit thinking where the provider allows it.
- **Multipliers**: fan-out is N× the work, each supervisor nudge is a re-send of
  the whole history, validation retries re-run the loop, and a finished fan-out
  group triggers an extra synthesis turn. Frugal should cap `max-steps` and
  `max-retries`, and default `agents.synthesize = false`.
- **The system prompt** itself (base + AGENTS.md + memory) is re-sent every
  step; a terser base prompt is free savings.

Caching cuts the other way: a per-turn memory block breaks the stable prefix
that makes server-side prefix caching work, so measure both — a cache hit is
cheaper than a token not sent. Get the numbers before choosing.

**M11 — Sandbox each worker** *(future; now evidence-backed)*: every worker
edits the user's real working tree, and a fan-out of N workers edits it
*concurrently*. This is no longer hypothetical — a three-worker newsletter
fan-out had all three workers write `bluecollar-newsletter/draft-1.md`, last
writer wins, two drafts lost. The planner's bias toward a single worker is
partly a workaround for a missing isolation boundary.

Isolation solves three things at once, which is why it's worth more than the
per-worker output paths that would patch the collision:
- **Collision** — each worker gets its own tree, so N workers can touch the
  same filenames without clobbering each other.
- **Security** — a worker is the least trusted thing in the system: a model
  running unattended, often a cheap one, on a task the parent wrote. Today it
  has the same filesystem reach as the user. PLAN's safety guard is explicitly
  "best-effort, not a sandbox"; this is where a real boundary belongs.
- **Undo** — a bad worker edit is currently only recoverable by hand through
  git. Borrow rustopedia's `scratch.rs` (`ScratchOverlay`): `git worktree
add --detach <tmp> HEAD`, then mirror uncommitted tracked changes and
untracked-but-not-ignored files so the copy matches what the user is actually
editing, not just the last commit. Removed on drop.

What it buys, in order of value:
- **Safe fan-out.** Each worker gets its own overlay, so three workers can edit
  the same files without clobbering each other; the parent reviews the diffs and
  applies the ones it accepts. Today "workers share one cwd with no isolation"
  is a hard constraint on how fan-out can be used.
- **Validation without side effects.** `--until "cargo test"` currently runs
  against the live tree; in an overlay a failed attempt leaves nothing behind.
- **An honest undo.** Right now a bad worker edit is only recoverable through
  git by hand.

Costs to weigh: a worktree per worker is disk and setup time (rustopedia's
mirroring step is the expensive part), it needs a git repo (so degrade
gracefully to today's behavior when there isn't one), and the parent needs a
merge/apply step that doesn't exist yet. Non-code work (the newsletter fan-out)
doesn't need any of this, so it should be opt-in per spawn or inferred from
whether the task touches tracked files.

**M12 — Per-role model routing** *(future)*: the harness already makes several
model calls that are not the user's turn — the fan-out planner, the compaction
summarizer, the memory classifier, the fan-out judge, and the supervisor's
unbuilt `model` mode. All of them currently inherit the session's model, which
is both expensive (a frontier model summarizing a transcript) and, more
importantly, unmeasurable: there is no way to ask which model is good at which
internal job.

**Shape (2026-08-21).** Three different questions get conflated here, and only
two of them are worth building.

*Roles are mechanical.* The harness always knows which job it is doing at the
call site, so this is a lookup and nothing more:

```toml
[roles]
main       = "openrouter/qwen/qwen3.8-27b"
worker     = "omlx/Qwen3.5-9B-OptiQ-4bit"
compaction = "omlx/Qwen3.5-2B-4bit"   # mechanical; a 2B can summarize
extraction = "omlx/Qwen3.5-2B-4bit"
planner    = "omlx/Qwen3.5-9B-OptiQ-4bit"
judge      = "openrouter/qwen/qwen3.8-27b"
```

Values are keys into `[models]` (§ config), which is why that table is keyed by
the spec rather than being an ordered array: a role points *at* a profile. Two
of these roles already exist informally as `model` and `agents.model`; M12 is
naming the rest rather than inventing a mechanism.

*Hard capabilities should be discovered, not declared.* Whether a model accepts
images, or emits tool calls, is a fact about the model, and hand-maintaining it
in config guarantees it goes stale. OpenRouter reports
`architecture.input_modalities` and `supported_parameters` on `/api/v1/models`;
a local server reports its accepted parameters at `/openapi.json`. Discovery
belongs there, cached, with config as an override for when the source is wrong.
Routing an image task to a text-only model should fail with "this model has no
vision" rather than a confusing provider error.

*Task-kind routing ("use the writing model") is the speculative one.* It needs
someone to decide a task *is* writing, which is a classifier: another model
call, or a heuristic that will be wrong in ways nobody can predict. The cheap
90% is that the user already knows — `/model` for the session, `--worker-model`
per spawn, and a workflow (§8a) naming a model per stage. Build those first and
see whether an automatic classifier is still wanted; it probably is not.

Order of work: roles first (mechanical, immediate cost win on compaction and
extraction), discovery second (needed before any capability check means
anything), classification last or never.

Routing means naming those roles in config and letting each pick a model:

```toml
[agents]
model = "openrouter/deepseek/deepseek-v4-flash-0731"   # workers (exists today)

[roles]                                    # the harness's own calls
planner    = "openrouter/qwen/qwen3.5-9b"
classifier = "openrouter/qwen/qwen3.5-9b"  # memory extraction
compactor  = "openrouter/qwen/qwen3.5-9b"
judge      = "openrouter/moonshotai/kimi-k3"
```

Mechanically this is small: `ModelOverride` and `Agent::fork_with` already do
the work, `llm::client_for` already builds a client per provider, and
`Agent::ask` is the single choke point every helper call passes through.

**Why it matters more than it looks.** Every failure worth fixing in these
roles so far has been *format compliance, not intelligence*: a local 27B
answered the planner with its own deliberation, a frontier judge twice reported
rule compliance it had not checked, and the memory classifier's whole job is to
emit `scope|kind|subject|content|importance` or nothing at all. Those are jobs
where a small, narrowly-trained model can plausibly beat a large general one —
and routing is what makes that a *measurable* claim instead of a hunch. Run a
role on a big model and a small one, compare format-compliance rates, and the
question "is a fine-tune worth it here?" answers itself before anyone trains
anything.

That is also the honest sequencing for the "ship specialist models" idea:
**routing first, training much later, if ever.** Fine-tuning carries data,
eval, quantization, hosting and versioning costs, against base models that
improve quarterly and already cost ~$0.10/Mtok. The tool-shaped tasks
(reading commits, extracting from a docx) are not model-limited at all —
`bash`, `grep`, and `doc` already do them deterministically. Refusal-ablated
models are a user's choice to configure, not something to ship: they trade
away instruction-following, which is exactly what this harness exists to
compensate for.

## 8a. Workflows, designed 2026-08-20

Grounded in a real run rather than a hypothetical: `worksmith spawn -n 3 "write
candidate articles ... then choose the best one"`. That worked, and everything
awkward about it points at the same thing.

### What the real run showed

- The shape is right. Cheap workers draft, the smart parent judges. The judgment
  it produced named four reasons and caught an unsourced statistic.
- **The planner invented a third task nobody wanted.** Asked for two articles
  with `-n 3`, it produced two drafts plus an "analysis notes" document. A stage
  count belongs in a file, not in a flag the planner reinterprets.
- **The pick was prose.** A paragraph saying candidate 2, which a human can act
  on and the loop cannot. Nothing downstream could branch on it.
- **It ended by asking a question** in a non-interactive command, so the answer
  reached nobody.
- **Rerunning it means retyping it**, and any change to the prompt silently
  changes the experiment.

### Shape

`workflows/<name>.toml` in the project (and `~/.worksmith/workflows/` for ones
you reuse everywhere). Discovered like skills, run with `/workflow <name>` or
`worksmith workflow <name>`.

```toml
name = "candidates"
description = "Draft N candidates, pick one against a rubric"

[[step]]
id = "draft"
workers = 3                      # explicit, not a planner's guess
prompt = "Write one candidate article on {{topic}}. Save it to {{out}}."
model = "omlx/Qwen3.5-9B"        # optional per-stage model
out = "candidate-{{worker}}.md"  # each worker gets its own path

[[step]]
id = "judge"
after = "draft"
kind = "judge"                   # a verdict, not prose
rubric = "references/style-guide.md"
prompt = "Pick the strongest candidate for a blue-collar engineering audience."
on-fail = { next = "draft", max-retries = 1 }
```

### The three decisions that matter

**Stages pass paths, not payloads.** Workers share a cwd, so stage 2 reads what
stage 1 wrote. Re-sending a 2,000-word draft through four stages is 4x the
context for nothing. `out` exists so each worker writes somewhere predictable,
which also fixes the collision M11 is filed for.

**A judge stage returns a verdict, not prose.** Fixed shape: `{ choice, reason,
confidence }`, or `{ pass, reason, refs }` for a review stage. That is what lets
`on-fail` mean something. A judge that writes paragraphs is a stage the loop
cannot branch on, which is exactly what the real run produced. This is the same
question as open decision #9 (what is a check?) seen from the other end: a
`judge` check kind and a judge *stage* want the same verdict type, and should
share one.

**Variables are named and few.** `{{topic}}` from the invocation, `{{worker}}`
and `{{out}}` from the runner. No general templating language. The moment a
workflow file needs conditionals it wants to be a program, and this is not the
place to grow one.

### Deliberately not

- **Not skills.** The Agent Skills spec covers none of orchestration,
  determinism, or validation. Forking a format thirty tools agree on to smuggle
  in a state machine costs the interop and gains nothing.
- **Not a pipe syntax.** `/spawn "a" | "b"` is shell idiom in a place where you
  write prose to a model.
- **Not a DAG.** `after` gives a linear chain with fan-out inside a stage. That
  covers draft/review/revise and the run above. A general dependency graph is a
  much larger thing to earn.

### Depends on

`--until` per worker exists (2026-08-20). What is missing is the verdict type,
which decision #9 should settle first, and per-worker output paths, which want
M11's tree-per-worker to be genuinely safe. A first version that only does
`workers = N` plus a judge stage would already beat retyping the command, and
would have prevented the invented third task.

## 10a. Working order (decided 2026-08-20)

Milestones above are the map; this is the route, most valuable first. Written
down because the ordering is an argument, not a preference, and the argument is
easy to lose.

1. **Approval gate for irreversible and outward-facing commands.** *(security,
   in progress)* There is no approval mechanism at all today — the only gate is
   `dangerous_command`'s six regexes, which cover catastrophic *local*
   destruction (fork bomb, `dd`, `mkfs`, `curl | sh`, `chmod -R /`, `rm -rf /`)
   and nothing else. Observed in real use: a 27B ran `git push` and staged files
   unattended. Nothing blocks `git push`, `gh`, `sudo`, `curl -X POST` with your
   files in the body, or writes outside the cwd.

   The thesis argues *for* this rather than against it. Codex and Claude Code
   assume a strong model whose judgment about side effects is usually sound and
   still prompt; worksmith assumes the opposite kind of model. The eval measured
   exactly that: on qwen3.5-9b, **10 of 21 raw failures had outcome `done`** —
   the model declared itself finished and was wrong. That is the model currently
   holding unattended shell and network access.

   Explicitly *not* solved by M11: a worktree overlay isolates the filesystem,
   and `git push` escapes it by definition. Two different threats — outward
   actions need an approval gate, local collisions need a sandbox.

2. ~~**Give workers the validator.**~~ **Done (2026-08-20)**: `/spawn --until`,
   `[agents] validate`, and the validator now reaches `run_turn`. Opt-in, not
   inherited, because N workers share one tree and would run the check N times
   concurrently — read-only checks are fine now, the general answer is M11.
   Still unmeasured, as below.

   *(original entry)* **Give workers the validator.** `worker.rs:404` passes `None` where the main
   loop passes a `CommandValidator`, so a worker stops when the model says it is
   done — the exact failure the eval measured. Half the product surface has none
   of the differentiator. This is one argument plus a `/spawn --until` flag, not
   the workflow file of §8 (that would sit *on top* of this plumbing later).
   Unmeasured afterwards: `run.py` cannot reach the worker layer, so the honest
   check is `workers` vs `workers --until` on the 9B rather than assuming the
   +34 transfers.

3. **UX pass (in progress).** Steering, `/agents tail`, and the picker overlay
   are done. Remaining: the `/model` picker — which needs a `[models]` config
   section, and that is the same table M9 wants for prices, so the two want
   doing together — and `@path` fuzzy picking.

4. **Sandboxing, OS-local rather than Docker** *(decided 2026-08-20)*. See §11.9
   and the reasoning recorded there: WASM cannot sandbox the native toolchain
   worksmith exists to run, and a container breaks "edit my working tree in
   place" while demanding the toolchain live in an image. macOS `sandbox-exec`
   profiles and Linux Landlock (+ optional network namespace) let the real
   toolchain run while restricting paths, and network denial is the only
   OS-level thing that actually stops push and exfiltration. **Planned in §10b**
   (2026-08-20); not built.

5. **M9 metrics, the cost-per-solved-task spine first.** The instrument, not
   decoration: it is what proves the differentiator to anyone else, and the only
   way M10 frugal mode can be judged. Pulls in the footer cost figure, which
   needs M9's price table. The metrics *tab* rides on M8's tabbed layout, which
   also answers "watch worker w2 work" — let tabs land as part of this rather
   than as their own goal.

6. **M11 worktree sandbox.** Already evidence-backed by the three-worker
   `draft-1.md` collision. Collision and undo — *not* a security boundary; that
   is item 4's job.

7. **MCP (§6) last.** It multiplies tool surface with third-party code under a
   harness that has no permission model. Doing it before (1) means arbitrary
   tools executing unattended on a weak model's say-so.

## 10b. Sandboxing (OS-local), planned 2026-08-20

Not built yet. Written down first because the decisions below are what make it
protective rather than merely annoying, and because getting them wrong means
`cargo build` stops working and the sandbox gets turned off forever.

### Why not the obvious two

**WASM is the wrong tool.** wasmtime sandboxes code compiled to wasm. What needs
containing here is native subprocesses: cargo, rustc, git, pandoc, python. Those
are not wasm binaries and will not be. (WASM *is* the right answer for a future
plugin system where third parties ship code we execute in-process. Remember that
when MCP arrives.)

**Docker costs more than it returns, as the default.** The toolchain has to exist
in the image, bind-mounting the working tree hands most of the filesystem back,
uid mapping is a chore, and "edit my repo in place" is the whole workflow.
Defensible for unattended runs; wrong for the interactive path.

**OS primitives fit.** macOS `sandbox-exec` (seatbelt) profiles, Linux Landlock
(kernel 5.13+, `landlock` crate) plus optionally a network namespace. The real
toolchain runs, paths are restricted, and network can be denied. Codex and Claude
Code both take this route.

### What is inside the boundary

Every `bash` tool invocation, and every validation command. Validation is not
optional here: `CommandValidator` spawns `bash -lc` directly and re-runs on every
retry, unattended. A sandbox with a validator-shaped hole is theatre.

Not the harness itself. Worksmith's own writes (sessions, memory, knowledge) are
in-process and stay outside.

### The decision that breaks everything if wrong: writable paths

A naive "writes only under cwd" policy breaks this very repo on the first build.
`CARGO_TARGET_DIR` here is `~/.cargo/target`, nowhere near the project. That is
the shape of the whole problem, so the default write-set is:

- the project directory (cwd), including `.git`
- `$TMPDIR` and `/tmp`
- the resolved cargo target dir (`cargo metadata`), `~/.cargo/registry`,
  `~/.cargo/git`
- tool caches that are write-or-fail: `~/.npm`, `~/.cache`, and on macOS the
  LibreOffice profile under `~/Library/Application Support` that `doc` needs

Reads stay broad. A weak model reading `/usr/include` is not the threat; the
threat is writes leaving the project and data leaving the machine. Narrowing
reads would break every toolchain lookup for little gain, and the secrets worth
protecting (`~/.ssh`, `~/.aws`, `.env`) are better handled as an explicit
read-deny list than as an allow-list nobody can complete.

### Network: allowed by default, deniable

Denying network by default breaks `cargo build` on a cold registry, `npm
install`, `pip`, and the `web` tool. It would be turned off within a day.

The approval gate already catches the outward-facing *commands* (push, curl with
a body, publish). So: network on by default, and `sandbox = "strict"` denies it
for unattended runs where nobody is there to approve anything. Strict is the
setting for `--print` in CI, not for a person at a terminal.

### Failure modes, which decide whether anyone keeps it on

`[tools] sandbox = "off" | "best-effort" | "required"`, defaulting to
best-effort.

- **best-effort**: unsupported platform or kernel warns once per session and runs
  anyway. Silently running unsandboxed while claiming otherwise is worse than
  either honest option.
- **required**: refuse to run commands at all rather than run them unconfined.
  For servers and CI.
- A command the sandbox denies returns an error the model can route around,
  naming the path or the operation refused, the same shape as an approval
  denial. It must not be a fatal turn error, or one stray write kills the run.

### Build order

1. A `Sandbox` trait with a no-op implementation, wired into `bash` and
   `CommandValidator`. Nothing changes behaviourally; this is the seam.
2. macOS seatbelt profile generated per invocation from the write-set above.
   Test: a write outside the project fails, `cargo test` in the project passes.
3. Linux Landlock. Same tests. Feature-detect the ABI and degrade per the policy
   above rather than assuming a kernel.
4. `strict` network denial (netns on Linux, seatbelt rules on macOS).

### Open questions

- Does the write-set need to be user-extendable (`[tools] sandbox-write = [...]`)
  on day one, or is discovering the gaps in dogfooding the better order?
- Do workers get a *tighter* profile than the session? They are the least trusted
  thing in the system, and M11 gives each its own tree anyway.
- Is `required` on by default for `worksmith spawn` (headless, unattended) even
  when the interactive default is best-effort?

## 11. Open decisions

1. `rig` vs hand-rolled provider layer (M0 decides). Rig's OpenAI-compat
   support matters more now that OpenRouter/RunPod/vLLM are first-class.
2. Ship `grep`/`find`/`ls` or pure 4-tool? (Recommend: ship them.)
3. docx→pdf default engine: LibreOffice headless (best fidelity, heavy
   install) vs pandoc (lighter, needs LaTeX) — decide from M0 spike.
4. ~~Project trust flow for `.worksmith/` configs.~~ **Done (2026-08-20):**
   ask-once, remembered by file *content* in `~/.worksmith/trust.toml`, so an
   edited config asks again. Undecided means not applied. Headless ignores and
   warns; `--trust-project` and `/trust revoke` are the escape hatches. Was the
   sharpest hole in the system: a cloned repo could run shell via
   `agent.validate` and redirect model traffic via `providers.*.base-url`.
   Project *skills* and `AGENTS.md` are deliberately not gated — they are
   instructions, not execution, and a malicious skill still has to get past the
   approval gate.
5. Memory: is project memory checked into the repo (shared, reviewable)
or gitignored + synced privately? (Recommend: in the repo.)
6. `rmcp` vs hand-rolled MCP stdio client (M4 decides; stdio is simple
   enough that hand-rolling is a realistic fallback).
7. Spawned agents: child processes (simple, isolated, matches pi's
   "spawn yourself" philosophy) vs in-process tokio sessions (faster, but
   one crash takes the whole UI). (Recommend: child processes.)
8. ~~Name.~~ **Decided: Worksmith** (CLI command `worksmith`, app dir
   `.worksmith/`).
9. **What is a validation check, exactly?** `--until` takes a shell command
   today, and that covers `cargo test` / `python3 test.py` — the cases the eval
   used. It does not obviously cover: "this HTTP endpoint returns 200", "this
   file matches a schema", "a model judges the draft against a rubric", or a
   check written in the project's own language rather than as a shell one-liner.
   Everything can be *forced* into a shell command, which is why the question
   hides: the tell is `--until "python3 -c '...'"` with an inline program in it.
   Options: keep shell-only and document it; add typed check kinds
   (`shell` | `http` | `file-exists` | `judge`); or let a check name a file the
   project already has. Decide before workflows (§8) fix a per-stage `validate`
   shape, since that inherits whatever this becomes.
