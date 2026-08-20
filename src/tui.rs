//! Ratatui front-end. Renders the four channels distinctly — **user**,
//! **assistant**, **tool** activity, and **thinking** — plus a footer with the
//! model, context %, and token counts. It's a subscriber to the event bus (the
//! keystone from M1); the agent loop is unchanged.
//!
//! Concurrency: the agent turn runs as a spawned task (session behind an async
//! mutex) so the UI keeps rendering and stays responsive to Esc (abort) while
//! the model streams.

use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as CEvent, EventStream, KeyCode, KeyEvent, KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, TurnResult};
use crate::event::{Event, EventBus};
use crate::llm::Thinking;
use crate::memory::{MemoryStore, Scope};
use crate::prompt::{build_system_prompt, build_worker_prompt};
use crate::session::Session;
use crate::validation::CommandValidator;
use crate::fanout::{
    FanOut, PendingFanOut, assign, fanout_notice, matching_files, parse_spawn, plan_fanout,
    spawn_notice,
};
use crate::report::{
    GroupAcc, group_report, single_report, truncate, truncate_chars, worker_headline,
};
use crate::config::Config;
use crate::llm::ModelOverride;
use crate::supervisor::SupervisorConfig;
use crate::worker::WorkerManager;

/// A planner call in flight: the task producing subtasks, plus everything the
/// resulting spawn needs — system prompt, the original request, and the model
/// the workers will run on.
type PlannedFanOut =
    (JoinHandle<crate::fanout::FanOutPlan>, String, String, Option<ModelOverride>);

/// Which channel a transcript line belongs to — drives its color/gutter.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    User,
    Assistant,
    Thinking,
    Tool,
    ToolResult,
    Diff,
    Notice,
    Error,
}

struct Item {
    kind: Kind,
    text: String,
}

/// How many lines of a long tool result to show before capping (Ctrl+O expands).
const TOOL_RESULT_PREVIEW_LINES: usize = 15;

/// Slash commands offered by Tab-completion.
const COMMANDS: &[&str] = &[
    "/help", "/new", "/compact", "/memory", "/knowledge", "/skill", "/spawn", "/agents",
    "/validate", "/fast", "/think", "/quit",
];

/// Active Tab-completion state (candidates for the current token).
struct Completion {
    candidates: Vec<String>,
    idx: usize,
    token_start: usize,
}

/// Max visible rows for the multi-line composer before it scrolls internally.
const MAX_INPUT_ROWS: usize = 8;

struct App {
    items: Vec<Item>,
    input: String,
    /// Cursor position as a char index into `input` (0..=char_count).
    cursor: usize,
    /// Submitted-prompt history and the current navigation position.
    history: Vec<String>,
    history_idx: Option<usize>,
    /// The in-progress line stashed while browsing history.
    draft: String,
    /// Lines scrolled up from the bottom; 0 = following the tail.
    scroll_up: u16,
    follow: bool,
    collapse_tools: bool,
    running: bool,
    model: String,
    context_limit: usize,
    last_prompt_tokens: u32,
    /// Reasoning tokens the last completion spent. The number that explains a
    /// long, silent step — without it, "thinking" is just an animation.
    last_reasoning_tokens: u32,
    /// Reasoning streamed so far in the current step, so the count climbs live
    /// instead of only appearing once the step is over.
    step_reasoning_chars: usize,
    /// `Some("length")` means the last completion was cut off mid-output.
    last_finish_reason: Option<String>,
    total_out_tokens: u64,
    validate_cmd: Option<String>,
    status: String,
    // index of the in-progress assistant / thinking item, for delta appends
    cur_assistant: Option<usize>,
    cur_thinking: Option<usize>,
    // Cached wrapped rows so scrolling doesn't rebuild the whole transcript.
    cached_rows: Vec<Line<'static>>,
    cache_width: u16,
    dirty: bool,
    // Tab-completion state for the composer.
    completion: Option<Completion>,
    // Cosmetic/among-turn state.
    show_thinking: bool,
    spinner: usize,
    turn_start: Option<std::time::Instant>,
    agents_running: usize,
    agents_queued: usize,
    /// Fast mode: the model answers without a reasoning pass.
    /// Thinking mode as shown in the footer: `off`, `on`, or a budget like `2k`.
    /// `None` means the provider's default, which we don't claim to know.
    think_label: Option<String>,
    /// `agents.fanout = "auto"` — bare `/spawn` may fan out on its own.
    fanout_auto: bool,
    /// `agents.synthesize` — after a fan-out group reports back, run a turn
    /// that combines their results into one answer.
    synthesize: bool,
    /// Set by `/spawn` when the fan-out needs a planner call; run_loop picks it
    /// up and runs it off the UI task.
    pending_fanout: Option<PendingFanOut>,
    /// Set by `/memory extract`; run_loop runs the classifier off the UI task.
    pending_extract: bool,
    /// Set by `/memory mine [n]`; run_loop does the model half off the UI task.
    /// Carries the cap on how many sessions to read in this run.
    pending_mine: Option<usize>,
}

impl App {
    fn new(model: String, context_limit: usize, validate_cmd: Option<String>) -> Self {
        App {
            items: Vec::new(),
            input: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_idx: None,
            draft: String::new(),
            scroll_up: 0,
            follow: true,
            collapse_tools: false,
            running: false,
            model,
            context_limit,
            last_prompt_tokens: 0,
            last_reasoning_tokens: 0,
            step_reasoning_chars: 0,
            last_finish_reason: None,
            total_out_tokens: 0,
            validate_cmd,
            status: "/help for keys and commands".into(),
            cur_assistant: None,
            cur_thinking: None,
            cached_rows: Vec::new(),
            cache_width: 0,
            dirty: true,
            completion: None,
            show_thinking: true,
            spinner: 0,
            turn_start: None,
            agents_running: 0,
            agents_queued: 0,
            think_label: None,
            fanout_auto: true,
            synthesize: true,
            pending_fanout: None,
            pending_extract: false,
            pending_mine: None,
        }
    }

    fn push(&mut self, kind: Kind, text: impl Into<String>) {
        self.items.push(Item { kind, text: text.into() });
        self.dirty = true;
    }

    /// Rebuild the wrapped-row cache only when content or width changed.
    fn ensure_rows(&mut self, width: u16) {
        if self.dirty || self.cache_width != width {
            self.cached_rows =
                build_rows(&self.items, self.collapse_tools, self.show_thinking, width);
            self.cache_width = width;
            self.dirty = false;
        }
    }

    // ---- composer (input editing) ----

    fn byte_at(&self, char_idx: usize) -> usize {
        self.input
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(self.input.len())
    }

    fn char_len(&self) -> usize {
        self.input.chars().count()
    }

    fn insert_str(&mut self, s: &str) {
        let at = self.byte_at(self.cursor);
        self.input.insert_str(at, s);
        self.cursor += s.chars().count();
        self.completion = None;
    }

    fn insert_char(&mut self, c: char) {
        let at = self.byte_at(self.cursor);
        self.input.insert(at, c);
        self.cursor += 1;
        self.completion = None;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = self.byte_at(self.cursor - 1);
        let end = self.byte_at(self.cursor);
        self.input.replace_range(start..end, "");
        self.cursor -= 1;
    }

    /// Delete the word (and preceding whitespace) before the cursor.
    fn delete_word(&mut self) {
        let mut i = self.cursor;
        let chars: Vec<char> = self.input.chars().collect();
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        let start = self.byte_at(i);
        let end = self.byte_at(self.cursor);
        self.input.replace_range(start..end, "");
        self.cursor = i;
    }

    fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn move_right(&mut self) {
        if self.cursor < self.char_len() {
            self.cursor += 1;
        }
    }

    /// Move to the start / end of the current logical line.
    fn move_home(&mut self) {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = self.cursor;
        while i > 0 && chars[i - 1] != '\n' {
            i -= 1;
        }
        self.cursor = i;
    }

    fn move_end(&mut self) {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = self.cursor;
        while i < chars.len() && chars[i] != '\n' {
            i += 1;
        }
        self.cursor = i;
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.history_idx = None;
        self.completion = None;
    }

    fn set_input(&mut self, text: String) {
        self.cursor = text.chars().count();
        self.input = text;
        self.history_idx = None;
        self.completion = None;
    }

    /// Take the composed input for submission, resetting the composer and
    /// recording history.
    fn take_input(&mut self) -> String {
        let text = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.history_idx = None;
        self.completion = None;
        let trimmed = text.trim();
        if !trimmed.is_empty() && self.history.last().map(String::as_str) != Some(trimmed) {
            self.history.push(trimmed.to_string());
        }
        text
    }

