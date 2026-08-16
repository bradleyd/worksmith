//! The hand-rolled async agent loop.
//!
//! Two layers (this shape is what M6 workers and the M7 supervisor extend):
//! - **inner loop** (`run_until_idle`): stream a completion, run tool calls,
//!   feed results back, repeat until the model stops calling tools — with
//!   stuck detection (repeated identical calls → nudge → escalate).
//! - **outer loop** (`run_turn`): after the model goes idle, run the task's
//!   validator; on failure, inject a re-plan directive and try again, bounded
//!   by `max_retries`. Terminate on a *passing check*, not on "I'm done".

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::event::{Event, EventBus};
use crate::llm::{ChatRequest, LlmClient, Message, StreamEvent};
use crate::session::Session;
use crate::tools::{ToolContext, ToolRegistry};
use crate::validation::Validator;

/// Hard cap on the size of any single tool result fed back to the model, so one
/// runaway tool call (a huge grep, `cat` of a big file) can't blow the context
/// window. Full compaction is separate; this is the always-on backstop.
const MAX_TOOL_RESULT_BYTES: usize = 24_000;

/// How a user turn ended.
#[derive(Debug, Clone)]
pub enum TurnOutcome {
    /// Model finished and validation passed (or there was no validator).
    Done,
    /// Validation kept failing until retries were exhausted.
    ValidationFailed(String),
    /// The model got stuck repeating itself and was escalated.
    Stuck(String),
    /// Hit the per-attempt step cap.
    MaxSteps,
    /// Cancelled mid-turn.
    Aborted,
}

impl TurnOutcome {
    pub fn label(&self) -> String {
        match self {
            TurnOutcome::Done => "done".into(),
            TurnOutcome::ValidationFailed(r) => format!("validation failed: {r}"),
            TurnOutcome::Stuck(r) => format!("stuck: {r}"),
            TurnOutcome::MaxSteps => "hit step limit".into(),
            TurnOutcome::Aborted => "aborted".into(),
        }
    }
    pub fn is_success(&self) -> bool {
        matches!(self, TurnOutcome::Done)
    }
}

/// Result of a user turn: the final assistant text plus the outcome.
pub struct TurnResult {
    pub text: String,
    pub outcome: TurnOutcome,
}

