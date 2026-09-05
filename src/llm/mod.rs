//! LLM client abstraction: message/tool types, a streaming client trait, and
//! provider implementations. M1 ships an OpenAI-compatible client (vLLM/Qwen,
//! OpenRouter, RunPod, local). Anthropic is a later second implementation.

pub mod openai;
pub mod rescue;

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
    /// The reasoning the model emitted before this message, when the provider
    /// exposed it. Recorded for the transcript only: `message_to_json` builds
    /// the wire payload field by field, so this never goes back to the
    /// provider. Without it, a turn that spends its whole budget thinking and
    /// returns nothing leaves no trace of what it was thinking about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// The provider's `finish_reason` for the completion this message came
    /// from. "length" on an empty message is the difference between "the model
    /// was done" and "the model was cut off mid-thought".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    /// Which model answered. A worker override or a role switch means the
    /// session model is not the answer, and "which model actually served this?"
    /// was otherwise unanswerable from our own artifacts — it took reading the
    /// provider's log files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Self { role: Role::System, content: Some(text.into()), tool_calls: vec![], tool_call_id: None, name: None, reasoning: None, finish_reason: None, model: None }
    }
    pub fn user(text: impl Into<String>) -> Self {
        Self { role: Role::User, content: Some(text.into()), tool_calls: vec![], tool_call_id: None, name: None, reasoning: None, finish_reason: None, model: None }
    }
    pub fn assistant(content: Option<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self { role: Role::Assistant, content, tool_calls, tool_call_id: None, name: None, reasoning: None, finish_reason: None, model: None }
    }
    pub fn tool_result(call_id: impl Into<String>, name: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            tool_calls: vec![],
            tool_call_id: Some(call_id.into()),
            name: Some(name.into()),
            reasoning: None,
            finish_reason: None,
            model: None,
        }
    }

    /// Attach the provider's reasoning trace and finish reason. Transcript-only
    /// metadata; it does not change what is sent back to the model.
    pub fn with_trace(
        mut self,
        reasoning: Option<String>,
        finish_reason: Option<String>,
        model: Option<String>,
    ) -> Self {
        self.reasoning = reasoning;
        self.finish_reason = finish_reason;
        self.model = model;
        self
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
    /// The share of `completion_tokens` spent reasoning, when the provider
    /// breaks it out (`completion_tokens_details.reasoning_tokens`). This is the
    /// number that explains a turn that took a minute and said nothing.
    pub reasoning_tokens: u32,
}

/// Rough token estimate for the pieces Worksmith assembled into a prompt.
///
/// The provider's `prompt_tokens` remains the source of truth. These numbers
/// are local attribution: enough to answer "what made this request large?"
/// without writing prompt text into the session log.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextBreakdown {
    pub system_tokens: u32,
    pub loaded_skill_tokens: u32,
    pub memory_tokens: u32,
    pub history_tokens: u32,
    pub latest_user_tokens: u32,
    pub tool_schema_tokens: u32,
}

/// Marker attached to failures that are worth trying again: a dropped
/// connection, a timeout, a 429, a 5xx. Carried as the *source* of the error so
/// the message the user sees stays the real one.
#[derive(Debug)]
pub struct Transient;

impl std::fmt::Display for Transient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "temporary failure")
    }
}

impl std::error::Error for Transient {}

/// Whether an error carries the [`Transient`] marker, or is a transport failure
/// reqwest already knows is one. A tunnel that goes away while the agent is
/// mid-run is the common case and it is entirely recoverable.
pub fn is_transient(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        if cause.downcast_ref::<Transient>().is_some() {
            return true;
        }
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|e| e.is_connect() || e.is_timeout() || e.is_request())
    })
}

/// A request for one model completion.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    /// Local attribution for prompt size. It is not sent to providers.
    pub context_breakdown: Option<ContextBreakdown>,
    /// f64, not f32. `serde_json::Value` has no f32 variant, so an f32 is
    /// widened on the way out and its representation error becomes visible:
    /// `0.7f32` serializes as `0.699999988079071`. Most providers ignore the
    /// extra digits; Z.AI rejects the request outright ("The temperature
    /// parameter is illegal.：限制小数点[2]位"). TOML parses floats as f64
    /// already, so the narrowing bought nothing and only lost precision.
    pub temperature: Option<f64>,
    /// Nucleus and top-k sampling, when the model asks for specific values.
    /// Unset means "whatever the server defaults to", which is what every
    /// request did before `[models]` existed.
    pub top_p: Option<f64>,
    pub top_k: Option<u32>,
    pub max_tokens: Option<u32>,
    /// How much the model may deliberate before answering. `None` sends nothing
    /// at all, which is the default: providers disagree about these fields and a
    /// strict one rejects the request outright.
    pub thinking: Option<Thinking>,
    /// Per-request provider routing, overriding the provider's configured
    /// `sort`. Live-settable (`/route`) for the same reason thinking is: you
    /// learn which lever you want while the work is in front of you.
    pub sort: Option<String>,
}

