//! The session's model as a swappable set.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use worksmith::agent::Agent;
use worksmith::event::EventBus;
use worksmith::llm::{ChatRequest, Completion, LlmClient, StreamEvent};
use worksmith::tools::{ToolContext, ToolRegistry};

/// Never called: these tests read what *would* be sent, not what comes back.
struct Silent;

#[async_trait]
impl LlmClient for Silent {
    async fn stream(
        &self,
        _req: ChatRequest,
        _sink: mpsc::Sender<StreamEvent>,
        _cancel: CancellationToken,
    ) -> anyhow::Result<Completion> {
        Ok(Completion::default())
    }
}

fn agent(ctx: ToolContext) -> Agent {
    Agent::new(
        Arc::new(Silent),
        Arc::new(ToolRegistry::with_builtins()),
        EventBus::new(),
        "test/model".to_string(),
        None,
        None,
        8,
        1,
        3,
        32_000,
        6,
        ctx,
    )
}

/// A worker forked onto another model must take that model's whole set —
/// window and sampling included, not just its name and client.
#[test]
fn a_fork_onto_another_model_takes_its_context_window_too() {
    use worksmith::config::ModelSettings;
    use worksmith::llm::ModelOverride;

    let parent = agent(ToolContext::default()); // 32_000 window, no sampling
    let over = ModelOverride {
        client: Arc::new(Silent),
        model: "cheap/model".to_string(),
        settings: ModelSettings { top_p: Some(0.8), top_k: Some(20), ..Default::default() },
        context_limit: 8_192,
        temperature: Some(0.6),
        missing_key_env: None,
    };

    let worker = parent.fork_with(EventBus::new(), "w1".to_string(), Some(over));
    let active = worker.current();

    assert_eq!(active.model, "cheap/model");
    // The bug: these four came from the parent, so an 8k worker ran with the
    // session's 32k window and compaction never fired.
    assert_eq!(active.context_limit, 8_192, "the window must move with the model");
    assert_eq!(active.temperature, Some(0.6));
    assert_eq!(active.top_p, Some(0.8));
    assert_eq!(active.top_k, Some(20));

    assert_eq!(parent.current().context_limit, 32_000, "the parent is untouched");
}

/// A switch mid-session must not retarget work already running.
#[test]
fn a_fork_does_not_share_the_parents_model_cell() {
    let parent = agent(ToolContext::default());
    let worker = parent.fork(EventBus::new(), "w1".to_string());

    let mut swapped = parent.current();
    swapped.model = "other/model".to_string();
    swapped.context_limit = 4_096;
    parent.set_model(swapped);

    assert_eq!(worker.current().model, "test/model", "a running worker keeps its model");
    assert_eq!(worker.current().context_limit, 32_000);
    assert_eq!(parent.current().model, "other/model");
}