    /// Recall the previous / next history entry into the composer.
    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_idx {
            None => {
                self.draft = self.input.clone();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_idx = Some(next);
        self.input = self.history[next].clone();
        self.cursor = self.char_len();
        self.completion = None;
    }

    fn history_next(&mut self) {
        match self.history_idx {
            None => {}
            Some(i) if i + 1 < self.history.len() => {
                self.history_idx = Some(i + 1);
                self.input = self.history[i + 1].clone();
                self.cursor = self.char_len();
            }
            Some(_) => {
                // Past the newest entry → restore the stashed draft.
                self.history_idx = None;
                self.input = std::mem::take(&mut self.draft);
                self.cursor = self.char_len();
            }
        }
        self.completion = None;
    }

    /// Scroll toward older content.
    fn scroll_up(&mut self, n: u16) {
        self.follow = false;
        self.scroll_up = self.scroll_up.saturating_add(n);
    }

    /// Scroll toward the newest content; re-enable follow at the bottom.
    fn scroll_down(&mut self, n: u16) {
        self.scroll_up = self.scroll_up.saturating_sub(n);
        if self.scroll_up == 0 {
            self.follow = true;
        }
    }

    fn apply_event(&mut self, ev: Event) {
        match ev {
            Event::UserMessage { text } => {
                self.push(Kind::User, text);
                self.cur_assistant = None;
                self.cur_thinking = None;
            }
            Event::Thinking { text } => {
                self.step_reasoning_chars += text.len();
                match self.cur_thinking {
                    Some(i) => self.items[i].text.push_str(&text),
                    None => {
                        self.items.push(Item { kind: Kind::Thinking, text });
                        self.cur_thinking = Some(self.items.len() - 1);
                    }
                }
                self.dirty = true;
            }
            Event::MessageDelta { text } => {
                match self.cur_assistant {
                    Some(i) => self.items[i].text.push_str(&text),
                    None => {
                        self.items.push(Item { kind: Kind::Assistant, text });
                        self.cur_assistant = Some(self.items.len() - 1);
                    }
                }
                self.dirty = true;
            }
            Event::AssistantMessage { .. } => {} // already streamed via deltas
            Event::ToolCall { name, arguments, .. } => {
                self.push(Kind::Tool, tool_summary(&name, &arguments));
                self.cur_assistant = None;
                self.cur_thinking = None;
            }
            Event::ToolResult { ok, output, name, .. } => {
                // Successful edit/write results are unified diffs → render as such.
                if ok && matches!(name.as_str(), "edit" | "write") {
                    self.push(Kind::Diff, output);
                } else {
                    let prefix = if ok { "" } else { "[error] " };
                    self.push(Kind::ToolResult, format!("{prefix}{output}"));
                }
            }
            Event::Nudge { reason } => self.push(Kind::Notice, format!("↻ {reason}")),
            Event::Validation { ok, detail } => {
                if ok {
                    self.push(Kind::Notice, format!("✓ validation passed: {detail}"));
                } else {
                    self.push(Kind::Error, format!("✗ validation failed: {detail}"));
                }
            }
            Event::Compaction { messages_before, messages_after } => {
                self.push(
                    Kind::Notice,
                    format!("⟲ compacted context: {messages_before} → {messages_after} messages"),
                );
            }
            Event::Usage {
                prompt_tokens,
                completion_tokens,
                reasoning_tokens,
                finish_reason,
                ..
            } => {
                self.last_prompt_tokens = prompt_tokens;
                self.total_out_tokens += completion_tokens as u64;
                self.last_reasoning_tokens = reasoning_tokens;
                self.step_reasoning_chars = 0;
                self.last_finish_reason = finish_reason;
            }
            Event::Warning { message } => self.push(Kind::Notice, format!("⚠ {message}")),
            Event::Error { message } => self.push(Kind::Error, message),
            Event::SessionStarted { .. } | Event::TurnComplete { .. } => {}
        }
    }
}

/// Run the TUI to completion (until the user quits).
#[allow(clippy::too_many_arguments)]
pub async fn run_tui(
    agent: Agent,
    session: Session,
    bus: EventBus,
    cwd: PathBuf,
    model: String,
    validate_cmd: Option<String>,
    bash_timeout: Duration,
    context_limit: usize,
    agents_max: usize,
    supervisor: SupervisorConfig,
    fanout_auto: bool,
    synthesize: bool,
    config: Config,
) -> Result<()> {
    let mut terminal = setup_terminal()?;
    let res = run_loop(
        &mut terminal,
        agent,
        session,
        bus,
        cwd,
        model,
        validate_cmd,
        bash_timeout,
        context_limit,
        agents_max,
        supervisor,
        fanout_auto,
        synthesize,
        config,
    )
    .await;
    restore_terminal(&mut terminal)?;
    res
}

type Term = Terminal<CrosstermBackend<Stdout>>;

fn setup_terminal() -> Result<Term> {
    enable_raw_mode().context("enabling raw mode")?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste)
        .context("entering alternate screen")?;
    Terminal::new(CrosstermBackend::new(out)).context("creating terminal")
}

