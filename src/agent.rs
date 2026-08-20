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
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::event::{Event, EventBus};
use crate::llm::{ChatRequest, LlmClient, Message, ModelOverride, StreamEvent, Thinking};
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
    /// A destructive command was refused and the turn was hard-stopped.
    Blocked(String),
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
            TurnOutcome::Blocked(r) => format!("blocked: {r}"),
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

/// A mailbox for injecting messages into a *running* turn — supervisor nudges
/// today, interactive steering later. Cheap to clone; the agent drains it at the
/// top of every step, so a message lands before the next model call.
#[derive(Clone, Default)]
pub struct Steering {
    inbox: Arc<Mutex<Vec<String>>>,
}

impl Steering {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, message: impl Into<String>) {
        self.inbox.lock().unwrap().push(message.into());
    }

    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.inbox.lock().unwrap())
    }
}

/// How much the model deliberates before answering, toggleable while a session
/// is running — the "feeling lucky" switch, plus the budget in between. Shared
/// by handle so the TUI can flip it between turns without rebuilding the agent.
#[derive(Clone, Default)]
pub struct ThinkingMode(Arc<Mutex<Option<Thinking>>>);

impl ThinkingMode {
    pub fn new(value: Option<Thinking>) -> Self {
        ThinkingMode(Arc::new(Mutex::new(value)))
    }

    /// `None` = leave the provider's default alone (send nothing).
    pub fn get(&self) -> Option<Thinking> {
        *self.0.lock().unwrap()
    }

    pub fn set(&self, value: Option<Thinking>) {
        *self.0.lock().unwrap() = value;
    }

    /// Is thinking explicitly off (fast mode)?
    pub fn is_fast(&self) -> bool {
        self.get() == Some(Thinking::Off)
    }

    /// Flip between fast and thinking; an unset mode turns fast first. Returns
    /// whether fast mode is now on.
    pub fn toggle_fast(&self) -> bool {
        let fast = !self.is_fast();
        self.set(Some(if fast { Thinking::Off } else { Thinking::On }));
        fast
    }

    /// A short label for the footer: `off`, `on`, or the budget.
    pub fn label(&self) -> Option<String> {
        match self.get()? {
            Thinking::Off => Some("off".to_string()),
            Thinking::On => Some("on".to_string()),
            Thinking::Budget(n) if n >= 1000 => Some(format!("{}k", n / 1000)),
            Thinking::Budget(n) => Some(n.to_string()),
        }
    }
}

/// How many empty completions in a row to absorb with a nudge before calling
/// the turn stuck. One is usually a thinking model overshooting its budget and
/// it recovers when told to answer; a run of them is not going to.
const MAX_EMPTY_COMPLETIONS: u32 = 2;