/// How much deliberation to ask for. `Budget` is the middle setting between
/// "think as long as you like" and "don't think": a model with no budget fills
/// whatever `max_tokens` allows and can reach the cap without ever answering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Thinking {
    Off,
    On,
    /// At most this many reasoning tokens before answering.
    Budget(u32),
    /// How hard to think, in the providers' own vocabulary. Both OpenRouter and
    /// vLLM expose this natively, and for models that only understand effort a
    /// budget is converted into one anyway — so asking for it directly skips a
    /// translation we were paying for.
    Effort(Effort),
}

/// Reasoning effort levels, lowest to highest. `minimal` and `none` are not the
/// same thing: `none` is [`Thinking::Off`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    Minimal,
    Low,
    Medium,
    High,
    /// Above `high`. OpenRouter documents it, and some vLLM builds accept only
    /// `xhigh`, `medium` and `low` — servers disagree about which levels exist,
    /// so worksmith passes the word through and lets the provider object.
    Xhigh,
    Max,
}

impl Effort {
    pub fn parse(s: &str) -> Option<Effort> {
        match s.trim().to_ascii_lowercase().as_str() {
            "minimal" => Some(Effort::Minimal),
            "low" => Some(Effort::Low),
            "medium" | "med" => Some(Effort::Medium),
            "high" => Some(Effort::High),
            "xhigh" | "x-high" => Some(Effort::Xhigh),
            "max" => Some(Effort::Max),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Minimal => "minimal",
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
        }
    }
}

impl Thinking {
    /// Every setting except `Off` means the model reasons.
    pub fn enabled(self) -> bool {
        self != Thinking::Off
    }

    pub fn budget(self) -> Option<u32> {
        match self {
            Thinking::Budget(n) => Some(n),
            _ => None,
        }
    }

    pub fn effort(self) -> Option<Effort> {
        match self {
            Thinking::Effort(e) => Some(e),
            _ => None,
        }
    }
}

/// How a provider spells "don't think". There is no common field, and sending
/// the wrong one is worse than sending nothing — OpenRouter rejects
/// `chat_template_kwargs` outright.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThinkingDialect {
    /// OpenRouter: `reasoning: {"enabled": false}`.
    #[default]
    Reasoning,
    /// vLLM / oMLX / llama.cpp: `chat_template_kwargs: {"enable_thinking": false}`.
    ChatTemplate,
}

impl ThinkingDialect {
    /// Can this dialect express a reasoning *budget*, or only on/off? OpenRouter
    /// and OpenAI take `reasoning: {max_tokens}`; a chat-template kwarg is a
    /// bool and nothing more. Asking for a budget where none exists must say so
    /// rather than quietly behave like plain `on`.
    pub fn supports_budget(self) -> bool {
        matches!(self, ThinkingDialect::Reasoning)
    }

    /// The request field this dialect writes — for error messages that have to
    /// name what we actually sent.
    pub fn field(self) -> &'static str {
        match self {
            ThinkingDialect::Reasoning => "reasoning",
            ThinkingDialect::ChatTemplate => "chat_template_kwargs",
        }
    }

    pub fn parse(s: &str) -> Option<ThinkingDialect> {
        match s.trim().to_ascii_lowercase().as_str() {
            "reasoning" => Some(ThinkingDialect::Reasoning),
            "chat-template" | "chat_template" => Some(ThinkingDialect::ChatTemplate),
            _ => None,
        }
    }

    /// Guess from the endpoint when the config doesn't say. Hosted gateways take
    /// `reasoning`; self-hosted serving stacks take the template kwarg.
    ///
    /// This is a heuristic on a hostname, and it is wrong the moment a gateway
    /// sits behind a proxy or a vanity domain — so it is only ever a fallback
    /// for an unset `thinking-param`, and every caller carries the [`DialectSource`]
    /// alongside it so a rejected request can say which spelling it used and
    /// where that came from.
    pub fn guess_from_url(base_url: &str) -> ThinkingDialect {
        let u = base_url.to_ascii_lowercase();
        const HOSTED_GATEWAYS: [&str; 4] =
            ["openrouter.ai", "api.openai.com", "api.together.xyz", "api.groq.com"];
        if HOSTED_GATEWAYS.iter().any(|h| u.contains(h)) {
            ThinkingDialect::Reasoning
        } else {
            ThinkingDialect::ChatTemplate
        }
    }
}

