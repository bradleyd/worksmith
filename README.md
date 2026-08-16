# Worksmith

A minimal terminal coding-agent harness in Rust, built on the bet that the
**harness** — not the model — should do the work of keeping weaker/cheaper
models on task and driving to a *validation*. See [`PLAN.md`](PLAN.md) and
[`worksmith-memory-v1.md`](worksmith-memory-v1.md).

This is **M1** (the core loop). Status: usable single-agent coding harness with
streaming, model-driven tools, JSONL sessions, and SQLite memory. Validation
loop, TUI polish, doc tools, MCP/plugins, and spawned workers are later
milestones.

## What works in M1

- **Streaming, tool-calling agent loop** against any OpenAI-compatible endpoint
  (vLLM/Qwen, OpenRouter, RunPod, local).
- **Built-in tools:** `read`, `write`, `edit` (exact unique-match, multi-edit,
  atomic), `bash` (timeout + `WORKSMITH_SESSION_ID`), `grep`, `find`, `ls`.
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

# 3. Run — interactive REPL
worksmith

# one-shot, pipe-friendly
worksmith --print "summarize src/main.rs"

# machine-readable event stream
worksmith --mode json "list the rust files"
```

### REPL commands

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
