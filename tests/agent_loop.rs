//! Integration tests for the agent's outer loop: validation-driven re-planning
//! and stuck-detection escalation, driven by a scripted mock LLM client (no
//! network). This is the machinery M6 workers and the M7 supervisor extend.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use worksmith::agent::{Agent, TurnOutcome};
use worksmith::event::EventBus;
use worksmith::llm::{ChatRequest, Completion, LlmClient, Message, StreamEvent, ToolCall};
use worksmith::session::Session;
use worksmith::tools::{ToolContext, ToolRegistry};
use worksmith::validation::CommandValidator;

/// A mock client that replays a scripted queue of completions (one per step).
/// An exhausted queue yields a default (no tool calls) = "model done".
struct MockClient {
    responses: Mutex<VecDeque<Completion>>,
}

impl MockClient {
    fn new(responses: Vec<Completion>) -> Self {
        Self { responses: Mutex::new(responses.into()) }
    }
}

#[async_trait]
impl LlmClient for MockClient {
    async fn stream(
        &self,
        _req: ChatRequest,
        _sink: mpsc::Sender<StreamEvent>,
        _cancel: CancellationToken,
    ) -> anyhow::Result<Completion> {
        Ok(self.responses.lock().unwrap().pop_front().unwrap_or_default())
    }
}

fn tool_call(name: &str, args: &str) -> Completion {
    Completion {
        content: None,
        reasoning: None,
        tool_calls: vec![ToolCall { id: "c1".into(), name: name.into(), arguments: args.into() }],
        usage: Default::default(),
        finish_reason: Some("tool_calls".into()),
    }
}

fn done(text: &str) -> Completion {
    Completion { content: Some(text.into()), ..Default::default() }
}

/// A tool call whose arguments were cut off (invalid JSON) with finish_reason
/// "length" — simulates hitting the output-token limit mid-call.
fn truncated_call(name: &str, partial_args: &str) -> Completion {
    Completion {
        content: None,
        reasoning: None,
        tool_calls: vec![ToolCall { id: "c1".into(), name: name.into(), arguments: partial_args.into() }],
        usage: Default::default(),
        finish_reason: Some("length".into()),
    }
}

fn build_agent(client: MockClient, cwd: &std::path::Path, stuck_threshold: u32) -> Agent {
    Agent::new(
        Arc::new(client),
        Arc::new(ToolRegistry::with_builtins()),
        EventBus::new(),
        "mock".into(),
        None,
        None,
        20,              // max_steps
        3,               // max_retries
        stuck_threshold, // stuck_threshold
        1_000_000,       // context_limit (high: no compaction in these tests)
        6,               // keep_recent_turns
        ToolContext {
            cwd: cwd.to_path_buf(),
            session_id: "test".into(),
            bash_timeout: Duration::from_secs(10),
        },
    )
}

