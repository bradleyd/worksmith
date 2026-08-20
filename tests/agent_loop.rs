//! Integration tests for the agent's outer loop: validation-driven re-planning
//! and stuck-detection escalation, driven by a scripted mock LLM client (no
//! network). This is the machinery M6 workers and the M7 supervisor extend.

mod common;

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
    build_agent_with_client(Arc::new(client), cwd, stuck_threshold)
}

fn build_agent_with_client(
    client: Arc<dyn LlmClient>,
    cwd: &std::path::Path,
    stuck_threshold: u32,
) -> Agent {
    Agent::new(
        client,
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
            is_worker: false,
        },
    )
}

#[tokio::test]
async fn validation_drives_replan_until_pass() {
    common::isolate_home();
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
    common::isolate_home();
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
    common::isolate_home();
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
    common::isolate_home();
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
            is_worker: false,
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
    common::isolate_home();
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
async fn destructive_command_blocks_the_turn() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let canary = dir.path().join("canary.txt");
    std::fs::write(&canary, "alive").unwrap();
    let mut session = Session::create_at(&dir.path().join("s.jsonl"), dir.path()).unwrap();

    // The model tries something catastrophic, then (never reached) claims done.
    let client = MockClient::new(vec![
        tool_call("bash", r#"{"command":"rm -rf *"}"#),
        done("all clean"),
    ]);
    let agent = build_agent(client, dir.path(), 3);

    let result = agent
        .run_turn(&mut session, "clean up", "system", None, CancellationToken::new())
        .await
        .unwrap();

    assert!(matches!(result.outcome, TurnOutcome::Blocked(_)), "outcome: {:?}", result.outcome);
    assert!(canary.exists(), "the destructive command must not have run");
}

#[tokio::test]
async fn no_validator_completes_when_model_stops() {
    common::isolate_home();
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

/// A completion that spent its whole output budget reasoning: thinking came
/// back, an answer never did, and the provider says it was cut off.
fn reasoning_only() -> Completion {
    Completion {
        content: None,
        reasoning: Some("let me think about this at length...".into()),
        tool_calls: vec![],
        usage: Default::default(),
        finish_reason: Some("length".into()),
    }
}

#[tokio::test]
async fn reasoning_only_completion_is_nudged_not_treated_as_done() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create_at(&dir.path().join("s.jsonl"), dir.path()).unwrap();

    // Budget exhausted mid-thought once, then a real answer after the nudge.
    let client = MockClient::new(vec![reasoning_only(), done("here is the review")]);
    let agent = build_agent(client, dir.path(), 3);

    let result = agent
        .run_turn(&mut session, "review it", "system", None, CancellationToken::new())
        .await
        .unwrap();

    assert!(matches!(result.outcome, TurnOutcome::Done), "outcome: {:?}", result.outcome);
    assert_eq!(result.text, "here is the review", "the empty turn must not pass as the answer");
}

#[tokio::test]
async fn a_model_that_never_answers_ends_stuck_not_done() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create_at(&dir.path().join("s.jsonl"), dir.path()).unwrap();

    // Nothing but reasoning, forever. Reporting "done" here is the bug: the
    // turn ends with an empty answer and no sign that anything went wrong.
    let client = MockClient::new(vec![reasoning_only(), reasoning_only(), reasoning_only()]);
    let agent = build_agent(client, dir.path(), 3);

    let result = agent
        .run_turn(&mut session, "review it", "system", None, CancellationToken::new())
        .await
        .unwrap();

    let TurnOutcome::Stuck(reason) = &result.outcome else {
        panic!("outcome: {:?}", result.outcome);
    };
    assert!(reason.contains("max-tokens"), "the reason must name the fix: {reason}");
}

#[tokio::test]
async fn the_reasoning_trace_and_finish_reason_are_persisted() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let mut session = Session::create_at(&path, dir.path()).unwrap();

    let client = MockClient::new(vec![reasoning_only(), done("here is the review")]);
    let agent = build_agent(client, dir.path(), 3);
    agent
        .run_turn(&mut session, "review it", "system", None, CancellationToken::new())
        .await
        .unwrap();

    // Diagnosing an empty turn from the transcript alone requires both: what
    // the model was thinking, and whether it was cut off or chose to stop.
    let log = std::fs::read_to_string(&path).unwrap();
    assert!(log.contains("let me think about this at length"), "reasoning missing from {log}");
    assert!(log.contains("\"finish_reason\":\"length\""), "finish_reason missing from {log}");
}

/// Records what the client was actually asked for, so a test can assert on the
/// request rather than the reply.
struct RecordingClient {
    seen: Arc<Mutex<Vec<Option<worksmith::llm::Thinking>>>>,
    reply: String,
    prompt_tokens: u32,
}

