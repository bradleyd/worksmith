//! `write` — create or overwrite a file, creating parent dirs as needed.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolContext, ToolOutput, resolve_path};

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

        if let Some(parent) = full.parent()
            && let Err(e) = std::fs::create_dir_all(parent) {
                return ToolOutput::error(format!("cannot create {}: {e}", parent.display()));
            }
        match std::fs::write(&full, content) {
            Ok(()) => ToolOutput::ok(format!(
                "wrote {} ({} bytes)",
                full.display(),
                content.len()
            )),
            Err(e) => ToolOutput::error(format!("cannot write {}: {e}", full.display())),
        }
    }
}
