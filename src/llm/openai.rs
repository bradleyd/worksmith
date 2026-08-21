//! OpenAI-compatible streaming client. Works against vLLM (Qwen via the
//! `hermes` tool-call parser), OpenRouter, RunPod, and local servers — only
//! the base URL and (optional) API key change.

use anyhow::{Context, bail};
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{
    ChatRequest, Completion, DialectSource, LlmClient, Message, Role, StreamEvent, Thinking,
    ToolCall, Usage,
};

/// OpenAI-compatible chat client.
pub struct OpenAiCompatClient {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    thinking_dialect: crate::llm::ThinkingDialect,
    dialect_source: DialectSource,
    /// Request field for a reasoning budget, when this provider has one.
    budget_param: Option<String>,
    /// OpenRouter provider routing preference.
    sort: Option<String>,
    /// A dropped reasoning budget is worth saying once, not once per step.
    warned_no_budget: std::sync::atomic::AtomicBool,
}

impl OpenAiCompatClient {
    /// `base_url` should include the API root (e.g. `http://host:8000/v1`).
    pub fn new(http: reqwest::Client, base_url: impl Into<String>, api_key: Option<String>) -> Self {
        let base_url = base_url.into();
        let base_url = base_url.trim_end_matches('/').to_string();
        let thinking_dialect = crate::llm::ThinkingDialect::guess_from_url(&base_url);
        Self {
            http,
            base_url,
            api_key,
            thinking_dialect,
            dialect_source: DialectSource::Guessed,
            budget_param: None,
            sort: None,
            warned_no_budget: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Override the guessed dialect (config `thinking-param`).
    pub fn with_thinking_dialect(
        mut self,
        d: crate::llm::ThinkingDialect,
        source: DialectSource,
    ) -> Self {
        self.thinking_dialect = d;
        self.dialect_source = source;
        self
    }

    /// The field this provider uses for a reasoning token budget (vLLM's
    /// `thinking_token_budget`), when it has one.
    pub fn with_budget_param(mut self, param: Option<String>) -> Self {
        self.budget_param = param;
        self
    }

    /// OpenRouter provider routing (`throughput` | `latency` | `price`).
    pub fn with_sort(mut self, sort: Option<String>) -> Self {
        self.sort = sort;
        self
    }

    /// The resolved dialect and where it came from, for status display.
    pub fn dialect(&self) -> (crate::llm::ThinkingDialect, DialectSource) {
        (self.thinking_dialect, self.dialect_source)
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }
}

#[async_trait::async_trait]
impl LlmClient for OpenAiCompatClient {
    async fn stream(
        &self,
        req: ChatRequest,
        sink: mpsc::Sender<StreamEvent>,
        cancel: CancellationToken,
    ) -> anyhow::Result<Completion> {
        // A reasoning budget this dialect cannot express is dropped, not
        // silently downgraded to plain "think as long as you like" — say so once
        // so the setting isn't trusted to be doing something it isn't.
        if let Some(Thinking::Budget(n)) = req.thinking
            && !self.thinking_dialect.supports_budget()
            && self.budget_param.is_none()
            && !self.warned_no_budget.swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            let _ = sink
                .send(StreamEvent::Warning(format!(
                    "this provider speaks `{}` ({}), which has no reasoning budget — \
                     the {n}-token budget is ignored and thinking is simply on",
                    self.thinking_dialect.field(),
                    self.dialect_source.describe(),
                )))
                .await;
        }

        let body = build_request_body(
            &req,
            self.thinking_dialect,
            self.budget_param.as_deref(),
            req.sort.as_deref().or(self.sort.as_deref()),
        );

        let mut builder = self.http.post(self.endpoint()).json(&body);
        if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key);
        }