#[async_trait]
impl LlmClient for RecordingClient {
    async fn stream(
        &self,
        req: ChatRequest,
        _sink: mpsc::Sender<StreamEvent>,
        _cancel: CancellationToken,
    ) -> anyhow::Result<Completion> {
        self.seen.lock().unwrap().push(req.thinking);
        Ok(Completion {
            content: Some(self.reply.clone()),
            usage: worksmith::llm::Usage {
                prompt_tokens: self.prompt_tokens,
                completion_tokens: 5,
                total_tokens: self.prompt_tokens + 5,
                reasoning_tokens: 0,
            },
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn helper_calls_never_inherit_the_session_thinking_budget() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));

    // A 2000-token reasoning budget inside ask()'s 512-token ceiling is not
    // satisfiable: the call returns empty every time, which is what left the
    // memory store empty while looking like "nothing worth saving".
    let client = RecordingClient {
        seen: seen.clone(),
        reply: "summary".into(),
        prompt_tokens: 0,
    };
    let agent = build_agent_with_client(Arc::new(client), dir.path(), 3)
        .with_thinking(Some(worksmith::llm::Thinking::Budget(2000)));

    agent.ask("system", "user", 512).await.unwrap();

    assert_eq!(
        seen.lock().unwrap().as_slice(),
        &[Some(worksmith::llm::Thinking::Off)],
        "helper calls are format-following, not reasoning"
    );
}

#[tokio::test]
async fn compaction_uses_the_providers_token_count_not_the_estimate() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create_at(&dir.path().join("s.jsonl"), dir.path()).unwrap();

    // Short messages: the char-based estimate is nowhere near the limit. But the
    // provider reports a prompt far larger, because the system prompt, tool
    // schemas and skill text are in the request and not in this vector.
    for _ in 0..3 {
        session.append_message(Message::user("hi")).unwrap();
        session.append_message(Message::assistant(Some("ok".into()), vec![])).unwrap();
    }

    let seen = Arc::new(Mutex::new(Vec::new()));
    let client = RecordingClient { seen, reply: "answer".into(), prompt_tokens: 900 };
    let agent = Agent::new(
        Arc::new(client),
        Arc::new(ToolRegistry::with_builtins()),
        EventBus::new(),
        "mock".into(),
        None,
        None,
        20,
        3,
        3,
        1000, // context_limit → compaction at 750
        1,    // keep_recent_turns
        ToolContext {
            cwd: dir.path().to_path_buf(),
            session_id: "test".into(),
            bash_timeout: Duration::from_secs(10),
            is_worker: false,
        },
    );

    // First turn: nothing reported yet, so the estimate applies and nothing
    // compacts. It also records the provider's 900-token prompt.
    agent
        .run_turn(&mut session, "one", "system", None, CancellationToken::new())
        .await
        .unwrap();
    let after_first = session.messages().len();

    // Second turn: 900 > 750, so compaction runs even though the estimate is
    // still tiny.
    agent
        .run_turn(&mut session, "two", "system", None, CancellationToken::new())
        .await
        .unwrap();

    assert!(
        session.messages().len() < after_first + 2,
        "history should have collapsed: {} messages after {} + a turn",
        session.messages().len(),
        after_first
    );
}

#[tokio::test]
async fn a_compacted_session_stays_compacted_when_reopened() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let mut session = Session::create_at(&path, dir.path()).unwrap();

    for _ in 0..3 {
        session.append_message(Message::user("x".repeat(300))).unwrap();
        session.append_message(Message::assistant(Some("y".repeat(300)), vec![])).unwrap();
    }

    let client = MockClient::new(vec![done("SUMMARY: earlier work"), done("final answer")]);
    let agent = Agent::new(
        Arc::new(client),
        Arc::new(ToolRegistry::with_builtins()),
        EventBus::new(),
        "mock".into(),
        None,
        None,
        20,
        3,
        3,
        200, // tiny context limit → compaction runs
        1,
        ToolContext {
            cwd: dir.path().to_path_buf(),
            session_id: "test".into(),
            bash_timeout: Duration::from_secs(10),
            is_worker: false,
        },
    );

    agent
        .run_turn(&mut session, "new question", "system", None, CancellationToken::new())
        .await
        .unwrap();
    let live: Vec<String> =
        session.messages().iter().map(|m| m.content.clone().unwrap_or_default()).collect();
    assert_eq!(live.len(), 3, "compacted in memory");
    drop(session);

    // Reopening must reproduce the compacted view. Replaying the raw message
    // entries instead would rebuild the full pre-compaction history and lose the
    // summary — the session would silently undo its own context management.
    let reopened = Session::open(&path).unwrap();
    let after: Vec<String> =
        reopened.messages().iter().map(|m| m.content.clone().unwrap_or_default()).collect();
    assert_eq!(after, live, "reopened history must match the compacted one");
    assert!(after[0].contains("SUMMARY: earlier work"), "the summary survived: {after:?}");
}
