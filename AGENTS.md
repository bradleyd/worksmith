# AGENTS.md — Worksmith

Instructions for AI agents working on this repository. (Loaded into the system
prompt automatically; keep it tight — it costs tokens every turn.)

## What this is

Worksmith is a minimal terminal coding-agent harness in Rust. The bet (the
differentiator) is the **guidance layer** — keeping weaker/cheaper models on
task and driving them to a validation — not the tool list. Keep the core
minimal; richness comes from the loop, not features. See `PLAN.md` for the
roadmap and `worksmith-memory-v1.md` for the memory design.

## Build / test / lint

- `cargo build` — build.
- `cargo test` — unit + integration tests. **Run before considering work done.**
- `cargo clippy` — **must be warning-clean** (this repo keeps 0 warnings).
- `cargo run -- [ARGS]` — run it. Note: the cargo target dir is customized
  (`~/.cargo/target`), so there is no `./target/debug/worksmith`; use
  `cargo run` or read `cargo metadata` for the path.

## Layout (single crate)

`src/`: `main.rs` (CLI/entry), `tui.rs` (ratatui frontend, default UI),
`agent.rs` (the model↔tool loop + validation/stuck/compaction), `llm/`
(streaming OpenAI-compat client), `event.rs` (typed event bus — the keystone),
`session.rs` (JSONL sessions), `config.rs`, `prompt.rs`, `tools/`
(read/write/edit/bash/grep/find/ls + `doc` + safety guard), `memory.rs`
(SQLite + FTS5), `knowledge.rs` (project text, chunked + indexed),
`validation.rs`, `worker.rs` (spawned sub-agents), `supervisor.rs` (rules-based
worker watchdog), `fanout.rs` (one `/spawn` → N workers), `report.rs` (worker
results formatted for the parent). Front-end-agnostic logic lives outside
`tui.rs` so the plain REPL (`main.rs`) shares it. Tests live in `tests/` plus in-module `#[cfg(test)]`.

## Conventions

- Match the surrounding code's style, naming, and comment density. Comments are
  terse and explain *why*, not *what*.
- Every change: add/extend tests, then `cargo test` + `cargo clippy` clean.
- Tests use a scripted mock `LlmClient` (see `tests/agent_loop.rs`) — no network.
  Any test that creates a session or opens global memory must call
  `common::isolate_home()` first (`tests/common/mod.rs`), which points
  `WORKSMITH_HOME` at a per-process scratch dir. Without it the test writes into
  the developer's real `~/.worksmith/sessions/`.
  The TUI is smoke-tested under a PTY (`script -q /dev/null …`), since it needs a
  real terminal.
- Don't edit `reference/` — those are gitignored TypeScript design clones (pi,
  gemini-cli) for reference only.
- Sessions are JSONL by design; memory/knowledge are SQLite. Durable *memory* is
  distilled decisions/constraints/preferences/facts/lessons — NOT things
  derivable from the code. Those are *knowledge* (`knowledge.rs`): the repo's own
  text, chunked and FTS5-indexed, rebuildable and never prompt-injected wholesale.

## Runtime notes

- Target model class is **Qwen3-27B** (vLLM/OpenRouter). Keep prompts and tool
  outputs small; `max-tokens` should be ≥4096 (a low cap truncates file-writing
  tool calls).
- Destructive `bash` commands are refused by the safety guard
  (`tools::dangerous_command`) and hard-stop the turn.
