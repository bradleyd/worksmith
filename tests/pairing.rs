//! Who a pairing checkpoint may interrupt.

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

fn names(a: &Agent) -> Vec<String> {
    a.advertised_tools().into_iter().map(|d| d.name).collect()
}

/// Build a bare agent; the client is never called by these tests.
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

#[test]
fn pairing_off_does_not_even_advertise_the_checkpoint() {
    // Off has to mean "not in the payload". `defs()` rides every request, so a
    // schema for a tool that will never fire is real money in a 32k window.
    let a = agent(ToolContext::default());
    assert!(!names(&a).iter().any(|n| n == "checkpoint"));

    let a = a.with_pairing(true);
    assert!(names(&a).iter().any(|n| n == "checkpoint"));
}

#[test]
fn a_spawned_worker_never_inherits_pairing() {
    // Nobody is watching a background task, so a blocking question would stall
    // it against a user who does not know it was asked — and a fan-out of five
    // would queue five questions behind one composer.
    let parent = agent(ToolContext::default()).with_pairing(true);
    let worker = parent.fork(EventBus::new(), "w1".to_string());

    assert!(parent.pairing_on(), "the session is pairing");
    assert!(!worker.pairing_on(), "the worker is not");
    assert!(!names(&worker).iter().any(|n| n == "checkpoint"));
}

#[test]
fn turning_pairing_on_mid_session_does_not_reach_running_workers() {
    // `/pair` is a session switch, not a global one. Unlike `route`, the flag
    // is not shared with a fork.
    let parent = agent(ToolContext::default());
    let worker = parent.fork(EventBus::new(), "w1".to_string());
    parent.set_pairing(true);

    assert!(parent.pairing_on());
    assert!(!worker.pairing_on(), "a running worker must not start interrupting");
}
