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
    DisableMouseCapture, EnableMouseCapture, Event as CEvent, EventStream, KeyCode, KeyEvent,
    KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, TurnResult};
use crate::event::{Event, EventBus};
use crate::memory::{MemoryStore, Scope};
use crate::prompt::build_system_prompt;
use crate::session::Session;
use crate::validation::CommandValidator;

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
const COMMANDS: &[&str] =
    &["/help", "/new", "/compact", "/memory", "/validate", "/quit"];

/// Active Tab-completion state (candidates for the current token).
struct Completion {
    candidates: Vec<String>,
    idx: usize,
    token_start: usize,
}

struct App {
    items: Vec<Item>,
    input: String,
    /// Lines scrolled up from the bottom; 0 = following the tail.
    scroll_up: u16,
    follow: bool,
    collapse_tools: bool,
    running: bool,
    model: String,
    context_limit: usize,
    last_prompt_tokens: u32,
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
}

impl App {
    fn new(model: String, context_limit: usize, validate_cmd: Option<String>) -> Self {
        App {
            items: Vec::new(),
            input: String::new(),
            scroll_up: 0,
            follow: true,
            collapse_tools: false,
            running: false,
            model,
            context_limit,
            last_prompt_tokens: 0,
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
            Event::Usage { prompt_tokens, completion_tokens, .. } => {
                self.last_prompt_tokens = prompt_tokens;
                self.total_out_tokens += completion_tokens as u64;
            }
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
    )
    .await;
    restore_terminal(&mut terminal)?;
    res
}

type Term = Terminal<CrosstermBackend<Stdout>>;

fn setup_terminal() -> Result<Term> {
    enable_raw_mode().context("enabling raw mode")?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture).context("entering alternate screen")?;
    Terminal::new(CrosstermBackend::new(out)).context("creating terminal")
}

fn restore_terminal(terminal: &mut Term) -> Result<()> {
    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture).ok();
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
) -> Result<()> {
    let agent = Arc::new(agent);
    let session = Arc::new(AsyncMutex::new(session));
    let mem = MemoryStore::open(Some(&cwd)).or_else(|_| MemoryStore::open(None))?;

    let mut app = App::new(model, context_limit, validate_cmd);
    let mut bus_rx = bus.subscribe();
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(120));

    // Startup header.
    app.push(Kind::Notice, format!("worksmith · {}", app.model));
    app.push(Kind::Notice, format!("cwd: {}", cwd.display()));
    if !crate::config::load_project_instructions(&cwd).trim().is_empty() {
        app.push(Kind::Notice, "loaded project instructions (AGENTS.md/CLAUDE.md)".to_string());
    }
    if let Some(c) = app.validate_cmd.clone() {
        app.push(Kind::Notice, format!("validation: {c}"));
    }
    app.push(
        Kind::Notice,
        "Tab complete · Ctrl+O tools · Ctrl+T thinking · Esc abort · /help · /quit".to_string(),
    );

    let mut turn: Option<JoinHandle<Result<TurnResult>>> = None;
    let mut cancel = CancellationToken::new();

    loop {
        // Rebuild the wrapped-row cache only if content/width changed, then draw.
        let width = terminal.size().map(|s| s.width).unwrap_or(80);
        app.ensure_rows(width);
        terminal.draw(|f| ui(f, &app))?;

        tokio::select! {
            // Terminal input.
            maybe_ev = events.next() => {
                match maybe_ev {
                    Some(Ok(CEvent::Key(key))) => {
                        match handle_key(key, &mut app, &agent, &session, &mem, &cwd,
                                         bash_timeout, &mut turn, &mut cancel).await? {
                            Flow::Quit => break,
                            Flow::Continue => {}
                        }
                    }
                    Some(Ok(CEvent::Mouse(m))) => match m.kind {
                        MouseEventKind::ScrollUp => app.scroll_up(3),
                        MouseEventKind::ScrollDown => app.scroll_down(3),
                        _ => {}
                    },
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
        KeyCode::Esc => {
            if app.running {
                cancel.cancel();
                app.status = "aborting…".into();
            } else {
                app.input.clear();
            }
        }
        KeyCode::PageUp => app.scroll_up(10),
        KeyCode::PageDown => app.scroll_down(10),
        // Ctrl+U / Ctrl+D: half-page scroll (also friendlier on Mac keyboards).
        KeyCode::Char('u') if ctrl => app.scroll_up(10),
        KeyCode::Char('d') if ctrl => app.scroll_down(10),
        KeyCode::Up => app.scroll_up(3),
        KeyCode::Down => app.scroll_down(3),
        KeyCode::Home => {
            app.follow = false;
            app.scroll_up = u16::MAX; // clamped to the top when rendering
        }
        KeyCode::End => {
            app.scroll_up = 0;
            app.follow = true;
        }
        KeyCode::Backspace => {
            app.input.pop();
        }
        KeyCode::Enter => {
            let input = app.input.trim().to_string();
            if input.is_empty() {
                return Ok(Flow::Continue);
            }
            app.input.clear();

            // Commands (start with '/', or bare quit/exit).
            if input == "/quit" || input == "/exit" || input == "quit" || input == "exit" {
                return Ok(Flow::Quit);
            }
            if input.starts_with('/')
                && handle_command(&input, app, agent, session, mem, cwd).await?
            {
                return Ok(Flow::Continue);
            }

            if app.running {
                app.status = "a turn is already running (Esc to abort)".into();
                return Ok(Flow::Continue);
            }

            // Start a turn.
            let sys = build_system_prompt(cwd, mem);
            let message = expand_file_mentions(&input, cwd);
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
                let validator =
                    cmd.map(|c| CommandValidator::new(c, cwd2.clone(), bash_timeout));
                let mut sess = s.lock().await;
                a.run_turn(&mut sess, &message, &sys, validator.as_ref().map(|v| v as _), tok).await
            }));
        }
        // Ignore control-chords; accept normal (and shifted) chars.
        KeyCode::Char(c) if !ctrl => app.input.push(c),
        _ => {}
    }
    Ok(Flow::Continue)
}

