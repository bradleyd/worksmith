//! `memory` and `knowledge` — the agent's own access to what the harness knows.
//!
//! Split deliberately (`worksmith-memory-v1.md` §3): `memory` holds a small set
//! of distilled decisions/constraints/preferences/facts/lessons, `knowledge` is
//! the project's own text, chunked and rebuildable. "What did we decide?" is a
//! memory question; "what does the architecture doc say?" is a knowledge one.
//!
//! Stores are opened per call rather than held in the context: SQLite opens are
//! cheap, and a `Connection` isn't `Sync`, so this keeps tools thread-free.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolContext, ToolOutput};
use crate::knowledge::KnowledgeStore;
use crate::memory::{KINDS, MemoryStore, Scope};

/// How many results a search returns by default.
const DEFAULT_LIMIT: usize = 5;

fn limit_of(args: &Value) -> usize {
    args.get("limit").and_then(|v| v.as_u64()).unwrap_or(DEFAULT_LIMIT as u64).clamp(1, 20) as usize
}

pub struct MemoryTool;

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &str {
        "memory"
    }

    fn description(&self) -> &str {
        "Durable memory across sessions: decisions, constraints, preferences, facts, and \
         lessons. `search` before assuming how this project does something. `remember` only \
         what will still matter next week — not file locations, tool output, or anything \
         re-derivable from the code."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["search", "remember"],
                    "description": "search existing memories, or record a new one"
                },
                "query": { "type": "string", "description": "search text (action=search)" },
                "scope": {
                    "type": "string",
                    "enum": ["global", "project"],
                    "description": "global = true everywhere; project = this repo only (default)"
                },
                "kind": {
                    "type": "string",
                    "enum": ["decision", "constraint", "preference", "fact", "lesson"]
                },
                "subject": { "type": "string", "description": "short topic key, e.g. \"durable memory\"" },
                "content": { "type": "string", "description": "the memory itself, one or two sentences" },
                "importance": { "type": "integer", "description": "0-100, default 60" },
                "limit": { "type": "integer" }
            },
            "required": ["action"]
        })
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let store = match MemoryStore::open(Some(&ctx.cwd)) {
            Ok(s) => s,
            Err(e) => return ToolOutput::error(format!("memory unavailable: {e}")),
        };
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("search");

        match action {
            "search" => {
                let Some(query) = args.get("query").and_then(|v| v.as_str()) else {
                    return ToolOutput::error("missing required argument: query");
                };
                match store.search(query, limit_of(&args)) {
                    Ok(hits) if hits.is_empty() => {
                        ToolOutput::ok("(no memories matched — nothing has been recorded on this)")
                    }
                    Ok(hits) => {
                        let mut out = String::new();
                        for h in hits {
                            out.push_str(&format!(
                                "[{}/{}] {}: {}\n",
                                h.row.scope, h.row.kind, h.row.subject, h.row.content
                            ));
                        }
                        ToolOutput::ok(out)
                    }
                    Err(e) => ToolOutput::error(format!("memory search failed: {e}")),
                }
            }
            "remember" => {
                let kind = args.get("kind").and_then(|v| v.as_str()).unwrap_or("fact");
                if !KINDS.contains(&kind) {
                    return ToolOutput::error(format!(
                        "invalid kind `{kind}`; expected one of {KINDS:?}"
                    ));
                }
                let (Some(subject), Some(content)) = (
                    args.get("subject").and_then(|v| v.as_str()),
                    args.get("content").and_then(|v| v.as_str()),
                ) else {
                    return ToolOutput::error("remember needs both `subject` and `content`");
                };
                let scope = args
                    .get("scope")
                    .and_then(|v| v.as_str())
                    .and_then(Scope::parse)
                    .unwrap_or(Scope::Project);
                let importance =
                    args.get("importance").and_then(|v| v.as_i64()).unwrap_or(60).clamp(0, 100);

                // Workers propose; only the main session writes durable memory
                // directly (§8). A worker's finding still has to survive review.
                let result = if ctx.is_worker {
                    store.propose(scope, kind, subject, content, importance)
                } else {
                    store.remember_deduped(scope, kind, subject, content, importance)
                };
                match result {
                    Ok((row, true)) if ctx.is_worker => ToolOutput::ok(format!(
                        "proposed {} [{}/{}] {} — pending approval by the main session",
                        row.id, row.scope, row.kind, row.subject
                    )),
                    Ok((row, true)) => ToolOutput::ok(format!(
                        "remembered {} [{}/{}] {}",
                        row.id, row.scope, row.kind, row.subject
                    )),
                    Ok((row, false)) => ToolOutput::ok(format!(
                        "already known ({}): {}: {} — nothing written",
                        row.id, row.subject, row.content
                    )),
                    Err(e) => ToolOutput::error(format!("memory write failed: {e}")),
                }
            }
            other => ToolOutput::error(format!("unknown action `{other}` (search|remember)")),
        }
    }
}

pub struct KnowledgeTool;

#[async_trait]
impl Tool for KnowledgeTool {
    fn name(&self) -> &str {
        "knowledge"
    }

    fn description(&self) -> &str {
        "Full-text search over this project's own documents and source, chunked into an index. \
         Use it to find what the repo already says about a topic before reading files at \
         random. The index maintains itself; `index` forces a rebuild."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["search", "index"] },
                "query": { "type": "string", "description": "search text (action=search)" },
                "limit": { "type": "integer" }
            },
            "required": ["action"]
        })
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let store = match KnowledgeStore::open(&ctx.cwd) {
            Ok(s) => s,
            Err(e) => return ToolOutput::error(format!("knowledge unavailable: {e}")),
        };
        match args.get("action").and_then(|v| v.as_str()).unwrap_or("search") {
            "index" => match store.index() {
                Ok(stats) => {
                    let pruned = store.prune().unwrap_or(0);
                    ToolOutput::ok(format!(
                        "indexed {} file(s) into {} chunk(s); {} unchanged, {} stale source(s) removed",
                        stats.files, stats.chunks, stats.skipped_unchanged, pruned
                    ))
                }
                Err(e) => ToolOutput::error(format!("indexing failed: {e}")),
            },
            "search" => {
                let Some(query) = args.get("query").and_then(|v| v.as_str()) else {
                    return ToolOutput::error("missing required argument: query");
                };
                match store.search(query, limit_of(&args)) {
                    Ok(hits) if hits.is_empty() => {
                        ToolOutput::ok("(no matches in the project's own documents or source)")
                    }
                    Ok(hits) => {
                        let mut out = String::new();
                        for h in hits {
                            out.push_str(&format!("--- {} (chunk {})\n{}\n\n", h.source, h.ord, h.text));
                        }
                        ToolOutput::ok(out)
                    }
                    Err(e) => ToolOutput::error(format!("knowledge search failed: {e}")),
                }
            }
            other => ToolOutput::error(format!("unknown action `{other}` (search|index)")),
        }
    }
}
