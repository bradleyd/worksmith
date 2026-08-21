//! The typed event stream — the keystone of the harness. Every step of a turn
//! is published here; the JSONL session writer, `--mode json` output, and
//! (later) the TUI and spawned-worker views are all just subscribers.

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// A single event in the agent's lifecycle. Serialized with a `type` tag so the
/// `--mode json` stream is self-describing.
// Deserialize as well as Serialize: events are written into the session file so
// the loop's history can be read back after the run that produced it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    SessionStarted { id: String },
    UserMessage { text: String },
    /// A model call is in flight. The supervisor's idle rule measures time
    /// since the last event, and a slow request emits nothing at all while it
    /// waits — so without these it counts queueing as spinning. Three workers
    /// sharing one local server is enough to trip a 20s timeout on prefill.
    ModelCallStarted,
    ModelCallFinished,
    /// A streamed chunk of model reasoning/thinking (display-only).
    Thinking { text: String },
    /// A streamed chunk of assistant text.
    MessageDelta { text: String },
    /// The final assembled assistant text for a step.
    AssistantMessage { text: String },
    ToolCall { id: String, name: String, arguments: String },
    ToolResult { id: String, name: String, ok: bool, output: String },
    Usage {
        prompt_tokens: u32,
        completion_tokens: u32,
        total_tokens: u32,
        /// Share of `completion_tokens` spent reasoning rather than answering.
        reasoning_tokens: u32,
        /// Why the completion stopped — `"length"` means it was cut off.
        finish_reason: Option<String>,
    },
    /// The supervisor/loop nudged the model back on track (stuck detection).
    Nudge { reason: String },
    /// Old history was summarized to stay under the context limit.
    Compaction { messages_before: usize, messages_after: usize },
    /// Result of running the task's validation check.
    Validation { ok: bool, detail: String },
    /// A setting was not honored, or something is off but not fatal.
    Warning { message: String },
    Error { message: String },
    /// The whole user turn finished, with its final outcome (done, validation
    /// failed, stuck, max steps, aborted).
    TurnComplete { outcome: String },
}

/// A cheap clonable handle to the broadcast bus.
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    /// Publish an event. Errors (no subscribers) are intentionally ignored.
    pub fn emit(&self, event: Event) {
        let _ = self.tx.send(event);
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}
