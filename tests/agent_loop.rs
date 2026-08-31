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

/// Handover notes as a plain string, for clients that reply with one directly.
const HANDOVER_NOTES: &str = "## Goal\nCarry the session across a compaction.\n\
     ## Locations\nsrc/agent.rs:1219 — the compaction prompt\n\
     ## Established\nThe JSONL keeps everything.\n## Unfinished\nNothing.";

/// A summary shaped like the handover notes compaction now asks for. The old
/// fixtures were one-liners, which is exactly the failure the guard exists to
/// catch — the summarizer answering conversationally and taking every file and
/// line number down with it.
/// An agent whose window is small enough that compaction actually cuts. With
/// build_agent's 1,000,000 the keep budget swallows the whole history, split is
/// 0, and `compact` returns having done nothing — which quietly makes any test
/// of compaction pass for the wrong reason.
fn agent_small_window(client: MockClient, cwd: &std::path::Path) -> Agent {
    Agent::new(
        Arc::new(client),
        Arc::new(ToolRegistry::with_builtins()),
        EventBus::new(),
        "mock".into(),
        None,
        None,
        20,
        3,
        3,
        2_000, // window: keep budget ~666 tokens, so a long history is cut
        1,
        ToolContext {
            cwd: cwd.to_path_buf(),
            session_id: "test".into(),
            bash_timeout: Duration::from_secs(10),
            is_worker: false,
            ..Default::default()
        },
    )
}

fn handover(marker: &str) -> Completion {
    done(&format!(
        "## Goal\n{marker}\n\n\
         ## Locations\n\
         src/agent.rs:1219 — the compaction prompt\n\
         src/session.rs:216 — Session::compact, which swaps the messages\n\
         src/tools/mod.rs:88 — MAX_TOOL_RESULT_BYTES, the per-result cap\n\n\
         ## Established\n\
         {marker}\n\
         The working set is replaced by these notes; the JSONL keeps everything.\n\
         Compaction fires at 75% of the model's context window.\n\n\
         ## Unfinished\n\
         Nothing outstanding for this fixture."
    ))
}

fn tool_call(name: &str, args: &str) -> Completion {
    Completion {
        content: None,
        reasoning: None,
        tool_calls: vec![ToolCall { id: "c1".into(), name: name.into(), arguments: args.into() }],
        usage: Default::default(),
        finish_reason: Some("tool_calls".into()),
        rescued: None,
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
        rescued: None,
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
            ..Default::default()
        },
    )
}

/// Fails the first `fail_times` calls the way a dropped tunnel does, then
/// behaves. Counts calls so a test can prove the retry happened.
struct FlakyClient {
    fail_times: Mutex<usize>,
    calls: Arc<Mutex<usize>>,
    transient: bool,
}

#[async_trait]
impl LlmClient for FlakyClient {
    async fn stream(
        &self,
        _req: ChatRequest,
        _sink: mpsc::Sender<StreamEvent>,
        _cancel: CancellationToken,
    ) -> anyhow::Result<Completion> {
        *self.calls.lock().unwrap() += 1;
        let mut left = self.fail_times.lock().unwrap();
        if *left > 0 {
            *left -= 1;
            return if self.transient {
                Err(anyhow::Error::new(worksmith::llm::Transient)
                    .context("connection error: Connection reset by peer (os error 54)"))
            } else {
                Err(anyhow::anyhow!("LLM HTTP 401: bad key"))
            };
        }
        Ok(Completion { content: Some("done".into()), ..Default::default() })
    }
}

/// Rejects every request the way vLLM does: with a *lower bound* on the prompt
/// ("at least N"), derived from the limit and the output asked for. Give back
/// 512 output tokens and the reported prompt grows by 512, so shrinking output
/// in a loop chases its own tail.
struct TightWindowClient {
    calls: Arc<Mutex<usize>>,
    /// Accept once the request asks for less than this much output.
    accept_below: u32,
}

