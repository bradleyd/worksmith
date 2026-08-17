//! Spawned workers (sub-agents). Each worker is a forked [`Agent`] running a
//! delegated task on its own event bus + session, in-process. The manager
//! tracks live status; the supervisor (M7) will watch the same event streams.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::agent::Agent;
use crate::event::Event;
use crate::event::EventBus;
use crate::session::Session;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WorkerStatus {
    Running,
    Done,
    Failed,
    Stopped,
}

impl WorkerStatus {
    pub fn label(&self) -> &'static str {
        match self {
            WorkerStatus::Running => "running",
            WorkerStatus::Done => "done",
            WorkerStatus::Failed => "failed",
            WorkerStatus::Stopped => "stopped",
        }
    }
    pub fn is_running(&self) -> bool {
        matches!(self, WorkerStatus::Running)
    }
}

struct Runtime {
    status: WorkerStatus,
    last: String,
    tool_calls: usize,
    /// Files the worker created/edited (deterministic, from write/edit calls).
    changed: Vec<String>,
    result: String,
}

/// A point-in-time view of a worker for display.
#[derive(Clone)]
pub struct WorkerSummary {
    pub id: String,
    pub task: String,
    pub status: WorkerStatus,
    pub last: String,
    pub tool_calls: usize,
    pub changed: Vec<String>,
    pub result: String,
    /// The worker's session id (its full transcript is at that session file).
    pub session_id: String,
}

struct Worker {
    id: String,
    task: String,
    session_id: String,
    runtime: Arc<Mutex<Runtime>>,
    cancel: CancellationToken,
    /// Whether this worker's terminal status has been surfaced to the user.
    reported: bool,
    _handle: JoinHandle<()>,
}

impl Worker {
    fn summary(&self) -> WorkerSummary {
        let r = self.runtime.lock().unwrap();
        WorkerSummary {
            id: self.id.clone(),
            task: self.task.clone(),
            status: r.status,
            last: r.last.clone(),
            tool_calls: r.tool_calls,
            changed: r.changed.clone(),
            result: r.result.clone(),
            session_id: self.session_id.clone(),
        }
    }
}

/// Tracks spawned workers and enforces the concurrency cap.
pub struct WorkerManager {
    template: Arc<Agent>,
    cwd: PathBuf,
    max: usize,
    workers: Vec<Worker>,
    counter: usize,
}

impl WorkerManager {
    pub fn new(template: Arc<Agent>, cwd: PathBuf, max: usize) -> Self {
        Self { template, cwd, max, workers: Vec::new(), counter: 0 }
    }

    pub fn running_count(&self) -> usize {
        self.workers.iter().filter(|w| w.summary().status.is_running()).count()
    }

    /// Spawn a worker for `task` with the given `system` prompt. Returns the new
    /// worker's id, or an error if the concurrency cap is reached.
    pub fn spawn(&mut self, task: String, system: String) -> Result<String, String> {
        if self.running_count() >= self.max {
            return Err(format!("worker limit reached ({} running)", self.max));
        }
        self.counter += 1;
        let id = format!("w{}", self.counter);

        let session = Session::create(&self.cwd).map_err(|e| format!("session: {e}"))?;
        let session_id = session.id.clone();
        let bus = EventBus::new();
        let agent = self.template.fork(bus.clone(), session_id.clone());
        let mut rx = bus.subscribe();
        drop(bus); // the forked agent keeps a sender clone

        let runtime = Arc::new(Mutex::new(Runtime {
            status: WorkerStatus::Running,
            last: "starting…".into(),
            tool_calls: 0,
            changed: Vec::new(),
            result: String::new(),
        }));
        let cancel = CancellationToken::new();

        let rt = runtime.clone();
        let cancel_task = cancel.clone();
        let task_run = task.clone();
        let handle = tokio::spawn(async move {
            let mut session = session;
            let agent = agent;
            let turn = agent.run_turn(&mut session, &task_run, &system, None, cancel_task);
            tokio::pin!(turn);
            loop {
                tokio::select! {
                    ev = rx.recv() => {
                        if let Ok(e) = ev {
                            let mut g = rt.lock().unwrap();
                            update_last(&mut g, e);
                        }
                    }
                    res = &mut turn => {
                        let mut g = rt.lock().unwrap();
                        match res {
                            Ok(r) => {
                                g.result = r.text.clone();
                                if r.outcome.is_success() {
                                    g.status = WorkerStatus::Done;
                                    g.last = "done".into();
                                } else {
                                    g.status = WorkerStatus::Stopped;
                                    g.last = r.outcome.label();
                                }
                            }
                            Err(e) => {
                                g.status = WorkerStatus::Failed;
                                let msg = e.to_string();
                                g.last = first_line(&msg);
                                g.result = msg;
                            }
                        }
                        break;
                    }
                }
            }
        });

        self.workers.push(Worker {
            id: id.clone(),
            task,
            session_id,
            runtime,
            cancel,
            reported: false,
            _handle: handle,
        });
        Ok(id)
    }

    /// Workers that reached a terminal state since the last call (each returned
    /// once). Used to surface completions to the user without polling.
    pub fn take_newly_finished(&mut self) -> Vec<WorkerSummary> {
        let mut out = Vec::new();
        for w in &mut self.workers {
            if w.reported {
                continue;
            }
            let s = w.summary();
            if !s.status.is_running() {
                w.reported = true;
                out.push(s);
            }
        }
        out
    }

    pub fn list(&self) -> Vec<WorkerSummary> {
        self.workers.iter().map(Worker::summary).collect()
    }

    pub fn get(&self, id: &str) -> Option<WorkerSummary> {
        self.workers.iter().find(|w| w.id == id).map(Worker::summary)
    }

    /// Request cancellation of a worker. Returns false if no such id.
    pub fn kill(&self, id: &str) -> bool {
        match self.workers.iter().find(|w| w.id == id) {
            Some(w) => {
                w.cancel.cancel();
                true
            }
            None => false,
        }
    }
}

fn update_last(g: &mut Runtime, e: Event) {
    match e {
        Event::ToolCall { name, arguments, .. } => {
            g.tool_calls += 1;
            g.last = format!("⚙ {name}");
            // Deterministically record file mutations for the parent/supervisor.
            if matches!(name.as_str(), "write" | "edit")
                && let Some(path) = serde_json::from_str::<serde_json::Value>(&arguments)
                    .ok()
                    .and_then(|v| v.get("path").and_then(|p| p.as_str()).map(String::from))
                && !g.changed.contains(&path)
            {
                g.changed.push(path);
            }
        }
        Event::AssistantMessage { text } => {
            if let Some(l) = text.lines().rev().find(|l| !l.trim().is_empty()) {
                g.last = l.to_string();
            }
        }
        Event::Nudge { reason } => g.last = format!("↻ {reason}"),
        Event::Validation { ok, .. } => {
            g.last = if ok { "✓ validated".into() } else { "✗ validation failed".into() }
        }
        Event::Error { message } => g.last = first_line(&message),
        _ => {}
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
}
