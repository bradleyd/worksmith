//! Spawned workers (sub-agents). Each worker is a forked [`Agent`] running a
//! delegated task on its own event bus + session, in-process. The manager
//! tracks live status; each worker's watcher task also runs the
//! [`Supervisor`](crate::supervisor) over that same event stream — nudging via
//! steering, and cancelling the worker when it escalates.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, Steering};
use crate::event::Event;
use crate::event::EventBus;
use crate::session::Session;
use crate::supervisor::{Action, Supervisor, SupervisorConfig};

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
    /// Supervisor interventions so far.
    nudges: usize,
    /// Set when the supervisor pulled this worker off the floor; it wins over
    /// the (necessarily "aborted") turn outcome when reporting.
    escalation: Option<String>,
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
    /// How many times the supervisor nudged this worker.
    pub nudges: usize,
    /// Why the supervisor stopped it, if it did.
    pub escalation: Option<String>,
    /// The fan-out this worker belongs to, if it was one of several.
    pub group: Option<u64>,
}

struct Worker {
    id: String,
    task: String,
    session_id: String,
    group: Option<u64>,
    runtime: Arc<Mutex<Runtime>>,
    cancel: CancellationToken,
    /// Channel for injecting messages into the running worker (nudges).
    steering: Steering,
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
            nudges: 0,
            escalation: None,
        }));
        let cancel = CancellationToken::new();

        let rt = runtime.clone();
        let cancel_task = cancel.clone();
        let cancel_sup = cancel.clone();
        let task_run = task.clone();
        let mut supervisor = Supervisor::new(self.supervisor.clone());
        let steer_sup = steering.clone();
        let handle = tokio::spawn(async move {
            let mut session = session;
            let agent = agent;
            let turn = agent.run_turn(&mut session, &task_run, &system, None, cancel_task);
            tokio::pin!(turn);
            // Absolute deadline for the idle rule, pushed out by every event.
            let idle = supervisor.idle_timeout();
            let mut deadline = Instant::now() + idle;
            loop {
                tokio::select! {
                    ev = rx.recv() => {
                        if let Ok(e) = ev {
                            deadline = Instant::now() + idle;
                            let action = supervisor.observe(&e);
                            let mut g = rt.lock().unwrap();
                            update_last(&mut g, e);
                            apply(&mut g, action, &steer_sup, &cancel_sup);
                        }
                    }
                    // Only armed when the supervisor is on; otherwise this branch
                    // is disabled and the worker is never interrupted.
                    _ = tokio::time::sleep_until(deadline.into()), if supervisor.is_on() => {
                        deadline = Instant::now() + idle;
                        let action = supervisor.on_idle();
                        let mut g = rt.lock().unwrap();
                        apply(&mut g, action, &steer_sup, &cancel_sup);
                    }
                    res = &mut turn => {
                        // The turn can finish with events still buffered on the
                        // bus (select picks a ready branch at random) — drain
                        // them so the final summary isn't missing tool calls.
                        let mut pending = Vec::new();
                        while let Ok(e) = rx.try_recv() {
                            pending.push(e);
                        }
                        let mut g = rt.lock().unwrap();
                        for e in pending {
                            update_last(&mut g, e);
                        }
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
                        // An escalation is why the turn ended; report that, not
                        // the bare "aborted" the cancellation produced.
                        if let Some(reason) = g.escalation.clone() {
                            g.status = WorkerStatus::Stopped;
                            g.last = format!("supervisor: {reason}");
                            if g.result.trim().is_empty() {
                                g.result = format!("stopped by supervisor — {reason}");
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
            group,
            runtime,
            cancel,
            steering,
            reported: false,
            _handle: handle,
        });
        Ok(id)
    }

    /// Inject a steering message into a running worker (manual `/agents nudge`,
    /// same mechanism the supervisor uses). False if there's no such worker.
    pub fn nudge(&self, id: &str, message: &str) -> bool {
        match self.workers.iter().find(|w| w.id == id) {
            Some(w) => {
                w.steering.push(message);
                let mut g = w.runtime.lock().unwrap();
                g.nudges += 1;
                true
            }
            None => false,
        }
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

/// Carry out a supervisor decision: nudge = steer the worker's next step;
/// escalate = pull it off the floor (cancel) and record why.
fn apply(
    g: &mut Runtime,
    action: Option<Action>,
    steering: &Steering,
    cancel: &CancellationToken,
) {
    match action {
        Some(Action::Nudge(directive)) => {
            g.nudges += 1;
            steering.push(directive);
        }
        Some(Action::Escalate(reason)) => {
            g.last = format!("supervisor: {reason}");
            g.escalation = Some(reason);
            cancel.cancel();
        }
        None => {}
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
