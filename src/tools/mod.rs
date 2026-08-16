//! Built-in tools and the registry that exposes them to the model. Each tool
//! advertises a JSON Schema and returns a structured [`ToolOutput`].

mod bash;
mod edit;
mod read;
mod search;
mod write;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::llm::ToolDef;

/// Runtime context passed to every tool invocation.
pub struct ToolContext {
    pub cwd: PathBuf,
    pub session_id: String,
    pub bash_timeout: Duration,
}

/// The structured result of a tool run.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: false }
    }
    pub fn error(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: true }
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
