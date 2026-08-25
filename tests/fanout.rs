//! Fan-out: one `/spawn` becoming several workers, and the queue that lets a
//! fan-out exceed `agents.max` without dropping work. Mock LLM, no network.

mod common;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use worksmith::agent::Agent;
use worksmith::event::EventBus;
use worksmith::llm::{ChatRequest, Completion, LlmClient, StreamEvent};
use worksmith::tools::{ToolContext, ToolRegistry};
use worksmith::worker::{WorkerManager, WorkerStatus};

/// Every worker finishes immediately with a fixed line.
struct DoneClient;

#[async_trait]
impl LlmClient for DoneClient {
    async fn stream(
        &self,
        _req: ChatRequest,
        _sink: mpsc::Sender<StreamEvent>,
        _cancel: CancellationToken,
    ) -> anyhow::Result<Completion> {
        Ok(Completion { content: Some("finished".into()), ..Default::default() })
    }
}

/// Returns scripted text, and records what it was asked — used for the planner.
struct ScriptedClient {
    responses: Mutex<VecDeque<String>>,
}

#[async_trait]
impl LlmClient for ScriptedClient {
    async fn stream(
        &self,
        _req: ChatRequest,
        _sink: mpsc::Sender<StreamEvent>,
        _cancel: CancellationToken,
    ) -> anyhow::Result<Completion> {
        // An exhausted script means "past the scripted part" — the workers that
        // run after the planner still need a real answer. Returning an empty
        // string here makes the agent nudge for a non-empty reply and give up.
        let text = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| "finished".to_string());
        Ok(Completion { content: Some(text), ..Default::default() })
    }
}


/// A test override. Settings are what a model with no `[models."…"]` entry
/// resolves to, so these tests exercise the plain case; the fields exist
/// because a model is not just a name — see `ModelOverride`.
fn over(
    client: std::sync::Arc<dyn worksmith::llm::LlmClient>,
    model: &str,
) -> worksmith::llm::ModelOverride {
    worksmith::llm::ModelOverride {
        client,
        model: model.into(),
        settings: Default::default(),
        context_limit: 128_000,
        temperature: None,
        missing_key_env: None,
    }
}

fn agent_with(client: Arc<dyn LlmClient>, cwd: &std::path::Path) -> Agent {
    Agent::new(
        client,
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
            session_id: "template".into(),
            bash_timeout: Duration::from_secs(10),
            is_worker: false,
            ..Default::default()
        },
    )
}

#[tokio::test]
async fn fanout_beyond_the_cap_queues_and_drains() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let agent = Arc::new(agent_with(Arc::new(DoneClient), dir.path()));
    let mut mgr = WorkerManager::new(agent, dir.path().to_path_buf(), 2);

    let tasks: Vec<String> = (1..=5).map(|i| format!("task {i}")).collect();
    let report = mgr.spawn_many(tasks, "system".into(), "the original ask".into());
    assert_eq!(report.started.len(), 2, "only the cap starts immediately");
    assert_eq!(report.queued, 3);

    // Drive it the way the UI loop does: pump as slots free.
    let mut all = report.started.clone();
    for _ in 0..400 {
        assert!(mgr.running_count() <= 2, "the cap must hold while draining");
        all.extend(mgr.pump());
        if all.len() == 5 && mgr.running_count() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert_eq!(all.len(), 5, "every queued task eventually started");
    assert_eq!(mgr.queued_count(), 0);
    for id in &all {
        assert_eq!(mgr.get(id).unwrap().status, WorkerStatus::Done);
    }
    // Each worker got its own task, not a shared one.
    let mut seen: Vec<String> = mgr.list().into_iter().map(|w| w.task).collect();
    seen.sort();
    assert_eq!(seen, vec!["task 1", "task 2", "task 3", "task 4", "task 5"]);
}

#[tokio::test]
async fn planner_splits_a_request_into_workers() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let client = Arc::new(ScriptedClient {
        responses: Mutex::new(VecDeque::from(vec![
            "write about WAL\nwrite about FTS5\nwrite about JSON1".to_string(),
        ])),
    });
    let agent = Arc::new(agent_with(client, dir.path()));

    // The planner is just a one-shot `ask`; the TUI wraps it in a task.
    let text = agent.ask("split it", "3 articles on sqlite", 512).await.unwrap();
    let tasks: Vec<String> = text.lines().map(str::to_string).collect();
    assert_eq!(tasks.len(), 3);

    let mut mgr = WorkerManager::new(agent, dir.path().to_path_buf(), 4);
    let report = mgr.spawn_many(tasks, "system".into(), "the original ask".into());
    assert_eq!(report.started.len(), 3);
    assert_eq!(report.queued, 0);

    for id in &report.started {
        for _ in 0..200 {
            if !mgr.get(id).unwrap().status.is_running() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(mgr.get(id).unwrap().status, WorkerStatus::Done);
    }
    let tasks: Vec<String> = mgr.list().into_iter().map(|w| w.task).collect();
    assert!(tasks.iter().any(|t| t.contains("FTS5")), "subtask text reached the worker");
}

