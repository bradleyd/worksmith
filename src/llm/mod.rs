//! LLM client abstraction: message/tool types, a streaming client trait, and
//! provider implementations. M1 ships an OpenAI-compatible client (vLLM/Qwen,
//! OpenRouter, RunPod, local). Anthropic is a later second implementation.

pub mod openai;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// A conversation role. Serialized lowercase to match the OpenAI wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A single tool invocation requested by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON string of the arguments (parsed by the tool layer).
    pub arguments: String,
}

/// One conversation message. Covers user/system/assistant text, assistant
/// tool-call requests, and tool results (via `tool_call_id`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Set on `Role::Tool` messages to link a result back to its call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Optional tool name (for `Role::Tool` messages).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Self { role: Role::System, content: Some(text.into()), tool_calls: vec![], tool_call_id: None, name: None }
    }
    pub fn user(text: impl Into<String>) -> Self {
        Self { role: Role::User, content: Some(text.into()), tool_calls: vec![], tool_call_id: None, name: None }
    }
    pub fn assistant(content: Option<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self { role: Role::Assistant, content, tool_calls, tool_call_id: None, name: None }
    }
    pub fn tool_result(call_id: impl Into<String>, name: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            tool_calls: vec![],
            tool_call_id: Some(call_id.into()),
            name: Some(name.into()),
        }
    }
}

/// A tool definition advertised to the model (JSON Schema parameters).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Token accounting for a completion.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

/// A request for one model completion.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

/// Incremental events emitted during streaming. The client forwards these to
/// the caller's channel as they arrive off the wire.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A chunk of assistant text.
    TextDelta(String),
    /// A chunk of model reasoning/thinking (from the provider's `reasoning`
    /// field). Display-only — never sent back to the model.
    ReasoningDelta(String),
    /// A tool call began (name known).
    ToolCallStarted { index: usize, id: String, name: String },
    /// A chunk of a tool call's argument JSON.
    ToolCallArgsDelta { index: usize, delta: String },
    /// Final token usage (if the provider reports it).
    Usage(Usage),
}

/// The assembled result of a streamed completion.
#[derive(Debug, Clone, Default)]
pub struct Completion {
    pub content: Option<String>,
    /// Accumulated reasoning/thinking, if the provider exposed it.
    pub reasoning: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub finish_reason: Option<String>,
}

/// A streaming chat client. Implementations push [`StreamEvent`]s to `sink`
/// as they arrive and return the assembled [`Completion`] when done.
#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn stream(
        &self,
        req: ChatRequest,
        sink: mpsc::Sender<StreamEvent>,
        cancel: CancellationToken,
    ) -> anyhow::Result<Completion>;
}

/// Build a streaming client for a resolved provider+model. Shared by the main
/// session and by workers running on a different (usually cheaper) model, so
/// there's one place that knows how a provider becomes a client.
pub fn client_for(resolved: &crate::config::ResolvedModel) -> anyhow::Result<std::sync::Arc<dyn LlmClient>> {
    use anyhow::{Context, bail};
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .context("building HTTP client")?;
    match resolved.provider.kind.as_str() {
        "openai-compat" => Ok(std::sync::Arc::new(openai::OpenAiCompatClient::new(
            http,
            resolved.provider.base_url.clone(),
            resolved.api_key.clone(),
        ))),
        other => bail!("provider type `{other}` is not supported (use `openai-compat`)"),
    }
}

/// A worker's model, resolved and ready: the client that speaks to it and the
/// model name to ask for. Kept together because a cheaper model often lives
/// behind a different provider, not just a different name.
#[derive(Clone)]
pub struct ModelOverride {
    pub client: std::sync::Arc<dyn LlmClient>,
    pub model: String,
}

impl ModelOverride {
    /// Resolve `spec` (`provider/model`, or a bare model when one provider is
    /// configured) against the config.
    pub fn resolve(config: &crate::config::Config, spec: &str) -> anyhow::Result<ModelOverride> {
        let resolved = config.resolve_model(Some(spec))?;
        Ok(ModelOverride { client: client_for(&resolved)?, model: resolved.model })
    }
}