#[async_trait]
impl LlmClient for TightWindowClient {
    async fn stream(
        &self,
        req: ChatRequest,
        _sink: mpsc::Sender<StreamEvent>,
        _cancel: CancellationToken,
    ) -> anyhow::Result<Completion> {
        *self.calls.lock().unwrap() += 1;
        let asked = req.max_tokens.unwrap_or(0);
        if asked < self.accept_below {
            return Ok(Completion { content: Some("done".into()), ..Default::default() });
        }
        anyhow::bail!(
            "LLM HTTP 400 Bad Request: {{\"error\":{{\"message\":\"This model's maximum \
             context length is 32768 tokens. However, you requested {asked} output tokens and \
             your prompt contains at least {} input tokens, for a total of at least 32769 \
             tokens.\"}}}}",
            32769 - asked
        )
    }
}

#[tokio::test]
async fn a_prompt_bound_request_compacts_instead_of_shrinking_forever() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create_at(&dir.path().join("s.jsonl"), dir.path()).unwrap();

    // A real run logged 441 of these warnings: each retry gave back 512 output
    // tokens, the reported prompt grew by 512, and the turn died having never
    // once tried making the prompt smaller.
    let calls = Arc::new(Mutex::new(0));
    let client = TightWindowClient { calls: calls.clone(), accept_below: 0 };
    let agent = build_agent_with_client(Arc::new(client), dir.path(), 3);

    let _ = agent
        .run_turn(&mut session, "write chapter 10", "system", None, CancellationToken::new())
        .await;

    let n = *calls.lock().unwrap();
    assert!(n <= 4, "one shrink, one compaction, then stop — not {n} attempts");
}

#[tokio::test]
async fn a_dropped_connection_is_retried_not_fatal() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create_at(&dir.path().join("s.jsonl"), dir.path()).unwrap();

    // An ssh tunnel that times out mid-run used to end the turn outright.
    let calls = Arc::new(Mutex::new(0));
    let client = FlakyClient {
        fail_times: Mutex::new(2),
        calls: calls.clone(),
        transient: true,
    };
    let agent = build_agent_with_client(Arc::new(client), dir.path(), 3);

    let result = agent
        .run_turn(&mut session, "hello", "system", None, CancellationToken::new())
        .await
        .unwrap();

    assert!(matches!(result.outcome, TurnOutcome::Done), "outcome: {:?}", result.outcome);
    assert_eq!(*calls.lock().unwrap(), 3, "two failures should cost two retries");
}

#[tokio::test]
async fn a_rejected_key_is_not_retried() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create_at(&dir.path().join("s.jsonl"), dir.path()).unwrap();

    // Retrying a 401 just spends the user's time three times over.
    let calls = Arc::new(Mutex::new(0));
    let client = FlakyClient {
        fail_times: Mutex::new(2),
        calls: calls.clone(),
        transient: false,
    };
    let agent = build_agent_with_client(Arc::new(client), dir.path(), 3);

    let result = agent
        .run_turn(&mut session, "hello", "system", None, CancellationToken::new())
        .await;

    assert!(result.is_err() || !matches!(result.unwrap().outcome, TurnOutcome::Done));
    assert_eq!(*calls.lock().unwrap(), 1, "a permanent error should be asked once");
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
    let client = MockClient::new(vec![handover("SUMMARY: earlier work"), done("final answer")]);
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
            ..Default::default()
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
        rescued: None,
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
/// Records what the client was actually asked for: the thinking setting and the
/// output budget, both of which the agent decides rather than passes through.
type Asked = (Option<worksmith::llm::Thinking>, u32);