/// Why the inner loop stopped.
enum IdleReason {
    ModelDone,
    Stuck(String),
    MaxSteps,
    Aborted,
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
    max_retries: usize,
    stuck_threshold: u32,
    context_limit: usize,
    keep_recent_turns: usize,
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
        max_retries: usize,
        stuck_threshold: u32,
        context_limit: usize,
        keep_recent_turns: usize,
        tool_ctx: ToolContext,
    ) -> Self {
        Self {
            client,
            registry,
            bus,
            model,
            temperature,
            max_tokens,
            max_steps,
            max_retries,
            stuck_threshold,
            context_limit,
            keep_recent_turns,
            tool_ctx,
        }
    }

    /// Run one user turn to completion. `validator` (if any) gates success:
    /// the turn isn't done until it passes or retries run out.
    pub async fn run_turn(
        &self,
        session: &mut Session,
        user_input: &str,
        system_prompt: &str,
        validator: Option<&dyn Validator>,
        cancel: CancellationToken,
    ) -> Result<TurnResult> {
        session.append_message(Message::user(user_input))?;
        self.bus.emit(Event::UserMessage { text: user_input.to_string() });

        let mut final_text = String::new();
        let mut retries_left = self.max_retries;

        let outcome = loop {
            let idle = self.run_until_idle(session, system_prompt, &mut final_text, &cancel).await?;

            match idle {
                IdleReason::Aborted => break TurnOutcome::Aborted,
                IdleReason::MaxSteps => break TurnOutcome::MaxSteps,
                IdleReason::Stuck(r) => break TurnOutcome::Stuck(r),
                IdleReason::ModelDone => {
                    let Some(v) = validator else { break TurnOutcome::Done };

                    match v.validate().await {
                        Ok(()) => {
                            self.bus.emit(Event::Validation {
                                ok: true,
                                detail: v.describe(),
                            });
                            break TurnOutcome::Done;
                        }
                        Err(reason) => {
                            self.bus.emit(Event::Validation {
                                ok: false,
                                detail: reason.clone(),
                            });
                            if retries_left == 0 {
                                break TurnOutcome::ValidationFailed(reason);
                            }
                            retries_left -= 1;
                            let directive = format!(
                                "The validation check {} did not pass:\n\n{}\n\nRevise your \
                                 approach and fix the underlying problem, then finish.",
                                v.describe(),
                                reason
                            );
                            self.bus.emit(Event::Nudge {
                                reason: format!("validation failed; re-planning ({retries_left} retries left)"),
                            });
                            session.append_message(Message::user(directive))?;
                        }
                    }
                }
            }
        };

        self.bus.emit(Event::TurnComplete { outcome: outcome.label() });
        Ok(TurnResult { text: final_text, outcome })
    }

    /// Inner loop: run model↔tool steps until the model stops calling tools,
    /// gets stuck, hits the step cap, or is cancelled.
    async fn run_until_idle(
        &self,
        session: &mut Session,
        system_prompt: &str,
        final_text: &mut String,
        cancel: &CancellationToken,
    ) -> Result<IdleReason> {
        let mut call_counts: HashMap<String, u32> = HashMap::new();
        let mut nudged: HashSet<String> = HashSet::new();

        for _step in 0..self.max_steps {
            if cancel.is_cancelled() {
                return Ok(IdleReason::Aborted);
            }

            // Compact if the working history is approaching the context limit.
            if estimate_tokens(session.messages()) > self.compaction_trigger()
                && let Err(e) = self.compact(session).await
            {
                // Compaction is best-effort; a failure shouldn't kill the turn.
                self.bus.emit(Event::Error { message: format!("compaction failed: {e}") });
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

            // Forward streamed text + reasoning deltas to the bus.
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

            // A tool call whose arguments were cut off (hit the output-token
            // limit) is invalid JSON. It must NOT be stored verbatim: re-sending
            // that history makes the provider reject the whole request (HTTP
            // 400). Store a sanitized copy and feed a clear error back so the
            // model retries with a smaller call.
            let truncated = completion.finish_reason.as_deref() == Some("length");
            let mut stored_calls = completion.tool_calls.clone();
            for c in &mut stored_calls {
                if serde_json::from_str::<serde_json::Value>(&c.arguments).is_err() {
                    c.arguments = "{}".to_string();
                }
            }
            let assistant = Message::assistant(completion.content.clone(), stored_calls);
            session.append_message(assistant)?;

            if let Some(text) = &completion.content
                && !text.is_empty()
            {
                self.bus.emit(Event::AssistantMessage { text: text.clone() });
                *final_text = text.clone();
            }

            if completion.tool_calls.is_empty() {
                return Ok(IdleReason::ModelDone);
            }

            // Stuck detection: escalate if the model repeats an identical call.
            for call in &completion.tool_calls {
                let sig = format!("{}::{}", call.name, call.arguments);
                let count = call_counts.entry(sig).or_insert(0);
                *count += 1;
                if *count >= self.stuck_threshold + 2 {
                    return Ok(IdleReason::Stuck(format!(
                        "repeated `{}` {} times with identical arguments",
                        call.name, count
                    )));
                }
            }

            // Execute tool calls and feed results back.
            for call in &completion.tool_calls {
                self.bus.emit(Event::ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                });

                let (ok, raw) = match serde_json::from_str::<serde_json::Value>(&call.arguments) {
                    Ok(v) => {
                        let o = self.registry.run(&call.name, v, &self.tool_ctx).await;
                        (!o.is_error, o.content)
                    }
                    Err(e) => {
                        let hint = if truncated {
                            " — the response was cut off by the output-token limit; make the \
                             call smaller (e.g. write the file in parts)"
                        } else {
                            ""
                        };
                        (false, format!("invalid JSON arguments for `{}`: {e}{hint}", call.name))
                    }
                };
                let content = cap_tool_output(raw);

                self.bus.emit(Event::ToolResult {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    ok,
                    output: content.clone(),
                });
                session.append_message(Message::tool_result(&call.id, &call.name, content))?;
            }

            // Nudge once when a call first hits the stuck threshold.
            for call in &completion.tool_calls {
                let sig = format!("{}::{}", call.name, call.arguments);
                if call_counts.get(&sig).copied().unwrap_or(0) >= self.stuck_threshold
                    && !nudged.contains(&sig)
                {
                    nudged.insert(sig);
                    let reason = format!(
                        "You've made the same `{}` call repeatedly without new information. \
                         Step back and try a different approach.",
                        call.name
                    );
                    self.bus.emit(Event::Nudge { reason: reason.clone() });
                    session.append_message(Message::user(reason))?;
                }
            }
        }

        Ok(IdleReason::MaxSteps)
    }

    fn compaction_trigger(&self) -> usize {
        // Compact at 75% of the context limit, leaving headroom for the reply.
        self.context_limit * 3 / 4
    }

    /// Summarize old turns into a single note and keep the recent turns
    /// verbatim, shrinking the working context. The full transcript stays in the
    /// session JSONL. Public so `/compact` can force it.
    pub async fn compact(&self, session: &mut Session) -> Result<()> {
        // Gather what we need, then drop the borrow before the await.
        let (before, recent, transcript) = {
            let msgs = session.messages();
            let split = compaction_split(msgs, self.keep_recent_turns);
            if split == 0 {
                return Ok(()); // nothing old enough to summarize
            }
            (msgs.len(), msgs[split..].to_vec(), render_transcript(&msgs[..split]))
        };

        let sys = "You are compacting a coding-agent conversation. Summarize the \
                   exchange below into concise notes a future agent needs to \
                   continue: decisions made, files created or edited, key facts \
                   learned, and any unfinished work. Preserve specifics (paths, \
                   names, values). Omit chatter.";
        let req = ChatRequest {
            model: self.model.clone(),
            messages: vec![Message::system(sys), Message::user(transcript)],
            tools: vec![],
            temperature: Some(0.2),
            max_tokens: Some(1024),
        };

        // Drain the stream sink; we only want the final summary text.
        let (tx, mut rx) = mpsc::channel::<StreamEvent>(64);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let completion = self.client.stream(req, tx, CancellationToken::new()).await?;
        let _ = drain.await;

        let summary = completion.content.unwrap_or_default();
        if summary.trim().is_empty() {
            return Ok(()); // don't discard history for an empty summary
        }

        let mut new_msgs =
            Vec::with_capacity(recent.len() + 1);
        new_msgs.push(Message::user(format!("[Summary of earlier conversation]\n{summary}")));
        new_msgs.extend(recent);
        let after = new_msgs.len();

        session.replace_messages(new_msgs)?;
        self.bus.emit(Event::Compaction { messages_before: before, messages_after: after });
        Ok(())
    }
}