/// Returns true if the input was a recognized command (already handled).
async fn handle_command(
    input: &str,
    app: &mut App,
    agent: &Arc<Agent>,
    session: &Arc<AsyncMutex<Session>>,
    mem: &MemoryStore,
    cwd: &Path,
) -> Result<bool> {
    let mut parts = input.trim_start_matches('/').split_whitespace();
    let head = parts.next().unwrap_or("");
    match head {
        "help" | "h" => {
            app.push(
                Kind::Notice,
                "keys: Enter=send  Esc=abort/clear  Ctrl+C=quit  Ctrl+O=collapse tools  \
                 PgUp/PgDn=scroll\ncommands: /new /compact /memory [list|global|project] \
                 /validate <cmd|off> /quit   @path includes a file"
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
        "memory" | "mem" => {
            let scope = match parts.next() {
                Some("global") => Some(Scope::Global),
                Some("project") => Some(Scope::Project),
                _ => None,
            };
            match mem.list(scope) {
                Ok(rows) if rows.is_empty() => app.push(Kind::Notice, "(no memories)".to_string()),
                Ok(rows) => {
                    for r in rows {
                        app.push(
                            Kind::Notice,
                            format!("[{}/{}] {}: {}", r.scope, r.kind, r.subject, r.content),
                        );
                    }
                }
                Err(e) => app.push(Kind::Error, format!("memory error: {e}")),
            }
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
            app.status = completion_status(c);
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

    if let Some(rest) = token.strip_prefix('/') {
        // Only in command position (first token).
        if token_start != 0 {
            return None;
        }
        let cands: Vec<String> = COMMANDS
            .iter()
            .filter(|c| c[1..].starts_with(rest))
            .map(|c| format!("{c} "))
            .collect();
        (!cands.is_empty()).then_some((token_start, cands))
    } else if let Some(rest) = token.strip_prefix('@') {
        let cands: Vec<String> =
            complete_path(rest, cwd).into_iter().map(|p| format!("@{p}")).collect();
        (!cands.is_empty()).then_some((token_start, cands))
    } else {
        None
    }
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
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3), Constraint::Length(1)])
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
    let title = if app.running { " working… " } else { " message " };
    let para = Paragraph::new(app.input.as_str())
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
    // Cursor at end of input.
    let x = area.x + 1 + (app.input.chars().count() as u16 % area.width.saturating_sub(2).max(1));
    f.set_cursor_position((x, area.y + 1));
}

#[cfg(test)]
mod tests {
    use super::*;

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
        a.apply_event(Event::Usage { prompt_tokens: 500, completion_tokens: 20, total_tokens: 520 });
        a.apply_event(Event::Usage { prompt_tokens: 600, completion_tokens: 30, total_tokens: 630 });
        assert_eq!(a.last_prompt_tokens, 600);
        assert_eq!(a.total_out_tokens, 50);
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

fn render_footer(f: &mut Frame, area: Rect, app: &App) {
    let pct = (app.last_prompt_tokens as usize * 100)
        .checked_div(app.context_limit)
        .unwrap_or(0)
        .min(999);
    let left = format!(
        " {}  ctx {}% ({}/{})  ↓{}",
        app.model, pct, app.last_prompt_tokens, app.context_limit, app.total_out_tokens
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
