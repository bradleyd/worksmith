//! Sub-worker manager: a spawned worker runs its task to completion and its
//! status/result are observable. Driven by a mock LLM client (no network).

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

#[tokio::test]
async fn worker_runs_task_to_completion() {
    let dir = tempfile::tempdir().unwrap();
    // Workers create their own session files under the global sessions dir, but
    // that's fine — we only assert on manager state here.
    let agent = Arc::new(template_agent(vec![done("worker finished the job")], dir.path()));
    let mut mgr = WorkerManager::new(agent, dir.path().to_path_buf(), 4);

    let id = mgr.spawn("do the thing".into(), "system".into()).unwrap();
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
    let dir = tempfile::tempdir().unwrap();
    let agent = Arc::new(template_agent(vec![done("all done")], dir.path()));
    let mut mgr = WorkerManager::new(agent, dir.path().to_path_buf(), 4);

    let id = mgr.spawn("task".into(), "system".into()).unwrap();
    let _ = wait_terminal(&mgr, &id).await;

    let first = mgr.take_newly_finished();
    assert_eq!(first.len(), 1, "should report the finished worker once");
    assert_eq!(first[0].id, id);

    let second = mgr.take_newly_finished();
    assert!(second.is_empty(), "should not report the same worker again");
}

#[tokio::test]
async fn worker_records_changed_files() {
    let dir = tempfile::tempdir().unwrap();
    let agent = Arc::new(template_agent(
        vec![
            tool_call("write", r#"{"path":"out.txt","content":"hi"}"#),
            done("wrote the file"),
        ],
        dir.path(),
    ));
    let mut mgr = WorkerManager::new(agent, dir.path().to_path_buf(), 4);

    let id = mgr.spawn("make out.txt".into(), "system".into()).unwrap();
    let _ = wait_terminal(&mgr, &id).await;

    let summary = mgr.get(&id).unwrap();
    assert_eq!(summary.changed, vec!["out.txt".to_string()], "should record the changed file");
    assert!(dir.path().join("out.txt").exists(), "worker should have written the file");
    assert!(!summary.session_id.is_empty());
}

#[tokio::test]
async fn worker_respects_concurrency_cap() {
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

    let _id = mgr.spawn("busy".into(), "system".into()).unwrap();
    // Second spawn should be rejected while the first is still running.
    let second = mgr.spawn("another".into(), "system".into());
    assert!(second.is_err(), "cap of 1 should reject a second worker");
}
