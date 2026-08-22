//! Built-in tools and the registry that exposes them to the model. Each tool
//! advertises a JSON Schema and returns a structured [`ToolOutput`].

mod bash;
pub mod approval;
pub mod policy;
mod doc;
mod edit;
mod read;
mod recall;
mod search;
mod skill;
mod web;
pub(crate) use search::{display_rel, walk};
mod write;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::llm::ToolDef;

pub use bash::dangerous_command;

/// Runtime context passed to every tool invocation.
#[derive(Clone)]
pub struct ToolContext {
    pub cwd: PathBuf,
    pub session_id: String,
    pub bash_timeout: Duration,
    /// Spawned workers *propose* memories instead of writing them (§8).
    pub is_worker: bool,
    /// Who to ask before an outward-facing or irreversible action. Defaults to
    /// approving everything, which is right for tests and for the eval harness
    /// and wrong for anything a user is watching — the real front ends install
    /// their own.
    pub approver: std::sync::Arc<dyn approval::Approver>,
    /// Skills loaded this session, name -> body. The agent pins these to the
    /// system prompt, above the compaction line: a skill is standing
    /// instruction, not conversation, and compacting it away made the model
    /// load the same 4kB pack eight times in one session.
    pub loaded_skills: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
}

impl Default for ToolContext {
    fn default() -> Self {
        Self {
            cwd: PathBuf::from("."),
            session_id: String::new(),
            bash_timeout: Duration::from_secs(120),
            is_worker: false,
            approver: std::sync::Arc::new(approval::AutoApprove),
            loaded_skills: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }
}

/// The structured result of a tool run.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    /// If true, the whole turn is aborted (e.g. a refused destructive command).
    pub fatal: bool,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: false, fatal: false }
    }
    pub fn error(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: true, fatal: false }
    }
    /// A hard stop: the command was refused and the turn should end immediately.
    pub fn blocked(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: true, fatal: true }
    }
}

/// A tool the model can call.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema for the tool's arguments.
    fn parameters(&self) -> Value;
    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput;

    fn to_def(&self) -> ToolDef {
        ToolDef {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.parameters(),
        }
    }
}

/// Registry of available tools, keyed by name.
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
    order: Vec<String>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: HashMap::new(), order: Vec::new() }
    }

    /// All built-in tools: read/write/edit/bash/grep/find/ls.
    pub fn with_builtins() -> Self {
        let mut r = Self::new();
        r.register(Box::new(read::ReadTool));
        r.register(Box::new(write::WriteTool));
        r.register(Box::new(edit::EditTool));
        r.register(Box::new(bash::BashTool));
        r.register(Box::new(search::GrepTool));
        r.register(Box::new(search::FindTool));
        r.register(Box::new(search::LsTool));
        r.register(Box::new(doc::DocTool));
        r.register(Box::new(recall::MemoryTool));
        r.register(Box::new(recall::KnowledgeTool));
        r.register(Box::new(web::WebTool));
        r.register(Box::new(skill::SkillTool));
        r
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        let name = tool.name().to_string();
        if !self.tools.contains_key(&name) {
            self.order.push(name.clone());
        }
        self.tools.insert(name, tool);
    }

    /// Tool definitions to advertise to the model, in registration order.
    pub fn defs(&self) -> Vec<ToolDef> {
        self.order.iter().filter_map(|n| self.tools.get(n)).map(|t| t.to_def()).collect()
    }

    /// Run a tool by name. Unknown tools return an error output (fed back to the
    /// model rather than crashing the turn).
    pub async fn run(&self, name: &str, args: Value, ctx: &ToolContext) -> ToolOutput {
        match self.tools.get(name) {
            Some(tool) => tool.run(args, ctx).await,
            None => ToolOutput::error(format!("unknown tool: {name}")),
        }
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve a possibly-relative path argument against the tool cwd.
fn resolve_path(ctx: &ToolContext, p: &str) -> PathBuf {
    let path = PathBuf::from(p);
    if path.is_absolute() { path } else { ctx.cwd.join(path) }
}

/// Ask before writing outside the working directory. The user pointed the agent
/// at a project; a path that leaves it — `/etc/hosts`, `~/.ssh/config`, a
/// sibling repo — is a different kind of act than editing the code in front of
/// it, and is not what "edit this file" was understood to mean.
///
/// Returns the refusal text if the write must not proceed.
async fn approve_write_outside_cwd(ctx: &ToolContext, full: &Path) -> Option<String> {
    if !policy::path_escapes_cwd(full, &ctx.cwd) {
        return None;
    }
    let reason = "writes outside the working directory";
    let what = full.display().to_string();
    match ctx.approver.ask(&what, reason).await {
        approval::Approval::Once | approval::Approval::AlwaysThisSession => None,
        approval::Approval::Deny => Some(format!(
            "the user did not approve writing outside {} (to {what}).\n\
             Do not retry it. Work inside the project directory instead.",
            ctx.cwd.display()
        )),
    }
}

/// A summary line + unified diff of a file change, for `edit`/`write` output.
/// The TUI colorizes this (it knows the tool name); plain modes show it as text.
fn unified_diff(label: &str, old: &str, new: &str) -> String {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(old, new);
    let (mut ins, mut del) = (0usize, 0usize);
    for c in diff.iter_all_changes() {
        match c.tag() {
            ChangeTag::Insert => ins += 1,
            ChangeTag::Delete => del += 1,
            ChangeTag::Equal => {}
        }
    }
    let summary = format!("{label} (+{ins} -{del})");
    if ins == 0 && del == 0 {
        return format!("{summary} — no changes");
    }
    let body = diff
        .unified_diff()
        .context_radius(3)
        .header("before", "after")
        .to_string();
    format!("{summary}\n{body}")
}