fn restore_terminal(terminal: &mut Term) -> Result<()> {
    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    )
    .ok();
    terminal.show_cursor().ok();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_loop(
    terminal: &mut Term,
    agent: Agent,
    session: Session,
    bus: EventBus,
    cwd: PathBuf,
    model: String,
    validate_cmd: Option<String>,
    bash_timeout: Duration,
    context_limit: usize,
    agents_max: usize,
    supervisor: SupervisorConfig,
    fanout_auto: bool,
    synthesize: bool,
    config: Config,
) -> Result<()> {
    let agent = Arc::new(agent);
    let session = Arc::new(AsyncMutex::new(session));
    let mem = MemoryStore::open(Some(&cwd)).or_else(|_| MemoryStore::open(None))?;
    // Workers may run on a cheaper model than the session (`agents.model`).
    let worker_model = match config.agents_model() {
        Some(spec) => match ModelOverride::resolve(&config, spec) {
            Ok(m) => Some(m),
            Err(e) => {
                eprintln!("agents.model `{spec}` is unusable: {e:#}");
                None
            }
        },
        None => None,
    };
    let mut workers = WorkerManager::new(agent.clone(), cwd.clone(), agents_max)
        .with_supervisor(supervisor)
        .with_default_model(worker_model);

    let mut app = App::new(model, context_limit, validate_cmd);
    app.fanout_auto = fanout_auto;
    app.think_label = agent.thinking_mode().label();
    app.synthesize = synthesize;
    let mut bus_rx = bus.subscribe();
    // Option so we can drop the input reader while an external editor owns the tty.
    let mut events: Option<EventStream> = Some(EventStream::new());
    let mut ticker = tokio::time::interval(Duration::from_millis(120));
    let mut pending_edit = false;

    // Startup header.
    app.push(Kind::Notice, format!("worksmith · {}", app.model));
    app.push(Kind::Notice, format!("cwd: {}", cwd.display()));
    if !crate::config::load_project_instructions(&cwd).trim().is_empty() {
        app.push(Kind::Notice, "loaded project instructions (AGENTS.md/CLAUDE.md)".to_string());
    }
    if let Some(c) = app.validate_cmd.clone() {
        app.push(Kind::Notice, format!("validation: {c}"));
    }
    if !workers.supervisor_config().is_on() {
        app.push(Kind::Notice, "supervisor: off (workers run unwatched)".to_string());
    }
    app.push(
        Kind::Notice,
        "Tab complete · Ctrl+O tools · Ctrl+T thinking · Esc abort · /help · /quit".to_string(),
    );

    let mut turn: Option<JoinHandle<Result<TurnResult>>> = None;
    let mut cancel = CancellationToken::new();
    // A planner call in flight, with the system prompt its workers will use.
    let mut fanout: Option<PlannedFanOut> = None;
    // Fan-out groups still collecting their members' results.
    let mut groups: Vec<GroupAcc> = Vec::new();
    // A memory-extraction classifier call in flight.
    let mut extract: Option<JoinHandle<Result<String, String>>> = None;
    // A mining run in flight: the model half only — the proposals are filed back
    // on this task, where the memory store lives.
    type MineResults = (Vec<(String, Result<String, String>)>, crate::mining::MineReport);
    let mut mine: Option<JoinHandle<MineResults>> = None;

    loop {
        // Start queued workers whose slot just freed.
        for id in workers.pump() {
            app.push(Kind::Notice, format!("started {id} (from the queue)"));
        }

        // Surface any workers that just finished (so you don't have to poll).
        for w in workers.take_newly_finished() {
            app.push(Kind::Notice, worker_headline(&w));
            if app.follow {
                app.scroll_up = 0;
            }

            // A grouped worker waits for its siblings so the parent gets one
            // combined report instead of N disconnected ones.
            match w.group.and_then(|g| workers.group_info(g).map(|(r, t)| (g, r.to_string(), t))) {
                Some((group, request, total)) => {
                    let acc = match groups.iter_mut().find(|a| a.group == group) {
                        Some(a) => a,
                        None => {
                            groups.push(GroupAcc {
                                group,
                                request,
                                total,
                                done: Vec::new(),
                            });
                            groups.last_mut().unwrap()
                        }
                    };
                    acc.done.push(w);
                    if acc.done.len() < acc.total {
                        continue;
                    }
                    let acc = groups.swap_remove(
                        groups.iter().position(|a| a.group == group).unwrap(),
                    );
                    let report = group_report(&acc);
                    app.push(Kind::Notice, format!("all {} workers finished", acc.done.len()));
                    deliver_to_parent(&app, &agent, &session, report).await;
                    // Ask the parent to turn the pieces into one answer.
                    if app.synthesize && turn.is_none() {
                        let ask = format!(
                            "Your {} background workers just reported back (above). Combine \
                             their results into one answer to the original request: {}",
                            acc.done.len(),
                            acc.request
                        );
                        start_turn(
                            ask, &mut app, &agent, &session, &mem, &cwd, bash_timeout,
                            &mut turn, &mut cancel,
                        );
                    }
                }
                None => {
                    let report = single_report(&w);
                    deliver_to_parent(&app, &agent, &session, report).await;
                }
            }
        }

        // Rebuild the wrapped-row cache only if content/width changed, then draw.
        app.agents_running = workers.running_count();
        app.agents_queued = workers.queued_count();
        let width = terminal.size().map(|s| s.width).unwrap_or(80);
        app.ensure_rows(width);
        terminal.draw(|f| ui(f, &app))?;

        tokio::select! {
            // Terminal input.
            maybe_ev = events.as_mut().unwrap().next(), if events.is_some() => {
                match maybe_ev {
                    Some(Ok(CEvent::Key(key))) => {
                        match handle_key(key, &mut app, &agent, &session, &mem, &cwd,
                                         bash_timeout, &mut turn, &mut cancel, &mut workers,
                                         &config).await? {
                            Flow::Quit => break,
                            Flow::Continue => {}
                            Flow::ExternalEdit => pending_edit = true,
                        }
                        // /memory extract: classify the transcript off the UI task.
                        if app.pending_extract {
                            app.pending_extract = false;
                            if extract.is_some() || app.running {
                                app.push(
                                    Kind::Notice,
                                    "busy — try /memory extract once the turn finishes"
                                        .to_string(),
                                );
                            } else {
                                let transcript = {
                                    let s = session.lock().await;
                                    render_recent(&s, 40)
                                };
                                if transcript.trim().is_empty() {
                                    app.push(Kind::Notice, "(nothing to distill yet)".to_string());
                                } else {
                                    app.status = "distilling memories…".into();
                                    let a = agent.clone();
                                    extract = Some(tokio::spawn(async move {
                                        // A failed extraction must not read as
                                        // "nothing worth saving" — that is how an
                                        // empty memory store looks healthy.
                                        a.ask(crate::memory::EXTRACTION_PROMPT, &transcript, 512)
                                            .await
                                            .map_err(|e| e.to_string())
                                    }));
                                }
                            }
                        }

                        // /memory mine: pick the sessions here (the store can't
                        // cross a task boundary), classify them off the UI task.
                        if let Some(limit) = app.pending_mine.take() {
                            if mine.is_some() || extract.is_some() || app.running {
                                app.push(
                                    Kind::Notice,
                                    "busy — try /memory mine once the turn finishes".to_string(),
                                );
                            } else {
                                match crate::mining::plan(&mem, &cwd, limit) {
                                    Err(e) => app.push(Kind::Error, format!("mine failed: {e}")),
                                    Ok(p) if p.items.is_empty() => {
                                        app.push(Kind::Notice, p.report.summary())
                                    }
                                    Ok(p) => {
                                        app.status =
                                            format!("mining {} sessions…", p.items.len());
                                        let a = agent.clone();
                                        let report = p.report.clone();
                                        let items = p.items;
                                        mine = Some(tokio::spawn(async move {
                                            let results =
                                                crate::mining::classify(&a, &items, |_, _| {})
                                                    .await;
                                            (results, report)
                                        }));
                                    }
                                }
                            }
                        }

                        // /spawn asked for a planned fan-out: run the model call
                        // off this task so the UI keeps drawing.
                        if let Some(pf) = app.pending_fanout.take() {
                            let a = agent.clone();
                            let max = agents_max;
                            let request = pf.task.clone();
                            fanout = Some((
                                tokio::spawn(async move {
                                    plan_fanout(a, pf.task, pf.want, max).await
                                }),
                                pf.system,
                                request,
                                pf.model,
                            ));
                        }
                    }
                    Some(Ok(CEvent::Mouse(m))) => match m.kind {
                        MouseEventKind::ScrollUp => app.scroll_up(3),
                        MouseEventKind::ScrollDown => app.scroll_down(3),
                        _ => {}
                    },
                    // Bracketed paste: insert the whole payload at the cursor
                    // (multi-line and all) instead of firing Enter per line.
                    Some(Ok(CEvent::Paste(text))) => app.insert_str(&text),
                    Some(Ok(_)) => {} // resize etc — redraw next loop
                    Some(Err(_)) | None => break,
                }
            }

            // Agent events → transcript.
            ev = bus_rx.recv() => {
                match ev {
                    Ok(e) => {
                        app.apply_event(e);
                        if app.follow { app.scroll_up = 0; }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                }
            }

            // Memory extraction finished.
            res = async { (&mut extract.as_mut().unwrap()).await }, if extract.is_some() => {
                extract = None;
                app.status = "/help for keys and commands".into();
                match res {
                    Ok(Ok(text)) => {
                        let candidates = crate::memory::parse_candidates(&text);
                        if candidates.is_empty() {
                            app.push(Kind::Notice, "nothing worth remembering".to_string());
                        }
                        for c in candidates {
                            match mem.remember_deduped(
                                c.scope, &c.kind, &c.subject, &c.content, c.importance,
                            ) {
                                Ok((row, true)) => app.push(
                                    Kind::Notice,
                                    format!(
                                        "remembered {} [{}/{}] {}: {}",
                                        row.id, row.scope, row.kind, row.subject, row.content
                                    ),
                                ),
                                Ok((row, false)) => app.push(
                                    Kind::Notice,
                                    format!("already known: {}: {}", row.subject, row.content),
                                ),
                                Err(e) => app.push(Kind::Error, format!("memory error: {e}")),
                            }
                        }
                    }
                    Ok(Err(e)) => app.push(Kind::Error, format!("extraction failed: {e}")),
                    Err(e) => app.push(Kind::Error, format!("extraction task failed: {e}")),
                }
                if app.follow { app.scroll_up = 0; }
            }

            // Mining finished: file the proposals from here, where the store is.
            res = async { (&mut mine.as_mut().unwrap()).await }, if mine.is_some() => {
                mine = None;
                app.status = "/help for keys and commands".into();
                match res {
                    Ok((results, report)) => {
                        let report = crate::mining::record(&mem, results, report);
                        app.push(Kind::Notice, report.summary());
                        for f in &report.failed {
                            app.push(Kind::Error, format!("mine: {f}"));
                        }
                    }
                    Err(e) => app.push(Kind::Error, format!("mining task failed: {e}")),
                }
                if app.follow { app.scroll_up = 0; }
            }

            // Fan-out planning finished.
            res = async { (&mut fanout.as_mut().unwrap().0).await }, if fanout.is_some() => {
                let (_, system, request, over) = fanout.take().unwrap();
                app.status = "/help for keys and commands".into();
                match res {
                    Ok(plan) if plan.tasks.is_empty() => {
                        app.push(Kind::Error, "fan-out planning produced no tasks".to_string());
                    }
                    Ok(plan) => {
                        // Say how these tasks were arrived at, so a fan-out that
                        // looks wrong can be diagnosed without a rebuild.
                        app.push(Kind::Notice, plan.note.clone());
                        if plan.tasks.len() > 1 {
                            for (i, t) in plan.tasks.iter().enumerate() {
                                app.push(Kind::Notice, format!("  {}. {}", i + 1, truncate(t, 100)));
                            }
                        }
                        let report = workers.spawn_many_on(plan.tasks, system, request, over);
                        app.push(Kind::Notice, fanout_notice(&report));
                    }
                    Err(e) => app.push(Kind::Error, format!("fan-out planning failed: {e}")),
                }
                if app.follow { app.scroll_up = 0; }
            }

            // Turn finished.
            res = async { turn.as_mut().unwrap().await }, if turn.is_some() => {
                turn = None;
                app.running = false;
                app.turn_start = None;
                match res {
                    Ok(Ok(r)) => app.status = format!("[{}]", r.outcome.label()),
                    Ok(Err(e)) => app.push(Kind::Error, format!("turn error: {e:#}")),
                    Err(_) => app.push(Kind::Error, "turn task failed".to_string()),
                }
            }

            // Spinner animation while a turn runs.
            _ = ticker.tick() => {
                if app.running {
                    app.spinner = app.spinner.wrapping_add(1);
                }
            }
        }

        // Ctrl+G: suspend the TUI, edit the composer in $EDITOR, resume.
        if pending_edit {
            pending_edit = false;
            restore_terminal(terminal).ok();
            drop(events.take()); // stop the input reader so the editor owns the tty
            let edited = external_edit(&app.input);
            *terminal = setup_terminal()?;
            events = Some(EventStream::new());
            terminal.clear().ok();
            app.dirty = true;
            if let Some(text) = edited {
                app.set_input(text);
                app.status = "loaded from editor".into();
            }
        }
    }

    // If a turn is still running on quit, cancel and let it wind down.
    cancel.cancel();
    if let Some(t) = turn.take() {
        let _ = t.await;
    }
    Ok(())
}

enum Flow {
    Continue,
    Quit,
    /// Suspend the TUI and open the composer in `$EDITOR`.
    ExternalEdit,
}

#[allow(clippy::too_many_arguments)]
async fn handle_key(
    key: KeyEvent,
    app: &mut App,
    agent: &Arc<Agent>,
    session: &Arc<AsyncMutex<Session>>,
    mem: &MemoryStore,
    cwd: &Path,
    bash_timeout: Duration,
    turn: &mut Option<JoinHandle<Result<TurnResult>>>,
    cancel: &mut CancellationToken,
    workers: &mut WorkerManager,
    config: &Config,
) -> Result<Flow> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    // Any key other than Tab ends an in-progress completion cycle.
    if key.code != KeyCode::Tab {
        app.completion = None;
    }

    match key.code {
        KeyCode::Char('c') if ctrl => return Ok(Flow::Quit),
        KeyCode::Tab => complete(app, cwd),
        KeyCode::Char('o') if ctrl => {
            app.collapse_tools = !app.collapse_tools;
            app.dirty = true;
            app.status = format!("tool output {}", if app.collapse_tools { "collapsed" } else { "expanded" });
        }
        KeyCode::Char('t') if ctrl => {
            app.show_thinking = !app.show_thinking;
            app.dirty = true;
            app.status = format!("thinking {}", if app.show_thinking { "shown" } else { "hidden" });
        }
        KeyCode::Char('p') if ctrl => {
            app.status = "model cycling: configure multiple models (coming soon)".into();
        }
        KeyCode::Char('g') if ctrl => return Ok(Flow::ExternalEdit),
        KeyCode::Esc => {
            if app.running {
                cancel.cancel();
                app.status = "aborting…".into();
            } else {
                app.clear_input();
            }
        }
        // Transcript scrolling (mouse wheel also works).
        KeyCode::PageUp => app.scroll_up(10),
        KeyCode::PageDown => app.scroll_down(10),
        KeyCode::Char('u') if ctrl => app.scroll_up(10),
        KeyCode::Char('d') if ctrl => app.scroll_down(10),
        // Composer editing.
        KeyCode::Up => app.history_prev(),
        KeyCode::Down => app.history_next(),
        KeyCode::Left => app.move_left(),
        KeyCode::Right => app.move_right(),
        KeyCode::Home => app.move_home(),
        KeyCode::End => app.move_end(),
        KeyCode::Char('w') if ctrl => app.delete_word(),
        KeyCode::Backspace => app.backspace(),
        // Alt/Shift+Enter inserts a newline; plain Enter sends.
        KeyCode::Enter if key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) => {
            app.insert_char('\n');
        }
        KeyCode::Enter => {
            let raw = app.take_input();
            let input = raw.trim().to_string();
            if input.is_empty() {
                return Ok(Flow::Continue);
            }

            // Commands (start with '/', or bare quit/exit).
            if input == "/quit" || input == "/exit" || input == "quit" || input == "exit" {
                return Ok(Flow::Quit);
            }
            if input.starts_with('/')
                && handle_command(&input, app, agent, session, mem, cwd, workers, config).await?
            {
                return Ok(Flow::Continue);
            }

            if app.running {
                app.status = "a turn is already running (Esc to abort)".into();
                return Ok(Flow::Continue);
            }

            // Start a turn.
            let message = expand_file_mentions(&input, cwd);
            start_turn(message, app, agent, session, mem, cwd, bash_timeout, turn, cancel);
        }
        // Ignore control-chords; accept normal (and shifted) chars at the cursor.
        KeyCode::Char(c) if !ctrl => app.insert_char(c),
        _ => {}
    }
    Ok(Flow::Continue)
}

