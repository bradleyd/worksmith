//! The hand-rolled async agent loop: stream a completion, run any tool calls,
//! feed results back, repeat until the model stops calling tools. Every step is
//! published to the [`EventBus`]. Cancellation aborts the in-flight stream.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::event::{Event, EventBus};
use crate::llm::{ChatRequest, LlmClient, Message, StreamEvent};
use crate::session::Session;
use crate::tools::{ToolContext, ToolRegistry};

/// Hard cap on the size of any single tool result fed back to the model, so one
/// runaway tool call (a huge grep, `cat` of a big file) can't blow the context
/// window. Full compaction is M2; this is the M1 backstop.
const MAX_TOOL_RESULT_BYTES: usize = 24_000;

/// Truncate a tool result to the byte cap on a char boundary, with a notice.
fn cap_tool_output(s: String) -> String {
    if s.len() <= MAX_TOOL_RESULT_BYTES {
        return s;
    }
    let mut end = MAX_TOOL_RESULT_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let shown = &s[..end];
    format!(
        "{shown}\n\n[output truncated: showed {end} of {} bytes — narrow the query or read a specific range]",
        s.len()
    )
}

/// Drives one or more turns against a model + tools.
pub struct Agent {
    client: Arc<dyn LlmClient>,
    registry: Arc<ToolRegistry>,
    bus: EventBus,
    model: String,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
    max_steps: usize,
    tool_ctx: ToolContext,
}

impl Agent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: Arc<dyn LlmClient>,
        registry: Arc<ToolRegistry>,
        bus: EventBus,
        model: String,
        temperature: Option<f32>,
        max_tokens: Option<u32>,
        max_steps: usize,
        tool_ctx: ToolContext,
    ) -> Self {
        Self { client, registry, bus, model, temperature, max_tokens, max_steps, tool_ctx }
    }

    /// Run one user turn to completion. `system_prompt` is rebuilt by the caller
    /// each turn (it folds in current memory). Returns the final assistant text.
    pub async fn run_turn(
        &self,
        session: &mut Session,
        user_input: &str,
        system_prompt: &str,
        cancel: CancellationToken,
    ) -> Result<String> {
        session.append_message(Message::user(user_input))?;
        self.bus.emit(Event::UserMessage { text: user_input.to_string() });

        let mut final_text = String::new();

        for _step in 0..self.max_steps {
            if cancel.is_cancelled() {
                break;
            }

            let mut messages = Vec::with_capacity(session.messages().len() + 1);
            messages.push(Message::system(system_prompt));
            messages.extend(session.messages().iter().cloned());

            let req = ChatRequest {
                model: self.model.clone(),
                messages,
                tools: self.registry.defs(),
                temperature: self.temperature,
                max_tokens: self.max_tokens,
            };

            // Forward streamed text deltas to the bus while the request runs.
            let (tx, mut rx) = mpsc::channel::<StreamEvent>(256);
            let bus = self.bus.clone();
            let forwarder = tokio::spawn(async move {
                while let Some(ev) = rx.recv().await {
                    match ev {
                        StreamEvent::TextDelta(t) => bus.emit(Event::MessageDelta { text: t }),
                        StreamEvent::ReasoningDelta(t) => bus.emit(Event::Thinking { text: t }),
                        _ => {}
                    }
                }
            });

            let completion = self.client.stream(req, tx, cancel.clone()).await;
            let _ = forwarder.await;

            let completion = match completion {
                Ok(c) => c,
                Err(e) => {
                    self.bus.emit(Event::Error { message: e.to_string() });
                    return Err(e);
                }
            };

            self.bus.emit(Event::Usage {
                prompt_tokens: completion.usage.prompt_tokens,
                completion_tokens: completion.usage.completion_tokens,
                total_tokens: completion.usage.total_tokens,
            });

            // Record the assistant message (text + any tool calls).
            let assistant =
                Message::assistant(completion.content.clone(), completion.tool_calls.clone());
            session.append_message(assistant)?;

            if let Some(text) = &completion.content
                && !text.is_empty() {
                    self.bus.emit(Event::AssistantMessage { text: text.clone() });
                    final_text = text.clone();
                }

            // No tool calls → the turn is done.
            if completion.tool_calls.is_empty() {
                break;
            }

            // Execute each tool call and feed the result back.
            for call in &completion.tool_calls {
                self.bus.emit(Event::ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                });

                let args: serde_json::Value = serde_json::from_str(&call.arguments)
                    .unwrap_or(serde_json::Value::Null);

                let output = self.registry.run(&call.name, args, &self.tool_ctx).await;
                let content = cap_tool_output(output.content);

                self.bus.emit(Event::ToolResult {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    ok: !output.is_error,
                    output: content.clone(),
                });

                session.append_message(Message::tool_result(&call.id, &call.name, content))?;
            }
        }

        self.bus.emit(Event::TurnComplete);
        Ok(final_text)
    }
}
