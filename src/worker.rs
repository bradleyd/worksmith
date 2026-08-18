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
use crate::llm::ModelOverride;
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
    /// Set when this worker runs on a model other than the parent's.
    pub model: Option<String>,
}

struct Worker {
    id: String,
    task: String,
    session_id: String,
    group: Option<u64>,
    model: Option<String>,
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
            nudges: r.nudges,
            escalation: r.escalation.clone(),
            group: self.group,
            model: self.model.clone(),
        }
    }
}

/// A task waiting for a free worker slot.
struct PendingTask {
    task: String,
    system: String,
    group: Option<u64>,
    model: Option<ModelOverride>,
}

/// A fan-out: several workers answering one request together.
struct GroupInfo {
    request: String,
    total: usize,
}

/// What happened to a spawn request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnOutcome {
    Started(String),
    /// All slots were busy; queued at this 1-based position.
    Queued(usize),
}

impl SpawnOutcome {
    /// The worker id, if it started immediately.
    pub fn started(&self) -> Option<&str> {
        match self {
            SpawnOutcome::Started(id) => Some(id),
            SpawnOutcome::Queued(_) => None,
        }
    }
}

/// The result of a fan-out: what started now, what's waiting.
#[derive(Debug, Clone, Default)]
pub struct FanOutReport {
    pub started: Vec<String>,
    pub queued: usize,
    /// Set when this fan-out is a group whose results are reported together.
    pub group: Option<u64>,
}

/// Tracks spawned workers and enforces the concurrency cap.
pub struct WorkerManager {
    template: Arc<Agent>,
    cwd: PathBuf,
    max: usize,
    supervisor: SupervisorConfig,
    /// Model every worker runs on unless a spawn overrides it (`agents.model`).
    default_model: Option<ModelOverride>,
    workers: Vec<Worker>,
    queued: VecDeque<PendingTask>,
    groups: HashMap<u64, GroupInfo>,
    counter: usize,
    next_group: u64,
}

impl WorkerManager {
    pub fn new(template: Arc<Agent>, cwd: PathBuf, max: usize) -> Self {
        Self {
            template,
            cwd,
            max,
            supervisor: SupervisorConfig::default(),
            default_model: None,
            workers: Vec::new(),
            queued: VecDeque::new(),
            groups: HashMap::new(),
            counter: 0,
            next_group: 0,
        }
    }

    /// Watch spawned workers with this policy (`agents.supervisor` et al).
    pub fn with_supervisor(mut self, cfg: SupervisorConfig) -> Self {
        self.supervisor = cfg;
        self
    }

    /// Run workers on this model by default — the cheap half of a
    /// cheap-workers/smart-parent split. `/spawn --model` overrides per spawn.
    pub fn with_default_model(mut self, model: Option<ModelOverride>) -> Self {
        self.default_model = model;
        self
    }

    pub fn supervisor_config(&self) -> &SupervisorConfig {
        &self.supervisor
    }

    pub fn running_count(&self) -> usize {
        self.workers.iter().filter(|w| w.summary().status.is_running()).count()
    }

    /// Spawn a worker for `task` with the given `system` prompt. At the
    /// concurrency cap the task is queued instead and started by [`Self::pump`]
    /// when a slot frees.
    pub fn spawn(&mut self, task: String, system: String) -> Result<SpawnOutcome, String> {
        self.spawn_in(task, system, None, None)
    }

    /// Spawn on a specific model instead of the manager's default.
    pub fn spawn_on(
        &mut self,
        task: String,
        system: String,
        model: Option<ModelOverride>,
    ) -> Result<SpawnOutcome, String> {
        self.spawn_in(task, system, None, model)
    }

    fn spawn_in(
        &mut self,
        task: String,
        system: String,
        group: Option<u64>,
        model: Option<ModelOverride>,
    ) -> Result<SpawnOutcome, String> {
        let model = model.or_else(|| self.default_model.clone());
        if self.running_count() >= self.max {
            self.queued.push_back(PendingTask { task, system, group, model });
            return Ok(SpawnOutcome::Queued(self.queued.len()));
        }
        self.start(task, system, group, model).map(SpawnOutcome::Started)
    }

    /// Spawn one worker per task. More than one becomes a *group*: they're
    /// answering `request` together, so the parent can report on them as a unit.
    /// A session-creation failure drops that one task, not the whole fan-out.
    pub fn spawn_many(
        &mut self,
        tasks: Vec<String>,
        system: String,
        request: String,
    ) -> FanOutReport {
        self.spawn_many_on(tasks, system, request, None)
    }

    /// Fan out onto a specific model — three cheap drafters, one smart judge.
    pub fn spawn_many_on(
        &mut self,
        tasks: Vec<String>,
        system: String,
        request: String,
        model: Option<ModelOverride>,
    ) -> FanOutReport {
        let group = if tasks.len() > 1 {
            self.next_group += 1;
            self.groups.insert(
                self.next_group,
                GroupInfo { request, total: tasks.len() },
            );
            Some(self.next_group)
        } else {
            None
        };

        let mut report = FanOutReport { group, ..Default::default() };
        for task in tasks {
            match self.spawn_in(task, system.clone(), group, model.clone()) {
                Ok(SpawnOutcome::Started(id)) => report.started.push(id),
                Ok(SpawnOutcome::Queued(_)) => report.queued += 1,
                Err(_) => {
                    // The group is now one worker short; don't wait forever on it.
                    if let Some(g) = group.and_then(|g| self.groups.get_mut(&g)) {
                        g.total = g.total.saturating_sub(1);
                    }
                }
            }
        }
        report
    }

    /// The request a fan-out was serving, and how many workers are in it.
    pub fn group_info(&self, group: u64) -> Option<(&str, usize)> {
        self.groups.get(&group).map(|g| (g.request.as_str(), g.total))
    }

    /// Start queued tasks while slots are free. Returns the ids just started.
    /// Called from the UI loop's poll, beside `take_newly_finished`.
    pub fn pump(&mut self) -> Vec<String> {
        let mut started = Vec::new();
        while self.running_count() < self.max {
            let Some(p) = self.queued.pop_front() else {
                break;
            };
            match self.start(p.task, p.system, p.group, p.model) {
                Ok(id) => started.push(id),
                Err(_) => continue, // couldn't create a session; skip this one
            }
        }
        started
    }

    pub fn queued_count(&self) -> usize {
        self.queued.len()
    }

    /// Discard everything still waiting. Returns how many were dropped.
    /// Dropped tasks are removed from their group's expected count, so a group
    /// that loses members still completes instead of hanging forever.
    pub fn drop_queued(&mut self) -> usize {
        let n = self.queued.len();
        for p in self.queued.drain(..) {
            if let Some(g) = p.group.and_then(|g| self.groups.get_mut(&g)) {
                g.total = g.total.saturating_sub(1);
            }
        }
        n
    }

    /// Actually launch a worker. Callers gate on the concurrency cap.
    fn start(
        &mut self,
        task: String,
        system: String,
        group: Option<u64>,
        model: Option<ModelOverride>,
    ) -> Result<String, String> {
        self.counter += 1;
        let id = format!("w{}", self.counter);

        let session = Session::create(&self.cwd).map_err(|e| format!("session: {e}"))?;
        let session_id = session.id.clone();
        let bus = EventBus::new();
        let steering = Steering::new();
        let model_label = model.as_ref().map(|m| m.model.clone());
        let agent = self
            .template
            .fork_with(bus.clone(), session_id.clone(), model)
            .with_steering(steering.clone());
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
            model: model_label,
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