/// Returns true if the input was a recognized command (already handled).
#[allow(clippy::too_many_arguments)]
async fn handle_command(
    input: &str,
    app: &mut App,
    agent: &Arc<Agent>,
    session: &Arc<AsyncMutex<Session>>,
    mem: &MemoryStore,
    cwd: &Path,
    workers: &mut WorkerManager,
    config: &Config,
) -> Result<bool> {
    let mut parts = input.trim_start_matches('/').split_whitespace();
    let head = parts.next().unwrap_or("");
    match head {
        "help" | "h" => {
            app.push(
                Kind::Notice,
                "keys: Enter=send  Alt+Enter=newline  Ctrl+G=$EDITOR  Esc=abort/clear  \
                 Ctrl+C=quit  Ctrl+O=tools  Ctrl+T=thinking  Tab=complete\n\
                 commands: /new /compact /memory [search|extract|mine|pending|approve] \
                 /knowledge [index|search] /skill [name] /validate <cmd|off> \
                 /fast [on|off|auto] \
                 /think [on|off|auto|<tokens>] \
                 /spawn [-n N | --each-files <regex>] [--model <spec>] <task> \
                 /agents [kill|show|nudge <id> | drop-queued] /quit   @path includes a file"
                    .to_string(),
            );
        }
        "new" => {
            let mut s = session.lock().await;
            *s = Session::create(cwd)?;
            app.items.clear();
            app.cur_assistant = None;
            app.cur_thinking = None;
            app.dirty = true;
            app.push(Kind::Notice, format!("started new session {}", s.id));
        }
        "compact" => {
            if app.running {
                app.status = "can't compact while a turn is running".into();
            } else {
                let mut s = session.lock().await;
                match agent.compact(&mut s).await {
                    Ok(()) => app.status = "compacted".into(),
                    Err(e) => app.push(Kind::Error, format!("compaction error: {e:#}")),
                }
            }
        }
        "memory" | "mem" => memory_command(app, mem, parts),
        "spawn" => {
            let args = input.trim_start_matches('/')[head.len()..].trim();
            match parse_spawn(args, app.fanout_auto) {
                Err(msg) => app.push(Kind::Notice, msg),
                Ok(req) => {
                    let system = build_worker_prompt(cwd, mem);
                    // `--model` overrides `agents.model` for this spawn only.
                    let over = match req.model.as_deref() {
                        Some(spec) => match ModelOverride::resolve(config, spec) {
                            Ok(m) => Some(m),
                            Err(e) => {
                                app.push(Kind::Error, format!("--model `{spec}`: {e:#}"));
                                return Ok(true);
                            }
                        },
                        None => None,
                    };
                    match req.fanout {
                        // Planner-driven: hand off to the caller, which runs it
                        // off the UI thread (a model call would freeze the TUI).
                        FanOut::Auto | FanOut::Count(_) if !matches!(req.fanout, FanOut::Count(1)) => {
                            let want = match req.fanout {
                                FanOut::Count(n) => Some(n),
                                _ => None,
                            };
                            app.status = "planning fan-out…".into();
                            app.pending_fanout =
                                Some(PendingFanOut { task: req.task, want, system, model: over });
                        }
                        FanOut::Files(pattern) => {
                            match matching_files(cwd, &pattern) {
                                Err(e) => app.push(Kind::Error, e),
                                Ok(files) if files.is_empty() => {
                                    app.push(Kind::Notice, format!("no files match `{pattern}`"));
                                }
                                Ok(files) => {
                                    let tasks: Vec<String> =
                                        files.iter().map(|f| assign(&req.task, f)).collect();
                                    let report = workers.spawn_many_on(
                                        tasks, system, req.task.clone(), over,
                                    );
                                    app.push(Kind::Notice, fanout_notice(&report));
                                }
                            }
                        }
                        // -n 1 (or an explicit single): today's path, no planner.
                        _ => match workers.spawn_on(req.task.clone(), system, over) {
                            Ok(outcome) => app.push(Kind::Notice, spawn_notice(&outcome, &req.task)),
                            Err(e) => app.push(Kind::Error, format!("spawn failed: {e}")),
                        },
                    }
                }
            }
        }
        "agents" | "workers" => agents_command(app, workers, parts),
        "knowledge" | "know" => knowledge_command(app, cwd, parts),
        "skill" | "skills" => skill_command(app, cwd, parts),
        "fast" | "lucky" => {
            let mode = agent.thinking_mode();
            let rest: Vec<&str> = parts.collect();
            match rest.first().copied() {
                Some("on") => mode.set(Some(Thinking::Off)),
                Some("off") => mode.set(Some(Thinking::On)),
                Some("auto") => mode.set(None),
                Some(other) => {
                    app.push(Kind::Error, format!("usage: /fast [on|off|auto] (got {other})"));
                    return Ok(true);
                }
                None => {
                    mode.toggle_fast();
                }
            }
            app.think_label = mode.label();
            let msg = match mode.get() {
                Some(Thinking::Off) => "fast mode on — answering without thinking first".to_string(),
                Some(Thinking::On) => "fast mode off — thinking before answering".to_string(),
                Some(Thinking::Budget(n)) => format!("thinking capped at {n} tokens"),
                None => "thinking left to the provider's default".to_string(),
            };
            app.push(Kind::Notice, msg);
        }
        "think" => {
            let mode = agent.thinking_mode();
            let rest: Vec<&str> = parts.collect();
            // A budget is the setting between "as long as it likes" and "not at
            // all": the reasoning gets its own cap, so it can't eat the whole
            // output budget and leave nothing for an answer.
            let set = match rest.first().copied() {
                None | Some("on") => Some(Thinking::On),
                Some("off") => Some(Thinking::Off),
                Some("auto") => None,
                Some(n) => match parse_budget(n) {
                    Some(n) => Some(Thinking::Budget(n)),
                    None => {
                        app.push(
                            Kind::Error,
                            format!("usage: /think [on|off|auto|<tokens>] (got {n})"),
                        );
                        return Ok(true);
                    }
                },
            };
            mode.set(set);
            app.think_label = mode.label();
            let msg = match set {
                Some(Thinking::Off) => "thinking off — answering directly".to_string(),
                Some(Thinking::On) => "thinking on, uncapped".to_string(),
                Some(Thinking::Budget(n)) => format!(
                    "thinking capped at {n} tokens — the rest of max-tokens is left for the answer"
                ),
                None => "thinking left to the provider's default".to_string(),
            };
            app.push(Kind::Notice, msg);
        }
        "validate" => {
            let rest: Vec<&str> = parts.collect();
            let rest = rest.join(" ");
            match rest.as_str() {
                "" => {
                    let cur = app.validate_cmd.clone().unwrap_or_else(|| "(none)".into());
                    app.push(Kind::Notice, format!("validation: {cur}"));
                }
                "off" | "none" => {
                    app.validate_cmd = None;
                    app.push(Kind::Notice, "validation cleared".to_string());
                }
                cmd => {
                    app.validate_cmd = Some(cmd.to_string());
                    app.push(Kind::Notice, format!("validation: `{cmd}`"));
                }
            }
        }
        _ => {
            app.push(Kind::Error, format!("unknown command: /{head}"));
        }
    }
    Ok(true)
}