struct RecordingClient {
    seen: Arc<Mutex<Vec<Asked>>>,
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
        self.seen.lock().unwrap().push((req.thinking, req.max_tokens.unwrap_or(0)));
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
        reply: HANDOVER_NOTES.into(),
        prompt_tokens: 0,
    };
    let agent = build_agent_with_client(Arc::new(client), dir.path(), 3)
        .with_thinking(Some(worksmith::llm::Thinking::Budget(2000)));

    agent.ask("system", "user", 512).await.unwrap();

    assert_eq!(
        seen.lock().unwrap().first().map(|(t, _)| *t),
        Some(Some(worksmith::llm::Thinking::Off)),
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
    // The same client answers the turn *and* the summary call, so the reply has
    // to be shaped like handover notes or compaction refuses it.
    let client = RecordingClient { seen, reply: HANDOVER_NOTES.into(), prompt_tokens: 900 };
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
            ..Default::default()
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

/// The screenshot bug (2026-08-22): provider prompt 25106, estimate 11781.
/// "Keep a third of the window" in estimate units kept nearly the whole real
/// prompt, so compaction fired every step and freed ~9% each time. The keep
/// budget must subtract the measured overhead (provider − estimate) first.
#[tokio::test]
async fn compaction_cuts_deeper_when_overhead_dwarfs_the_estimate() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create_at(&dir.path().join("s.jsonl"), dir.path()).unwrap();

    // ~300 estimated tokens of messages, in many small pieces so a boundary
    // exists wherever the budget lands.
    for _ in 0..15 {
        session.append_message(Message::user("x".repeat(40))).unwrap();
        session.append_message(Message::assistant(Some("y".repeat(40)), vec![])).unwrap();
    }
    let est_before = 15 * 2 * 40 / 4; // ~300

    // Provider reports 900 against a 1000 window: trigger (750) fires, and the
    // overhead is 900 − ~300 = ~600 — bigger than the naive keep budget of 333.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let client = RecordingClient { seen, reply: HANDOVER_NOTES.into(), prompt_tokens: 900 };
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
        1000,
        1, // keep_recent_turns — useless here by design; the token path decides
        ToolContext {
            cwd: dir.path().to_path_buf(),
            session_id: "test".into(),
            bash_timeout: Duration::from_secs(10),
            is_worker: false,
            ..Default::default()
        },
    );

    agent.run_turn(&mut session, "one", "system", None, CancellationToken::new()).await.unwrap();
    agent.run_turn(&mut session, "two", "system", None, CancellationToken::new()).await.unwrap();

    let est_after: usize = session
        .messages()
        .iter()
        .map(|m| m.content.as_deref().map_or(0, |c| c.len()) / 4)
        .sum();
    assert!(
        est_after < est_before / 2,
        "with overhead 600 of a 1000 window, the naive keep (333) would retain \
         nearly everything; the overhead-aware keep must cut hard: {est_after} vs {est_before}"
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

    let client = MockClient::new(vec![handover("SUMMARY: earlier work"), done("final answer")]);
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
            ..Default::default()
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

#[tokio::test]
async fn a_message_that_misses_the_turn_is_recoverable_rather_than_lost() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create_at(&dir.path().join("s.jsonl"), dir.path()).unwrap();

    let agent = build_agent(MockClient::new(vec![done("answered")]), dir.path(), 3);
    let steering = agent.steering();

    agent
        .run_turn(&mut session, "hi", "system", None, CancellationToken::new())
        .await
        .unwrap();

    // Typed just as the turn ended: too late to be drained by a step, and it
    // would otherwise sit in the mailbox until some later turn happened to
    // start. The user pressed Enter, so it has to be recoverable.
    steering.push("actually, use tabs");
    assert_eq!(steering.drain(), vec!["actually, use tabs"]);
    assert!(steering.drain().is_empty(), "draining twice must not duplicate it");
}