/// The parent's own steering mailbox is how a worker report reaches a turn
/// that's already running — this is the path `deliver_to_parent` uses.
struct LoopUntilSeen {
    needle: &'static str,
}

#[async_trait]
impl LlmClient for LoopUntilSeen {
    async fn stream(
        &self,
        req: ChatRequest,
        _sink: mpsc::Sender<StreamEvent>,
        _cancel: CancellationToken,
    ) -> anyhow::Result<Completion> {
        let seen = req.messages.iter().any(|m| {
            m.content.as_deref().map(|c| c.contains(self.needle)).unwrap_or(false)
        });
        if seen {
            return Ok(Completion { content: Some("got the report".into()), ..Default::default() });
        }
        Ok(Completion {
            tool_calls: vec![worksmith::llm::ToolCall {
                id: "c".into(),
                name: "ls".into(),
                arguments: "{}".into(),
            }],
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn a_worker_report_reaches_a_running_parent_turn() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let agent = Arc::new(agent_with(
        Arc::new(LoopUntilSeen { needle: "[w1] done" }),
        dir.path(),
    ));
    let mut session = worksmith::session::Session::create(dir.path()).unwrap();

    let steering = agent.steering();
    let a = agent.clone();
    let turn = tokio::spawn(async move {
        a.run_turn(&mut session, "keep working", "system", None, CancellationToken::new()).await
    });

    // A worker finishes while the parent is mid-turn.
    steering.push("A background worker you spawned finished.\n\n[w1] done — task: find hashmaps");

    let result = turn.await.unwrap().unwrap();
    assert_eq!(result.text, "got the report", "the parent turn consumed the worker report");
}

/// Records which model name each request asked for, so a test can prove a
/// worker ran on the override rather than the parent's model.
struct ModelRecordingClient {
    seen: Mutex<Vec<String>>,
}

#[async_trait]
impl LlmClient for ModelRecordingClient {
    async fn stream(
        &self,
        req: ChatRequest,
        _sink: mpsc::Sender<StreamEvent>,
        _cancel: CancellationToken,
    ) -> anyhow::Result<Completion> {
        self.seen.lock().unwrap().push(req.model.clone());
        Ok(Completion { content: Some("done".into()), ..Default::default() })
    }
}

#[tokio::test]
async fn workers_run_on_the_overridden_model() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();

    // The parent is on "smart"; workers are pointed at "cheap" with its own client.
    let parent = Arc::new(ModelRecordingClient { seen: Mutex::new(Vec::new()) });
    let cheap = Arc::new(ModelRecordingClient { seen: Mutex::new(Vec::new()) });
    let agent = Arc::new(agent_with(parent.clone(), dir.path()));

    let over = over(cheap.clone(), "cheap-model");
    let mut mgr = WorkerManager::new(agent, dir.path().to_path_buf(), 4)
        .with_default_model(Some(over.clone()));

    let id = mgr
        .spawn("draft something".into(), "system".into())
        .unwrap()
        .started()
        .unwrap()
        .to_string();
    for _ in 0..200 {
        if !mgr.get(&id).unwrap().status.is_running() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert_eq!(mgr.get(&id).unwrap().status, WorkerStatus::Done);
    assert_eq!(
        cheap.seen.lock().unwrap().as_slice(),
        ["cheap-model"],
        "the worker must call the override's client with the override's model"
    );
    assert!(
        parent.seen.lock().unwrap().is_empty(),
        "the parent's client must not have been used for worker work"
    );
    // And it's visible, so you can tell which model produced which result.
    assert_eq!(mgr.get(&id).unwrap().model.as_deref(), Some("cheap-model"));
}

#[tokio::test]
async fn a_per_spawn_model_beats_the_default_and_queued_work_keeps_it() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let parent = Arc::new(ModelRecordingClient { seen: Mutex::new(Vec::new()) });
    let default_m = Arc::new(ModelRecordingClient { seen: Mutex::new(Vec::new()) });
    let chosen = Arc::new(ModelRecordingClient { seen: Mutex::new(Vec::new()) });
    let agent = Arc::new(agent_with(parent, dir.path()));

    // Cap of 1 so the second and third tasks queue and must carry the override.
    let mut mgr = WorkerManager::new(agent, dir.path().to_path_buf(), 1)
        .with_default_model(Some(over(default_m.clone(), "default-model")));

    let report = mgr.spawn_many_on(
        vec!["a".into(), "b".into(), "c".into()],
        "system".into(),
        "three drafts".into(),
        Some(over(chosen.clone(), "chosen-model")),
    );
    assert_eq!(report.started.len(), 1, "cap of 1");
    assert_eq!(report.queued, 2);

    for _ in 0..400 {
        mgr.pump();
        if mgr.queued_count() == 0 && mgr.running_count() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert_eq!(chosen.seen.lock().unwrap().len(), 3, "all three, including queued ones");
    assert!(
        default_m.seen.lock().unwrap().is_empty(),
        "an explicit --model must beat agents.model for every worker in the fan-out"
    );
}