/// `/memory [list|global|project | show <id> | forget <id> | add <scope> <kind> <subject> <content…>]`
fn memory_command<'a>(app: &mut App, mem: &MemoryStore, mut parts: impl Iterator<Item = &'a str>) {
    let sub = parts.next().unwrap_or("list");
    match sub {
        "list" | "" => memory_list(app, mem, None),
        "global" => memory_list(app, mem, Some(Scope::Global)),
        "project" => memory_list(app, mem, Some(Scope::Project)),
        "show" => match parts.next() {
            Some(id) => match mem.get(id) {
                Ok(Some(r)) => app.push(
                    Kind::Notice,
                    format!(
                        "[{}/{}] {} (importance {}, {})\n{}",
                        r.scope, r.kind, r.subject, r.importance, r.status, r.content
                    ),
                ),
                Ok(None) => app.push(Kind::Notice, format!("(no memory {id})")),
                Err(e) => app.push(Kind::Error, format!("memory error: {e}")),
            },
            None => app.push(Kind::Notice, "usage: /memory show <id>".to_string()),
        },
        "forget" => match parts.next() {
            Some(id) => match mem.forget(id) {
                Ok(true) => app.push(Kind::Notice, format!("forgot {id}")),
                Ok(false) => app.push(Kind::Notice, format!("(no memory {id})")),
                Err(e) => app.push(Kind::Error, format!("memory error: {e}")),
            },
            None => app.push(Kind::Notice, "usage: /memory forget <id>".to_string()),
        },
        "search" | "find" => {
            let query = parts.collect::<Vec<_>>().join(" ");
            if query.trim().is_empty() {
                app.push(Kind::Notice, "usage: /memory search <query>".to_string());
            } else {
                match mem.search(&query, 10) {
                    Ok(hits) if hits.is_empty() => {
                        app.push(Kind::Notice, format!("(nothing remembered about \"{query}\")"))
                    }
                    Ok(hits) => {
                        for h in hits {
                            app.push(
                                Kind::Notice,
                                format!(
                                    "{:.2}  {}  [{}/{}] {}: {}",
                                    h.score, h.row.id, h.row.scope, h.row.kind, h.row.subject,
                                    h.row.content
                                ),
                            );
                        }
                    }
                    Err(e) => app.push(Kind::Error, format!("memory error: {e}")),
                }
            }
        }
        "extract" | "distill" => {
            // Signals the caller to run the classifier off the UI task.
            app.pending_extract = true;
        }
        "mine" => {
            // Default to a small bite: each session read is one model call, and
            // an archive of a thousand should not be one blocking command.
            let limit = parts.next().and_then(|n| n.parse::<usize>().ok()).unwrap_or(10);
            if limit == 0 {
                app.push(Kind::Error, "usage: /memory mine [sessions]".to_string());
            } else {
                app.pending_mine = Some(limit);
            }
        }
        "pending" | "proposed" => match mem.pending() {
            Ok(rows) if rows.is_empty() => {
                app.push(Kind::Notice, "(no proposals from workers)".to_string())
            }
            Ok(rows) => {
                for r in rows {
                    app.push(
                        Kind::Notice,
                        format!(
                            "{}  [{}/{}] {}: {}  (/memory approve {} | /memory forget {})",
                            r.id, r.scope, r.kind, r.subject, r.content, r.id, r.id
                        ),
                    );
                }
            }
            Err(e) => app.push(Kind::Error, format!("memory error: {e}")),
        },
        "approve" => match parts.next() {
            Some(id) => match mem.approve(id) {
                Ok(true) => app.push(Kind::Notice, format!("approved {id}")),
                Ok(false) => app.push(Kind::Notice, format!("(no pending proposal {id})")),
                Err(e) => app.push(Kind::Error, format!("memory error: {e}")),
            },
            None => app.push(Kind::Notice, "usage: /memory approve <id>".to_string()),
        },
        "add" => {
            let scope = parts.next().and_then(Scope::parse);
            let kind = parts.next().map(str::to_string);
            let subject = parts.next().map(str::to_string);
            let content = parts.collect::<Vec<_>>().join(" ");
            match (scope, kind, subject) {
                (Some(scope), Some(kind), Some(subject)) if !content.is_empty() => {
                    match mem.remember(scope, &kind, &subject, &content, 60) {
                        Ok(r) => app.push(Kind::Notice, format!("remembered {} [{}] {}", r.id, r.kind, r.subject)),
                        Err(e) => app.push(Kind::Error, format!("memory error: {e}")),
                    }
                }
                _ => app.push(
                    Kind::Notice,
                    "usage: /memory add <global|project> <decision|constraint|preference|fact|lesson> <subject> <content…>"
                        .to_string(),
                ),
            }
        }
        other => app.push(Kind::Error, format!("unknown /memory subcommand: {other}")),
    }
}

fn memory_list(app: &mut App, mem: &MemoryStore, scope: Option<Scope>) {
    match mem.list(scope) {
        Ok(rows) if rows.is_empty() => app.push(Kind::Notice, "(no memories)".to_string()),
        Ok(rows) => {
            for r in rows {
                app.push(
                    Kind::Notice,
                    format!("{}  [{}/{}] {}: {}", r.id, r.scope, r.kind, r.subject, r.content),
                );
            }
        }
        Err(e) => app.push(Kind::Error, format!("memory error: {e}")),
    }
}

/// Put a worker report where the parent model will actually read it: into a
/// running turn via the steering mailbox, or into the session history for the
/// next one. Without this the parent never learns what its workers did.
async fn deliver_to_parent(
    app: &App,
    agent: &Arc<Agent>,
    session: &Arc<AsyncMutex<Session>>,
    report: String,
) {
    if !app.running
        && let Ok(mut s) = session.try_lock()
        && s.append_message(crate::llm::Message::user(report.clone())).is_ok()
    {
        return;
    }
    // A turn owns the session while it runs; steering lands at its next step.
    agent.steering().push(report);
}

/// Kick off a user turn as a background task. Shared by typed input and by the
/// synthesis turn that runs when a fan-out group reports back.
#[allow(clippy::too_many_arguments)]
fn start_turn(
    message: String,
    app: &mut App,
    agent: &Arc<Agent>,
    session: &Arc<AsyncMutex<Session>>,
    mem: &MemoryStore,
    cwd: &Path,
    bash_timeout: Duration,
    turn: &mut Option<JoinHandle<Result<TurnResult>>>,
    cancel: &mut CancellationToken,
) {
    let sys = build_system_prompt(cwd, mem);
    *cancel = CancellationToken::new();
    let a = agent.clone();
    let s = session.clone();
    let tok = cancel.clone();
    let cmd = app.validate_cmd.clone();
    let cwd2 = cwd.to_path_buf();
    app.running = true;
    app.turn_start = Some(std::time::Instant::now());
    app.status = "working (Esc aborts)".into();
    app.follow = true;
    app.scroll_up = 0;
    *turn = Some(tokio::spawn(async move {
        let validator = cmd.map(|c| CommandValidator::new(c, cwd2.clone(), bash_timeout));
        let mut sess = s.lock().await;
        a.run_turn(&mut sess, &message, &sys, validator.as_ref().map(|v| v as _), tok).await
    }));
}

/// Render the tail of a session as plain text for the memory classifier. Tool
/// *calls* are named but their output is dropped — tool results are exactly the
/// bulk that must never become durable memory (`worksmith-memory-v1.md` §11).
fn render_recent(session: &Session, max_messages: usize) -> String {
    use crate::llm::Role;
    let msgs = session.messages();
    let start = msgs.len().saturating_sub(max_messages);
    let mut out = String::new();
    for m in &msgs[start..] {
        let role = match m.role {
            Role::System => continue,
            Role::Tool => {
                continue;
            }
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        if let Some(c) = &m.content
            && !c.trim().is_empty()
        {
            out.push_str(&format!("[{role}] {}\n", truncate_chars(c, 2_000)));
        }
        for tc in &m.tool_calls {
            out.push_str(&format!("[{role} called {}]\n", tc.name));
        }
    }
    out
}

/// `/skill [name]` — list installed skills, or load one into the transcript so
/// it applies to the rest of the session without waiting for the model to ask.
fn skill_command<'a>(app: &mut App, cwd: &Path, mut parts: impl Iterator<Item = &'a str>) {
    let catalog = crate::skill::SkillCatalog::discover(cwd);
    match parts.next() {
        None => {
            if catalog.is_empty() {
                app.push(
                    Kind::Notice,
                    "(no skills — add one under .worksmith/skills/<name>/SKILL.md, or \
                     ~/.claude/skills/ to share it with other tools)"
                        .to_string(),
                );
            }
            for s in catalog.skills() {
                app.push(Kind::Notice, format!("{}: {}", s.name, s.description));
            }
            for note in catalog.notes() {
                app.push(Kind::Notice, note.clone());
            }
        }
        Some(name) => match catalog.get(name) {
            Some(skill) => match skill.body() {
                Ok(body) => app.push(
                    Kind::Notice,
                    format!("skill `{}` ({})\n\n{}", skill.name, skill.dir.display(), body.trim()),
                ),
                Err(e) => app.push(Kind::Error, format!("could not read `{name}`: {e}")),
            },
            None => app.push(Kind::Error, format!("no skill named `{name}`")),
        },
    }
}

/// `/knowledge [index | search <query> | status]` — the project's own text,
/// chunked and searchable. Rebuildable, so `index` is always safe to re-run.
fn knowledge_command<'a>(
    app: &mut App,
    cwd: &Path,
    mut parts: impl Iterator<Item = &'a str>,
) {
    let store = match crate::knowledge::KnowledgeStore::open(cwd) {
        Ok(s) => s,
        Err(e) => {
            app.push(Kind::Error, format!("knowledge unavailable: {e}"));
            return;
        }
    };
    match parts.next().unwrap_or("status") {
        "index" | "reindex" => match store.index() {
            Ok(stats) => {
                let pruned = store.prune().unwrap_or(0);
                app.push(
                    Kind::Notice,
                    format!(
                        "indexed {} file(s) → {} chunk(s) · {} unchanged · {} stale removed",
                        stats.files, stats.chunks, stats.skipped_unchanged, pruned
                    ),
                );
            }
            Err(e) => app.push(Kind::Error, format!("indexing failed: {e}")),
        },
        "search" | "find" => {
            let query = parts.collect::<Vec<_>>().join(" ");
            if query.trim().is_empty() {
                app.push(Kind::Notice, "usage: /knowledge search <query>".to_string());
                return;
            }
            match store.search(&query, 5) {
                Ok(hits) if hits.is_empty() => {
                    app.push(Kind::Notice, "(no matches — try /knowledge index)".to_string())
                }
                Ok(hits) => {
                    for h in hits {
                        app.push(
                            Kind::Notice,
                            format!("{} (chunk {})\n{}", h.source, h.ord, truncate(&h.text, 300)),
                        );
                    }
                }
                Err(e) => app.push(Kind::Error, format!("knowledge search failed: {e}")),
            }
        }
        "status" | "" => match store.chunk_count() {
            Ok(n) => app.push(Kind::Notice, format!("knowledge index: {n} chunk(s)")),
            Err(e) => app.push(Kind::Error, format!("knowledge error: {e}")),
        },
        other => app.push(Kind::Error, format!("unknown /knowledge subcommand: {other}")),
    }
}