#[tokio::test]
async fn steering_the_agent_consumed_is_not_offered_again() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create_at(&dir.path().join("s.jsonl"), dir.path()).unwrap();

    let agent = build_agent(
        MockClient::new(vec![tool_call("ls", r#"{"path":"."}"#), done("answered")]),
        dir.path(),
        3,
    );
    let steering = agent.steering();
    steering.push("look in src/ instead");

    agent
        .run_turn(&mut session, "hi", "system", None, CancellationToken::new())
        .await
        .unwrap();

    // The turn drained it, so the caller must not start a second turn with it.
    assert!(steering.drain().is_empty(), "a delivered message must not be re-sent");
    let delivered = session
        .messages()
        .iter()
        .any(|m| m.content.as_deref().is_some_and(|c| c.contains("look in src/ instead")));
    assert!(delivered, "and it should have reached the conversation");
}

#[tokio::test]
async fn the_session_records_which_model_answered() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let mut session = Session::create_at(&path, dir.path()).unwrap();

    let agent = build_agent(MockClient::new(vec![done("hi")]), dir.path(), 3);
    agent
        .run_turn(&mut session, "hello", "system", None, CancellationToken::new())
        .await
        .unwrap();

    // "Which model actually served this?" was unanswerable from our own
    // artifacts: a worker override or a role switch means the session's model
    // is not the answer, and confirming it took reading the provider's logs.
    let answered = session
        .messages()
        .iter()
        .filter_map(|m| m.model.as_deref())
        .collect::<Vec<_>>();
    assert_eq!(answered, vec!["mock"], "the answering model is on the message");

    let log = std::fs::read_to_string(&path).unwrap();
    assert!(log.contains("\"model\":\"mock\""), "and in the session file: {log}");
}

#[tokio::test]
async fn the_session_records_what_the_loop_did() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let mut session = Session::create_at(&path, dir.path()).unwrap();

    let agent = build_agent(
        MockClient::new(vec![tool_call("ls", r#"{"path":"."}"#), done("all set")]),
        dir.path(),
        3,
    );
    agent
        .run_turn(&mut session, "look around", "system", None, CancellationToken::new())
        .await
        .unwrap();

    // Messages say what was said. They cannot say when a model call started, why
    // a nudge fired, or that a stream ended without a finish reason — which is
    // why diagnosing worker failures meant reading the provider's own logs.
    let events = worksmith::session::events(&path).unwrap();
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| match e.event {
            worksmith::event::Event::ModelCallStarted => "call-start",
            worksmith::event::Event::ModelCallFinished => "call-end",
            worksmith::event::Event::ToolCall { .. } => "tool",
            worksmith::event::Event::Usage { .. } => "usage",
            worksmith::event::Event::TurnComplete { .. } => "turn-complete",
            _ => "other",
        })
        .collect();

    assert!(kinds.contains(&"call-start") && kinds.contains(&"call-end"), "{kinds:?}");
    assert!(kinds.contains(&"tool"), "tool calls are in the history: {kinds:?}");
    assert!(kinds.contains(&"turn-complete"), "and how the turn ended: {kinds:?}");
    assert!(events.iter().all(|e| e.ts > 0), "each event carries when it happened");

    // Per-token deltas would multiply the file by the length of every answer.
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(!raw.contains("message_delta"), "streaming deltas are not recorded");

    // And the replayable conversation is unaffected by the extra entries.
    let reopened = Session::open(&path).unwrap();
    assert_eq!(reopened.messages().len(), session.messages().len());
}

/// A model switch recorded in the session must read back after the process
/// that wrote it has exited. `Event` is `Deserialize` as well as `Serialize`
/// for exactly this: old sessions stay readable as the enum grows, and a
/// `ModelChanged` written by one run is visible to the next.
#[test]
fn a_model_change_round_trips_through_the_session_jsonl() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let mut session = Session::create_at(&path, dir.path()).unwrap();

    let ev = worksmith::event::Event::ModelChanged {
        from: "big/model".to_string(),
        to: "cheap/model".to_string(),
    };
    session.append_event(&ev).unwrap();

    let events = worksmith::session::events(&path).unwrap();
    assert_eq!(events.len(), 1, "the event entry is in the file: {events:?}");
    assert!(
        matches!(&events[0].event, worksmith::event::Event::ModelChanged { from, to }
            if from == "big/model" && to == "cheap/model"),
        "the switch reads back with both sides intact: {:?}",
        events[0].event
    );
}

#[tokio::test]
async fn output_tokens_are_clamped_to_what_the_window_can_hold() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create_at(&dir.path().join("s.jsonl"), dir.path()).unwrap();

    let seen = Arc::new(Mutex::new(Vec::new()));
    let client = RecordingClient {
        seen: seen.clone(),
        reply: "ok".into(),
        // The server reports a prompt that nearly fills a 4k window.
        prompt_tokens: 3_600,
    };
    let agent = Agent::new(
        Arc::new(client),
        Arc::new(ToolRegistry::with_builtins()),
        EventBus::new(),
        "mock".into(),
        None,
        Some(8_192), // ask for far more output than the window has left
        20,
        3,
        3,
        4_096, // context_limit
        6,
        ToolContext {
            cwd: dir.path().to_path_buf(),
            session_id: "test".into(),
            bash_timeout: Duration::from_secs(10),
            is_worker: false,
            ..Default::default()
        },
    );

    // First turn establishes the reported prompt size; the second must fit.
    for _ in 0..2 {
        agent
            .run_turn(&mut session, "hi", "system", None, CancellationToken::new())
            .await
            .unwrap();
    }

    // The failure this prevents: 24577 prompt + 8192 output against a 32768
    // model, rejected by one token.
    let asked = seen.lock().unwrap().last().unwrap().1;
    assert!(
        asked + 3_600 <= 4_096,
        "prompt {} + output {asked} must fit the window",
        3_600
    );
    assert!(asked >= 256, "but never clamped to nothing: {asked}");
}