        let resp = builder.send().await.context("sending chat completion request")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            // A 400 right after we volunteered a thinking field is nearly always
            // the wrong spelling for this provider — and if the spelling was a
            // guess off the hostname, the fix is one config line the user has no
            // reason to know about. Say which field we sent and where it came from.
            if status == reqwest::StatusCode::BAD_REQUEST && req.thinking.is_some() {
                bail!(
                    "LLM HTTP {status}: {text}\n\nworksmith sent a `{}` field ({}). If this \
                     provider wants the other spelling, set `thinking-param` to \"{}\" under \
                     its [providers.*] section.",
                    self.thinking_dialect.field(),
                    self.dialect_source.describe(),
                    match self.thinking_dialect {
                        crate::llm::ThinkingDialect::Reasoning => "chat-template",
                        crate::llm::ThinkingDialect::ChatTemplate => "reasoning",
                    },
                );
            }
            bail!("LLM HTTP {status}: {text}");
        }

        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut acc = Accumulator::default();

        'outer: loop {
            let next = tokio::select! {
                _ = cancel.cancelled() => break 'outer,
                chunk = stream.next() => chunk,
            };
            let Some(chunk) = next else { break };
            let bytes = chunk.context("reading stream chunk")?;
            buf.extend_from_slice(&bytes);

            // Process complete newline-delimited SSE lines. Splitting on '\n'
            // (ASCII) never bisects a multibyte char, so lossy decode is safe.
            while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=nl).collect();
                let line = String::from_utf8_lossy(&line);
                let line = line.trim();
                let Some(data) = line.strip_prefix("data:") else { continue };
                let data = data.trim();
                if data == "[DONE]" {
                    break 'outer;
                }
                if data.is_empty() {
                    continue;
                }
                // Providers report mid-stream failures *in band*: HTTP 200,
                // then `data: {"error": {...}}`. Dropping those as unparseable
                // turns a rate-limited request into an empty, successful-looking
                // answer — which is exactly how a 429'd planner came back as
                // "the model said nothing" instead of "the call failed".
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(data)
                    && let Some(err) = v.get("error")
                {
                    let msg = err
                        .get("message")
                        .and_then(|m| m.as_str())
                        .map(str::to_string)
                        .unwrap_or_else(|| err.to_string());
                    bail!("LLM stream error: {msg}");
                }
                let chunk: ChunkResp = match serde_json::from_str(data) {
                    Ok(c) => c,
                    Err(_) => continue, // tolerate keep-alives / partials
                };
                acc.apply(chunk, &sink).await;
            }
        }

        Ok(acc.into_completion())
    }
}

/// Build the JSON request body by hand — this gives us exact control over the
/// assistant `tool_calls` / tool-result shapes that OpenAI-compat servers are
/// picky about.
fn build_request_body(
    req: &ChatRequest,
    dialect: crate::llm::ThinkingDialect,
    budget_param: Option<&str>,
    sort: Option<&str>,
) -> serde_json::Value {
    let messages: Vec<serde_json::Value> = req.messages.iter().map(message_to_json).collect();

    let mut body = serde_json::json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
        "stream_options": { "include_usage": true },
    });

    if !req.tools.is_empty() {
        let tools: Vec<serde_json::Value> = req
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect();
        body["tools"] = serde_json::Value::Array(tools);
    }
    // OpenRouter provider routing. Other servers ignore an unknown field, and
    // it is only sent when asked for.
    if let Some(sort) = sort {
        body["provider"] = serde_json::json!({ "sort": sort });
    }
    if let Some(t) = req.temperature {
        body["temperature"] = serde_json::json!(t);
    }
    if let Some(p) = req.top_p {
        body["top_p"] = serde_json::json!(p);
    }
    if let Some(k) = req.top_k {
        body["top_k"] = serde_json::json!(k);
    }
    if let Some(m) = req.max_tokens {
        body["max_tokens"] = serde_json::json!(m);
    }
    // Only speak up when the caller asked. Reasoning is on by default at every
    // provider, and an unrecognized field is a 400, not a shrug.
    if let Some(think) = req.thinking {
        match dialect {
            crate::llm::ThinkingDialect::Reasoning => {
                // A budget is the useful setting here: `max_tokens` caps the
                // reasoning alone, leaving the rest of the request's budget for
                // an actual answer. OpenRouter converts it to an effort level
                // for models that only understand effort.
                body["reasoning"] = match (think.budget(), think.effort()) {
                    (Some(n), _) => serde_json::json!({ "max_tokens": n }),
                    (_, Some(e)) => serde_json::json!({ "effort": e.as_str() }),
                    _ => serde_json::json!({ "enabled": think.enabled() }),
                };
            }
            crate::llm::ThinkingDialect::ChatTemplate => {
                body["chat_template_kwargs"] =
                    serde_json::json!({ "enable_thinking": think.enabled() });
                // vLLM enforces `thinking_token_budget` server-side: it forces
                // the reasoning to end when the budget is spent, rather than
                // asking the model to be brief. Sent only when the provider
                // says it has the field, since llama.cpp and friends do not.
                if let (Some(n), Some(key)) = (think.budget(), budget_param) {
                    body[key] = serde_json::json!(n);
                }
                // vLLM takes reasoning_effort directly and injects
                // enable_thinking from it.
                if let Some(e) = think.effort() {
                    body["reasoning_effort"] = serde_json::json!(e.as_str());
                }
            }
        }
    }
    body
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn message_to_json(m: &Message) -> serde_json::Value {
    let mut obj = serde_json::json!({ "role": role_str(m.role) });
    // Content is always present (null allowed) except we keep it simple.
    obj["content"] = match &m.content {
        Some(c) => serde_json::json!(c),
        None => serde_json::Value::Null,
    };
    if !m.tool_calls.is_empty() {
        let calls: Vec<serde_json::Value> = m
            .tool_calls
            .iter()
            .map(|tc| {
                serde_json::json!({
                    "id": tc.id,
                    "type": "function",
                    "function": { "name": tc.name, "arguments": tc.arguments }
                })
            })
            .collect();
        obj["tool_calls"] = serde_json::Value::Array(calls);
    }
    if let Some(id) = &m.tool_call_id {
        obj["tool_call_id"] = serde_json::json!(id);
    }
    obj
}

