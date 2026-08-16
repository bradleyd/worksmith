//! `read` — read a text file, optionally a line range, with line numbers.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolContext, ToolOutput, resolve_path};

pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a text file from the filesystem. Returns the content with line \
         numbers. Use `offset` and `limit` to read a slice of a large file."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path (absolute or relative to cwd)." },
                "offset": { "type": "integer", "description": "1-based line to start from.", "minimum": 1 },
                "limit": { "type": "integer", "description": "Max number of lines to read.", "minimum": 1 }
            },
            "required": ["path"]
        })
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
            return ToolOutput::error("missing required argument: path");
        };
        let full = resolve_path(ctx, path);
        let text = match std::fs::read_to_string(&full) {
            Ok(t) => t,
            Err(e) => return ToolOutput::error(format!("cannot read {}: {e}", full.display())),
        };

        let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(1).max(1) as usize;
        let limit = args.get("limit").and_then(|v| v.as_u64()).map(|v| v as usize);

        let mut out = String::new();
        let mut count = 0usize;
        for (i, line) in text.lines().enumerate() {
            let lineno = i + 1;
            if lineno < offset {
                continue;
            }
            if let Some(lim) = limit
                && count >= lim {
                    break;
                }
            out.push_str(&format!("{lineno:>6}\t{line}\n"));
            count += 1;
        }

        if out.is_empty() {
            return ToolOutput::ok("(no lines in range or empty file)");
        }
        ToolOutput::ok(out)
    }
}