/// A client that rejects the first request the way vLLM does, then succeeds.
struct ContextLimitClient {
    seen: Arc<Mutex<Vec<u32>>>,
}

#[async_trait]
impl LlmClient for ContextLimitClient {
    async fn stream(
        &self,
        req: ChatRequest,
        _sink: mpsc::Sender<StreamEvent>,
        _cancel: CancellationToken,
    ) -> anyhow::Result<Completion> {
        let asked = req.max_tokens.unwrap_or(0);
        self.seen.lock().unwrap().push(asked);
        if asked as usize + 24_577 > 32_768 {
            anyhow::bail!(
                "LLM HTTP 400 Bad Request: {{\"error\":{{\"message\":\"This model's maximum \
                 context length is 32768 tokens. However, you requested {asked} output tokens \
                 and your prompt contains at least 24577 input tokens, for a total of at least \
                 {} tokens.\"}}}}",
                asked as usize + 24_577
            );
        }
        Ok(Completion { content: Some("fits now".into()), ..Default::default() })
    }
}

#[tokio::test]
async fn a_request_that_cannot_fit_is_retried_with_the_servers_numbers() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create_at(&dir.path().join("s.jsonl"), dir.path()).unwrap();

    // Our own estimate cannot see the system prompt, the tool schemas or loaded
    // skills, so on a resumed session it reads far under the truth and the
    // clamp lets a doomed request through. The server knows both numbers.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::new(
        Arc::new(ContextLimitClient { seen: seen.clone() }),
        Arc::new(ToolRegistry::with_builtins()),
        EventBus::new(),
        "mock".into(),
        None,
        Some(8_192),
        20,
        3,
        3,
        32_768,
        6,
        ToolContext {
            cwd: dir.path().to_path_buf(),
            session_id: "test".into(),
            bash_timeout: Duration::from_secs(10),
            is_worker: false,
            ..Default::default()
        },
    );

    let result = agent
        .run_turn(&mut session, "write chapter 10", "system", None, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(result.text, "fits now", "the retry succeeded: {:?}", result.outcome);
    let asks = seen.lock().unwrap().clone();
    assert_eq!(asks.len(), 2, "one rejection, one retry: {asks:?}");
    assert_eq!(asks[0], 8_192, "first ask is what the config wanted");
    assert!(
        asks[1] as usize + 24_577 <= 32_768,
        "the retry fits inside the window: {asks:?}"
    );
}

#[tokio::test]
async fn one_long_turn_can_still_be_compacted() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create_at(&dir.path().join("s.jsonl"), dir.path()).unwrap();

    // Real work: one instruction, then a great many tool results. Cutting only
    // at user-message boundaries meant keeping the last 6 *turns* was keeping
    // everything, so compaction never fired and the context grew until the
    // server refused the request.
    session.append_message(Message::user("write chapter 10")).unwrap();
    for i in 0..40 {
        session
            .append_message(Message::assistant(
                None,
                vec![ToolCall {
                    id: format!("c{i}"),
                    name: "read".into(),
                    arguments: format!("{{\"path\":\"ch{i}.md\"}}"),
                }],
            ))
            .unwrap();
        session
            .append_message(Message::tool_result(format!("c{i}"), "read", "x".repeat(2_000)))
            .unwrap();
    }
    let before = session.messages().len();

    let client = MockClient::new(vec![handover("SUMMARY: read forty files"), done("chapter written")]);
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
        4_096, // a small window, so the transcript is far over the trigger
        6,     // keep_recent_turns: only one user message exists, so unusable
        ToolContext {
            cwd: dir.path().to_path_buf(),
            session_id: "test".into(),
            bash_timeout: Duration::from_secs(10),
            is_worker: false,
            ..Default::default()
        },
    );

    agent
        .run_turn(&mut session, "carry on", "system", None, CancellationToken::new())
        .await
        .unwrap();

    assert!(
        session.messages().len() < before,
        "it compacted: {} -> {}",
        before,
        session.messages().len()
    );
    // A kept slice starting at a tool result would be a `tool` message with no
    // assistant tool_calls before it, which providers reject outright.
    let first = &session.messages()[1]; // [0] is the summary
    assert!(
        !matches!(first.role, worksmith::llm::Role::Tool),
        "the kept slice must not start with an orphaned tool result"
    );
}