// ---- Streaming chunk deserialization -------------------------------------

#[derive(Deserialize)]
struct ChunkResp {
    #[serde(default)]
    choices: Vec<ChunkChoice>,
    #[serde(default)]
    usage: Option<UsageResp>,
}

#[derive(Deserialize)]
struct ChunkChoice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    /// Reasoning/thinking. Providers disagree on the field name: OpenRouter
    /// uses `reasoning`, vLLM uses `reasoning_content`. Accept both.
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<DeltaToolCall>,
}

#[derive(Deserialize)]
struct DeltaToolCall {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<DeltaFunction>,
}

#[derive(Deserialize)]
struct DeltaFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct UsageResp {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default)]
    total_tokens: u32,
    /// OpenAI-style breakdown; absent on servers that don't report it.
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Deserialize)]
struct CompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: u32,
}

#[derive(Default)]
struct AccTool {
    id: String,
    name: String,
    args: String,
    announced: bool,
}

#[derive(Default)]
struct Accumulator {
    content: String,
    reasoning: String,
    tools: Vec<AccTool>,
    finish_reason: Option<String>,
    usage: Usage,
}

/// Strip stray `<tool_call>` wrapper fragments that leaky providers bleed into
/// content. Only exact tag tokens (and the specific broken tails we've seen) are
/// removed, so normal prose is untouched.
fn strip_toolcall_noise(s: &str) -> String {
    let mut out = s.to_string();
    for tok in ["</tool_call>", "<tool_call>", "</tool_call", "<tool_call", "ool_call>"] {
        if out.contains(tok) {
            out = out.replace(tok, "");
        }
    }
    out
}

