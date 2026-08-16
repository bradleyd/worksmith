//! System-prompt assembly: base instructions + project `AGENTS.md`/`CLAUDE.md`
//! + the injected `<MEMORY>` section. Shared by the REPL and the TUI.

use std::path::Path;

use crate::config;
use crate::memory::MemoryStore;

pub const BASE_SYSTEM_PROMPT: &str = "\
You are Worksmith, a terminal coding agent. You accomplish tasks by calling \
tools (read, write, edit, bash, grep, find, ls) rather than by guessing. \
Prefer reading files before editing them. Make minimal, correct changes and \
verify your work (build/tests) when possible. Be concise in prose — the user \
is in a terminal. When the task is complete, stop calling tools and give a \
short summary.";

/// Build the full system prompt for a turn: base + project instructions +
/// currently-relevant memory.
pub fn build_system_prompt(cwd: &Path, mem: &MemoryStore) -> String {
    let mut s = String::from(BASE_SYSTEM_PROMPT);

    let instructions = config::load_project_instructions(cwd);
    if !instructions.trim().is_empty() {
        s.push_str("\n\n");
        s.push_str(&instructions);
    }

    let memory = mem.memory_section(20).unwrap_or_default();
    if !memory.trim().is_empty() {
        s.push_str("\n\n");
        s.push_str(&memory);
    }
    s
}