/// A turn that ends badly has to say what to do about it. The failure this
/// covers: hitting the step limit left four words in the footer, the next
/// keystroke took them away, and nothing said the work was still there.
#[test]
fn a_turn_that_ends_badly_says_what_to_do_next() {
    use worksmith::agent::TurnOutcome;

    let hit = TurnOutcome::MaxSteps(50);
    assert_eq!(hit.label(), "hit step limit (50)", "the number, not just the fact");
    let advice = hit.advice().expect("the step limit must explain itself");
    assert!(advice.contains("50"), "names the cap that was hit: {advice}");
    assert!(advice.contains("continue"), "says the work is resumable: {advice}");
    assert!(advice.contains("max-steps"), "names the setting: {advice}");

    for bad in [
        TurnOutcome::ValidationFailed("cargo test".into()),
        TurnOutcome::Stuck("read the same file 4 times".into()),
        TurnOutcome::Blocked("rm -rf refused".into()),
    ] {
        let a = bad.advice().unwrap_or_else(|| panic!("{} says nothing", bad.label()));
        assert!(a.len() > 40, "{}: too terse to act on", bad.label());
    }

    // Success needs no announcement, and an abort was the user's own doing.
    assert!(TurnOutcome::Done.advice().is_none());
    assert!(TurnOutcome::Aborted.advice().is_none(), "do not narrate what they just did");
}

// ---- harness-raised checkpoints -------------------------------------------

/// An asker that answers once, and records what it was asked.
struct Scripted {
    answer: Option<String>,
    asked: Mutex<Vec<(String, String)>>,
}

#[async_trait]
impl worksmith::tools::approval::Asker for Scripted {
    async fn ask_text(&self, subject: &str, question: &str) -> Option<String> {
        self.asked.lock().unwrap().push((subject.into(), question.into()));
        self.answer.clone()
    }
}

fn agent_pairing(client: MockClient, cwd: &std::path::Path, asker: Arc<Scripted>) -> Agent {
    Agent::new(
        Arc::new(client),
        Arc::new(ToolRegistry::with_builtins()),
        EventBus::new(),
        "mock".into(),
        None,
        None,
        20,
        3,
        3,
        1_000_000,
        6,
        ToolContext {
            cwd: cwd.to_path_buf(),
            session_id: "test".into(),
            bash_timeout: Duration::from_secs(10),
            is_worker: false,
            asker,
            ..Default::default()
        },
    )
    .with_pairing(true)
}