/// `/agents [list | kill <id> | show <id>]`
fn agents_command<'a>(
    app: &mut App,
    workers: &mut WorkerManager,
    mut parts: impl Iterator<Item = &'a str>,
) {
    match parts.next().unwrap_or("list") {
        "list" | "" => {
            let list = workers.list();
            if list.is_empty() && workers.queued_count() == 0 {
                app.push(Kind::Notice, "(no agents)".to_string());
            } else {
                for w in list {
                    let nudges =
                        if w.nudges > 0 { format!(" · {} nudges", w.nudges) } else { String::new() };
                    let on = match &w.model {
                        Some(m) => format!(" · on {m}"),
                        None => String::new(),
                    };
                    app.push(
                        Kind::Notice,
                        format!(
                            "{} [{}] {} tools · {} changed{}{} · {} — {}",
                            w.id,
                            w.status.label(),
                            w.tool_calls,
                            w.changed.len(),
                            nudges,
                            on,
                            truncate(&w.last, 40),
                            truncate(&w.task, 40)
                        ),
                    );
                }
                if workers.queued_count() > 0 {
                    app.push(Kind::Notice, format!("({} queued)", workers.queued_count()));
                }
            }
        }
        "drop-queued" | "clear-queue" => {
            let n = workers.drop_queued();
            app.push(Kind::Notice, format!("dropped {n} queued task(s)"));
        }
        "nudge" | "steer" => {
            let id = parts.next().map(str::to_string);
            let message = parts.collect::<Vec<_>>().join(" ");
            match id {
                Some(id) if !message.trim().is_empty() => {
                    if workers.nudge(&id, &message) {
                        app.push(Kind::Notice, format!("nudged {id}"));
                    } else {
                        app.push(Kind::Notice, format!("(no agent {id})"));
                    }
                }
                _ => app.push(Kind::Notice, "usage: /agents nudge <id> <message>".to_string()),
            }
        }
        "kill" | "stop" => match parts.next() {
            Some(id) if workers.kill(id) => app.push(Kind::Notice, format!("killing {id}")),
            Some(id) => app.push(Kind::Notice, format!("(no agent {id})")),
            None => app.push(Kind::Notice, "usage: /agents kill <id>".to_string()),
        },
        "show" | "result" => match parts.next() {
            Some(id) => match workers.get(id) {
                Some(w) => {
                    let mut body = format!("[{}]", w.status.label());
                    if w.nudges > 0 {
                        body.push_str(&format!(" · {} supervisor nudges", w.nudges));
                    }
                    if let Some(reason) = &w.escalation {
                        body.push_str(&format!("\nstopped by supervisor: {reason}"));
                    }
                    if !w.changed.is_empty() {
                        body.push_str(&format!("\nchanged: {}", w.changed.join(", ")));
                    }
                    if let Ok(p) = Session::path_for_id(&w.session_id) {
                        body.push_str(&format!("\nsession: {}", p.display()));
                    }
                    if w.result.is_empty() {
                        body.push_str(&format!("\n{}", w.last));
                    } else {
                        body.push_str(&format!("\n{}", w.result));
                    }
                    app.push(Kind::Notice, format!("{id} {body}"));
                }
                None => app.push(Kind::Notice, format!("(no agent {id})")),
            },
            None => app.push(Kind::Notice, "usage: /agents show <id>".to_string()),
        },
        other => app.push(Kind::Error, format!("unknown /agents subcommand: {other}")),
    }
}

/// A compact, readable one-liner for a tool call instead of raw JSON args.
fn tool_summary(name: &str, arguments: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(arguments).unwrap_or(serde_json::Value::Null);
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    match name {
        "bash" => s("command").map(|c| format!("bash: {c}")),
        "read" | "write" | "edit" | "ls" => s("path").map(|p| format!("{name} {p}")),
        "grep" => match (s("pattern"), s("path")) {
            (Some(p), Some(path)) => Some(format!("grep /{p}/ {path}")),
            (Some(p), None) => Some(format!("grep /{p}/")),
            _ => None,
        },
        "find" => s("name").map(|n| format!("find /{n}/")),
        "doc" => {
            let action = s("action").unwrap_or_default();
            match s("path").or_else(|| s("out")) {
                Some(p) => Some(format!("doc {action} {p}")),
                None => Some(format!("doc {action}")),
            }
        }
        _ => None,
    }
    .unwrap_or_else(|| format!("{name} {arguments}"))
}

/// Tab-complete the current token: `/command` in command position, or `@path`
/// file references anywhere. Repeated Tab cycles the candidates.
fn complete(app: &mut App, cwd: &Path) {
    if let Some(c) = &mut app.completion {
        if c.candidates.len() > 1 {
            c.idx = (c.idx + 1) % c.candidates.len();
            app.input.truncate(c.token_start);
            app.input.push_str(&c.candidates[c.idx]);
            let status = completion_status(c);
            app.cursor = app.char_len();
            app.status = status;
        }
        return;
    }

    let Some((start, candidates)) = compute_completions(&app.input, cwd) else {
        return;
    };
    app.input.truncate(start);
    app.input.push_str(&candidates[0]);
    let compl = Completion { candidates, idx: 0, token_start: start };
    app.status = completion_status(&compl);
    app.cursor = app.char_len();
    app.completion = Some(compl);
}

fn completion_status(c: &Completion) -> String {
    if c.candidates.len() == 1 {
        return String::new();
    }
    let preview: Vec<String> = c
        .candidates
        .iter()
        .take(8)
        .map(|s| s.trim().trim_start_matches('@').to_string())
        .collect();
    format!("⇥ {}/{}  {}", c.idx + 1, c.candidates.len(), preview.join("  "))
}

/// Compute completion candidates for the current (last) token. Returns the byte
/// offset where the token starts and the replacement strings.
fn compute_completions(input: &str, cwd: &Path) -> Option<(usize, Vec<String>)> {
    let token_start = input.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
    let token = &input[token_start..];

    // @path references anywhere.
    if let Some(rest) = token.strip_prefix('@') {
        let cands: Vec<String> =
            complete_path(rest, cwd).into_iter().map(|p| format!("@{p}")).collect();
        return (!cands.is_empty()).then_some((token_start, cands));
    }

    // /command in the first-token position.
    if token_start == 0 {
        let rest = token.strip_prefix('/')?;
        let cands: Vec<String> = COMMANDS
            .iter()
            .filter(|c| c[1..].starts_with(rest))
            .map(|c| format!("{c} "))
            .collect();
        return (!cands.is_empty()).then_some((token_start, cands));
    }

    // Subcommand / argument completion for a /command.
    let tokens: Vec<&str> = input.split_whitespace().collect();
    let first = *tokens.first()?;
    if !first.starts_with('/') {
        return None;
    }
    let prev = input[..token_start].split_whitespace().count();
    let cands = arg_completions(first, prev, token, &tokens, cwd)?;
    (!cands.is_empty()).then_some((token_start, cands))
}

/// Complete a subcommand or argument for a `/command`.
fn arg_completions(
    first: &str,
    prev: usize,
    token: &str,
    tokens: &[&str],
    cwd: &Path,
) -> Option<Vec<String>> {
    let opts: &[&str] = match first.trim_start_matches('/') {
        "agents" | "workers" if prev == 1 => {
            &["list", "show", "kill", "nudge", "drop-queued"]
        }
        "spawn" if prev == 1 => &["-n", "--each-files", "--model"],
        "knowledge" | "know" if prev == 1 => &["index", "search", "status"],
        "skill" | "skills" if prev == 1 => return Some(skill_names(token, cwd)),
        "fast" | "lucky" if prev == 1 => &["on", "off", "auto"],
        "think" if prev == 1 => &["on", "off", "auto", "2000"],
        "validate" if prev == 1 => &["off"],
        "memory" | "mem" => match prev {
            1 => &[
                "list", "global", "project", "search", "show", "pending", "approve", "extract",
                "mine",
                "forget", "add",
            ],
            2 if tokens.get(1) == Some(&"add") => &["global", "project"],
            3 if tokens.get(1) == Some(&"add") => {
                &["decision", "constraint", "preference", "fact", "lesson"]
            }
            _ => return None,
        },
        _ => return None,
    };
    Some(opts.iter().filter(|o| o.starts_with(token)).map(|o| format!("{o} ")).collect())
}

/// Installed skill names matching `token` — completion has to read the disk
/// here, since skills are discovered rather than compiled in.
fn skill_names(token: &str, cwd: &Path) -> Vec<String> {
    crate::skill::SkillCatalog::discover(cwd)
        .skills()
        .iter()
        .filter(|s| s.name.starts_with(token))
        .map(|s| format!("{} ", s.name))
        .collect()
}

/// Prefix-complete a relative file path against the filesystem. Directories get
/// a trailing `/`. Hidden entries are shown only when the prefix starts with `.`.
fn complete_path(prefix: &str, cwd: &Path) -> Vec<String> {
    let (dir_rel, file_start) = match prefix.rfind('/') {
        Some(i) => (&prefix[..=i], &prefix[i + 1..]),
        None => ("", prefix),
    };
    let dir = cwd.join(dir_rel);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(file_start) {
            continue;
        }
        if name.starts_with('.') && !file_start.starts_with('.') {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let mut p = format!("{dir_rel}{name}");
        if is_dir {
            p.push('/');
        }
        out.push(p);
    }
    out.sort();
    out.truncate(50);
    out
}

/// Open `current` in `$VISUAL`/`$EDITOR` (fallback `vi`); return the edited text
/// on success. The TUI must already be suspended (raw mode off) when called.
fn external_edit(current: &str) -> Option<String> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    external_edit_with(&editor, current)
}

