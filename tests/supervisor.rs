//! The supervisor watching real workers: a worker that repeats itself gets
//! nudged mid-run, and one that keeps it up gets pulled off the floor.
//! Driven by a mock LLM client (no network).

mod common;

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
            ..Default::default()
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
    common::isolate_home();
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
            ..Default::default()
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
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    // Never finishes on its own, and burns 100 completion tokens per step —
    // the supervisor has to stop it.
    let responses: VecDeque<Completion> = (0..200)
        .map(|_| Completion {
            usage: worksmith::llm::Usage {
                prompt_tokens: 0,
                completion_tokens: 100,
                total_tokens: 100,
                reasoning_tokens: 0,
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
async fn a_slow_request_is_not_mistaken_for_a_stuck_worker() {
    // The supervisor's idle rule measures time since the last event, and a call
    // in flight emits nothing while it waits. Nudging then cannot help: steering
    // is drained at the top of the *next* step, so the message arrives after the
    // call it meant to interrupt, having spent one of max_nudges. Three workers
    // sharing one local server hit this on prefill and were all stopped.
    common::isolate_home();
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
            ..Default::default()
        },
    ));
    // The call takes 300ms; the idle deadline is 100ms, so it passes three
    // times while the model is simply working.
    let mut mgr = WorkerManager::new(agent, dir.path().to_path_buf(), 4).with_supervisor(
        SupervisorConfig {
            idle_timeout: Duration::from_millis(100),
            max_nudges: 10,
            ..Default::default()
        },
    );

    let id = started(&mut mgr, "think hard");
    assert_eq!(wait_terminal(&mgr, &id).await, WorkerStatus::Done);
    assert_eq!(
        mgr.get(&id).unwrap().nudges,
        0,
        "waiting on the model is not being stuck"
    );
}

#[tokio::test]
async fn the_request_cap_is_not_derived_from_the_idle_timeout() {
    // Deriving it (6 x idle) killed three local workers whose only crime was
    // queueing behind each other: a 20s idle timeout made any call over 120s a
    // "hang", which is ordinary for three workers sharing one 9B.
    let cfg = SupervisorConfig { idle_timeout: Duration::from_secs(20), ..Default::default() };
    assert_eq!(
        cfg.request_timeout,
        Duration::from_secs(600),
        "a short idle timeout must not shorten how long a call may take"
    );
}

#[tokio::test]
async fn a_hung_request_is_stopped_rather_than_nudged() {
    // A call can genuinely hang, and stopping it is the only action that helps,
    // so silence far past the timeout still escalates.
    common::isolate_home();
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
            ..Default::default()
        },
    ));
    // 20ms deadline against a 300ms call: well past the multiple that separates
    // "slow" from "hung".
    let mut mgr = WorkerManager::new(agent, dir.path().to_path_buf(), 4).with_supervisor(
        SupervisorConfig {
            idle_timeout: Duration::from_millis(20),
            max_nudges: 10,
            // A call may take this long before it counts as hung; the 300ms
            // mock is well past it.
            request_timeout: Duration::from_millis(100),
            ..Default::default()
        },
    );

    let id = started(&mut mgr, "think hard");
    assert_eq!(wait_terminal(&mgr, &id).await, WorkerStatus::Stopped);
    let w = mgr.get(&id).unwrap();
    assert_eq!(w.nudges, 0, "escalated directly; a nudge would have been useless");
    let why = w.escalation.clone().unwrap_or_default();
    assert!(
        why.contains("no response from the model"),
        "the reason names what happened: {why:?}"
    );
}

