# AGENTS.md: Worksmith

Instructions for AI agents working on this repository. Loaded into the system
prompt automatically, so keep it tight. It costs tokens every turn.

## What this is

Worksmith is a minimal terminal coding-agent harness in Rust. The bet is the
**guidance layer**: keeping weaker and cheaper models on task, driving them to a
validation. Not the tool list. Keep the core minimal. Richness comes from the
loop, not from features. See `PLAN.md` for the roadmap (§10a is the working
order and why) and `worksmith-memory-v1.md` for the memory design.

## Build / test / lint

- `cargo build`: build.
- `cargo test`: unit + integration tests. **Run before considering work done.**
- `cargo clippy`: **must be warning-clean** (this repo keeps 0 warnings).
- `cargo run -- [ARGS]`: run it. Note: the cargo target dir is customized
  (`~/.cargo/target`), so there is no `./target/debug/worksmith`; use
  `cargo run` or read `cargo metadata` for the path.

## Layout (single crate)

`src/`: `main.rs` (CLI/entry), `tui.rs` (ratatui frontend, default UI),
`agent.rs` (the model/tool loop plus validation, stuck detection, compaction),
`llm/` (streaming OpenAI-compat client), `event.rs` (typed event bus, the
keystone), `session.rs` (JSONL sessions, and replaying compaction),
`config.rs`, `trust.rs` (is a project's own config allowed to apply?),
`prompt.rs`, `tools/` (read/write/edit/bash/grep/find/ls, plus `doc`, `web`,
`memory`, `knowledge` and `skill`, with `policy.rs` classifying commands and
`approval.rs` asking about the risky ones),
`memory.rs` (SQLite + FTS5), `mining.rs` (past sessions into memory proposals),
`knowledge.rs` (project text, chunked and indexed), `validation.rs`, `worker.rs`
(spawned sub-agents), `supervisor.rs` (rules-based worker watchdog), `fanout.rs`
(one `/spawn` into N workers), `report.rs` (worker results formatted for the
parent), `skill.rs` (Agent Skills discovery, the published format, not ours).
Front-end-agnostic logic lives outside `tui.rs` so the plain REPL (`main.rs`)
shares it. Tests live in `tests/` plus in-module `#[cfg(test)]`.

## Conventions

- Match the surrounding code's style, naming, and comment density. Comments are
  terse and explain *why*, not *what*.
- Every change: add/extend tests, then `cargo test` + `cargo clippy` clean.
- Tests use a scripted mock `LlmClient` (see `tests/agent_loop.rs`). No network.
  Any test that creates a session or opens global memory must call
  `common::isolate_home()` first (`tests/common/mod.rs`), which points
  `WORKSMITH_HOME` at a per-process scratch dir. Without it the test writes into
  the developer's real `~/.worksmith/sessions/`. Do not call `set_var` on
  `WORKSMITH_HOME` yourself in a test that shares a binary with others: the
  variable is process-wide and tests run in parallel, so a test that moves it
  breaks its neighbours. A test that genuinely needs its own home (first-run,
  project trust) gets its own file, since cargo gives each one a process.
  The TUI is smoke-tested under a PTY (`script -q /dev/null …`), since it needs a
  real terminal.
- Don't edit `reference/`. Those are gitignored TypeScript design clones (pi,
  gemini-cli) for reading only.
- Sessions are JSONL by design; memory and knowledge are SQLite. Durable
  *memory* is distilled decisions, constraints, preferences, facts and lessons.
  NOT things derivable from the code. Those are *knowledge* (`knowledge.rs`):
  the repo's own text, chunked and FTS5-indexed, rebuildable, and never
  prompt-injected wholesale.

## Runtime notes

- Target model class is **Qwen3-27B** (vLLM/OpenRouter). Keep prompts and tool
  outputs small; `max-tokens` should be ≥4096 (a low cap truncates file-writing
  tool calls). `max-tokens` covers reasoning *and* output: this model class has
  no sense of a budget and will reason until it hits the cap, so prefer
  `thinking = <budget>` over raising the cap, and `/fast` when the task doesn't
  need deliberation. The footer's `↻` is the reasoning spend and `⚠cut` means
  the last completion was truncated.
- Don't guess a provider's parameters. Local servers publish them at
  `/openapi.json` (`components.schemas.ChatCompletionRequest.properties`), and
  they disagree on names: oMLX takes `thinking_budget`, vLLM
  `thinking_token_budget`. Fields absent from the schema are accepted and
  ignored, not rejected, so probing cannot tell you what is honored.
- A project's `.worksmith/config.toml` is not applied until trusted
  (`trust.rs`); `Config::load` gates it, `Config::load_trusted` skips the gate
  and is what tests and `--trust-project` use. Decisions are keyed by file
  content, so an edit re-asks.
- Command safety is two tiers in `tools::policy`. **Refuse** (catastrophic and
  local) hard-stops the turn; `tools::dangerous_command` is the same list under
  its older name. **Ask** (outward-facing or irreversible: push, sudo, publish,
  send data, write outside the cwd) goes to `tools::approval::Approver`; the TUI
  prompts, headless refuses, `--approve-all` allows. Denial is an error the model
  is expected to route around, not a fatal stop. Keep the ask-list short: a
  prompt the user answers reflexively is worse than no prompt.