fn external_edit_with(editor: &str, current: &str) -> Option<String> {
    let path = std::env::temp_dir().join(format!("worksmith-compose-{}.md", std::process::id()));
    std::fs::write(&path, current).ok()?;

    // `EDITOR` may include args (e.g. "code -w"); the file path goes last.
    let mut parts = editor.split_whitespace();
    let prog = parts.next()?;
    let status = std::process::Command::new(prog).args(parts).arg(&path).status();

    let result = match status {
        Ok(s) if s.success() => std::fs::read_to_string(&path).ok(),
        _ => None,
    };
    let _ = std::fs::remove_file(&path);
    // Drop the trailing newline editors add, keep internal ones.
    result.map(|s| s.strip_suffix('\n').map(str::to_string).unwrap_or(s))
}

/// Replace `@path` tokens by appending the referenced files' contents.
fn expand_file_mentions(input: &str, cwd: &Path) -> String {
    let mut appended = String::new();
    for token in input.split_whitespace() {
        if let Some(path) = token.strip_prefix('@') {
            let full = if Path::new(path).is_absolute() {
                PathBuf::from(path)
            } else {
                cwd.join(path)
            };
            if let Ok(content) = std::fs::read_to_string(&full) {
                appended.push_str(&format!("\n\n<file path=\"{path}\">\n{content}\n</file>"));
            }
        }
    }
    if appended.is_empty() { input.to_string() } else { format!("{input}{appended}") }
}

// ---- rendering ------------------------------------------------------------

fn ui(f: &mut Frame, app: &App) {
    // The composer grows with its content (up to MAX_INPUT_ROWS), + borders.
    let lines = app.input.split('\n').count().clamp(1, MAX_INPUT_ROWS);
    let input_height = (lines + 2) as u16;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .split(f.area());

    render_transcript(f, chunks[0], app);
    render_input(f, chunks[1], app);
    render_footer(f, chunks[2], app);
}

fn render_transcript(f: &mut Frame, area: Rect, app: &App) {
    // Rows are pre-wrapped and cached (see App::ensure_rows); here we just slice
    // the tail (minus any manual scroll-up). Scrolling is therefore cheap.
    let rows = &app.cached_rows;
    let h = area.height as usize;
    let total = rows.len();
    let up = (app.scroll_up as usize).min(total.saturating_sub(1));
    let end = total.saturating_sub(up);
    let start = end.saturating_sub(h);
    let view = rows[start..end].to_vec();

    let para = Paragraph::new(view).block(Block::default().borders(Borders::NONE));
    f.render_widget(para, area);
}

/// Build the fully-wrapped, styled rows for the transcript (each row already
/// fits `width`). Tabs are expanded so widths are predictable.
fn build_rows(
    items: &[Item],
    collapse_tools: bool,
    show_thinking: bool,
    width: u16,
) -> Vec<Line<'static>> {
    let w = (width.max(12) as usize).saturating_sub(1);
    let mut rows: Vec<Line> = Vec::new();

    for item in items {
        if item.kind == Kind::Thinking && !show_thinking {
            continue;
        }
        if item.kind == Kind::Diff {
            render_diff(&mut rows, &item.text, collapse_tools, width);
            rows.push(Line::from(""));
            continue;
        }
        let (style, label): (Style, &str) = match item.kind {
            Kind::User => (Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD), "you ▸ "),
            Kind::Assistant => (Style::default().fg(Color::White), ""),
            Kind::Thinking => {
                (Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC), "thinking ")
            }
            Kind::Tool => (Style::default().fg(Color::Yellow), "⚙ "),
            Kind::ToolResult => (Style::default().fg(Color::DarkGray), "→ "),
            Kind::Notice => (Style::default().fg(Color::Blue), ""),
            Kind::Error => (Style::default().fg(Color::Red), "! "),
            Kind::Diff => unreachable!("diffs are rendered above"),
        };

        // Show short tool results in full; cap long ones (Ctrl+O expands).
        let expanded = item.text.replace('\t', "    ");
        let text = if item.kind == Kind::ToolResult && collapse_tools {
            let lines: Vec<&str> = expanded.lines().collect();
            if lines.len() > TOOL_RESULT_PREVIEW_LINES {
                let shown = lines[..TOOL_RESULT_PREVIEW_LINES].join("\n");
                format!("{shown}\n… (+{} lines · Ctrl+O)", lines.len() - TOOL_RESULT_PREVIEW_LINES)
            } else {
                expanded
            }
        } else {
            expanded
        };

        let indent = "  ";
        let mut first_row = true;
        for logical in text.split('\n') {
            let chars: Vec<char> = logical.chars().collect();
            let mut i = 0;
            loop {
                let prefix = if first_row {
                    label.to_string()
                } else {
                    indent.to_string()
                };
                let avail = w.saturating_sub(prefix.chars().count()).max(1);
                let seg: String = chars[i..(i + avail).min(chars.len())].iter().collect();
                rows.push(Line::from(vec![
                    Span::styled(prefix, style.add_modifier(Modifier::DIM)),
                    Span::styled(seg, style),
                ]));
                first_row = false;
                i += avail;
                if i >= chars.len() {
                    break;
                }
            }
        }
        rows.push(Line::from("")); // blank line between items
    }
    rows
}

fn render_input(f: &mut Frame, area: Rect, app: &App) {
    let title = if app.running {
        " working… ".to_string()
    } else {
        " message · Enter send · Alt+Enter newline ".to_string()
    };

    let (crow, ccol) = cursor_rowcol(&app.input, app.cursor);
    let inner_h = area.height.saturating_sub(2) as usize;
    // Vertical scroll so the cursor's row stays visible.
    let scroll = (crow + 1).saturating_sub(inner_h.max(1)) as u16;

    // No wrap: keeps cursor row/col math exact (long lines clip at the edge).
    let para = Paragraph::new(app.input.as_str())
        .block(Block::default().borders(Borders::ALL).title(title))
        .scroll((scroll, 0));
    f.render_widget(para, area);

    let inner_w = area.width.saturating_sub(2);
    let x = area.x + 1 + (ccol as u16).min(inner_w.saturating_sub(1));
    let y = area.y + 1 + (crow as u16).saturating_sub(scroll);
    f.set_cursor_position((x, y));
}

