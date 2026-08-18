//! The supervisor watching real workers: a worker that repeats itself gets
//! nudged mid-run, and one that keeps it up gets pulled off the floor.
//! Driven by a mock LLM client (no network).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use worksmith::agent::Agent;
use worksmith::event::EventBus;
use worksmith::llm::{ChatRequest, Completion, LlmClient, Message, Role, StreamEvent};
use worksmith::supervisor::{Mode, SupervisorConfig};
use worksmith::tools::{ToolContext, ToolRegistry};
use worksmith::worker::{WorkerManager, WorkerStatus};

/// Replays scripted completions and records the requests it was sent, so a test
/// can assert that a nudge actually reached the model's input.
struct MockClient {
    responses: Mutex<VecDeque<Completion>>,
    seen: Mutex<Vec<Vec<Message>>>,
}

#[async_trait]
impl LlmClient for MockClient {
    async fn stream(
        &self,
        req: ChatRequest,
        _sink: mpsc::Sender<StreamEvent>,
        _cancel: CancellationToken,
    ) -> anyhow::Result<Completion> {
        self.seen.lock().unwrap().push(req.messages.clone());
        Ok(self.responses.lock().unwrap().pop_front().unwrap_or_default())
    }
}

fn ls_call() -> Completion {
    Completion {
        tool_calls: vec![worksmith::llm::ToolCall {
            id: "c".into(),
            name: "ls".into(),
            arguments: "{}".into(),
        }],
        ..Default::default()
    }
}

fn template_agent(client: Arc<MockClient>, cwd: &std::path::Path) -> Agent {
    Agent::new(
        client,
        Arc::new(ToolRegistry::with_builtins()),
        EventBus::new(),
        "mock".into(),
        None,
        None,
        200,
        3,
        // High enough that the agent's own in-turn stuck detection stays out of
        // the way — this test is about the supervisor.
        1_000,
        1_000_000,
        6,
        ToolContext {
            cwd: cwd.to_path_buf(),
            session_id: "template".into(),
            bash_timeout: Duration::from_secs(10),
            is_worker: false,
        },
    )
}

