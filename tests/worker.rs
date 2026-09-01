//! Sub-worker manager: a spawned worker runs its task to completion and its
//! status/result are observable. Driven by a mock LLM client (no network).

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

struct MockClient {
    responses: Mutex<VecDeque<Completion>>,
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

fn done(text: &str) -> Completion {
    Completion { content: Some(text.into()), ..Default::default() }
}

fn tool_call(name: &str, args: &str) -> Completion {
    Completion {
        tool_calls: vec![worksmith::llm::ToolCall {
            id: "c".into(),
            name: name.into(),
            arguments: args.into(),
        }],
        ..Default::default()
    }
}

fn template_agent(responses: Vec<Completion>, cwd: &std::path::Path) -> Agent {
    Agent::new(
        Arc::new(MockClient { responses: Mutex::new(responses.into()) }),
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

async fn wait_terminal(mgr: &WorkerManager, id: &str) -> WorkerStatus {
    for _ in 0..200 {
        if let Some(s) = mgr.get(id) && !s.status.is_running() {
            return s.status;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("worker {id} did not finish in time");
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
async fn worker_runs_task_to_completion() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    // Workers create their own session files under the global sessions dir, but
    // that's fine — we only assert on manager state here.
    let agent = Arc::new(template_agent(vec![done("worker finished the job")], dir.path()));
    let mut mgr = WorkerManager::new(agent, dir.path().to_path_buf(), 4);

    let id = started(&mut mgr, "do the thing");
    assert_eq!(mgr.running_count(), 1);

    let status = wait_terminal(&mgr, &id).await;
    assert_eq!(status, WorkerStatus::Done);

    let summary = mgr.get(&id).unwrap();
    assert_eq!(summary.result, "worker finished the job");
    assert_eq!(summary.task, "do the thing");
    assert_eq!(mgr.running_count(), 0);
}

#[tokio::test]
async fn newly_finished_reports_once() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let agent = Arc::new(template_agent(vec![done("all done")], dir.path()));
    let mut mgr = WorkerManager::new(agent, dir.path().to_path_buf(), 4);

    let id = started(&mut mgr, "task");
    let _ = wait_terminal(&mgr, &id).await;

    let first = mgr.take_newly_finished();
    assert_eq!(first.len(), 1, "should report the finished worker once");
    assert_eq!(first[0].id, id);

    let second = mgr.take_newly_finished();
    assert!(second.is_empty(), "should not report the same worker again");
}

#[tokio::test]
async fn worker_records_changed_files() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let agent = Arc::new(template_agent(
        vec![
            tool_call("write", r#"{"path":"out.txt","content":"hi"}"#),
            done("wrote the file"),
        ],
        dir.path(),
    ));
    let mut mgr = WorkerManager::new(agent, dir.path().to_path_buf(), 4);

    let id = started(&mut mgr, "make out.txt");
    let _ = wait_terminal(&mgr, &id).await;

    let summary = mgr.get(&id).unwrap();
    assert_eq!(summary.changed, vec!["out.txt".to_string()], "should record the changed file");
    assert!(dir.path().join("out.txt").exists(), "worker should have written the file");
    assert!(!summary.session_id.is_empty());
}

#[tokio::test]
async fn worker_records_absolute_changed_files_relative_to_the_project() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.txt");
    let args = serde_json::json!({
        "path": out.to_string_lossy(),
        "content": "hi",
    })
    .to_string();
    let agent = Arc::new(template_agent(
        vec![tool_call("write", &args), done("wrote the file")],
        dir.path(),
    ));
    let mut mgr = WorkerManager::new(agent, dir.path().to_path_buf(), 4);

    let id = started(&mut mgr, "make out.txt");
    let _ = wait_terminal(&mgr, &id).await;

    let summary = mgr.get(&id).unwrap();
    assert_eq!(
        summary.changed,
        vec!["out.txt".to_string()],
        "absolute tool paths inside cwd should be reported relative to the project"
    );
    assert!(out.exists(), "worker should have written the file");
}

#[tokio::test]
async fn worker_respects_concurrency_cap() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    // A worker whose mock never returns "done" (always a tool call) stays busy.
    let busy: Vec<Completion> = (0..50)
        .map(|_| Completion {
            tool_calls: vec![worksmith::llm::ToolCall {
                id: "c".into(),
                name: "ls".into(),
                arguments: "{}".into(),
            }],
            ..Default::default()
        })
        .collect();
    let agent = Arc::new(template_agent(busy, dir.path()));
    let mut mgr = WorkerManager::new(agent, dir.path().to_path_buf(), 1);

    let _id = started(&mut mgr, "busy");
    // At the cap, a second spawn queues instead of starting.
    let second = mgr.spawn("another".into(), "system".into()).unwrap();
    assert_eq!(second, worksmith::worker::SpawnOutcome::Queued(1));
    assert_eq!(mgr.running_count(), 1, "cap of 1 must not be exceeded");
    assert_eq!(mgr.queued_count(), 1);
    assert_eq!(mgr.drop_queued(), 1, "queued work can be called off");
    assert_eq!(mgr.queued_count(), 0);
}

/// Until now a worker stopped when the model said it was done — `worker.rs`
/// passed no validator, so the harness's whole differentiator applied to the
/// main loop and not to half the product. On a small model that is the measured
/// failure: 10 of 21 eval failures had outcome `done` and were wrong.
#[tokio::test]
async fn a_checked_worker_replans_until_the_check_passes() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();

    // First attempt writes the wrong content; after the re-plan directive, the
    // second writes what the check actually wants.
    let agent = template_agent(
        vec![
            tool_call("write", r#"{"path":"out.txt","content":"bad"}"#),
            done("done (attempt 1)"),
            tool_call("write", r#"{"path":"out.txt","content":"good"}"#),
            done("fixed it (attempt 2)"),
        ],
        dir.path(),
    );
    let mut mgr = WorkerManager::new(Arc::new(agent), dir.path().to_path_buf(), 4);

    let outcome = mgr
        .spawn_checked(
            "make out.txt say good".into(),
            "system".into(),
            None,
            Some(r#"test "$(cat out.txt)" = good"#.into()),
        )
        .unwrap();
    let id = match outcome {
        worksmith::worker::SpawnOutcome::Started(id) => id,
        other => panic!("expected a started worker, got {other:?}"),
    };

    assert_eq!(wait_terminal(&mgr, &id).await, WorkerStatus::Done);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
        "good",
        "the worker should have been sent back until the check passed"
    );
}

/// Without a check, "done" is still taken at face value — the old behaviour,
/// kept deliberately so a fan-out doesn't run N concurrent checks in one tree.
#[tokio::test]
async fn an_unchecked_worker_still_stops_when_the_model_says_so() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let agent = template_agent(
        vec![tool_call("write", r#"{"path":"out.txt","content":"bad"}"#), done("all finished")],
        dir.path(),
    );
    let mut mgr = WorkerManager::new(Arc::new(agent), dir.path().to_path_buf(), 4);

    let id = started(&mut mgr, "make out.txt say good");
    assert_eq!(wait_terminal(&mgr, &id).await, WorkerStatus::Done);
    assert_eq!(std::fs::read_to_string(dir.path().join("out.txt")).unwrap(), "bad");
}

/// A worker's events go to its own bus and never reach the parent's transcript,
/// so `/agents` could show status but never *what it was doing*. The log is how
/// the parent sees inside a running worker.
#[tokio::test]
async fn a_workers_activity_can_be_followed_while_it_runs() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let agent = template_agent(
        vec![
            tool_call("write", r#"{"path":"a.txt","content":"one"}"#),
            tool_call("write", r#"{"path":"b.txt","content":"two"}"#),
            done("finished both files"),
        ],
        dir.path(),
    );
    let mut mgr = WorkerManager::new(Arc::new(agent), dir.path().to_path_buf(), 4);
    let id = started(&mut mgr, "write two files");
    assert_eq!(wait_terminal(&mgr, &id).await, WorkerStatus::Done);

    let (lines, next, missed) = mgr.log_since(&id, 0).expect("the worker exists");
    assert_eq!(missed, 0);
    assert!(next > 0, "the cursor advances past what was read");
    let joined = lines.join("\n");
    assert!(joined.contains("⚙ write"), "tool calls are visible: {joined}");
    assert!(joined.contains("finished both files"), "so is what it said: {joined}");

    // Reading again from the cursor yields nothing — a follower must not
    // re-print what it already showed on every poll.
    let (again, next2, _) = mgr.log_since(&id, next).unwrap();
    assert!(again.is_empty(), "nothing new: {again:?}");
    assert_eq!(next2, next);

    assert!(mgr.log_since("nope", 0).is_none(), "an unknown id is not a panic");
}