#[tokio::test]
async fn validation_drives_replan_until_pass() {
    let dir = tempfile::tempdir().unwrap();
    let session_path = dir.path().join("s.jsonl");
    let mut session = Session::create_at(&session_path, dir.path()).unwrap();

    // First attempt writes the wrong content (validation will fail); after the
    // re-plan directive, the second attempt writes the right content.
    let client = MockClient::new(vec![
        tool_call("write", r#"{"path":"out.txt","content":"bad"}"#),
        done("wrote it (attempt 1)"),
        tool_call("write", r#"{"path":"out.txt","content":"good"}"#),
        done("fixed it (attempt 2)"),
    ]);
    let agent = build_agent(client, dir.path(), 3);

    let validator = CommandValidator::new(
        r#"test "$(cat out.txt)" = good"#,
        dir.path().to_path_buf(),
        Duration::from_secs(10),
    );

    let result = agent
        .run_turn(
            &mut session,
            "make out.txt say good",
            "system",
            Some(&validator),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(matches!(result.outcome, TurnOutcome::Done), "outcome: {:?}", result.outcome);
    assert_eq!(std::fs::read_to_string(dir.path().join("out.txt")).unwrap(), "good");
}

#[tokio::test]
async fn validation_fails_after_retries_exhausted() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create_at(&dir.path().join("s.jsonl"), dir.path()).unwrap();

    // The model always writes the wrong content; validation never passes.
    let mut script = Vec::new();
    for _ in 0..8 {
        script.push(tool_call("write", r#"{"path":"out.txt","content":"bad"}"#));
        script.push(done("done"));
    }
    let agent = build_agent(MockClient::new(script), dir.path(), 3);

    let validator = CommandValidator::new(
        r#"test "$(cat out.txt)" = good"#,
        dir.path().to_path_buf(),
        Duration::from_secs(10),
    );

    let result = agent
        .run_turn(&mut session, "make it good", "system", Some(&validator), CancellationToken::new())
        .await
        .unwrap();

    assert!(
        matches!(result.outcome, TurnOutcome::ValidationFailed(_)),
        "outcome: {:?}",
        result.outcome
    );
}

#[tokio::test]
async fn repeated_identical_calls_escalate_to_stuck() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create_at(&dir.path().join("s.jsonl"), dir.path()).unwrap();

    // Same call forever → should escalate to Stuck (threshold 2 → at 4th call).
    let script = vec![tool_call("ls", r#"{"path":"."}"#); 8];
    let agent = build_agent(MockClient::new(script), dir.path(), 2);

    let result = agent
        .run_turn(&mut session, "look around", "system", None, CancellationToken::new())
        .await
        .unwrap();

    assert!(matches!(result.outcome, TurnOutcome::Stuck(_)), "outcome: {:?}", result.outcome);
}

#[tokio::test]
async fn compaction_summarizes_old_turns_when_over_limit() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create_at(&dir.path().join("s.jsonl"), dir.path()).unwrap();

    // Pre-fill with several bulky turns so the estimate exceeds a tiny limit.
    for _ in 0..3 {
        session.append_message(Message::user("x".repeat(300))).unwrap();
        session.append_message(Message::assistant(Some("y".repeat(300)), vec![])).unwrap();
    }

    // First stream call = the summarization pass; second = the actual turn.
    let client = MockClient::new(vec![done("SUMMARY: earlier work"), done("final answer")]);
    let agent = Agent::new(
        Arc::new(client),
        Arc::new(ToolRegistry::with_builtins()),
        EventBus::new(),
        "mock".into(),
        None,
        None,
        20,   // max_steps
        3,    // max_retries
        3,    // stuck_threshold
        200,  // context_limit (tiny → triggers compaction)
        1,    // keep_recent_turns
        ToolContext {
            cwd: dir.path().to_path_buf(),
            session_id: "test".into(),
            bash_timeout: Duration::from_secs(10),
        },
    );

    let result = agent
        .run_turn(&mut session, "new question", "system", None, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(result.text, "final answer");
    // History collapsed to: [summary, "new question", assistant("final answer")].
    assert_eq!(session.messages().len(), 3, "should have compacted old turns");
    assert!(
        session.messages()[0].content.as_deref().unwrap_or("").starts_with("[Summary"),
        "first message should be the summary"
    );
}

#[tokio::test]
async fn truncated_tool_call_recovers_without_poisoning_history() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create_at(&dir.path().join("s.jsonl"), dir.path()).unwrap();

    // First call is truncated (invalid JSON args); then the model recovers.
    let client = MockClient::new(vec![
        truncated_call("write", r#"{"path":"a.txt","content":"unterminated"#),
        done("recovered"),
    ]);
    let agent = build_agent(client, dir.path(), 3);

    let result = agent
        .run_turn(&mut session, "write a file", "system", None, CancellationToken::new())
        .await
        .unwrap();

    assert!(matches!(result.outcome, TurnOutcome::Done), "outcome: {:?}", result.outcome);

    // Every stored tool_call must have valid-JSON arguments (so re-sending the
    // history can't be rejected by the provider).
    for m in session.messages() {
        for tc in &m.tool_calls {
            serde_json::from_str::<serde_json::Value>(&tc.arguments)
                .unwrap_or_else(|_| panic!("stored tool call has invalid JSON args: {}", tc.arguments));
        }
    }
    // The model got a tool result explaining the failure.
    let saw_error = session.messages().iter().any(|m| {
        m.content.as_deref().map(|c| c.contains("invalid JSON arguments")).unwrap_or(false)
    });
    assert!(saw_error, "model should have received an invalid-args error result");
}

#[tokio::test]
async fn no_validator_completes_when_model_stops() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create_at(&dir.path().join("s.jsonl"), dir.path()).unwrap();

    let agent = build_agent(MockClient::new(vec![done("all done")]), dir.path(), 3);

    let result = agent
        .run_turn(&mut session, "hi", "system", None, CancellationToken::new())
        .await
        .unwrap();

    assert!(matches!(result.outcome, TurnOutcome::Done));
    assert_eq!(result.text, "all done");
}
