# Worksmith

A minimal terminal coding-agent harness in Rust, built on the bet that the
**harness** — not the model — should do the work of keeping weaker/cheaper
models on task and driving to a *validation*. See [`PLAN.md`](PLAN.md) and
[`worksmith-memory-v1.md`](worksmith-memory-v1.md).

Status: **M1–M3 done.** Usable single-agent coding harness with streaming,
model-driven tools, JSONL sessions, and SQLite memory (M1); a validation-driven
self-correcting loop, context compaction, and a four-channel ratatui TUI (M2);
and document tools for PDF/DOCX (M3). MCP/plugins (M4), scoped-memory retrieval
(M5), spawned workers (M6), and the supervisor (M7) are later milestones.

## What works in M1

- **Streaming, tool-calling agent loop** against any OpenAI-compatible endpoint
  (vLLM/Qwen, OpenRouter, RunPod, local).
- **Built-in tools:** `read`, `write`, `edit` (exact unique-match, multi-edit,
  atomic), `bash` (timeout + `WORKSMITH_SESSION_ID`), `grep`, `find`, `ls`.
- **Document tools:** `doc` (read/info/convert/extract/create) for PDF/DOCX/…
  via pandoc, poppler, and LibreOffice — clean text/markdown extraction and
  format conversion, with install hints when an engine is missing.
- **Typed event stream** → `--mode json` and JSONL session files.
- **Sessions** under `~/.worksmith/sessions/` with `--resume`/`--continue`.
- **Config** (`~/.worksmith/config.toml` + project override) and `AGENTS.md` /
  `CLAUDE.md` discovery.
- **SQLite memory** (global + project) with supersede semantics and `/memory`.

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

Keys: `Enter` send · `Esc` abort a running turn (or clear input) · `Ctrl+C`
quit · `Ctrl+O` expand/collapse long tool output · scroll with the mouse wheel,
`PgUp`/`PgDn`, `Ctrl+U`/`Ctrl+D`, `↑`/`↓`, `Home`/`End`. Commands: `/new`
`/compact` `/memory` `/validate <cmd|off>` `/quit`, and `@path` to include a
file. (Model cycling, vim keybindings, and themes are planned follow-ups.)

### Plain REPL commands (`--plain`)

```
/help                     show commands
/quit                     exit
/new                      start a new session
/memory [list|global|project]
/memory show <id> | /memory forget <id>
/memory add <scope> <kind> <subject> <content...>
@path                     include a file's contents in your message
```

Ctrl+C aborts the current turn; Ctrl+D exits.

## Development

```sh
cargo test        # unit + streaming/tool-call integration tests
cargo clippy
```
