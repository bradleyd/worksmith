//! The typed event stream — the keystone of the harness. Every step of a turn
//! is published here; the JSONL session writer, `--mode json` output, and
//! (later) the TUI and spawned-worker views are all just subscribers.

use serde::Serialize;
use tokio::sync::broadcast;

/// A single event in the agent's lifecycle. Serialized with a `type` tag so the
/// `--mode json` stream is self-describing.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    SessionStarted { id: String },
    UserMessage { text: String },
    /// A streamed chunk of model reasoning/thinking (display-only).
    Thinking { text: String },
    /// A streamed chunk of assistant text.
    MessageDelta { text: String },
    /// The final assembled assistant text for a step.
    AssistantMessage { text: String },
    ToolCall { id: String, name: String, arguments: String },
    ToolResult { id: String, name: String, ok: bool, output: String },
    Usage { prompt_tokens: u32, completion_tokens: u32, total_tokens: u32 },
    Error { message: String },
    /// The whole user turn finished (model produced no more tool calls).
    TurnComplete,
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
