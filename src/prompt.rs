//! System-prompt assembly: base instructions + project `AGENTS.md`/`CLAUDE.md`
//! + the skill catalog. Shared by the REPL and the TUI.

use std::path::Path;

use crate::config;
use crate::memory::MemoryStore;
use crate::skill::{MAX_CATALOG_CHARS, SkillCatalog};

pub const BASE_SYSTEM_PROMPT: &str = "\
You are Worksmith, a terminal coding agent. You accomplish tasks by calling \
tools (read, write, edit, bash, grep, find, ls) rather than by guessing. \
Prefer reading files before editing them. Make minimal, correct changes and \
verify your work (build/tests) when possible. Be concise in prose — the user \
is in a terminal. When the task is complete, stop calling tools and give a \
short summary.

MEMORY AND KNOWLEDGE
- `memory` holds what was decided, preferred, required, or learned across \
sessions. Search it before assuming how this project does something, and \
before contradicting an established decision.
- `knowledge` searches this project's own documents and source. Prefer it over \
guessing which file explains a topic.
- Remember only what will still matter next week: decisions, constraints, \
preferences, durable facts, hard-won lessons. Never store file locations, tool \
output, line numbers, one-time actions, generic programming knowledge, or \
anything re-derivable from the code — that is knowledge, not memory. Prefer \
storing nothing.";

/// Extra instructions for a spawned worker: nobody is watching, so don't ask —
/// and durable findings are *proposals*, not writes (see `worksmith-memory-v1.md` §8).
pub const WORKER_PREAMBLE: &str = "You are a background worker executing a delegated \
task autonomously. Do not ask the user questions; make reasonable assumptions, \
complete the task, and finish with a concise summary of what you did. If you \
learn something durable, `memory` records it as a *proposal* for the main \
session to approve — so propose sparingly, and only what outlives this task.";

/// The system prompt a spawned worker runs with.
pub fn build_worker_prompt(cwd: &Path, mem: &MemoryStore) -> String {
    format!("{}\n\n{}", build_system_prompt(cwd, mem), WORKER_PREAMBLE)
}

/// Build the stable system prompt for a turn: base + project instructions +
/// skill catalog. Per-turn memory is a separate dynamic message so this prefix
/// can still be provider-cached.
pub fn build_system_prompt(cwd: &Path, _mem: &MemoryStore) -> String {
    let mut s = String::from(BASE_SYSTEM_PROMPT);

    let instructions = config::load_project_instructions(cwd);
    if !instructions.trim().is_empty() {
        s.push_str("\n\n");
        s.push_str(&instructions);
    }

    // Only the catalog — name and description — rides in the prompt. The body
    // arrives when the model calls `skill`, which is the spec's progressive
    // disclosure and what keeps 20 installed skills from costing 20 skills.
    let skills = SkillCatalog::discover(cwd).prompt_section(MAX_CATALOG_CHARS);
    if !skills.trim().is_empty() {
        s.push_str("\n\n");
        s.push_str(&skills);
    }
    s
}