#[tokio::test]
async fn supervisor_off_leaves_the_worker_alone() {
    common::isolate_home();
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
    common::isolate_home();
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
    assert!(mgr.nudge(&id, "check the README instead").is_ok());
    // The two refusals are distinguishable on purpose: a worker that has
    // stopped used to be accepted and reported as nudged, and the message was
    // never read.
    let unknown = mgr.nudge("nope", "hi").unwrap_err();
    assert!(unknown.contains("no agent"), "unknown worker id: {unknown}");

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


/// A nudge aimed at a worker that has already finished is refused, not
/// swallowed.
///
/// Reported from use: `/agents nudge w2 continue` answered "nudged w2" for a
/// worker that had already hit its step limit. The steering mailbox is drained
/// by the running loop, so the message was pushed, never read, and left no
/// trace — which is how it was spotted, since a nudge that *is* consumed
/// appends an `Event::Nudge` and a user message to the session and there was
/// none.
#[tokio::test]
async fn nudging_a_finished_worker_is_refused() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    // Two calls then nothing: the worker finishes quickly and stays finished.
    let responses: VecDeque<Completion> = (0..2).map(|_| ls_call()).collect();
    let client = Arc::new(MockClient {
        responses: Mutex::new(responses),
        seen: Mutex::new(Vec::new()),
    });
    let agent = Arc::new(template_agent(client.clone(), dir.path()));
    let mut mgr = WorkerManager::new(agent, dir.path().to_path_buf(), 4)
        .with_supervisor(SupervisorConfig { mode: Mode::Off, ..Default::default() });

    let id = started(&mut mgr, "busy work");

    // Let it run out of scripted replies and settle into a terminal state.
    let mut done = false;
    for _ in 0..400 {
        if mgr.list().iter().any(|w| w.id == id && !w.status.is_running()) {
            done = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(done, "the worker never reached a terminal state");

    let err = mgr.nudge(&id, "continue").unwrap_err();
    assert!(
        err.contains("already"),
        "it must say the worker has stopped, not claim success: {err}"
    );

    // And the refusal is not counted as an intervention.
    let w = mgr.list().into_iter().find(|w| w.id == id).unwrap();
    assert_eq!(w.nudges, 0, "a refused nudge is not a nudge");
    // The timing a reader needs to tell "just now" from "half an hour ago".
    assert!(w.finished.is_some(), "a terminal worker records when it ended");
}

/// A worker inside a slow *tool* call is not nudged, end to end.
///
/// The unit test on `Supervisor::on_idle` covers the decision. This covers the
/// wiring, which is where the doubt actually is: whether `Event::ToolCall`
/// reaches `observe` through the worker loop at all, and whether the guard
/// survives the trip.
///
/// It exists because a live run escalated with "still off track after 2 nudges"
/// during a 60s bash call at a 20s stuck timeout, which is exactly three ticks,
/// and reading the code did not explain it. Guessing twice was already one time
/// too many.
#[tokio::test]
async fn a_worker_inside_a_slow_tool_call_is_not_nudged() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();

    // One bash call that outlives the whole nudge budget, then a plain answer.
    let mut responses: VecDeque<Completion> = VecDeque::new();
    responses.push_back(Completion {
        tool_calls: vec![worksmith::llm::ToolCall {
            id: "c".into(),
            name: "bash".into(),
            arguments: r#"{"command":"sleep 2"}"#.into(),
        }],
        ..Default::default()
    });
    responses.push_back(Completion {
        content: Some("done".into()),
        ..Default::default()
    });

    let client = Arc::new(MockClient {
        responses: Mutex::new(responses),
        seen: Mutex::new(Vec::new()),
    });
    let agent = Arc::new(template_agent(client, dir.path()));

    // 200ms idle, 2 nudges. The 2s sleep is ten ticks, so under the old
    // behaviour this is nudge, nudge, escalate several times over.
    let mut mgr = WorkerManager::new(agent, dir.path().to_path_buf(), 4).with_supervisor(
        SupervisorConfig {
            idle_timeout: Duration::from_millis(200),
            max_nudges: 2,
            request_timeout: Duration::from_secs(60),
            ..Default::default()
        },
    );

    let id = started(&mut mgr, "sleep then answer");
    let status = wait_terminal(&mgr, &id).await;

    let w = mgr.get(&id).unwrap();
    assert_eq!(
        w.nudges, 0,
        "a running tool must not cost a nudge; got {} and escalation {:?}",
        w.nudges, w.escalation
    );
    assert!(w.escalation.is_none(), "nor an escalation: {:?}", w.escalation);
    assert_eq!(status, WorkerStatus::Done, "and the worker finishes normally");
}