/// Rough token estimate (~4 chars/token) over messages: content + tool-call
/// arguments. Good enough to decide when to compact.
fn estimate_tokens(messages: &[Message]) -> usize {
    let mut chars = 0usize;
    for m in messages {
        if let Some(c) = &m.content {
            chars += c.len();
        }
        for tc in &m.tool_calls {
            chars += tc.name.len() + tc.arguments.len();
        }
    }
    chars / 4
}

/// Choose a split index so that everything from it onward is the last
/// `keep_turns` user-turns (a turn starts at a `User` message). Returns 0 when
/// there aren't more turns than we keep (nothing to compact). Splitting on turn
/// boundaries keeps assistant/tool-call/tool-result groups intact.
fn compaction_split(messages: &[Message], keep_turns: usize) -> usize {
    if keep_turns == 0 {
        return 0;
    }
    let user_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| matches!(m.role, crate::llm::Role::User))
        .map(|(i, _)| i)
        .collect();
    if user_indices.len() <= keep_turns {
        return 0;
    }
    user_indices[user_indices.len() - keep_turns]
}

/// Render messages as a plain transcript for the summarizer.
fn render_transcript(messages: &[Message]) -> String {
    use crate::llm::Role;
    let mut out = String::new();
    for m in messages {
        let role = match m.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        if let Some(c) = &m.content
            && !c.is_empty()
        {
            out.push_str(&format!("[{role}] {c}\n"));
        }
        for tc in &m.tool_calls {
            out.push_str(&format!("[{role} calls {}] {}\n", tc.name, tc.arguments));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Message;

    #[test]
    fn split_keeps_recent_turns_on_user_boundaries() {
        // 3 turns: user/assistant pairs.
        let msgs = vec![
            Message::user("t1"),
            Message::assistant(Some("a1".into()), vec![]),
            Message::user("t2"),
            Message::assistant(Some("a2".into()), vec![]),
            Message::user("t3"),
            Message::assistant(Some("a3".into()), vec![]),
        ];
        // Keep last 1 turn → split at index of the 3rd user message (index 4).
        assert_eq!(compaction_split(&msgs, 1), 4);
        // Keep 2 turns → split at 2nd user message (index 2).
        assert_eq!(compaction_split(&msgs, 2), 2);
        // Keep >= number of turns → nothing to compact.
        assert_eq!(compaction_split(&msgs, 3), 0);
        assert_eq!(compaction_split(&msgs, 9), 0);
    }
}

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
