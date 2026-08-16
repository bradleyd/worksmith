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

**Tier 1 — Skills & prompt templates (Markdown, free)**
Copies pi exactly. `~/.worksmith/skills/*/SKILL.md`, `.worksmith/skills/`,
`/skill:name`, prompt templates as `/name` markdown files with `{{var}}`.
Zero code, works for the majority of "extension" needs.

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
2. **Fan-out pattern:** `/spawn --each "file glob" "task template"` —
   N workers, one per match, results collected into one file. This is the
   parallel.rs example, productized. Fits doc tools beautifully
   ("convert every docx in this folder").

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
- Message queue: Enter = steering, Alt+Enter = follow-up (replicate — it's
  a big quality-of-life win).
- Agent panel: `/agents` list/tail/kill, footer running-count (§7).
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
  activity (§7), `/tree`, etc. Tab-switching keys (vim-like). Depends on the
  agent/worker milestones landing first — see M8.
- **Scrolling.** Mouse wheel + PageUp/Down + Ctrl+U/D (half-page) + Up/Down +
  Home/End, with follow-tail on new output. (Shipped in M2.)
- **Vim keybindings (later).** A vim mode for navigation/scrolling (`j/k`,
  `gg/G`, `Ctrl+U/D`, counts) and eventually modal editing in the composer.
  M8-era polish alongside the tabbed layout.
- **Themes (later).** Selectable color themes (e.g. a `[theme]` config section
  or `/theme`), plus respecting light/dark terminals. The four-channel palette
  (user/assistant/tool/thinking) should be theme-driven rather than hard-coded.
- **Command & path autocomplete (later).** In the composer: complete `/`
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

**M4 — Extensibility**: MCP client in core (stdio), `rmcp` decision,
plugin JSON-RPC protocol, `worksmith install/list`, tier-2 skill distribution,
prompt templates.

**M5 — Memory**: SQLite `memory.db` (global + project), CRUD, supersede,
exact-subject + FTS5 retrieval into prompt, `/memory`, `worksmith memory
export/import/sync` via git, auto-memory at compaction (opt-in). Follows
`worksmith-memory-v1.md`.

**M6 — Spawned agents**: `/spawn`, `/agents` panel, `worksmith spawn`,
per-agent model override, `--each` fan-out, `propose_memory` from workers.
(Unblocked by the M1 JSONL stream — that's why the stream is designed first.)

**M7 — Supervisor**: watch worker event streams; rules-based stuck detection
(idle timeout, repeated tool calls, runaway spend), bounded nudges via
steering, escalate/stop/re-spawn. Optional cheap-model observer.
`agents.supervisor = off | rules | model`. (§7 — depends on M6.)

**M8 — Nice to have**: vim-style tabbed TUI layout (main window + worker/agent
tabs + `/tree`, §9), `/tree` branching UI, `--mode rpc`, sandbox modes,
subscription logins (Anthropic/OpenAI OAuth), brew/binstall distribution,
agent-line-style workflow definitions, and porting rustopedia's Rust-development brain in as a skill so one binary
covers both general and Rust-specific work.

## 11. Open decisions

1. `rig` vs hand-rolled provider layer (M0 decides). Rig's OpenAI-compat
   support matters more now that OpenRouter/RunPod/vLLM are first-class.
2. Ship `grep`/`find`/`ls` or pure 4-tool? (Recommend: ship them.)
3. docx→pdf default engine: LibreOffice headless (best fidelity, heavy
   install) vs pandoc (lighter, needs LaTeX) — decide from M0 spike.
4. Project trust flow for `.worksmith/` configs (pi has one; simple ask-once is
   enough for v1).
5. Memory: is project memory checked into the repo (shared, reviewable)
or gitignored + synced privately? (Recommend: in the repo.)
6. `rmcp` vs hand-rolled MCP stdio client (M4 decides; stdio is simple
   enough that hand-rolling is a realistic fallback).
7. Spawned agents: child processes (simple, isolated, matches pi's
   "spawn yourself" philosophy) vs in-process tokio sessions (faster, but
   one crash takes the whole UI). (Recommend: child processes.)
8. ~~Name.~~ **Decided: Worksmith** (CLI command `worksmith`, app dir
   `.worksmith/`).
