//! `grep`, `find`, `ls` — filesystem search conveniences. A small recursive
//! walker skips the usual noise (`.git`, `target`, `node_modules`).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};

use super::{Tool, ToolContext, ToolOutput, resolve_path};

const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", ".worksmith"];
const MAX_HITS: usize = 500;

fn walk(root: &Path, out: &mut Vec<PathBuf>, limit: usize) {
    if out.len() >= limit {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            if SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            walk(&path, out, limit);
        } else {
            out.push(path);
        }
        if out.len() >= limit {
            return;
        }
    }
}

// ---- grep -----------------------------------------------------------------

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents by regular expression, recursively. Returns \
         `path:line:text` matches. Skips .git/target/node_modules."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regular expression to search for." },
                "path": { "type": "string", "description": "Directory or file to search (default: cwd)." }
            },
            "required": ["pattern"]
        })
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let Some(pattern) = args.get("pattern").and_then(|v| v.as_str()) else {
            return ToolOutput::error("missing required argument: pattern");
        };
        let re = match Regex::new(pattern) {
            Ok(r) => r,
            Err(e) => return ToolOutput::error(format!("invalid regex: {e}")),
        };
        let root = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => resolve_path(ctx, p),
            None => ctx.cwd.clone(),
        };

        let mut files = Vec::new();
        if root.is_file() {
            files.push(root.clone());
        } else {
            walk(&root, &mut files, 10_000);
        }

        let mut hits = 0usize;
        let mut out = String::new();
        for file in &files {
            let Ok(text) = std::fs::read_to_string(file) else { continue };
            let rel = display_rel(&ctx.cwd, file);
            for (i, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    out.push_str(&format!("{}:{}:{}\n", rel, i + 1, line.trim_end()));
                    hits += 1;
                    if hits >= MAX_HITS {
                        out.push_str(&format!("... (truncated at {MAX_HITS} matches)\n"));
                        return ToolOutput::ok(out);
                    }
                }
            }
        }

        if out.is_empty() {
            ToolOutput::ok("(no matches)")
        } else {
            ToolOutput::ok(out)
        }
    }
}

// ---- find -----------------------------------------------------------------

pub struct FindTool;

#[async_trait]
impl Tool for FindTool {
    fn name(&self) -> &str {
        "find"
    }

    fn description(&self) -> &str {
        "Find files whose name matches a regular expression, recursively. \
         Skips .git/target/node_modules."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Regular expression matched against the file name." },
                "path": { "type": "string", "description": "Directory to search (default: cwd)." }
            },
            "required": ["name"]
        })
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let Some(name) = args.get("name").and_then(|v| v.as_str()) else {
            return ToolOutput::error("missing required argument: name");
        };
        let re = match Regex::new(name) {
            Ok(r) => r,
            Err(e) => return ToolOutput::error(format!("invalid regex: {e}")),
        };
        let root = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => resolve_path(ctx, p),
            None => ctx.cwd.clone(),
        };

        let mut files = Vec::new();
        walk(&root, &mut files, 10_000);

        let mut out = String::new();
        let mut hits = 0usize;
        for file in &files {
            let fname = file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if re.is_match(&fname) {
                out.push_str(&format!("{}\n", display_rel(&ctx.cwd, file)));
                hits += 1;
                if hits >= MAX_HITS {
                    out.push_str(&format!("... (truncated at {MAX_HITS} results)\n"));
                    break;
                }
            }
        }

        if out.is_empty() {
            ToolOutput::ok("(no matches)")
        } else {
            ToolOutput::ok(out)
        }
    }
}

// ---- ls -------------------------------------------------------------------

pub struct LsTool;

#[async_trait]
impl Tool for LsTool {
    fn name(&self) -> &str {
        "ls"
    }

    fn description(&self) -> &str {
        "List the entries of a directory (directories are suffixed with `/`)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory to list (default: cwd)." }
            }
        })
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let root = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => resolve_path(ctx, p),
            None => ctx.cwd.clone(),
        };
        let entries = match std::fs::read_dir(&root) {
            Ok(e) => e,
            Err(e) => return ToolOutput::error(format!("cannot list {}: {e}", root.display())),
        };

        let mut names: Vec<String> = Vec::new();
        for entry in entries.flatten() {
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let mut name = entry.file_name().to_string_lossy().to_string();
            if is_dir {
                name.push('/');
            }
            names.push(name);
        }
        names.sort();

        if names.is_empty() {
            ToolOutput::ok("(empty directory)")
        } else {
            ToolOutput::ok(names.join("\n"))
        }
    }
}

fn display_rel(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}