impl Accumulator {
    async fn apply(&mut self, chunk: ChunkResp, sink: &mpsc::Sender<StreamEvent>) {
        if let Some(u) = chunk.usage {
            self.usage = Usage {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
                total_tokens: u.total_tokens,
                reasoning_tokens: u
                    .completion_tokens_details
                    .map(|d| d.reasoning_tokens)
                    .unwrap_or(0),
            };
            let _ = sink.send(StreamEvent::Usage(self.usage)).await;
        }

        for choice in chunk.choices {
            if let Some(reason) = choice.finish_reason {
                self.finish_reason = Some(reason);
            }
            if let Some(reasoning) = choice.delta.reasoning.or(choice.delta.reasoning_content)
                && !reasoning.is_empty() {
                    self.reasoning.push_str(&reasoning);
                    let _ = sink.send(StreamEvent::ReasoningDelta(reasoning)).await;
                }
            if let Some(text) = choice.delta.content {
                // Some providers (e.g. OpenRouter for Qwen) leak fragments of the
                // `<tool_call>` wrapper into content; strip that noise.
                let text = strip_toolcall_noise(&text);
                if !text.is_empty() {
                    self.content.push_str(&text);
                    let _ = sink.send(StreamEvent::TextDelta(text)).await;
                }
            }
            for dtc in choice.delta.tool_calls {
                let idx = dtc.index;
                if self.tools.len() <= idx {
                    self.tools.resize_with(idx + 1, AccTool::default);
                }
                let slot = &mut self.tools[idx];
                if let Some(id) = dtc.id {
                    slot.id = id;
                }
                if let Some(f) = dtc.function {
                    if let Some(name) = f.name
                        && !name.is_empty() {
                            slot.name = name;
                        }
                    if let Some(args) = f.arguments
                        && !args.is_empty() {
                            slot.args.push_str(&args);
                            let _ = sink
                                .send(StreamEvent::ToolCallArgsDelta { index: idx, delta: args })
                                .await;
                        }
                }
                if !slot.announced && !slot.name.is_empty() {
                    slot.announced = true;
                    let _ = sink
                        .send(StreamEvent::ToolCallStarted {
                            index: idx,
                            id: slot.id.clone(),
                            name: slot.name.clone(),
                        })
                        .await;
                }
            }
        }
    }

    fn into_completion(self) -> Completion {
        let tool_calls: Vec<ToolCall> = self
            .tools
            .into_iter()
            .filter(|t| !t.name.is_empty())
            .enumerate()
            .map(|(i, t)| ToolCall {
                id: if t.id.is_empty() { format!("call_{i}") } else { t.id },
                name: t.name,
                arguments: if t.args.is_empty() { "{}".to_string() } else { t.args },
            })
            .collect();

        let content = if self.content.is_empty() { None } else { Some(self.content) };
        let reasoning = if self.reasoning.is_empty() { None } else { Some(self.reasoning) };

        // Not every server breaks reasoning out of `completion_tokens` (vLLM
        // generally doesn't). An estimate beats a zero here: this number is the
        // whole point of the display, and "0" on a turn that visibly thought for
        // a minute reads as "reasoning is off".
        let mut usage = self.usage;
        if usage.reasoning_tokens == 0
            && let Some(r) = &reasoning
        {
            usage.reasoning_tokens = (r.len() / 4) as u32;
        }

        Completion {
            content,
            reasoning,
            tool_calls,
            usage,
            finish_reason: self.finish_reason,
        }
    }
}

#[cfg(test)]
mod thinking_tests {
    use super::*;
    use crate::llm::{Thinking, ThinkingDialect};

    fn req(thinking: Option<Thinking>) -> ChatRequest {
        ChatRequest {
            model: "m".into(),
            messages: vec![],
            tools: vec![],
            temperature: None,
            top_p: None,
            top_k: None,
            max_tokens: None,
            thinking,
            sort: None,
        }
    }

    #[test]
    fn silence_by_default() {
        // Sending nothing is what keeps every provider working; only opt-in.
        let b = build_request_body(&req(None), ThinkingDialect::Reasoning, None, None);
        assert!(b.get("reasoning").is_none());
        assert!(b.get("chat_template_kwargs").is_none());
    }

    #[test]
    fn each_provider_gets_its_own_spelling() {
        let b = build_request_body(&req(Some(Thinking::Off)), ThinkingDialect::Reasoning, None, None);
        assert_eq!(b["reasoning"], serde_json::json!({"enabled": false}));
        assert!(b.get("chat_template_kwargs").is_none(), "OpenRouter rejects this field");

        let b = build_request_body(&req(Some(Thinking::Off)), ThinkingDialect::ChatTemplate, None, None);
        assert_eq!(b["chat_template_kwargs"], serde_json::json!({"enable_thinking": false}));
        assert!(b.get("reasoning").is_none());
    }