/// Where a provider's [`ThinkingDialect`] came from. A guess that turns out to
/// be wrong produces an HTTP 400 from the provider; knowing the value was
/// guessed (and not configured) is the difference between a baffling error and
/// one that tells you to set `thinking-param`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialectSource {
    /// The provider's `thinking-param` said so.
    Explicit,
    /// Inferred from the base URL.
    Guessed,
}

impl DialectSource {
    pub fn describe(self) -> &'static str {
        match self {
            DialectSource::Explicit => "from this provider's `thinking-param`",
            DialectSource::Guessed => "guessed from the provider's base-url",
        }
    }
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
    /// Something about the request was quietly not honored — a setting this
    /// provider cannot express. Surfaced rather than swallowed, because a
    /// silently dropped setting looks exactly like one that isn't working.
    Warning(String),
}

/// The assembled result of a streamed completion.
#[derive(Debug, Clone, Default)]
pub struct Completion {
    pub content: Option<String>,
    /// Accumulated reasoning/thinking, if the provider exposed it.
    pub reasoning: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    /// Set when `tool_calls` was empty and a call had to be read out of the
    /// text (see `llm::rescue`). Carried on the completion rather than sent
    /// down the stream sink, because the sink reaches the display and nothing
    /// else: a warning emitted there is never written to the session, so the
    /// rate is unrecoverable afterwards — and the rate is the whole signal.
    pub rescued: Option<String>,
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

/// Silence between chunks before a request is abandoned, when a provider does
/// not set its own. Generous on purpose: the long gap is time-to-first-token,
/// and a loaded local server can take minutes to produce it. The job here is to
/// bound a hang, not to police slowness.
pub const DEFAULT_STREAM_IDLE_SECS: u64 = 600;

// A guard, not a test: the job of this timeout is to bound a hang, not to
// police slowness, and a default low enough to interrupt a loaded local server
// mid-prefill would do real damage quietly. Fails the build, not a test run.
const _: () = assert!(
    DEFAULT_STREAM_IDLE_SECS >= 300,
    "too low: a queued local model can take minutes to produce a first token"
);

/// Build a streaming client for a resolved provider+model. Shared by the main
/// session and by workers running on a different (usually cheaper) model, so
/// there's one place that knows how a provider becomes a client.
pub fn client_for(resolved: &crate::config::ResolvedModel) -> anyhow::Result<std::sync::Arc<dyn LlmClient>> {
    client_and_http(resolved).map(|(c, _)| c)
}

/// As [`client_for`], but also hands back the underlying HTTP client.
///
/// Anything else that needs to talk to the same provider must reuse this one.
/// Building a second is not a small waste: it is synchronous and reads the
/// macOS keychain, which cost 8 seconds cold and blocked the runtime while it
/// did — no timer fires through it.
pub fn client_and_http(
    resolved: &crate::config::ResolvedModel,
) -> anyhow::Result<(std::sync::Arc<dyn LlmClient>, reqwest::Client)> {
    use anyhow::{Context, bail};
    // `read_timeout`, not `timeout`. A total cap would kill a legitimate long
    // generation — a 515s call is a measurement here, not a hypothetical. This
    // bounds the *gap between chunks*, so a server that accepts the request and
    // then goes silent cannot hang the session indefinitely, which is otherwise
    // exactly what happens: the supervisor's `request-timeout` watches spawned
    // workers and nothing watches the main loop.
    let idle = std::time::Duration::from_secs(
        resolved.provider.stream_idle_timeout.unwrap_or(DEFAULT_STREAM_IDLE_SECS),
    );
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .read_timeout(idle)
        .build()
        .context("building HTTP client")?;
    // Warn once per variable, not once per worker: a fan-out of five would
    // otherwise print the same line five times.
    if let Some(var) = &resolved.missing_key_env {
        static WARNED: std::sync::Mutex<Option<std::collections::HashSet<String>>> =
            std::sync::Mutex::new(None);
        let mut guard = WARNED.lock().unwrap();
        let seen = guard.get_or_insert_with(std::collections::HashSet::new);
        if seen.insert(var.clone()) {
            eprintln!(
                "warning: ${var} is not set, so requests to `{}` go out with no API key. \
                 Export it, or drop `api-key-env` if the endpoint needs none.",
                resolved.provider.base_url
            );
        }
    }

    match resolved.provider.kind.as_str() {
        "openai-compat" => {
            let mut c = openai::OpenAiCompatClient::new(
                http.clone(),
                resolved.provider.base_url.clone(),
                resolved.api_key.clone(),
            );
            if let Some(d) = resolved.provider.thinking_param.as_deref().and_then(ThinkingDialect::parse)
            {
                c = c.with_thinking_dialect(d, DialectSource::Explicit);
            }
            c = c
                .with_budget_param(resolved.provider.reasoning_budget_param.clone())
                .with_sort(resolved.provider.sort.clone())
                .with_stream_idle(idle);
            Ok((std::sync::Arc::new(c), http))
        }
        other => bail!("provider type `{other}` is not supported (use `openai-compat`)"),
    }
}

/// Ask an openai-compat server what window it actually serves, and say so when
/// the config disagrees.
///
/// Both directions are worth catching and neither announces itself today.
/// Over-declaring is loud but late: compaction waits for a trigger the server
/// rejects the request long before reaching (`config.rs`). Under-declaring is
/// *silent* and expensive — compaction fires early, throws away tool results
/// the model was working from, the model re-reads the same files, and the turn
/// runs out of steps. From the outside that is indistinguishable from a model
/// that is simply bad at the task, and it took a session-file autopsy to tell
/// the difference.
///
/// Best-effort: many servers do not publish `max_model_len`, and one that does
/// not is not a problem. Never fails a request.
pub async fn warn_on_context_mismatch(
    http: &reqwest::Client,
    base_url: &str,
    model: &str,
    configured: usize,
) -> Option<String> {
    // Takes a client rather than building one. Building a `reqwest::Client` is
    // *synchronous* and, with `rustls-native-certs` on macOS, reads the system
    // keychain — measured at 8 seconds cold on a real machine. A second client
    // built at startup for this check meant worksmith sat there before drawing
    // the TUI, and because the cost is synchronous no timer could interrupt it:
    // a `tokio::time::timeout` around the whole thing did not fire.
    let resp = http.get(format!("{base_url}/models")).send().await.ok()?;
    let body: serde_json::Value = resp.json().await.ok()?;
    let served = body
        .get("data")?
        .as_array()?
        .iter()
        .find(|m| m.get("id").and_then(|i| i.as_str()) == Some(model))?
        .get("max_model_len")?
        .as_u64()? as usize;

    context_mismatch(model, served, configured)
}

/// The comparison, without the network. Separated so it can be tested: the
/// interesting part is the judgement, and a test that re-derives the arithmetic
/// proves only that it can do arithmetic.
pub fn context_mismatch(model: &str, served: usize, configured: usize) -> Option<String> {
    // A little slack: a config rounded to 128000 against a served 131072 is
    // someone being approximate, not someone being wrong.
    let slack = served / 20;
    if configured > served + slack {
        Some(format!(
            "`{model}` serves {served} tokens but `context` is set to {configured}. Requests \
             will be rejected before compaction ever fires — lower it to {served}."
        ))
    } else if configured + slack < served {
        Some(format!(
            "`{model}` serves {served} tokens but `context` is set to {configured}, so part of \
             the window goes unused and compaction fires early — which throws away work the \
             model then has to redo. Raise it to {served}."
        ))
    } else {
        None
    }
}

/// A worker's model, resolved and ready: the client that speaks to it and the
/// model name to ask for. Kept together because a cheaper model often lives
/// behind a different provider, not just a different name.
#[derive(Clone)]
pub struct ModelOverride {
    pub client: std::sync::Arc<dyn LlmClient>,
    pub model: String,
    /// Sampling and prices for *this* model, already resolved against the
    /// config. Carried because a model is not just a name: switching to one
    /// and keeping the previous model's temperature or context window is the
    /// half-swap that makes a request the server rejects.
    pub settings: crate::config::ModelSettings,
    /// This model's window, `[models."…"].context` falling back to the global
    /// `agent.context-limit` — the same expression startup uses, so a model
    /// reached this way is configured exactly as one started on.
    pub context_limit: usize,
    /// Sampling temperature after the same precedence startup applies: the
    /// model's own entry wins, the global `temperature` is the fallback.
    pub temperature: Option<f64>,
    /// Set when `api-key-env` names a variable that is not exported. The
    /// caller should surface it — in a TUI as a notice, never as `eprintln!`,
    /// which paints over the frame.
    pub missing_key_env: Option<String>,
}

impl ModelOverride {
    /// Resolve `spec` (`provider/model`, or a bare model when one provider is
    /// configured) against the config.
    pub fn resolve(config: &crate::config::Config, spec: &str) -> anyhow::Result<ModelOverride> {
        let resolved = config.resolve_model(Some(spec))?;
        Ok(ModelOverride {
            client: client_for(&resolved)?,
            model: resolved.model,
            context_limit: resolved.settings.context.unwrap_or_else(|| config.context_limit()),
            temperature: resolved.settings.temperature.or(config.temperature),
            settings: resolved.settings,
            missing_key_env: resolved.missing_key_env,
        })
    }
}