/// Why the inner loop stopped.
enum IdleReason {
    ModelDone,
    Stuck(String),
    Blocked(String),
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
    steering: Steering,
    thinking: ThinkingMode,
    /// The provider's prompt-token count for the most recent completion — what
    /// the next request will cost, as opposed to what we estimate it costs.
    last_prompt_tokens: AtomicU32,
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
            steering: Steering::new(),
            thinking: ThinkingMode::default(),
            last_prompt_tokens: AtomicU32::new(0),
        }
    }

    /// Ask the model to skip its reasoning pass. On a small Qwen this is the
    /// difference between 500 completion tokens and 31 for the same question —
    /// the loop is expected to catch what the model no longer deliberates over.
    pub fn with_thinking(mut self, thinking: Option<Thinking>) -> Self {
        self.thinking = ThinkingMode::new(thinking);
        self
    }

    /// Handle to this agent's thinking switch, so a front-end can flip it.
    pub fn thinking_mode(&self) -> ThinkingMode {
        self.thinking.clone()
    }

    /// This agent's steering mailbox — push to it to inject a message into the
    /// next step of a running turn (worker reports, supervisor nudges).
    pub fn steering(&self) -> Steering {
        self.steering.clone()
    }

    /// Attach a steering mailbox (the supervisor's channel into this agent).
    pub fn with_steering(mut self, steering: Steering) -> Self {
        self.steering = steering;
        self
    }

    /// Create a sibling agent that shares this one's client, tools, and config
    /// but runs on its own event bus and session. Used to spawn workers.
    pub fn fork(&self, bus: EventBus, session_id: String) -> Agent {
        self.fork_with(bus, session_id, None)
    }

    /// Fork onto a different model — the cheap-workers/smart-parent split. The
    /// override carries its own client, since a cheaper model often lives
    /// behind a different provider rather than just a different name.
    pub fn fork_with(
        &self,
        bus: EventBus,
        session_id: String,
        model: Option<ModelOverride>,
    ) -> Agent {
        let mut tool_ctx = self.tool_ctx.clone();
        tool_ctx.session_id = session_id;
        tool_ctx.is_worker = true;
        let (client, model) = match model {
            Some(o) => (o.client, o.model),
            None => (self.client.clone(), self.model.clone()),
        };
        Agent {
            client,
            registry: self.registry.clone(),
            bus,
            model,
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            max_steps: self.max_steps,
            max_retries: self.max_retries,
            stuck_threshold: self.stuck_threshold,
            context_limit: self.context_limit,
            keep_recent_turns: self.keep_recent_turns,
            tool_ctx,
            // A fork gets a fresh mailbox — its supervisor attaches its own.
            steering: Steering::new(),
            // A worker takes the parent's setting as it stands now; flipping
            // the parent later shouldn't retroactively change a running worker.
            thinking: ThinkingMode::new(self.thinking.get()),
            // Its own context, so its own accounting.
            last_prompt_tokens: AtomicU32::new(0),
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
        self.bus.emit(Event::UserMessage {
            text: user_input.to_string(),
        });

        let mut final_text = String::new();
        let mut retries_left = self.max_retries;

        let outcome = loop {
            let idle = self
                .run_until_idle(session, system_prompt, &mut final_text, &cancel)
                .await?;

            match idle {
                IdleReason::Aborted => break TurnOutcome::Aborted,
                IdleReason::MaxSteps => break TurnOutcome::MaxSteps,
                IdleReason::Stuck(r) => break TurnOutcome::Stuck(r),
                IdleReason::Blocked(r) => break TurnOutcome::Blocked(r),
                IdleReason::ModelDone => {
                    let Some(v) = validator else {
                        break TurnOutcome::Done;
                    };

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
                                reason: format!(
                                    "validation failed; re-planning ({retries_left} retries left)"
                                ),
                            });
                            session.append_message(Message::user(directive))?;
                        }
                    }
                }
            }
        };

        self.bus.emit(Event::TurnComplete {
            outcome: outcome.label(),
        });
        Ok(TurnResult {
            text: final_text,
            outcome,
        })
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
        let mut empty_completions = 0u32;

        for _step in 0..self.max_steps {
            if cancel.is_cancelled() {
                return Ok(IdleReason::Aborted);
            }

            // Steering: anything the supervisor (or the user) posted since the
            // last step lands as a user message before the next model call.
            for message in self.steering.take() {
                self.bus.emit(Event::Nudge {
                    reason: message.clone(),
                });
                session.append_message(Message::user(message))?;
            }

            // Compact if the working history is approaching the context limit.
            // `estimate_tokens` only sees the session's messages — not the system
            // prompt, the tool schemas, or loaded skill text, all of which are in
            // every real request. It therefore reads low, by a margin that grows
            // with the toolset, so trust the provider's own count once we have
            // one and keep the estimate only for the first step of a session.
            if self.working_tokens(session) > self.compaction_trigger()
                && let Err(e) = self.compact(session).await
            {
                // Compaction is best-effort; a failure shouldn't kill the turn.
                self.bus.emit(Event::Error {
                    message: format!("compaction failed: {e}"),
                });
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
                thinking: self.thinking.get(),
            };

            // Forward streamed text + reasoning deltas to the bus.
            let (tx, mut rx) = mpsc::channel::<StreamEvent>(256);
            let bus = self.bus.clone();
            let forwarder = tokio::spawn(async move {
                while let Some(ev) = rx.recv().await {
                    match ev {
                        StreamEvent::TextDelta(t) => bus.emit(Event::MessageDelta { text: t }),
                        StreamEvent::ReasoningDelta(t) => bus.emit(Event::Thinking { text: t }),
                        StreamEvent::Warning(message) => bus.emit(Event::Warning { message }),
                        _ => {}
                    }
                }
            });

            let completion = self.client.stream(req, tx, cancel.clone()).await;
            let _ = forwarder.await;

            let completion = match completion {
                Ok(c) => c,
                Err(e) => {
                    self.bus.emit(Event::Error {
                        message: e.to_string(),
                    });
                    return Err(e);
                }
            };

            self.last_prompt_tokens.store(completion.usage.prompt_tokens, Ordering::Relaxed);
            self.bus.emit(Event::Usage {
                prompt_tokens: completion.usage.prompt_tokens,
                completion_tokens: completion.usage.completion_tokens,
                total_tokens: completion.usage.total_tokens,
                reasoning_tokens: completion.usage.reasoning_tokens,
                finish_reason: completion.finish_reason.clone(),
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
            let assistant = Message::assistant(completion.content.clone(), stored_calls)
                .with_trace(completion.reasoning.clone(), completion.finish_reason.clone());
            session.append_message(assistant)?;

            if let Some(text) = &completion.content
                && !text.is_empty()
            {
                self.bus
                    .emit(Event::AssistantMessage { text: text.clone() });
                *final_text = text.clone();
            }

            if completion.tool_calls.is_empty() {
                // Nothing at all came back: no text, no calls. A thinking model
                // can spend its entire output budget deliberating and never
                // reach an answer, which arrives here looking exactly like a
                // finished turn. Scoring that as ModelDone ends the turn with an
                // empty reply and no clue why — the failure that made a chapter
                // review report "done" after 60 seconds of visible thinking.
                let said_nothing = completion.content.as_deref().unwrap_or("").trim().is_empty();
                if said_nothing {
                    empty_completions += 1;
                    if empty_completions > MAX_EMPTY_COMPLETIONS {
                        return Ok(IdleReason::Stuck(if truncated {
                            "the model spent its whole output budget reasoning and never \
                             answered — raise max-tokens, or run with --fast / /fast"
                                .to_string()
                        } else {
                            "the model returned an empty response".to_string()
                        }));
                    }
                    let nudge = if truncated {
                        "Your last response hit the output-token limit while still reasoning, \
                         so nothing came back. Skip further deliberation: make the next tool \
                         call, or give your answer directly, right now."
                    } else {
                        "Your last response was empty. Make a tool call or give your answer."
                    };
                    self.bus.emit(Event::Nudge {
                        reason: nudge.to_string(),
                    });
                    session.append_message(Message::user(nudge))?;
                    continue;
                }
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
            let mut blocked: Option<String> = None;
            for call in &completion.tool_calls {
                self.bus.emit(Event::ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                });

                let (ok, fatal, raw) =
                    match serde_json::from_str::<serde_json::Value>(&call.arguments) {
                        Ok(v) => {
                            let o = self.registry.run(&call.name, v, &self.tool_ctx).await;
                            (!o.is_error, o.fatal, o.content)
                        }
                        Err(e) => {
                            let hint = if truncated {
                                " — the response was cut off by the output-token limit; make the \
                             call smaller (e.g. write the file in parts)"
                            } else {
                                ""
                            };
                            (
                                false,
                                false,
                                format!("invalid JSON arguments for `{}`: {e}{hint}", call.name),
                            )
                        }
                    };
                let content = cap_tool_output(raw);

                self.bus.emit(Event::ToolResult {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    ok,
                    output: content.clone(),
                });
                session.append_message(Message::tool_result(
                    &call.id,
                    &call.name,
                    content.clone(),
                ))?;

                // A refused destructive command hard-stops the turn immediately.
                if fatal {
                    blocked = Some(content);
                    break;
                }
            }
            if let Some(reason) = blocked {
                self.bus.emit(Event::Error {
                    message: reason.clone(),
                });
                return Ok(IdleReason::Blocked(reason));
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
                    self.bus.emit(Event::Nudge {
                        reason: reason.clone(),
                    });
                    session.append_message(Message::user(reason))?;
                }
            }
        }

        Ok(IdleReason::MaxSteps)
    }

    /// What the next request will actually cost in prompt tokens: the provider's
    /// count for the last one, falling back to a rough estimate before any
    /// completion has landed.
    fn working_tokens(&self, session: &Session) -> usize {
        let reported = self.last_prompt_tokens.load(Ordering::Relaxed) as usize;
        reported.max(estimate_tokens(session.messages()))
    }

    fn compaction_trigger(&self) -> usize {
        // Compact at 75% of the context limit, leaving headroom for the reply.
        self.context_limit * 3 / 4
    }

    /// One-shot helper completion: no tools, no session, no streaming to the
    /// bus — just the model's text. Used for the harness's own side calls
    /// (compaction, fan-out planning, memory extraction), not for user turns.
    ///
    /// Thinking is forced OFF regardless of the session's setting. These calls
    /// are format-following, not reasoning, and they run on budgets of 512-2048
    /// tokens — a session-level reasoning budget larger than that ceiling makes
    /// the call structurally impossible to satisfy, and it comes back empty
    /// every time. That is how memory extraction silently produced nothing.
    pub async fn ask(&self, system: &str, user: &str, max_tokens: u32) -> Result<String> {
        let req = ChatRequest {
            model: self.model.clone(),
            messages: vec![Message::system(system), Message::user(user)],
            tools: vec![],
            temperature: Some(0.2),
            max_tokens: Some(max_tokens),
            thinking: Some(Thinking::Off),
        };
        // Drain the stream sink; we only want the assembled text.
        let (tx, mut rx) = mpsc::channel::<StreamEvent>(64);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let completion = self.client.stream(req, tx, CancellationToken::new()).await?;
        let _ = drain.await;
        let text = completion.content.unwrap_or_default();
        if text.trim().is_empty() {
            // Empty is never a useful answer to a helper call, and silently
            // returning it makes the caller blame its own parsing. The usual
            // cause is a thinking model spending the whole budget before it
            // starts answering.
            anyhow::bail!(
                "model returned no content (max_tokens may have been consumed by reasoning \
                 — try --fast or a larger budget)"
            );
        }
        Ok(text)
    }

    /// Summarize old turns into a single note and keep the recent turns
    /// verbatim, shrinking the working context. The full transcript stays in the
    /// session JSONL. Public so `/compact` can force it.
    pub async fn compact(&self, session: &mut Session) -> Result<()> {
        // Gather what we need, then drop the borrow before the await.
        let (before, split, transcript) = {
            let msgs = session.messages();
            let split = compaction_split(msgs, self.keep_recent_turns);
            if split == 0 {
                return Ok(()); // nothing old enough to summarize
            }
            (msgs.len(), split, render_transcript(&msgs[..split]))
        };

        let sys = "You are compacting a coding-agent conversation. Summarize the \
                   exchange below into concise notes a future agent needs to \
                   continue: decisions made, files created or edited, key facts \
                   learned, and any unfinished work. Preserve specifics (paths, \
                   names, values). Omit chatter.";
        let summary = self.ask(sys, &transcript, 1024).await?;
        if summary.trim().is_empty() {
            return Ok(()); // don't discard history for an empty summary
        }

        session.compact(&summary, split)?;
        let after = session.messages().len();
        self.bus.emit(Event::Compaction {
            messages_before: before,
            messages_after: after,
        });
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