/// The model deciding when to checkpoint does not work — a 27B with the tool
/// available made twenty edits and never called it. So the harness raises one
/// where it already knows something is wrong, and the model cannot decline.
#[tokio::test]
async fn a_check_failing_twice_asks_the_user_and_follows_the_answer() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create_at(&dir.path().join("s.jsonl"), dir.path()).unwrap();

    let mut script = Vec::new();
    for _ in 0..8 {
        script.push(tool_call("write", r#"{"path":"out.txt","content":"bad"}"#));
        script.push(done("done"));
    }
    let asker = Arc::new(Scripted {
        answer: Some("write the word good, not bad".into()),
        asked: Mutex::new(Vec::new()),
    });
    let agent = agent_pairing(MockClient::new(script), dir.path(), asker.clone());
    let validator = CommandValidator::new(
        r#"test "$(cat out.txt)" = good"#,
        dir.path().to_path_buf(),
        Duration::from_secs(10),
    );

    let _ = agent
        .run_turn(&mut session, "make it good", "system", Some(&validator), CancellationToken::new())
        .await
        .unwrap();

    let asked = asker.asked.lock().unwrap();
    assert_eq!(asked.len(), 1, "asked once, on the second failure — not every time");
    assert!(asked[0].0.contains("failed twice"), "subject: {}", asked[0].0);

    // The answer has to reach the model, or the checkpoint was theatre.
    let transcript: String =
        session.messages().iter().filter_map(|m| m.content.clone()).collect::<Vec<_>>().join("\n");
    assert!(
        transcript.contains("write the word good, not bad"),
        "the user's answer never reached the model"
    );
}

#[tokio::test]
async fn a_skipped_or_unwatched_checkpoint_changes_nothing() {
    // The direction that matters: an eval run has nobody to teach and still has
    // to do the work. Skipping must leave the loop exactly as it was.
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create_at(&dir.path().join("s.jsonl"), dir.path()).unwrap();

    let mut script = Vec::new();
    for _ in 0..8 {
        script.push(tool_call("write", r#"{"path":"out.txt","content":"bad"}"#));
        script.push(done("done"));
    }
    let asker = Arc::new(Scripted { answer: None, asked: Mutex::new(Vec::new()) });
    let agent = agent_pairing(MockClient::new(script), dir.path(), asker.clone());
    let validator = CommandValidator::new(
        r#"test "$(cat out.txt)" = good"#,
        dir.path().to_path_buf(),
        Duration::from_secs(10),
    );

    let result = agent
        .run_turn(&mut session, "make it good", "system", Some(&validator), CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(asker.asked.lock().unwrap().len(), 1, "it still asked");
    assert!(
        matches!(result.outcome, TurnOutcome::ValidationFailed(_)),
        "a skip leaves the outcome alone: {:?}",
        result.outcome
    );
}

#[tokio::test]
async fn pairing_off_never_interrupts() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create_at(&dir.path().join("s.jsonl"), dir.path()).unwrap();

    let mut script = Vec::new();
    for _ in 0..8 {
        script.push(tool_call("write", r#"{"path":"out.txt","content":"bad"}"#));
        script.push(done("done"));
    }
    let asker = Arc::new(Scripted { answer: Some("x".into()), asked: Mutex::new(Vec::new()) });
    // Same agent, pairing left off.
    let agent = agent_pairing(MockClient::new(script), dir.path(), asker.clone());
    agent.set_pairing(false);
    let validator = CommandValidator::new(
        r#"test "$(cat out.txt)" = good"#,
        dir.path().to_path_buf(),
        Duration::from_secs(10),
    );

    let _ = agent
        .run_turn(&mut session, "make it good", "system", Some(&validator), CancellationToken::new())
        .await
        .unwrap();

    assert!(asker.asked.lock().unwrap().is_empty(), "/pair off means never asked");
}

/// The cap on its own is not trouble — a long job can want another turn. The
/// cap reached with *nothing written* is: measured at 50 steps, 46 reads, 21
/// greps, one file opened seventeen times, and not one edit.
#[tokio::test]
async fn burning_every_step_without_writing_anything_asks_for_a_way_in() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create_at(&dir.path().join("s.jsonl"), dir.path()).unwrap();

    // Reads forever, never writes — the observed failure, in miniature.
    // Distinct calls, so this reaches the step cap rather than tripping the
    // stuck detector — the observed run searched widely and never repeated
    // itself into a corner; it simply never started.
    let script: Vec<_> = (0..80)
        .map(|i| tool_call("bash", &format!(r#"{{"command":"echo reading part {i}"}}"#)))
        .collect();
    let asker = Arc::new(Scripted {
        answer: Some("edit src/event.rs, the enum is at the top".into()),
        asked: Mutex::new(Vec::new()),
    });
    let agent = agent_pairing(MockClient::new(script), dir.path(), asker.clone());

    let _ = agent
        .run_turn(&mut session, "do the thing", "system", None, CancellationToken::new())
        .await
        .unwrap();

    let asked = asker.asked.lock().unwrap();
    assert_eq!(asked.len(), 1, "asked once, not once per exhausted budget");
    assert!(asked[0].0.contains("nothing written"), "subject: {}", asked[0].0);

    let transcript: String =
        session.messages().iter().filter_map(|m| m.content.clone()).collect::<Vec<_>>().join("\n");
    assert!(transcript.contains("the enum is at the top"), "the answer must reach the model");
}

#[tokio::test]
async fn a_turn_that_wrote_something_is_not_interrupted_at_the_cap() {
    // Hitting the cap after real work is a long job, not a stuck one. Asking
    // there would be an interruption with nothing behind it.
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create_at(&dir.path().join("s.jsonl"), dir.path()).unwrap();

    let mut script = vec![tool_call("write", r#"{"path":"a.txt","content":"x"}"#)];
    script.extend(
        (0..80).map(|i| tool_call("bash", &format!(r#"{{"command":"echo part {i}"}}"#))),
    );
    let asker = Arc::new(Scripted { answer: Some("x".into()), asked: Mutex::new(Vec::new()) });
    let agent = agent_pairing(MockClient::new(script), dir.path(), asker.clone());

    let result = agent
        .run_turn(&mut session, "do the thing", "system", None, CancellationToken::new())
        .await
        .unwrap();

    assert!(asker.asked.lock().unwrap().is_empty(), "it wrote something; do not interrupt");
    assert!(matches!(result.outcome, TurnOutcome::MaxSteps(_)), "{:?}", result.outcome);
}

// ---- compaction refuses to trade context for a sentence --------------------

/// Compaction was silently destroying context on every fire. The summarizer,
/// asked politely for "concise notes", answered conversationally: measured
/// summaries of 101 and 99 characters, both "Let me look at X next" — the
/// model's next intention, not what it had learned. Every file and line number
/// went with it, and it re-read them all.
#[tokio::test]
async fn a_summary_that_is_not_one_is_refused_and_the_history_kept() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create_at(&dir.path().join("s.jsonl"), dir.path()).unwrap();

    // Enough history that compaction has something to bite on, then a
    // summarizer that replies the way the real one did.
    for i in 0..12 {
        session.append_message(Message::user(format!("do part {i}: {}", "x".repeat(400)))).unwrap();
        session
            .append_message(Message::assistant(Some(format!("did part {i}")), vec![]))
            .unwrap();
    }
    let before = session.messages().len();

    let agent = agent_small_window(
        MockClient::new(vec![done("Let me look at the REPL's handle_command next.")]),
        dir.path(),
    );
    agent.compact(&mut session).await.unwrap();

    assert_eq!(
        session.messages().len(),
        before,
        "a one-line 'let me look at X' is not notes; keep the history rather than trade \
         every location for it"
    );
}

#[tokio::test]
async fn real_handover_notes_are_accepted() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create_at(&dir.path().join("s.jsonl"), dir.path()).unwrap();
    for i in 0..12 {
        session.append_message(Message::user(format!("do part {i}: {}", "x".repeat(400)))).unwrap();
        session
            .append_message(Message::assistant(Some(format!("did part {i}")), vec![]))
            .unwrap();
    }
    let before = session.messages().len();

    let notes = "## Goal\nAdd Event::ModelChanged.\n\n## Locations\n\
                 src/event.rs:14 — the Event enum\n\
                 src/tui.rs:859 — App::apply, exhaustive\n\
                 src/tui.rs:2476 — the /history renderer, exhaustive\n\n\
                 ## Established\nActiveModel and set_model already exist; steps 1 and 2 are done.\n\
                 The three matches are exhaustive and fail to compile until updated.\n\n\
                 ## Unfinished\nThe /model command itself, and the REPL.";
    let agent = agent_small_window(MockClient::new(vec![done(notes)]), dir.path());
    agent.compact(&mut session).await.unwrap();

    assert!(
        session.messages().len() < before,
        "real notes compact the history: {} -> {}",
        before,
        session.messages().len()
    );
    let kept: String =
        session.messages().iter().filter_map(|m| m.content.clone()).collect::<Vec<_>>().join("\n");
    assert!(kept.contains("src/tui.rs:2476"), "the locations survive, which is the point");
}
