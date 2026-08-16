//! `edit` — exact-match string replacement, one or more disjoint edits per call.
//!
//! Semantics (the rules that make the agent reliable):
//! - `old_string` must match **exactly** (including whitespace).
//! - Without `replace_all`, the match must be **unique**: 0 matches is an
//!   error (not found), >1 is an error (ambiguous — add more context).
//! - With `replace_all: true`, every occurrence is replaced (>=1 required).
//! - Multiple edits apply in order to the in-memory buffer, then the file is
//!   written once. If any edit fails, nothing is written.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolContext, ToolOutput, resolve_path};

pub struct EditTool;

struct EditOp {
    old: String,
    new: String,
    replace_all: bool,
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Replace exact text in a file. Provide `old_string`/`new_string` for a \
         single edit, or `edits` for several disjoint edits in one call. Each \
         `old_string` must match uniquely unless `replace_all` is set. All \
         edits are applied atomically (file is written only if all succeed)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path (absolute or relative to cwd)." },
                "old_string": { "type": "string", "description": "Exact text to replace (single-edit form)." },
                "new_string": { "type": "string", "description": "Replacement text (single-edit form)." },
                "replace_all": { "type": "boolean", "description": "Replace all occurrences (single-edit form)." },
                "edits": {
                    "type": "array",
                    "description": "Multiple disjoint edits, applied in order.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_string": { "type": "string" },
                            "new_string": { "type": "string" },
                            "replace_all": { "type": "boolean" }
                        },
                        "required": ["old_string", "new_string"]
                    }
                }
            },
            "required": ["path"]
        })
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
            return ToolOutput::error("missing required argument: path");
        };
        let full = resolve_path(ctx, path);

        let ops = match collect_ops(&args) {
            Ok(ops) if ops.is_empty() => {
                return ToolOutput::error("no edits provided (need old_string/new_string or edits[])");
            }
            Ok(ops) => ops,
            Err(e) => return ToolOutput::error(e),
        };

        let mut content = match std::fs::read_to_string(&full) {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(format!("cannot read {}: {e}", full.display())),
        };

        let mut applied = 0usize;
        for (i, op) in ops.iter().enumerate() {
            let count = content.matches(&op.old).count();
            if count == 0 {
                return ToolOutput::error(format!(
                    "edit {}: old_string not found in {}",
                    i + 1,
                    full.display()
                ));
            }
            if count > 1 && !op.replace_all {
                return ToolOutput::error(format!(
                    "edit {}: old_string is ambiguous ({count} matches) — add surrounding context or set replace_all",
                    i + 1
                ));
            }
            if op.replace_all {
                content = content.replace(&op.old, &op.new);
                applied += count;
            } else {
                content = content.replacen(&op.old, &op.new, 1);
                applied += 1;
            }
        }

        if let Err(e) = std::fs::write(&full, &content) {
            return ToolOutput::error(format!("cannot write {}: {e}", full.display()));
        }
        ToolOutput::ok(format!(
            "applied {} edit(s) ({applied} replacement(s)) to {}",
            ops.len(),
            full.display()
        ))
    }
}

fn collect_ops(args: &Value) -> Result<Vec<EditOp>, String> {
    let mut ops = Vec::new();

    // Single-edit form.
    if let Some(old) = args.get("old_string").and_then(|v| v.as_str()) {
        let new = args.get("new_string").and_then(|v| v.as_str()).unwrap_or("");
        let replace_all = args.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false);
        ops.push(EditOp { old: old.to_string(), new: new.to_string(), replace_all });
    }

    // Multi-edit form.
    if let Some(arr) = args.get("edits").and_then(|v| v.as_array()) {
        for (i, e) in arr.iter().enumerate() {
            let old = e
                .get("old_string")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("edits[{i}]: missing old_string"))?;
            let new = e.get("new_string").and_then(|v| v.as_str()).unwrap_or("");
            let replace_all = e.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false);
            ops.push(EditOp { old: old.to_string(), new: new.to_string(), replace_all });
        }
    }

    Ok(ops)
}
