//! `grep`, `find`, `ls` — filesystem search conveniences. Walking is
//! ripgrep's own walker (the `ignore` crate): it honours `.gitignore` and skips
//! the usual noise (`.git`, `target`, `node_modules`).

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};

use super::{Tool, ToolContext, ToolOutput, resolve_path};

const SKIP_DIRS: &[&str] = &[
    "target",
    "node_modules",
    "dist",
    "build",
    "vendor",
    "reference",
];
const MAX_HITS: usize = 500;
/// Skip files larger than this when searching contents (lock files, blobs).
const MAX_FILE_SIZE: u64 = 512 * 1024;
/// Byte cap on total grep output fed back (defense in depth with MAX_HITS).
const MAX_GREP_BYTES: usize = 48 * 1024;

fn skip_dir(name: &str) -> bool {
    // Skip hidden dirs (.git, .worksmith, .venv, …) and known heavy dirs.
    name.starts_with('.') || SKIP_DIRS.contains(&name)
}

/// The walker every search shares. Hidden *files* stay in (`.env.example`),
/// hidden and heavy directories go, and `.gitignore` is honoured even outside a
/// git repo, so build output and virtualenvs never reach a search.
fn walker(root: &Path, glob: Option<&str>) -> Result<ignore::WalkBuilder, String> {
    let mut b = ignore::WalkBuilder::new(root);
    b.hidden(false)
        .require_git(false)
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(|e| {
            let is_dir = e.file_type().is_some_and(|t| t.is_dir());
            // depth 0 is the root the caller asked for; never skip it.
            e.depth() == 0 || !is_dir || !skip_dir(&e.file_name().to_string_lossy())
        });
    if let Some(g) = glob {
        let mut o = ignore::overrides::OverrideBuilder::new(root);
        o.add(g).map_err(|e| format!("invalid glob `{g}`: {e}"))?;
        b.overrides(o.build().map_err(|e| format!("invalid glob `{g}`: {e}"))?);
    }
    Ok(b)
}

pub(crate) fn walk(root: &Path, out: &mut Vec<PathBuf>, limit: usize) {
    let Ok(b) = walker(root, None) else {
        return;
    };
    for entry in b.build().flatten() {
        if out.len() >= limit {
            return;
        }
        if entry.file_type().is_some_and(|t| t.is_file()) {
            out.push(entry.into_path());
        }
    }
}

/// One file's matching lines: (line number, text).
type FileHits = (PathBuf, Vec<(usize, String)>);

/// Matching lines per file, searched in parallel. Stops collecting once
/// `cap` lines are in hand; the caller truncates and says so.
fn grep_files(
    root: &Path,
    re: &Regex,
    glob: Option<&str>,
    cap: usize,
) -> Result<Vec<FileHits>, String> {
    let found = Mutex::new(Vec::new());
    let total = AtomicUsize::new(0);
    walker(root, glob)?.build_parallel().run(|| {
        Box::new(|entry| {
            use ignore::WalkState;
            if total.load(Ordering::Relaxed) >= cap {
                return WalkState::Quit;
            }
            let Ok(entry) = entry else {
                return WalkState::Continue;
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return WalkState::Continue;
            }
            // Oversized files are lock files and data blobs: noise in context.
            if entry.metadata().map(|m| m.len()).unwrap_or(0) > MAX_FILE_SIZE {
                return WalkState::Continue;
            }
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                return WalkState::Continue;
            };
            // One pass over the whole file rejects most files before any
            // line is split out.
            if !re.is_match(&text) {
                return WalkState::Continue;
            }
            let lines: Vec<(usize, String)> = text
                .lines()
                .enumerate()
                .filter(|(_, l)| re.is_match(l))
                .map(|(i, l)| (i + 1, l.trim_end().to_string()))
                .collect();
            if !lines.is_empty() {
                total.fetch_add(lines.len(), Ordering::Relaxed);
                found.lock().unwrap().push((entry.into_path(), lines));
            }
            WalkState::Continue
        })
    });
    let mut found = found.into_inner().unwrap();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

// ---- grep -----------------------------------------------------------------

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents by regular expression, recursively and in parallel \
         (ripgrep's walker). Returns `path:line:text` matches. Honours .gitignore and \
         skips .git/target/node_modules. Faster than grep or rg through bash."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regular expression to search for." },
                "path": { "type": "string", "description": "Directory or file to search (default: cwd)." },
                "glob": { "type": "string", "description": "Only files matching this glob, e.g. `*.rs`." },
                "ignore_case": { "type": "boolean", "description": "Match case-insensitively." }
            },
            "required": ["pattern"]
        })
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let Some(pattern) = args.get("pattern").and_then(|v| v.as_str()) else {
            return ToolOutput::error("missing required argument: pattern");
        };
        let ignore_case = args
            .get("ignore_case")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let re = match regex::RegexBuilder::new(pattern)
            .case_insensitive(ignore_case)
            .build()
        {
            Ok(r) => r,
            Err(e) => return ToolOutput::error(format!("invalid regex: {e}")),
        };
        let glob = args.get("glob").and_then(|v| v.as_str()).map(String::from);
        let root = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => resolve_path(ctx, p),
            None => ctx.cwd.clone(),
        };
        if let Some(refusal) = super::approve_read_outside_cwd(ctx, &root).await {
            return ToolOutput::error(refusal);
        }

        // CPU and disk bound: off the async runtime so the TUI keeps drawing.
        let walk_root = root.clone();
        let found = match tokio::task::spawn_blocking(move || {
            grep_files(&walk_root, &re, glob.as_deref(), MAX_HITS)
        })
        .await
        {
            Ok(Ok(f)) => f,
            Ok(Err(e)) => return ToolOutput::error(e),
            Err(e) => return ToolOutput::error(format!("grep failed: {e}")),
        };

        let mut hits = 0usize;
        let mut out = String::new();
        for (file, lines) in &found {
            let rel = display_rel(&ctx.cwd, file);
            for (n, line) in lines {
                out.push_str(&format!("{rel}:{n}:{line}\n"));
                hits += 1;
                if hits >= MAX_HITS || out.len() >= MAX_GREP_BYTES {
                    out.push_str(&format!(
                        "... (truncated at {hits} matches / byte limit — narrow the pattern, or scope with `path` or `glob`)\n"
                    ));
                    return ToolOutput::ok(out);
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
        if let Some(refusal) = super::approve_read_outside_cwd(ctx, &root).await {
            return ToolOutput::error(refusal);
        }

        let mut files = Vec::new();
        walk(&root, &mut files, 10_000);

        let mut out = String::new();
        let mut hits = 0usize;
        for file in &files {
            let fname = file
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
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
        if let Some(refusal) = super::approve_read_outside_cwd(ctx, &root).await {
            return ToolOutput::error(refusal);
        }
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

pub(crate) fn display_rel(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}