    #[test]
    fn a_budget_caps_reasoning_where_the_dialect_supports_it() {
        // The point of a budget: reasoning gets its own ceiling, so it cannot
        // eat all of max_tokens and leave nothing for the answer.
        let b = build_request_body(&req(Some(Thinking::Budget(2000))), ThinkingDialect::Reasoning, None, None);
        assert_eq!(b["reasoning"], serde_json::json!({"max_tokens": 2000}));
    }

    #[test]
    fn vllm_gets_a_server_enforced_reasoning_budget() {
        // vLLM has `thinking_token_budget` and enforces it: when the budget is
        // spent it forces the reasoning to end, rather than asking the model to
        // be brief. Opt-in per provider, since llama.cpp and LM Studio share the
        // chat-template dialect and have no such field.
        let b = build_request_body(
            &req(Some(Thinking::Budget(2000))),
            ThinkingDialect::ChatTemplate,
            Some("thinking_token_budget"),
            None,
        );
        assert_eq!(b["thinking_token_budget"], serde_json::json!(2000));
        assert_eq!(b["chat_template_kwargs"], serde_json::json!({"enable_thinking": true}));

        // Without the opt-in the field must not appear at all: an unknown key
        // is a 400 on a strict server.
        let b = build_request_body(
            &req(Some(Thinking::Budget(2000))),
            ThinkingDialect::ChatTemplate,
            None,
            None,
        );
        assert!(b.get("thinking_token_budget").is_none());
    }

    #[test]
    fn an_effort_level_goes_to_each_provider_in_its_own_words() {
        // OpenRouter takes it inside `reasoning`; vLLM takes it top-level and
        // derives enable_thinking from it. Asking directly skips the conversion
        // OpenRouter would otherwise do from a budget.
        let b = build_request_body(
            &req(Some(Thinking::Effort(crate::llm::Effort::High))),
            ThinkingDialect::Reasoning,
            None,
            None,
        );
        assert_eq!(b["reasoning"], serde_json::json!({"effort": "high"}));

        let b = build_request_body(
            &req(Some(Thinking::Effort(crate::llm::Effort::Low))),
            ThinkingDialect::ChatTemplate,
            None,
            None,
        );
        assert_eq!(b["reasoning_effort"], serde_json::json!("low"));
        assert_eq!(b["chat_template_kwargs"], serde_json::json!({"enable_thinking": true}));
    }

    #[test]
    fn provider_routing_is_only_sent_when_asked_for() {
        let b = build_request_body(&req(None), ThinkingDialect::Reasoning, None, None);
        assert!(b.get("provider").is_none(), "no routing preference by default");

        let b = build_request_body(&req(None), ThinkingDialect::Reasoning, None, Some("throughput"));
        assert_eq!(b["provider"], serde_json::json!({"sort": "throughput"}));
    }

    #[test]
    fn a_budget_degrades_to_plain_on_where_it_cannot_be_expressed() {
        // enable_thinking is a bool; there is nowhere to put a number. The
        // client warns rather than letting the setting look effective.
        let b = build_request_body(&req(Some(Thinking::Budget(2000))), ThinkingDialect::ChatTemplate, None, None);
        assert_eq!(b["chat_template_kwargs"], serde_json::json!({"enable_thinking": true}));
        assert!(!ThinkingDialect::ChatTemplate.supports_budget());
        assert!(ThinkingDialect::Reasoning.supports_budget());
    }

    #[test]
    fn dialect_is_guessed_from_the_endpoint() {
        use ThinkingDialect::*;
        assert_eq!(ThinkingDialect::guess_from_url("https://openrouter.ai/api/v1"), Reasoning);
        assert_eq!(ThinkingDialect::guess_from_url("http://127.0.0.1:8000/v1"), ChatTemplate);
        assert_eq!(ThinkingDialect::parse("chat-template"), Some(ChatTemplate));
        assert_eq!(ThinkingDialect::parse("nonsense"), None);
    }
}
