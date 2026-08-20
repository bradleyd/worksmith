//! `write` — create or overwrite a file, creating parent dirs as needed.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolContext, ToolOutput, resolve_path, unified_diff};

pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write content to a file, creating it (and any parent directories) or \
         overwriting it if it exists."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path (absolute or relative to cwd)." },
                "content": { "type": "string", "description": "Full file content to write." }
            },
            "required": ["path", "content"]
        })
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
            return ToolOutput::error("missing required argument: path");
        };
        let Some(content) = args.get("content").and_then(|v| v.as_str()) else {
            return ToolOutput::error("missing required argument: content");
        };
        let full = resolve_path(ctx, path);
        if let Some(refusal) = super::approve_write_outside_cwd(ctx, &full).await {
            return ToolOutput::error(refusal);
        }
        let existed = full.exists();
        let old = if existed { std::fs::read_to_string(&full).unwrap_or_default() } else { String::new() };

        if let Some(parent) = full.parent()
            && let Err(e) = std::fs::create_dir_all(parent) {
                return ToolOutput::error(format!("cannot create {}: {e}", parent.display()));
            }
        match std::fs::write(&full, content) {
            Ok(()) => {
                let verb = if existed { "overwrote" } else { "created" };
                let label = format!("{verb} {} ({} bytes)", full.display(), content.len());
                ToolOutput::ok(unified_diff(&label, &old, content))
            }
            Err(e) => ToolOutput::error(format!("cannot write {}: {e}", full.display())),
        }
    }
}