async fn wait_terminal(mgr: &WorkerManager, id: &str) -> WorkerStatus {
    for _ in 0..400 {
        if let Some(s) = mgr.get(id)
            && !s.status.is_running()
        {
            return s.status;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("worker {id} did not finish in time");
}

fn user_texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter(|m| matches!(m.role, Role::User))
        .filter_map(|m| m.content.clone())
        .collect()
}

/// Repeats the same tool call forever and only finishes once `needle` shows up
/// in its input — so "the worker finished" *is* the proof the nudge landed.
struct RepeatUntilNudged {
    needle: &'static str,
}

#[async_trait]
impl LlmClient for RepeatUntilNudged {
    async fn stream(
        &self,
        req: ChatRequest,
        _sink: mpsc::Sender<StreamEvent>,
        _cancel: CancellationToken,
    ) -> anyhow::Result<Completion> {
        if user_texts(&req.messages).iter().any(|t| t.contains(self.needle)) {
            return Ok(Completion {
                content: Some("took a different approach".into()),
                ..Default::default()
            });
        }
        Ok(ls_call())
    }
}

/// Spawn a worker that is expected to start immediately (not queue).
fn started(mgr: &mut WorkerManager, task: &str) -> String {
    mgr.spawn(task.into(), "system".into())
        .unwrap()
        .started()
        .expect("expected an immediate start, not a queued task")
        .to_string()
}

#[tokio::test]
async fn repeated_calls_are_nudged_into_the_worker() {
    let dir = tempfile::tempdir().unwrap();
    let agent = Arc::new(Agent::new(
        Arc::new(RepeatUntilNudged { needle: "identical arguments" }),
        Arc::new(ToolRegistry::with_builtins()),
        EventBus::new(),
        "mock".into(),
        None,
        None,
        200,
        3,
        1_000, // keep the agent's own in-turn stuck detection out of the way
        1_000_000,
        6,
        ToolContext {
            cwd: dir.path().to_path_buf(),
            session_id: "template".into(),
            bash_timeout: Duration::from_secs(10),
            is_worker: false,
        },
    ));
    let mut mgr = WorkerManager::new(agent, dir.path().to_path_buf(), 4).with_supervisor(
        SupervisorConfig { repeat_threshold: 3, max_nudges: 5, ..Default::default() },
    );

    let id = started(&mut mgr, "look around");
    // Only the supervisor's directive can end this run.
    assert_eq!(wait_terminal(&mgr, &id).await, WorkerStatus::Done);

    let summary = mgr.get(&id).unwrap();
    assert!(summary.nudges >= 1, "supervisor should have nudged the repeating worker");
    assert!(summary.escalation.is_none(), "a nudge is not an escalation");
    assert_eq!(summary.result, "took a different approach");
}

#[tokio::test]
async fn worker_is_escalated_after_its_nudges_run_out() {
    let dir = tempfile::tempdir().unwrap();
    // Never finishes on its own, and burns 100 completion tokens per step —
    // the supervisor has to stop it.
    let responses: VecDeque<Completion> = (0..200)
        .map(|_| Completion {
            usage: worksmith::llm::Usage {
                prompt_tokens: 0,
                completion_tokens: 100,
                total_tokens: 100,
            },
            ..ls_call()
        })
        .collect();
    let client = Arc::new(MockClient {
        responses: Mutex::new(responses),
        seen: Mutex::new(Vec::new()),
    });
    let agent = Arc::new(template_agent(client, dir.path()));
    let mut mgr = WorkerManager::new(agent, dir.path().to_path_buf(), 4).with_supervisor(
        SupervisorConfig {
            repeat_threshold: 2,
            max_nudges: 1,
            token_budget: Some(250), // blown by the third step
            ..Default::default()
        },
    );

    let id = started(&mut mgr, "spin forever");
    let status = wait_terminal(&mgr, &id).await;

    let summary = mgr.get(&id).unwrap();
    assert_eq!(status, WorkerStatus::Stopped);
    let reason = summary.escalation.expect("supervisor should have escalated");
    assert!(reason.contains("token budget"), "unexpected escalation reason: {reason}");
    assert!(
        summary.last.starts_with("supervisor:"),
        "escalation should win over the aborted outcome: {}",
        summary.last
    );
}

/// A worker that goes quiet (here: a slow model call) trips the idle rule.
struct SlowClient;

#[async_trait]
impl LlmClient for SlowClient {
    async fn stream(
        &self,
        _req: ChatRequest,
        _sink: mpsc::Sender<StreamEvent>,
        _cancel: CancellationToken,
    ) -> anyhow::Result<Completion> {
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(Completion { content: Some("finally".into()), ..Default::default() })
    }
}

#[tokio::test]
async fn silence_trips_the_idle_rule() {
    let dir = tempfile::tempdir().unwrap();
    let agent = Arc::new(Agent::new(
        Arc::new(SlowClient),
        Arc::new(ToolRegistry::with_builtins()),
        EventBus::new(),
        "mock".into(),
        None,
        None,
        20,
        3,
        1_000,
        1_000_000,
        6,
        ToolContext {
            cwd: dir.path().to_path_buf(),
            session_id: "template".into(),
            bash_timeout: Duration::from_secs(10),
            is_worker: false,
        },
    ));
    let mut mgr = WorkerManager::new(agent, dir.path().to_path_buf(), 4).with_supervisor(
        SupervisorConfig {
            idle_timeout: Duration::from_millis(50),
            max_nudges: 10, // don't escalate; we're asserting on the nudges
            ..Default::default()
        },
    );

    let id = started(&mut mgr, "think hard");
    assert_eq!(wait_terminal(&mgr, &id).await, WorkerStatus::Done);
    assert!(
        mgr.get(&id).unwrap().nudges >= 1,
        "a silent worker should have been nudged"
    );
}

#[tokio::test]
async fn supervisor_off_leaves_the_worker_alone() {
    let dir = tempfile::tempdir().unwrap();
    let mut responses: VecDeque<Completion> = (0..6).map(|_| ls_call()).collect();
    responses.push_back(Completion { content: Some("done".into()), ..Default::default() });
    let client = Arc::new(MockClient {
        responses: Mutex::new(responses),
        seen: Mutex::new(Vec::new()),
    });
    let agent = Arc::new(template_agent(client, dir.path()));
    let mut mgr = WorkerManager::new(agent, dir.path().to_path_buf(), 4).with_supervisor(
        SupervisorConfig {
            mode: Mode::Off,
            repeat_threshold: 2,
            idle_timeout: Duration::from_millis(1),
            ..Default::default()
        },
    );

    let id = started(&mut mgr, "repeat a lot");
    assert_eq!(wait_terminal(&mgr, &id).await, WorkerStatus::Done);

    let summary = mgr.get(&id).unwrap();
    assert_eq!(summary.nudges, 0, "off means off");
    assert!(summary.escalation.is_none());
}

#[tokio::test]
async fn manual_nudge_reaches_a_running_worker() {
    let dir = tempfile::tempdir().unwrap();
    let responses: VecDeque<Completion> = (0..200).map(|_| ls_call()).collect();
    let client = Arc::new(MockClient {
        responses: Mutex::new(responses),
        seen: Mutex::new(Vec::new()),
    });
    let agent = Arc::new(template_agent(client.clone(), dir.path()));
    let mut mgr = WorkerManager::new(agent, dir.path().to_path_buf(), 4)
        .with_supervisor(SupervisorConfig { mode: Mode::Off, ..Default::default() });

    let id = started(&mut mgr, "busy work");
    assert!(mgr.nudge(&id, "check the README instead"));
    assert!(!mgr.nudge("nope", "hi"), "unknown worker id");

    // Wait for the steering message to land in a request.
    let mut landed = false;
    for _ in 0..200 {
        if client
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|m| user_texts(m).iter().any(|t| t.contains("check the README instead")))
        {
            landed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    mgr.kill(&id);
    assert!(landed, "manual nudge should reach the worker's input");
    assert_eq!(mgr.get(&id).unwrap().nudges, 1);
}