/// Cursor (row, col) in logical lines for a char index into `input`.
fn cursor_rowcol(input: &str, cursor: usize) -> (usize, usize) {
    let (mut row, mut col) = (0usize, 0usize);
    for (i, ch) in input.chars().enumerate() {
        if i == cursor {
            break;
        }
        if ch == '\n' {
            row += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (row, col)
}

/// Render a unified diff with per-line color (summary yellow, `+` green, `-`
/// red, `@@` cyan, context dim), wrapped to width and capped when collapsed.
fn render_diff(rows: &mut Vec<Line<'static>>, text: &str, collapse: bool, width: u16) {
    let w = (width.max(12) as usize).saturating_sub(1);
    let all: Vec<&str> = text.lines().collect();
    let (shown, extra) = if collapse && all.len() > TOOL_RESULT_PREVIEW_LINES + 5 {
        (&all[..TOOL_RESULT_PREVIEW_LINES + 5], all.len() - (TOOL_RESULT_PREVIEW_LINES + 5))
    } else {
        (&all[..], 0)
    };

    for (i, raw) in shown.iter().enumerate() {
        let line = raw.replace('\t', "    ");
        let style = if i == 0 {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else if line.starts_with("@@") {
            Style::default().fg(Color::Cyan)
        } else if line.starts_with("+++") || line.starts_with("---") {
            Style::default().fg(Color::DarkGray)
        } else if line.starts_with('+') {
            Style::default().fg(Color::Green)
        } else if line.starts_with('-') {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let chars: Vec<char> = line.chars().collect();
        let mut idx = 0;
        loop {
            let prefix = if idx == 0 { "  " } else { "   " };
            let avail = w.saturating_sub(prefix.len()).max(1);
            let seg: String = chars[idx..(idx + avail).min(chars.len())].iter().collect();
            rows.push(Line::from(vec![
                Span::styled(prefix.to_string(), style.add_modifier(Modifier::DIM)),
                Span::styled(seg, style),
            ]));
            idx += avail;
            if idx >= chars.len() {
                break;
            }
        }
    }
    if extra > 0 {
        rows.push(Line::from(Span::styled(
            format!("  … (+{extra} more diff lines · Ctrl+O)"),
            Style::default().fg(Color::DarkGray),
        )));
    }
}

/// `7900` -> `7.9k`. The footer has room for a number, not a paragraph.
fn compact_tokens(n: u32) -> String {
    if n >= 1000 { format!("{:.1}k", n as f32 / 1000.0) } else { n.to_string() }
}

/// Accept `2000` or `2k` for a reasoning budget.
fn parse_budget(s: &str) -> Option<u32> {
    let s = s.trim();
    match s.strip_suffix(['k', 'K']) {
        Some(head) => head.trim().parse::<f32>().ok().map(|v| (v * 1000.0) as u32),
        None => s.parse::<u32>().ok(),
    }
    .filter(|n| *n > 0)
}

fn render_footer(f: &mut Frame, area: Rect, app: &App) {
    let pct = (app.last_prompt_tokens as usize * 100)
        .checked_div(app.context_limit)
        .unwrap_or(0)
        .min(999);
    // Reasoning spend: the live estimate while a step streams, the provider's
    // reported number once it lands. A step that thinks for a minute and says
    // nothing is otherwise indistinguishable from one that is merely slow.
    let live = (app.step_reasoning_chars / 4) as u32;
    let reasoning = live.max(app.last_reasoning_tokens);
    let reasoning = if reasoning > 0 { format!("  ↻{}", compact_tokens(reasoning)) } else { String::new() };
    // "length" means the model was cut off rather than finished.
    let cut = if app.last_finish_reason.as_deref() == Some("length") { "  ⚠cut" } else { "" };
    let fast = match &app.think_label {
        Some(l) => format!("  think:{l}"),
        None => String::new(),
    };
    let agents = if app.agents_running > 0 || app.agents_queued > 0 {
        let queued =
            if app.agents_queued > 0 { format!(" · {} queued", app.agents_queued) } else { String::new() };
        format!("  ↑{} agents{}", app.agents_running, queued)
    } else {
        String::new()
    };
    let tail = format!("{reasoning}{cut}{fast}{agents}");
    let left = format!(
        " {}  ctx {}% ({}/{})  ↓{}{}",
        app.model, pct, app.last_prompt_tokens, app.context_limit, app.total_out_tokens, tail
    );

    // While a turn runs, show an animated spinner + elapsed seconds.
    let status = if app.running {
        const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let elapsed = app.turn_start.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        format!("{} {elapsed}s  {}", SPIN[app.spinner % SPIN.len()], app.status)
    } else {
        app.status.clone()
    };
    let line = Line::from(vec![
        Span::styled(left, Style::default().fg(Color::Black).bg(Color::Cyan)),
        Span::raw("  "),
        Span::styled(status, Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;

    fn app() -> App {
        App::new("m".into(), 1000, None)
    }

    #[test]
    fn streaming_deltas_coalesce_per_channel() {
        let mut a = app();
        a.apply_event(Event::UserMessage { text: "hi".into() });
        a.apply_event(Event::Thinking { text: "let me ".into() });
        a.apply_event(Event::Thinking { text: "think".into() });
        a.apply_event(Event::MessageDelta { text: "Hel".into() });
        a.apply_event(Event::MessageDelta { text: "lo".into() });

        assert_eq!(a.items.len(), 3);
        assert!(matches!(a.items[0].kind, Kind::User));
        assert!(matches!(a.items[1].kind, Kind::Thinking));
        assert_eq!(a.items[1].text, "let me think");
        assert!(matches!(a.items[2].kind, Kind::Assistant));
        assert_eq!(a.items[2].text, "Hello");
    }

    #[test]
    fn tool_call_breaks_the_assistant_block() {
        let mut a = app();
        a.apply_event(Event::MessageDelta { text: "before".into() });
        a.apply_event(Event::ToolCall { id: "1".into(), name: "ls".into(), arguments: "{}".into() });
        a.apply_event(Event::MessageDelta { text: "after".into() });

        // before-assistant, tool, after-assistant → 3 separate items.
        assert_eq!(a.items.len(), 3);
        assert_eq!(a.items[0].text, "before");
        assert!(matches!(a.items[1].kind, Kind::Tool));
        assert_eq!(a.items[2].text, "after");
    }

    #[test]
    fn usage_updates_footer_counters() {
        let mut a = app();
        a.apply_event(Event::Usage {
            prompt_tokens: 500,
            completion_tokens: 20,
            total_tokens: 520,
            reasoning_tokens: 0,
            finish_reason: None,
        });
        a.apply_event(Event::Usage {
            prompt_tokens: 600,
            completion_tokens: 30,
            total_tokens: 630,
            reasoning_tokens: 0,
            finish_reason: None,
        });
        assert_eq!(a.last_prompt_tokens, 600);
        assert_eq!(a.total_out_tokens, 50);
    }

    #[test]
    fn reasoning_spend_is_visible_before_the_step_finishes() {
        // The failure this exists for: a step that thinks for a minute and
        // returns nothing looked identical to one that was merely slow.
        let mut a = app();
        a.apply_event(Event::Thinking { text: "x".repeat(8000) });
        assert_eq!(a.step_reasoning_chars, 8000, "live count climbs as reasoning streams");

        a.apply_event(Event::Usage {
            prompt_tokens: 100,
            completion_tokens: 2048,
            total_tokens: 2148,
            reasoning_tokens: 2000,
            finish_reason: Some("length".into()),
        });
        assert_eq!(a.last_reasoning_tokens, 2000, "the provider's number replaces the estimate");
        assert_eq!(a.step_reasoning_chars, 0, "the live count resets for the next step");
        assert_eq!(a.last_finish_reason.as_deref(), Some("length"), "cut-off is recorded");
    }

    #[test]
    fn a_dropped_setting_is_shown_not_swallowed() {
        let mut a = app();
        a.apply_event(Event::Warning { message: "budget ignored".into() });
        assert!(matches!(a.items[0].kind, Kind::Notice));
        assert!(a.items[0].text.contains("budget ignored"));
    }

    #[test]
    fn budgets_parse_in_both_spellings() {
        assert_eq!(parse_budget("2000"), Some(2000));
        assert_eq!(parse_budget("2k"), Some(2000));
        assert_eq!(parse_budget("1.5k"), Some(1500));
        assert_eq!(parse_budget("0"), None);
        assert_eq!(parse_budget("lots"), None);
        assert_eq!(compact_tokens(7900), "7.9k");
        assert_eq!(compact_tokens(42), "42");
    }

    #[test]
    fn composer_edits_at_cursor() {
        let mut a = app();
        for c in "helo".chars() {
            a.insert_char(c);
        }
        assert_eq!(a.input, "helo");
        assert_eq!(a.cursor, 4);
        // Cursor before 'o'; insert the missing 'l' → "hello".
        a.move_left();
        a.insert_char('l');
        assert_eq!(a.input, "hello");
        assert_eq!(a.cursor, 4); // between the new 'l' and 'o'
        a.move_end();
        a.backspace();
        assert_eq!(a.input, "hell");
    }

    #[test]
    fn composer_paste_is_multiline_at_cursor() {
        let mut a = app();
        a.insert_str("line1\nline2\nline3");
        assert_eq!(a.input.split('\n').count(), 3);
        assert_eq!(a.cursor, a.char_len());
        let (row, _col) = cursor_rowcol(&a.input, a.cursor);
        assert_eq!(row, 2, "cursor should be on the last pasted line");
    }

    #[test]
    fn composer_delete_word_and_home_end() {
        let mut a = app();
        a.insert_str("foo bar baz");
        a.delete_word();
        assert_eq!(a.input, "foo bar ");
        a.move_home();
        assert_eq!(a.cursor, 0);
        a.move_end();
        assert_eq!(a.cursor, a.char_len());
    }

    #[cfg(unix)]
    #[test]
    fn external_edit_runs_editor_and_reads_back() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("ed.sh");
        std::fs::write(&script, "#!/bin/sh\nprintf 'edited by editor' > \"$1\"\n").unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let out = external_edit_with(script.to_str().unwrap(), "original text");
        assert_eq!(out.as_deref(), Some("edited by editor"));
    }

    #[test]
    fn composer_history_recall() {
        let mut a = app();
        a.insert_str("first");
        let _ = a.take_input();
        a.insert_str("second");
        let _ = a.take_input();
        assert_eq!(a.history.len(), 2);

        a.insert_str("draft");
        a.history_prev();
        assert_eq!(a.input, "second");
        a.history_prev();
        assert_eq!(a.input, "first");
        a.history_next();
        assert_eq!(a.input, "second");
        a.history_next();
        assert_eq!(a.input, "draft", "past newest restores the draft");
    }

    #[test]
    fn completes_slash_commands() {
        let (start, c) = compute_completions("/me", Path::new(".")).unwrap();
        assert_eq!(start, 0);
        assert_eq!(c, vec!["/memory ".to_string()]);

        let (_, all) = compute_completions("/", Path::new(".")).unwrap();
        assert!(all.len() >= 5);

        // Not in command position → no command completion.
        assert!(compute_completions("hi /me", Path::new(".")).is_none());
    }

    #[test]
    fn completes_subcommands_and_args() {
        // /agents subcommands
        let (_, c) = compute_completions("/agents ", Path::new(".")).unwrap();
        assert!(c.contains(&"list ".to_string()) && c.contains(&"kill ".to_string()), "{c:?}");
        let (_, c) = compute_completions("/agents k", Path::new(".")).unwrap();
        assert_eq!(c, vec!["kill ".to_string()]);

        // /memory subcommands, then add's scope + kind
        let (_, c) = compute_completions("/memory ", Path::new(".")).unwrap();
        assert!(c.contains(&"forget ".to_string()) && c.contains(&"add ".to_string()), "{c:?}");
        let (_, c) = compute_completions("/memory add ", Path::new(".")).unwrap();
        assert_eq!(c, vec!["global ".to_string(), "project ".to_string()]);
        let (_, c) = compute_completions("/memory add project ", Path::new(".")).unwrap();
        assert!(c.contains(&"decision ".to_string()) && c.contains(&"lesson ".to_string()), "{c:?}");
    }

    #[test]
    fn completes_at_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("main.rs"), "").unwrap();
        std::fs::write(dir.path().join("mod.rs"), "").unwrap();

        let (start, c) = compute_completions("@m", dir.path()).unwrap();
        assert_eq!(start, 0);
        assert!(c.contains(&"@main.rs".to_string()), "{c:?}");
        assert!(c.contains(&"@mod.rs".to_string()), "{c:?}");

        // Directories get a trailing slash.
        let (_, d) = compute_completions("@s", dir.path()).unwrap();
        assert!(d.contains(&"@src/".to_string()), "{d:?}");
    }

    #[test]
    fn build_rows_wraps_to_width_and_labels_channels() {
        let mut a = app();
        a.apply_event(Event::UserMessage { text: "hello world this is a long line".into() });
        let rows = build_rows(&a.items, a.collapse_tools, true, 16);
        // Every row must fit the width (accounting for prefix + content spans).
        for row in &rows {
            let len: usize = row.spans.iter().map(|s| s.content.chars().count()).sum();
            assert!(len <= 16, "row too wide ({len}): {row:?}");
        }
        // The first row carries the "you ▸" label.
        let first: String = rows[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(first.contains("you"), "first row should label the user: {first}");
    }
}
