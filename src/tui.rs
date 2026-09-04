//! Ratatui front-end. Renders the four channels distinctly — **user**,
//! **assistant**, **tool** activity, and **thinking** — plus a footer with the
//! model, context %, and token counts. It's a subscriber to the event bus (the
//! keystone from M1); the agent loop is unchanged.
//!
//! Concurrency: the agent turn runs as a spawned task (session behind an async
//! mutex) so the UI keeps rendering and stays responsive to Esc (abort) while
//! the model streams.

use std::collections::HashSet;
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

mod composer;
mod footer;
mod modals;
mod overlay;
mod transcript;

use crate::agent::{ActiveModel, Agent, TurnResult};
use crate::event::{Event, EventBus};
use crate::llm::Thinking;
use crate::memory::{IdMatch, MemoryStore, Scope, short_id};
use crate::prompt::{build_system_prompt, build_worker_prompt};
use crate::session::Session;
use crate::validation::CommandValidator;
use crate::fanout::{
    FanOut, PendingFanOut, assign, fanout_notice, matching_files, parse_spawn, plan_fanout,
    spawn_notice,
};
use crate::report::{
    GroupAcc, common_opening, group_report, record_in_group, single_report, truncate,
    truncate_chars, worker_headline,
};
use crate::config::Config;
use crate::llm::ModelOverride;
use crate::supervisor::SupervisorConfig;
use crate::worker::{WorkerManager, WorkerSummary};

use composer::Composer;
#[cfg(test)]
use composer::{compute_completions, wrap_input};
use footer::{footer_legend, footer_status, footer_string, render_footer};
#[cfg(test)]
use footer::compact_tokens;
use modals::{ApprovalKey, AskAnswer, Modals};
use overlay::{Overlay, OverlayItem};
use transcript::{Item, Kind, Mode, Search, Transcript};
#[cfg(test)]
use transcript::{build_rows, row_text};

/// A planner call in flight, plus the inputs the resulting workers need.
struct PlannedFanOut {
    planner: JoinHandle<crate::fanout::FanOutPlan>,
    system: String,
    request: String,
    model: Option<ModelOverride>,
    /// The per-worker check, held across planning so a planned fan-out is
    /// validated the same as an explicit one.
    validate: Option<String>,
}

async fn join_in_flight<T>(
    task: &mut Option<JoinHandle<T>>,
) -> std::result::Result<T, tokio::task::JoinError> {
    task.as_mut()
        .expect("select branch guard ensures task is present")
        .await
}

async fn join_planned_fanout(
    fanout: &mut Option<PlannedFanOut>,
) -> std::result::Result<crate::fanout::FanOutPlan, tokio::task::JoinError> {
    let planner = &mut fanout
        .as_mut()
        .expect("select branch guard ensures fan-out is present")
        .planner;
    planner.await
}

/// Put `text` on the system clipboard using OSC 52, the terminal's own
/// clipboard escape. No pbcopy/xclip dependency, and it works over SSH — the
/// terminal emulator does the writing, wherever it is running.
///
/// Not every terminal enables it (some require opting in), which is why the
/// caller reports failure rather than assuming success.
fn copy_to_clipboard(text: &str) -> io::Result<()> {
    use base64::Engine;
    use std::io::Write;

    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut out = io::stdout();
    // ESC ] 52 ; c ; <base64> BEL — `c` is the clipboard selection.
    write!(out, "\x1b]52;c;{encoded}\x07")?;
    out.flush()
}

/// Slash commands offered by Tab-completion.
/// Every slash command with what it does. One table, so the completion popup,
/// Tab completion and `/help` cannot drift from each other — they used to be
/// three separate lists maintained by hand.
const COMMANDS: &[(&str, &str)] = &[
    ("/help", "keys and commands"),
    ("/new", "start a fresh session"),
    ("/compact", "summarize the history now"),
    ("/memory", "what is remembered — search, mine, review"),
    ("/knowledge", "the project's own docs and source"),
    ("/skill", "load a skill"),
    ("/spawn", "run a task in background workers"),
    ("/agents", "list workers, or tail one live"),
    ("/validate", "the check a turn must pass"),
    ("/fast", "answer without thinking first"),
    ("/think", "how hard to think: a level or a token budget"),
    ("/route", "which provider serves you (OpenRouter)"),
    ("/model", "switch model for this session (or list the configured ones)"),
    ("/pair", "stop at decisions so you learn the code being written"),
    ("/mouse", "wheel scrolls the transcript (off: the terminal keeps the wheel)"),
    ("/trust", "is this project's own config in effect?"),
    ("/history", "what the loop did, and when"),
    ("/quit", "exit"),
];

struct App {
    transcript: Transcript,
    composer: Composer,
    /// Where the current session lives, cached so read-only commands
    /// (/history) never touch the session lock — the agent holds that lock for
    /// the whole turn, and awaiting it from the event loop froze the TUI.
    session_path: std::path::PathBuf,
    running: bool,
    model: String,
    context_limit: usize,
    last_prompt_tokens: u32,
    /// Reasoning tokens the last completion spent. The number that explains a
    /// long, silent step — without it, "thinking" is just an animation.
    last_reasoning_tokens: u32,
    /// Every prompt token billed this session. Each request re-sends the
    /// history, so this is a running total of what was charged, not the size of
    /// the conversation.
    total_in_tokens: u64,
    /// Prices for the session's model, when the config gives them. A local
    /// model has none, and showing $0.00 would be a claim rather than a fact.
    prices: crate::config::ModelSettings,
    /// Worker spend, already priced by each worker's own model, refreshed from
    /// the manager each frame. Costed there rather than here because that is
    /// where the resolved `ModelSettings` lives — matching model names across
    /// this boundary silently reported nothing, since the name is stored
    /// without its provider prefix.
    agent_spend: crate::worker::WorkerSpend,
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
    // Cosmetic/among-turn state.
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
    /// The next `UserMessage` event is an internal synthesis prompt. It still
    /// enters the model context as user text, but the transcript must not imply
    /// the human typed it.
    synthetic_user_message: Option<String>,
    /// Set by `/spawn` when the fan-out needs a planner call; run_loop picks it
    /// up and runs it off the UI task.
    pending_fanout: Option<PendingFanOut>,
    /// Set by `/memory extract`; run_loop runs the classifier off the UI task.
    pending_extract: bool,
    /// Set by `/memory mine [n]`; run_loop does the model half off the UI task.
    /// Carries the cap on how many sessions to read in this run.
    pending_mine: Option<usize>,
    /// The `jj`-style escape: the pair, how fast it must be typed, and the
    /// pending first key. `None` disables it.
    insert_escape: Option<(char, char, Duration)>,
    pending_escape: Option<Instant>,
    /// A floating picker, when one is open. It owns the keyboard while up.
    overlay: Option<Overlay>,
    /// Which worker we're following, and how far we've printed. A worker's
    /// events go to its own bus, so this polls its recorded log instead.
    tail: Option<(String, usize)>,
    modals: Modals,
    /// OpenRouter provider routing, when set live with `/route`.
    route: Option<String>,
    /// Is mouse capture on? On by default, so the wheel scrolls the transcript
    /// you are looking at. Shift+drag still selects text — see `setup_terminal`.
    mouse: bool,
}

impl App {
    fn new(model: String, context_limit: usize, validate_cmd: Option<String>) -> Self {
        App {
            transcript: Transcript::default(),
            composer: Composer::default(),
            session_path: std::path::PathBuf::new(),
            running: false,
            model,
            context_limit,
            last_prompt_tokens: 0,
            last_reasoning_tokens: 0,
            total_in_tokens: 0,
            prices: crate::config::ModelSettings::default(),
            agent_spend: crate::worker::WorkerSpend::default(),
            step_reasoning_chars: 0,
            last_finish_reason: None,
            total_out_tokens: 0,
            validate_cmd,
            status: "/help for keys and commands".into(),
            cur_assistant: None,
            cur_thinking: None,
            spinner: 0,
            turn_start: None,
            agents_running: 0,
            agents_queued: 0,
            think_label: None,
            fanout_auto: true,
            synthesize: true,
            synthetic_user_message: None,
            pending_fanout: None,
            pending_extract: false,
            pending_mine: None,
            insert_escape: Some(('j', 'j', Duration::from_millis(300))),
            pending_escape: None,
            overlay: None,
            tail: None,
            modals: Modals::default(),
            route: None,
            mouse: true,
        }
    }

    /// Everything on `App` that describes *this* session, cleared in one place.
    ///
    /// The context itself lives on `Session`, which `/new` replaces wholesale.
    /// These are the display fields that shadow it, and they used to be
    /// hand-cleared at the call site — so the transcript emptied while the
    /// footer went on reporting the old session's context, cost and token
    /// counts until the next `Usage` event happened to overwrite them. The
    /// cumulative ones never corrected themselves at all.
    ///
    /// A method rather than a list of assignments in the handler: the next
    /// per-session counter added to the footer has one obvious place to be
    /// reset, and forgetting it is a visible test failure rather than a footer
    /// that quietly describes two sessions at once.
    fn reset_for_new_session(&mut self, path: PathBuf) {
        self.session_path = path;
        self.transcript.clear_for_new_session();
        self.cur_assistant = None;
        self.cur_thinking = None;
        // Counters the footer reads. Totals are per-session: they sit next to a
        // per-session `ctx %`, and a cost that spans sessions would be
        // answering a different question than the number beside it.
        self.last_prompt_tokens = 0;
        self.last_reasoning_tokens = 0;
        self.step_reasoning_chars = 0;
        self.total_in_tokens = 0;
        self.total_out_tokens = 0;
        self.last_finish_reason = None;
    }

    fn push(&mut self, kind: Kind, text: impl Into<String>) {
        self.transcript.push(kind, text);
    }

    fn show_session_id(&mut self, id: &str) {
        self.push(Kind::Notice, format!("session {id}"));
    }

    // ---- normal mode ----

    /// Handle a character against the `jj`-style escape. Returns true when the
    /// pair completed and the caller should not insert this character.
    ///
    /// Unlike vim, this composer holds prose rather than code, so the window is
    /// short and the first key is still inserted immediately — a pause after a
    /// lone `j` leaves normal typing untouched, and the only cost of a false
    /// positive is one keystroke to get back.
    fn escape_pair(&mut self, c: char) -> bool {
        let Some((first, second, window)) = self.insert_escape else {
            return false;
        };
        let now = Instant::now();
        if c == second
            && let Some(at) = self.pending_escape
            && now.duration_since(at) <= window
            && self.composer.input.ends_with(first)
            && self.composer.cursor == self.composer.char_len()
        {
            self.composer.backspace(); // remove the first key, which was already inserted
            self.pending_escape = None;
            return true;
        }
        self.pending_escape = (c == first).then_some(now);
        false
    }

    /// Enter reading mode, putting the cursor on the last visible row.
    fn enter_normal(&mut self) {
        self.transcript.enter_normal();
    }

    fn enter_insert(&mut self) {
        self.transcript.enter_insert();
    }

    /// Move the cursor by `delta` rows, clamped, keeping it on screen.
    fn cursor_by(&mut self, delta: isize) {
        self.transcript.cursor_by(delta);
    }

    /// Which item a row belongs to. `item_starts` already records where each
    /// item begins, so this is the lookup that makes "yank what I'm looking at"
    /// mean the whole message rather than one wrapped line.
    fn item_at_row(&self, row: usize) -> Option<usize> {
        self.transcript.item_at_row(row)
    }

    fn set_search(&mut self, search: Option<Search>) {
        self.transcript.set_search(search);
    }

    fn mutate_search(&mut self, f: impl FnOnce(&mut Search)) {
        self.transcript.mutate_search(f);
    }

    /// Jump to the next match after the cursor, wrapping. Returns false when
    /// nothing matches, so the caller can say so instead of moving silently.
    fn jump_match(&mut self, forward: bool) -> bool {
        self.transcript.jump_match(forward)
    }

    /// Mark item `index` (and everything after it) as needing re-wrapping.
    fn touch(&mut self, index: usize) {
        self.transcript.touch(index);
    }

    /// Everything needs re-wrapping — a width change, or a toggle that changes
    /// how items render.
    fn touch_all(&mut self) {
        self.transcript.touch_all();
    }

    /// Rebuild the wrapped-row cache, doing only the work that changed.
    fn ensure_rows(&mut self, width: u16) {
        self.transcript.ensure_rows(width);
    }

    /// Scroll toward older content.
    fn scroll_up(&mut self, n: u16) {
        self.transcript.scroll_up(n);
    }

    /// Scroll toward the newest content; re-enable follow at the bottom.
    fn scroll_down(&mut self, n: u16) {
        self.transcript.scroll_down(n);
    }

    fn apply_event(&mut self, ev: Event) {
        match ev {
            Event::UserMessage { text } => {
                let is_synthetic = self.synthetic_user_message.as_deref() == Some(text.as_str());
                if is_synthetic {
                    self.synthetic_user_message = None;
                    self.push(Kind::Notice, format!("synthesis ▸ {text}"));
                } else {
                    self.push(Kind::User, text);
                }
                self.cur_assistant = None;
                self.cur_thinking = None;
            }
            Event::Thinking { text } => {
                self.step_reasoning_chars += text.len();
                // Only the item being written to is stale — that is the whole
                // point of tracking a start index per item.
                let at = match self.cur_thinking {
                    Some(i) => {
                        self.transcript.items[i].text.push_str(&text);
                        i
                    }
                    None => {
                        self.transcript.items.push(Item { kind: Kind::Thinking, text });
                        self.cur_thinking = Some(self.transcript.items.len() - 1);
                        self.transcript.items.len() - 1
                    }
                };
                self.touch(at);
            }
            Event::MessageDelta { text } => {
                let at = match self.cur_assistant {
                    Some(i) => {
                        self.transcript.items[i].text.push_str(&text);
                        i
                    }
                    None => {
                        self.transcript.items.push(Item { kind: Kind::Assistant, text });
                        self.cur_assistant = Some(self.transcript.items.len() - 1);
                        self.transcript.items.len() - 1
                    }
                };
                self.touch(at);
            }
            // Bookkeeping for the supervisor's idle rule; nothing to draw.
            Event::ModelCallStarted | Event::ModelCallFinished => {}
            Event::ModelChanged { from, to } => {
                self.push(Kind::Notice, format!("model changed: {from} → {to}"));
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
            Event::Checkpoint { kind, subject, detail } => {
                // `ask` renders when its answer comes back, not here — the
                // question is already on screen in the composer's prompt, and
                // printing it twice reads as the loop stuttering.
                let head = match kind.as_str() {
                    "yours" => format!("yours — {subject}"),
                    // Both halves of a question are already on screen: the
                    // composer prompt asked it, and answering echoed it back.
                    // These events exist for the session log, /history and
                    // --mode json, not to be printed a second time here.
                    "ask" | "answered" => return,
                    _ => subject.clone(),
                };
                self.push(Kind::Pair, format!("{head}\n  {detail}"));
            }
            Event::Nudge { reason } => self.push(Kind::Notice, format!("↻ {reason}")),
            Event::Validation { ok, detail } => {
                if ok {
                    self.push(Kind::Notice, format!("✓ validation passed: {detail}"));
                } else {
                    self.push(Kind::Error, format!("✗ validation failed: {detail}"));
                }
            }
            Event::MemoryUsed { ids } => {
                self.push(Kind::Notice, memory_used_summary(&ids));
            }
            Event::Compaction {
                messages_before,
                messages_after,
                tokens_before,
                tokens_after,
            } => {
                self.push(
                    Kind::Notice,
                    format!(
                        "⟲ compacted context: ~{tokens_before} → ~{tokens_after} tokens                          ({messages_before} → {messages_after} messages)"
                    ),
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
                self.total_in_tokens += prompt_tokens as u64;
                self.total_out_tokens += completion_tokens as u64;
                self.last_reasoning_tokens = reasoning_tokens;
                self.step_reasoning_chars = 0;
                self.last_finish_reason = finish_reason;
            }
            Event::Warning { message } => self.push(Kind::Notice, format!("⚠ {message}")),
            Event::Error { message } => self.push(Kind::Error, message),
            Event::SessionStarted { id } => self.show_session_id(&id),
            Event::TurnComplete { .. } => {}
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
    // Prices and sampling for the session's model, for the footer's cost.
    model_settings: crate::config::ModelSettings,
    // Questions from the agent's task: "may I run this?". The agent blocks on
    // the answer, so this loop must always send one.
    approvals: tokio::sync::mpsc::Receiver<crate::tools::approval::ApprovalRequest>,
    asks: tokio::sync::mpsc::Receiver<crate::tools::approval::TextRequest>,
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
        model_settings,
        approvals,
        asks,
    )
    .await;
    restore_terminal(&mut terminal)?;
    res
}

type Term = Terminal<CrosstermBackend<Stdout>>;

fn setup_terminal() -> Result<Term> {
    enable_raw_mode().context("enabling raw mode")?;
    let mut out = io::stdout();
    // Capture the mouse, so the wheel scrolls the transcript.
    //
    // This used to be off, on the reasoning that capture takes drag events from
    // the terminal and kills click-and-drag selection. That reasoning was
    // wrong: **Shift+drag bypasses mouse capture** in every mainstream terminal
    // (iTerm2, Terminal.app, kitty, wezterm, gnome-terminal), which is how vim,
    // tmux and htop all capture the mouse and stay copy-able. Verified here.
    //
    // Leaving it off was not neutral either, which is the real reason this
    // changed. We run in the alternate screen, which has no scrollback, so the
    // wheel has nothing native to do — and iTerm2 and Terminal.app default to
    // *alternate scroll mode*, translating wheel-up/down into Up/Down arrows.
    // The composer maps those to prompt history. So an uncaptured wheel did not
    // scroll anything; it silently walked you through your own past prompts.
    //
    // `/mouse off` restores the old behaviour for anyone whose terminal does
    // not do Shift+drag.
    execute!(out, EnterAlternateScreen, EnableBracketedPaste, EnableMouseCapture)
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
    // Prices and sampling for the session's model, for the footer's cost.
    model_settings: crate::config::ModelSettings,
    mut approvals: tokio::sync::mpsc::Receiver<crate::tools::approval::ApprovalRequest>,
    mut asks: tokio::sync::mpsc::Receiver<crate::tools::approval::TextRequest>,
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
        .with_default_model(worker_model)
        .with_default_validate(
            config.agents_validate().map(str::to_string),
            bash_timeout,
        );

    let mut app = App::new(model, context_limit, validate_cmd);
    {
        let s = session.lock().await;
        app.session_path = s.path().to_path_buf();
        app.show_session_id(&s.id);
    }
    app.insert_escape = config.insert_escape();
    app.prices = model_settings.clone();
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
            if app.transcript.follow {
                app.transcript.scroll_up = 0;
            }

            // A grouped worker waits for its siblings so the parent gets one
            // combined report instead of N disconnected ones.
            match w.group.and_then(|g| workers.group_info(g).map(|(r, t)| (g, r.to_string(), t))) {
                Some((group, request, total)) => {
                    let Some(acc) =
                        record_in_group(&mut groups, group, &request, total, w)
                    else {
                        continue; // siblings still running
                    };
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
                        start_synthetic_turn(
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

        // Stream anything new from the worker being followed. Polling its
        // recorded log rather than subscribing to its bus keeps this working
        // after the worker finishes, and survives a slow reader.
        if let Some((id, from)) = app.tail.clone() {
            match workers.log_since(&id, from) {
                Some((lines, next, missed)) => {
                    if missed > 0 {
                        app.push(
                            Kind::Notice,
                            format!("[{id}] … {missed} lines scrolled past"),
                        );
                    }
                    for l in lines {
                        // `Kind::Tool` prepends its own "⚙ ", and the log
                        // line already carries one — which is where the
                        // "⚙ [w1] ⚙ bash" double glyph came from, and why the
                        // nesting looked like it meant parent/child when it was
                        // an accident. `Kind::Notice` adds no label, so the
                        // worker's own glyph stands alone, and the bar makes
                        // the nesting real.
                        app.push(Kind::Notice, format!("{id} │ {l}"));
                    }
                    app.tail = Some((id, next));
                }
                // The worker was dropped (e.g. /new); stop rather than spin.
                None => app.tail = None,
            }
        }

        // Rebuild the wrapped-row cache only if content/width changed, then draw.
        app.agents_running = workers.running_count();
        app.agents_queued = workers.queued_count();
        // Polled, not plumbed. Forwarding worker `Usage` onto the parent bus
        // would also feed `last_prompt_tokens`, and `ctx` would then flicker
        // between this session's window and whichever worker reported last —
        // a number that is actively wrong rather than merely absent. Context
        // belongs to one conversation; spend belongs to the run.
        app.agent_spend = workers.token_totals(&app.prices);
        let width = terminal.size().map(|s| s.width).unwrap_or(80);
        app.ensure_rows(width);
        terminal.draw(|f| ui(f, &app))?;

        tokio::select! {
            // Terminal input.
            maybe_ev = events.as_mut().unwrap().next(), if events.is_some() => {
                match maybe_ev {
                    Some(Ok(CEvent::Key(key))) => {
                        let mut key_ctx = KeyContext {
                            agent: &agent,
                            session: &session,
                            mem: &mem,
                            cwd: &cwd,
                            bash_timeout,
                            turn: &mut turn,
                            cancel: &mut cancel,
                            workers: &mut workers,
                            config: &config,
                        };
                        match handle_key(key, &mut app, &mut key_ctx).await? {
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
                            fanout = Some(PlannedFanOut {
                                planner: tokio::spawn(async move {
                                    plan_fanout(a, pf.task, pf.want, max).await
                                }),
                                system: pf.system,
                                request,
                                model: pf.model,
                                validate: pf.validate,
                            });
                        }
                    }
                    Some(Ok(CEvent::Mouse(m))) => match m.kind {
                        MouseEventKind::ScrollUp => app.scroll_up(3),
                        MouseEventKind::ScrollDown => app.scroll_down(3),
                        _ => {}
                    },
                    // Bracketed paste: insert the whole payload at the cursor
                    // (multi-line and all) instead of firing Enter per line.
                    Some(Ok(CEvent::Paste(text))) => app.composer.paste(&text),
                    Some(Ok(_)) => {} // resize etc — redraw next loop
                    Some(Err(_)) | None => break,
                }
            }

            // Agent events → transcript.
            ev = bus_rx.recv() => {
                match ev {
                    Ok(e) => {
                        app.apply_event(e);
                        if app.transcript.follow { app.transcript.scroll_up = 0; }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                }
            }

            // Memory extraction finished.
            res = join_in_flight(&mut extract), if extract.is_some() => {
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
                if app.transcript.follow { app.transcript.scroll_up = 0; }
            }

            // The agent is asking whether it may do something outward-facing.
            // It is blocked until this loop answers, so nothing else matters
            // until the user decides.
            Some(req) = approvals.recv(), if !app.modals.approval_pending() => {
                app.push(
                    Kind::Error,
                    format!("⚠ approve? {}\n  {}", req.reason, req.command),
                );
                app.status = "y = once · a = always this session · n = no".into();
                alert("approval needed");
                app.modals.set_approval(req);
                if app.transcript.follow { app.transcript.scroll_up = 0; }
                app.transcript.dirty = true;
            }

            // A pairing checkpoint. The turn is blocked on it, but the user is
            // free to ignore it — Esc skips, and the work carries on without
            // their answer rather than stalling.
            Some(req) = asks.recv(), if !app.modals.ask_pending() => {
                app.push(
                    Kind::Pair,
                    format!("{}\n  {}", req.subject, req.question),
                );
                app.status = "type your answer · Enter to send · Esc to skip".into();
                alert("waiting on you");
                app.modals.set_ask(req);
                if app.transcript.follow { app.transcript.scroll_up = 0; }
                app.transcript.dirty = true;
            }

            // Mining finished: file the proposals from here, where the store is.
            res = join_in_flight(&mut mine), if mine.is_some() => {
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
                if app.transcript.follow { app.transcript.scroll_up = 0; }
            }

            // Fan-out planning finished.
            res = join_planned_fanout(&mut fanout), if fanout.is_some() => {
                let PlannedFanOut { system, request, model, validate, .. } = fanout.take().unwrap();
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
                            // Every task repeats the same setup, because each has
                            // to stand alone. Say it once, so the truncation falls
                            // on the shared half rather than the distinct one.
                            // Size the cut to the terminal, not to a constant.
                            // 100 columns was invisible on a narrow terminal and
                            // threw away half a wide one — and the tail is the
                            // part that distinguishes one task from another.
                            // Two rows' worth, so a long task still says
                            // something without the list becoming the screen.
                            let budget = (app.transcript.cache_width.max(40) as usize)
                                .saturating_sub(6)
                                .saturating_mul(2);
                            match common_opening(&plan.tasks) {
                                Some((shared, tails)) => {
                                    app.push(
                                        Kind::Notice,
                                        format!("  all: {}…", truncate(&shared, budget)),
                                    );
                                    for (i, t) in tails.iter().enumerate() {
                                        app.push(
                                            Kind::Notice,
                                            format!("  {}. …{}", i + 1, truncate(t, budget)),
                                        );
                                    }
                                }
                                None => {
                                    for (i, t) in plan.tasks.iter().enumerate() {
                                        app.push(
                                            Kind::Notice,
                                            format!("  {}. {}", i + 1, truncate(t, budget)),
                                        );
                                    }
                                }
                            }
                        }
                        // Say the check out loud. A fan-out ran unchecked for
                        // weeks because a broken `--until` was silently dropped
                        // — nobody misses a check they were never shown.
                        app.push(
                            Kind::Notice,
                            match &validate {
                                Some(c) => format!("  check: {c}"),
                                None => "  check: none — nothing verifies these workers"
                                    .to_string(),
                            },
                        );
                        let report =
                            workers.spawn_many_checked(plan.tasks, system, request, model, validate);
                        app.push(Kind::Notice, fanout_notice(&report));
                    }
                    Err(e) => app.push(Kind::Error, format!("fan-out planning failed: {e}")),
                }
                if app.transcript.follow { app.transcript.scroll_up = 0; }
            }

            // Turn finished.
            res = join_in_flight(&mut turn), if turn.is_some() => {
                turn = None;
                app.running = false;
                app.turn_start = None;
                match res {
                    Ok(Ok(r)) => {
                        app.status = format!("[{}]", r.outcome.label());
                        // The footer says four words and the next keystroke
                        // takes them away. A turn that ended badly has to leave
                        // something the user can scroll back to — and say what
                        // to do about it, since "hit step limit" answers
                        // nothing on its own.
                        if let Some(advice) = r.outcome.advice() {
                            app.push(Kind::Notice, advice);
                        }
                    }
                    Ok(Err(e)) => app.push(Kind::Error, format!("turn error: {e:#}")),
                    Err(_) => app.push(Kind::Error, "turn task failed".to_string()),
                }

                // Steering that arrived too late to be drained would otherwise
                // sit in the mailbox until some later turn happened to start.
                // The user pressed Enter; that has to produce an answer.
                let late = agent.steering().drain();
                if !late.is_empty() {
                    app.push(
                        Kind::Notice,
                        "(that arrived as the turn ended — starting a new one)".to_string(),
                    );
                    start_turn(
                        late.join("\n"),
                        &mut app,
                        &agent,
                        &session,
                        &mem,
                        &cwd,
                        bash_timeout,
                        &mut turn,
                        &mut cancel,
                    );
                }
            }

            // Spinner animation while a turn runs, and the loop's only heartbeat
            // while background work is alive. Gated so an idle UI doesn't force
            // a full redraw 8×/sec (the ticker is always ready; without a guard
            // every tick re-wraps and re-draws).
            //
            // `agents_running` is in the condition because the top of this loop
            // is where `take_newly_finished` surfaces a finished worker and
            // `pump` starts a queued one. Gated on `running` alone, neither
            // happens while the session sits idle: a worker finishes, nothing
            // wakes the loop, and the user sees nothing until they press a key.
            // Observed exactly that — a worker stopped, the transcript said
            // nothing, and `/agents` "fixed" it because typing woke the loop.
            //
            // `agents_queued` too, and not only for symmetry: the last running
            // worker finishing takes the count to zero from inside its own
            // task, so a loop gated on `running` alone would then sleep with a
            // full queue and never start any of it.
            _ = ticker.tick(),
                if app.running || app.agents_running > 0 || app.agents_queued > 0 => {
                app.spinner = app.spinner.wrapping_add(1);
            }
        }

        // Ctrl+G: suspend the TUI, edit the composer in $EDITOR, resume.
        if pending_edit {
            pending_edit = false;
            restore_terminal(terminal).ok();
            drop(events.take()); // stop the input reader so the editor owns the tty
            let edited = external_edit(&app.composer.input);
            *terminal = setup_terminal()?;
            events = Some(EventStream::new());
            terminal.clear().ok();
            app.transcript.dirty = true;
            if let Some(text) = edited {
                app.composer.set_input(text);
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

fn handle_approval_key(key: KeyEvent, app: &mut App, ctrl: bool) -> Option<Flow> {
    match app.modals.answer_approval_key(key, ctrl)? {
        ApprovalKey::WaitingForAnswer => {
            app.status = "y = once · a = always this session · n = no".into();
            Some(Flow::Continue)
        }
        ApprovalKey::Answered { note, quit } => {
            app.push(Kind::Notice, note.to_string());
            app.status = "/help for keys and commands".into();
            app.transcript.dirty = true;
            if quit {
                Some(Flow::Quit)
            } else {
                Some(Flow::Continue)
            }
        }
    }
}

fn handle_overlay_key(key: KeyEvent, app: &mut App, ctrl: bool) -> Option<Flow> {
    let mut close_overlay = false;
    let mut chosen_label = None;
    {
        let ov = app.overlay.as_mut()?;
        match key.code {
            KeyCode::Esc => close_overlay = true,
            KeyCode::Up => ov.move_by(-1),
            KeyCode::Down => ov.move_by(1),
            KeyCode::Char('p') if ctrl => ov.move_by(-1),
            KeyCode::Char('n') if ctrl => ov.move_by(1),
            KeyCode::Char('c') if ctrl => return Some(Flow::Quit),
            KeyCode::Backspace => ov.pop_filter(),
            KeyCode::Enter => {
                let (chosen, picking) = (ov.chosen(), ov.picking);
                close_overlay = true;
                // A reference has nothing to pick, so Enter just closes.
                if picking {
                    chosen_label = chosen;
                }
            }
            KeyCode::Char(c) if !ctrl => ov.push_filter(c),
            _ => {}
        }
    }
    if close_overlay {
        app.overlay = None;
    }
    if let Some(label) = chosen_label {
        // Put it in the composer rather than running it: a picker that fires
        // commands on Enter is a picker you cannot use to *look* at something.
        app.composer.set_input(format!("{label} "));
    }
    app.transcript.dirty = true;
    Some(Flow::Continue)
}

fn handle_search_key(key: KeyEvent, app: &mut App, ctrl: bool) -> Option<Flow> {
    if !app.transcript.search.as_ref().is_some_and(|s| s.typing) {
        return None;
    }
    match key.code {
        KeyCode::Esc => app.set_search(None),
        KeyCode::Enter => {
            app.mutate_search(|s| s.typing = false);
            if !app.jump_match(true) {
                let p = app.transcript.search.as_ref().map(|s| s.pattern.clone()).unwrap_or_default();
                app.status = format!("no match for `{p}`");
                app.set_search(None);
            }
        }
        KeyCode::Backspace => {
            app.mutate_search(|s| {
                s.pattern.pop();
            });
        }
        KeyCode::Char(c) if !ctrl => app.mutate_search(|s| s.pattern.push(c)),
        _ => {}
    }
    Some(Flow::Continue)
}

fn handle_normal_key(key: KeyEvent, app: &mut App, ctrl: bool) -> Result<Option<Flow>> {
    if app.transcript.mode != Mode::Normal {
        return Ok(None);
    }

    // A search being typed takes precedence: it is a prompt, not a mode.
    if let Some(flow) = handle_search_key(key, app, ctrl) {
        return Ok(Some(flow));
    }

    match key.code {
        KeyCode::Char('c') if ctrl => return Ok(Some(Flow::Quit)),
        // Back to typing. Several routes, because being stuck in a mode is
        // the failure people remember.
        KeyCode::Char('i') | KeyCode::Char('a') | KeyCode::Enter | KeyCode::Esc => {
            app.enter_insert();
            app.status = "insert".into();
        }
        KeyCode::Char('j') | KeyCode::Down => app.cursor_by(1),
        KeyCode::Char('k') | KeyCode::Up => app.cursor_by(-1),
        KeyCode::Char('d') if ctrl => app.cursor_by(10),
        KeyCode::Char('u') if ctrl => app.cursor_by(-10),
        KeyCode::PageDown => app.cursor_by(20),
        KeyCode::PageUp => app.cursor_by(-20),
        KeyCode::Char('G') => app.cursor_by(isize::MAX / 2),
        KeyCode::Char('g') => {
            // `gg` in vim; a single `g` here, since there is no other g-verb
            // to disambiguate from and a hidden two-key chord is worse.
            app.transcript.cursor_row = 0;
        }
        KeyCode::Char('/') => app.set_search(Some(Search { pattern: String::new(), typing: true })),
        KeyCode::Char('n') => {
            if !app.jump_match(true) {
                app.status = "no matches".into();
            }
        }
        KeyCode::Char('N') => {
            if !app.jump_match(false) {
                app.status = "no matches".into();
            }
        }
        // Yank the whole item under the cursor, not the wrapped row: what you
        // want is the message, the tool output, the code block.
        KeyCode::Char('y') => match app.item_at_row(app.transcript.cursor_row) {
            Some(i) => {
                let text = app.transcript.items[i].text.clone();
                match copy_to_clipboard(&text) {
                    Ok(()) => {
                        let n = text.lines().count();
                        app.status = format!("yanked {n} lines");
                    }
                    Err(e) => app.push(Kind::Error, format!("clipboard: {e}")),
                }
            }
            None => app.status = "nothing to yank".into(),
        },
        _ => {}
    }
    Ok(Some(Flow::Continue))
}

fn handle_hint_key(key: KeyEvent, app: &mut App) -> Option<Flow> {
    app.composer.hint.as_ref()?;
    match key.code {
        // Up/Down browse the list rather than input history: while typing a
        // command, the list is what you are looking at.
        KeyCode::Up => {
            app.composer.hint.as_mut()?.move_by(-1);
            app.transcript.dirty = true;
            Some(Flow::Continue)
        }
        KeyCode::Down => {
            app.composer.hint.as_mut()?.move_by(1);
            app.transcript.dirty = true;
            Some(Flow::Continue)
        }
        // Enter takes the highlighted row too. Falling through to the command
        // handler ran the half-typed text and answered "unknown command:
        // /agen", which is the opposite of what a visible, highlighted list
        // implies pressing Enter will do. An exactly-typed command still runs,
        // so muscle memory for `/help<Enter>` survives.
        KeyCode::Enter if hint_enter_accepts(&app.composer.input) => accept_hint(app),
        // Tab accepts the highlighted command. This is better than the old
        // blind prefix-cycling: you can see what you are accepting.
        KeyCode::Tab => accept_hint(app),
        // First Esc dismisses the list; a second one clears the composer, so
        // Esc never does two things at once.
        KeyCode::Esc => {
            app.composer.hint = None;
            app.transcript.dirty = true;
            Some(Flow::Continue)
        }
        _ => None,
    }
}

fn accept_hint(app: &mut App) -> Option<Flow> {
    let label = app.composer.hint.as_ref().and_then(|h| h.chosen())?;
    app.composer.set_input(format!("{label} "));
    app.composer.hint = None;
    app.transcript.dirty = true;
    Some(Flow::Continue)
}

struct KeyContext<'a> {
    agent: &'a Arc<Agent>,
    session: &'a Arc<AsyncMutex<Session>>,
    mem: &'a MemoryStore,
    cwd: &'a Path,
    bash_timeout: Duration,
    turn: &'a mut Option<JoinHandle<Result<TurnResult>>>,
    cancel: &'a mut CancellationToken,
    workers: &'a mut WorkerManager,
    config: &'a Config,
}

fn answer_pending_ask(app: &mut App, answer: Option<String>) -> bool {
    let Some(answer) = app.modals.answer_ask(answer) else {
        return false;
    };

    match answer {
        AskAnswer::Answered(input) => app.push(Kind::Pair, format!("you ▸ {input}")),
        AskAnswer::Skipped => app.push(Kind::Pair, "skipped".to_string()),
    }
    app.status = "/help for keys and commands".into();
    true
}

async fn handle_enter_key(app: &mut App, ctx: &mut KeyContext<'_>) -> Result<Flow> {
    let raw = app.composer.take_input();
    let input = raw.trim().to_string();
    if input.is_empty() {
        // Empty input with a pending checkpoint: answer with None (skip).
        if answer_pending_ask(app, None) {
            return Ok(Flow::Continue);
        }
        // Empty input with nothing pending: Enter does nothing. Without this
        // return it falls through to `start_turn` and spends a model call on an
        // empty prompt, which is what every stray Enter in a terminal would then
        // cost.
        return Ok(Flow::Continue);
    }

    // Answering a checkpoint, not starting a turn. Checked before the command
    // dispatch below so an answer that happens to start with a slash is still
    // an answer.
    if app.modals.ask_pending() {
        answer_pending_ask(app, Some(input));
        return Ok(Flow::Continue);
    }

    // Commands (start with '/', or bare quit/exit).
    if input == "/quit" || input == "/exit" || input == "quit" || input == "exit" {
        return Ok(Flow::Quit);
    }
    if input.starts_with('/') {
        let mut command_ctx = CommandContext::from(&mut *ctx);
        if handle_command(&input, app, &mut command_ctx).await? {
            // This return skips the `refresh_hint()` at the bottom of
            // `handle_insert_key` — the one whose comment says no edit path can
            // forget it. Running a command *is* an edit path: the composer is now
            // empty, so the list of matching commands is stale, and it hung on
            // screen until the next keystroke or an Esc. Reported as "/agents shows
            // the list and the popup will not go away".
            app.composer.refresh_hint();
            return Ok(Flow::Continue);
        }
    }
    let message = expand_file_mentions(&input, ctx.cwd);

    // Mid-turn, this is *steering*: the agent drains its mailbox at the top of
    // every step, so the message lands before the next model call. Previously
    // the composer was cleared and the text thrown away with a "a turn is
    // already running" notice — typing a correction while the model worked
    // simply destroyed it, in a harness whose stated bet is human-in-the-loop.
    if app.running {
        ctx.agent.steering().push(message);
        app.push(Kind::User, format!("↳ {input}"));
        app.status = "sent — the model sees it at its next step".into();
        if app.transcript.follow {
            app.transcript.scroll_up = 0;
        }
        return Ok(Flow::Continue);
    }

    start_turn(
        message,
        app,
        ctx.agent,
        ctx.session,
        ctx.mem,
        ctx.cwd,
        ctx.bash_timeout,
        ctx.turn,
        ctx.cancel,
    );
    app.composer.refresh_hint();
    Ok(Flow::Continue)
}

async fn handle_insert_key(
    key: KeyEvent,
    app: &mut App,
    ctx: &mut KeyContext<'_>,
    ctrl: bool,
) -> Result<Flow> {
    // Any key other than Tab ends an in-progress completion cycle.
    if key.code != KeyCode::Tab {
        app.composer.clear_completion();
    }

    match key.code {
        KeyCode::Char('c') if ctrl => return Ok(Flow::Quit),
        KeyCode::Tab => complete(app, ctx.cwd, ctx.mem, ctx.config),
        KeyCode::Char('o') if ctrl => {
            app.transcript.collapse_tools = !app.transcript.collapse_tools;
            // Changes how *every* item renders, not just the tail.
            app.touch_all();
            app.status = format!(
                "tool output {}",
                if app.transcript.collapse_tools { "collapsed" } else { "expanded" }
            );
        }
        KeyCode::Char('t') if ctrl => {
            app.transcript.show_thinking = !app.transcript.show_thinking;
            app.touch_all();
            app.status = format!(
                "thinking {}",
                if app.transcript.show_thinking { "shown" } else { "hidden" }
            );
        }
        KeyCode::Char('p') if ctrl => {
            app.status = "model cycling: configure multiple models (coming soon)".into();
        }
        KeyCode::Char('g') if ctrl => return Ok(Flow::ExternalEdit),
        KeyCode::Esc => {
            // Skipping a checkpoint outranks the other Esc meanings: the turn
            // is blocked on it, and "abort the turn" is not what someone who
            // just wants to move on is reaching for.
            if answer_pending_ask(app, None) {
                return Ok(Flow::Continue);
            } else if app.running {
                ctx.cancel.cancel();
                app.status = "aborting…".into();
            } else if app.composer.input.is_empty() {
                // Nothing to clear, so Esc means "stop typing, start reading".
                // Never steals an Esc that had a job to do.
                app.enter_normal();
                app.status = "normal · j k /search n N · y yank · i insert".into();
            } else {
                app.composer.clear_input();
            }
        }
        // Transcript scrolling (mouse wheel also works).
        KeyCode::PageUp => app.scroll_up(10),
        KeyCode::PageDown => app.scroll_down(10),
        KeyCode::Char('u') if ctrl => app.scroll_up(10),
        KeyCode::Char('d') if ctrl => app.scroll_down(10),
        // Composer editing.
        KeyCode::Up => app.composer.history_prev(),
        KeyCode::Down => app.composer.history_next(),
        KeyCode::Left => app.composer.move_left(),
        KeyCode::Right => app.composer.move_right(),
        KeyCode::Home => app.composer.move_home(),
        KeyCode::End => app.composer.move_end(),
        // Readline bindings, because every other text box in a terminal has
        // them and the hands go there without asking. Ctrl+U and Ctrl+D are
        // already transcript scrolling, so the kill-line pair is deliberately
        // absent rather than fighting over the keys.
        KeyCode::Char('a') if ctrl => app.composer.move_home(),
        KeyCode::Char('e') if ctrl => app.composer.move_end(),
        KeyCode::Char('w') if ctrl => app.composer.delete_word(),
        KeyCode::Backspace => app.composer.backspace(),
        // Alt/Shift+Enter inserts a newline when the terminal reports the
        // modifier. Many macOS terminals send Option+Enter as plain Enter, so
        // Ctrl+N is the portable fallback.
        _ if key_inserts_newline(&key) => {
            app.composer.insert_char('\n');
        }
        KeyCode::Enter => return handle_enter_key(app, ctx).await,
        // Ignore control-chords; accept normal (and shifted) chars at the cursor.
        KeyCode::Char(c) if !ctrl => {
            if app.escape_pair(c) {
                app.enter_normal();
                app.status = "normal · j k /search n N · y yank · i insert".into();
                app.composer.refresh_hint();
                return Ok(Flow::Continue);
            }
            app.composer.insert_char(c);
        }
        _ => {}
    }
    // One place, so no edit path can forget it.
    app.composer.refresh_hint();
    Ok(Flow::Continue)
}

async fn handle_key(key: KeyEvent, app: &mut App, ctx: &mut KeyContext<'_>) -> Result<Flow> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    // A pending approval owns the keyboard. The agent's task is blocked waiting
    // for the answer, so typing into the composer here would look like a hang;
    // and an approval answered by accident is the failure this exists to stop.
    if let Some(flow) = handle_approval_key(key, app, ctrl) {
        return Ok(flow);
    }

    // A picker owns the keyboard while it is up. Esc always returns to the
    // composer with whatever was typed still there — a modal you can get stuck
    // in is worse than no modal.
    if let Some(flow) = handle_overlay_key(key, app, ctrl) {
        return Ok(flow);
    }

    // Normal mode owns the alphabet. Nothing here is reachable unless you
    // deliberately entered it, and every route out is one key.
    if let Some(flow) = handle_normal_key(key, app, ctrl)? {
        return Ok(flow);
    }

    // The as-you-type hint is not modal — it only claims the keys it needs, and
    // only while it is visible.
    if let Some(flow) = handle_hint_key(key, app) {
        return Ok(flow);
    }

    handle_insert_key(key, app, ctx, ctrl).await
}

struct CommandContext<'a> {
    agent: &'a Arc<Agent>,
    session: &'a Arc<AsyncMutex<Session>>,
    mem: &'a MemoryStore,
    cwd: &'a Path,
    workers: &'a mut WorkerManager,
    config: &'a Config,
}

impl<'a, 'b> From<&'a mut KeyContext<'b>> for CommandContext<'a> {
    fn from(ctx: &'a mut KeyContext<'b>) -> Self {
        Self {
            agent: ctx.agent,
            session: ctx.session,
            mem: ctx.mem,
            cwd: ctx.cwd,
            workers: ctx.workers,
            config: ctx.config,
        }
    }
}

/// Returns true if the input was a recognized command (already handled).
async fn handle_command(input: &str, app: &mut App, ctx: &mut CommandContext<'_>) -> Result<bool> {
    let agent = ctx.agent;
    let session = ctx.session;
    let mem = ctx.mem;
    let cwd = ctx.cwd;
    let workers = &mut *ctx.workers;
    let config = ctx.config;
    let mut parts = input.trim_start_matches('/').split_whitespace();
    let head = parts.next().unwrap_or("");
    match head {
        "help" | "h" if parts.clone().next().is_none() => {
            // The command list as something you can filter and read, rather than
            // a wall you scroll back through. `/help keys` still prints it all.
            let items = COMMANDS
                .iter()
                .map(|(name, desc)| OverlayItem {
                    label: (*name).to_string(),
                    description: (*desc).to_string(),
                })
                .collect();
            app.overlay = Some(Overlay::new("commands · type to filter", items));
        }
        "help" | "h" if parts.clone().next().map(|s| s.to_ascii_lowercase()) == Some("footer".to_string()) => {
            // The footer's glyphs are unguessable (its own author had to ask);
            // a legend explains them without changing the footer. A reference,
            // not a picker: there is nothing to select.
            app.overlay = Some(Overlay::reference("footer legend · Esc close", footer_legend()));
        }
        "help" | "h" => {
            // One wrapped paragraph of everything was unreadable. Group it, put
            // one thing per line, and align the descriptions.
            app.push(
                Kind::Notice,
                "\
KEYS
  Esc / jj       (empty composer) read the transcript — see NORMAL below
  Enter          send — or steer the running turn
  Ctrl+N         newline             Alt/Shift+Enter if your terminal reports it
  Tab            complete             Ctrl+G       edit in $EDITOR
  Esc            abort / clear        Ctrl+C       quit
  Ctrl+O         show tool output     Ctrl+T       show thinking
  PageUp/Down    scroll               Ctrl+U/D     scroll
  /help footer   what the footer's glyphs mean

NORMAL MODE (Esc on an empty composer, or `jj`)
  j k  ↑ ↓       move            Ctrl+U/D   half page
  g  G           top / bottom    PageUp/Dn  page
  /              search          n  N       next / previous match
  y              yank the message under the cursor to the clipboard
  i  Enter  Esc  back to typing

SESSION
  /new                                start a fresh session
  /compact                            summarize the history now
  /validate <cmd|off>                 command that must pass before a turn is done
  /quit

MODEL
  /fast [on|off|auto]                 answer without thinking first
  /think [on|off|auto|low|high|<n>]   how hard to think: a level or a token budget
  /route [throughput|latency|price]   which provider serves you (OpenRouter)
  /model [provider/model|default]     switch model for this session (or list)
  /pair [on|off]                      stop at decisions: ask you, tell you why,
                                      or hand you the hard part. Main loop only —
                                      spawned workers never interrupt you.

MEMORY
  /memory                             list what is remembered
  /memory search <query>              search it
  /memory extract                     distill this session into proposals
  /memory mine [n]                    mine past sessions of this project
  /memory pending                     review proposals
  /memory approve <id|all>            accept one, or all of them
  /memory forget <id>                 delete one
  /memory show <id>                   show one in full
  /memory add <scope> <kind> <subject> <content...>

KNOWLEDGE & SKILLS
  /knowledge [index|search <query>]   the project's own docs and source
  /skill [name]                       load a skill

WORKERS
  /spawn [-n N | --each-files <re>] [--model <spec>] [--until <check>] <task>
        quote a multi-word check: --until `cargo test`. A fan-out check runs in
        every worker at once in one directory, so it must be read-only.
  /agents                             list them
  /agents tail <id>                   watch one work, live
  /agents show <id>                   its result once finished
  /agents [kill|nudge <id>|drop-queued]

PROJECT
  /trust                              is this project's own config in effect?
  /trust revoke                       decide again on the next start

TERMINAL
  /mouse [on|off]                     wheel scrolls the transcript (on by
                                      default; Shift+drag still selects text)

Ids accept any unique prefix, and Tab completes them. @path includes a file."
                    .to_string(),
            );
        }
        "new" => {
            if app.running {
                // The turn holds the session lock; awaiting it here would hang
                // the event loop until the turn ends (same freeze /history had).
                app.status = "can't start a new session while a turn is running".into();
                return Ok(true);
            }
            let mut s = session.lock().await;
            *s = Session::create(cwd)?;
            app.reset_for_new_session(s.path().to_path_buf());
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
                                Some(PendingFanOut {
                                task: req.task,
                                want,
                                system,
                                model: over,
                                validate: req.validate.clone(),
                            });
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
                                    let report = workers.spawn_many_checked(
                                        tasks,
                                        system,
                                        req.task.clone(),
                                        over,
                                        req.validate.clone(),
                                    );
                                    app.push(Kind::Notice, fanout_notice(&report));
                                }
                            }
                        }
                        // -n 1 (or an explicit single): today's path, no planner.
                        _ => match workers.spawn_checked(req.task.clone(), system, over, req.validate.clone()) {
                            Ok(outcome) => app.push(Kind::Notice, spawn_notice(&outcome, &req.task)),
                            Err(e) => app.push(Kind::Error, format!("spawn failed: {e}")),
                        },
                    }
                }
            }
        }
        "agents" | "workers" => agents_command(app, workers, parts),
        // What the loop did, and when. Reconstructing this from messages meant
        // reading the provider's logs and correlating timestamps by hand.
        "history" | "trace" => {
            let id = parts.next().map(str::to_string);
            // Never take the session lock here: the agent holds it for the
            // whole turn, and this runs on the event loop. /history froze the
            // TUI mid-run for exactly that reason. The JSONL is append-only,
            // so reading the file while the turn writes it is safe.
            let path = match &id {
                Some(id) => crate::session::Session::path_for_id(id).ok(),
                None => Some(app.session_path.clone()),
            };
            let Some(path) = path else {
                app.push(Kind::Error, "no such session".to_string());
                return Ok(true);
            };
            match crate::session::events(&path) {
                Ok(evs) if evs.is_empty() => app.push(
                    Kind::Notice,
                    "(no events recorded — a session from before they were kept)".to_string(),
                ),
                Ok(evs) => {
                    let start = evs.first().map(|e| e.ts).unwrap_or(0);
                    for e in evs.iter().rev().take(60).rev() {
                        app.push(
                            Kind::Notice,
                            format!("  +{:>4}s  {}", e.ts.saturating_sub(start), describe(&e.event)),
                        );
                    }
                    app.push(
                        Kind::Notice,
                        format!("{} events · /history <session-id> for a worker", evs.len()),
                    );
                }
                Err(e) => app.push(Kind::Error, format!("history: {e}")),
            }
        }
        "knowledge" | "know" => knowledge_command(app, cwd, parts),
        "skill" | "skills" => skill_command(app, cwd, parts),
        "fast" | "lucky" => fast_command(app, agent, parts),
        "think" => think_command(app, agent, parts),
        "trust" => trust_command(app, cwd, parts),
        "pair" => pair_command(app, agent, parts),
        "route" => route_command(app, agent, parts),
        "model" => {
            // A session-scoped switch: it retargets the running agent and the
            // footer's model/window/prices, but never writes config.toml. The
            // model is swapped as a *set* (client + window + sampling), so a
            // new model never runs on the previous one's numbers — the same
            // half-swap `ModelOverride::resolve` guards against at startup.
            //
            // The switch records into the session, so it takes the lock; a turn
            // holds that lock for its whole life, and awaiting it here would
            // freeze the loop (the same trap /new and /history hit).
            if app.running {
                app.status = "can't switch model while a turn is running".into();
                return Ok(true);
            }
            match parts.next() {
                None => {
                    let lines = model_list(&app.model, &config.models);
                    if lines.is_empty() {
                        app.push(
                            Kind::Notice,
                            "no [models.\"] entries configured — /model <provider/model> \
                             switches to one that is"
                                .to_string(),
                        );
                    } else {
                        app.push(Kind::Notice, lines.join("\n"));
                    }
                }
                Some("default") => {
                    // Revert to the configured default, whatever it is.
                    let Some(spec) = config.model.clone() else {
                        app.push(
                            Kind::Error,
                            "no default model configured — set `model` in config.toml".to_string(),
                        );
                        return Ok(true);
                    };
                    switch_model(app, agent, session, config, &spec).await;
                }
                Some(spec) => switch_model(app, agent, session, config, spec).await,
            }
        }
        "mouse" => mouse_command(app, &mut io::stdout(), parts),
        "validate" => validate_command(app, parts),
        _ => {
            app.push(Kind::Error, format!("unknown command: /{head}"));
        }
    }
    Ok(true)
}

fn validate_command<'a>(app: &mut App, parts: impl Iterator<Item = &'a str>) {
    let rest = parts.collect::<Vec<_>>().join(" ");
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

fn pair_command<'a>(
    app: &mut App,
    agent: &Agent,
    mut parts: impl Iterator<Item = &'a str>,
) {
    match parts.next() {
        None => app.push(Kind::Notice, pair_status(agent.pairing_on())),
        Some("on") => {
            agent.set_pairing(true);
            app.push(Kind::Notice, pair_status(true));
        }
        Some("off") => {
            agent.set_pairing(false);
            app.push(Kind::Notice, pair_status(false));
        }
        Some(other) => app.push(
            Kind::Error,
            format!("usage: /pair [on|off] (got {other})"),
        ),
    }
}

fn pair_status(on: bool) -> &'static str {
    if on {
        "pairing on — the loop will stop at decisions worth your say. Spawned workers never will."
    } else {
        "pairing off — the checkpoint is no longer offered to the model"
    }
}

fn route_command<'a>(app: &mut App, agent: &Agent, mut parts: impl Iterator<Item = &'a str>) {
    // Deliberately not folded into /fast. `sort` changes *which provider*
    // serves the request, and OpenRouter endpoints differ in quantization and
    // price. A speed button that silently swaps your backend is a surprise, not
    // a feature.
    match parts.next() {
        None => app.push(Kind::Notice, route_status(app.route.as_deref())),
        Some("auto") | Some("default") => {
            app.route = None;
            agent.set_route(None);
            app.push(Kind::Notice, "routing left to the provider".to_string());
        }
        Some(v @ ("throughput" | "latency" | "price")) => {
            let route = v.to_string();
            app.route = Some(route.clone());
            agent.set_route(Some(route));
            app.push(
                Kind::Notice,
                format!("routing on {v} — takes effect on the next turn"),
            );
        }
        Some(other) => app.push(
            Kind::Error,
            format!("usage: /route [throughput|latency|price|auto] (got {other})"),
        ),
    }
}

fn route_status(route: Option<&str>) -> String {
    match route {
        Some(s) => format!("routing: {s} (OpenRouter only)"),
        None => "routing: the provider's default (OpenRouter sorts on price)".to_string(),
    }
}

fn fast_command<'a>(app: &mut App, agent: &Agent, parts: impl Iterator<Item = &'a str>) {
    let mode = agent.thinking_mode();
    let rest: Vec<&str> = parts.collect();
    match rest.first().copied() {
        Some("on") => mode.set(Some(Thinking::Off)),
        Some("off") => mode.set(Some(Thinking::On)),
        Some("auto") => mode.set(None),
        Some(other) => {
            app.push(
                Kind::Error,
                format!("usage: /fast [on|off|auto] (got {other})"),
            );
            return;
        }
        None => {
            mode.toggle_fast();
        }
    }
    app.think_label = mode.label();
    app.push(Kind::Notice, fast_status(mode.get()));
}

fn fast_status(thinking: Option<Thinking>) -> String {
    match thinking {
        Some(Thinking::Off) => "fast mode on — answering without thinking first".to_string(),
        Some(Thinking::On) => "fast mode off — thinking before answering".to_string(),
        Some(Thinking::Budget(n)) => format!("thinking capped at {n} tokens"),
        Some(Thinking::Effort(e)) => format!("thinking effort: {}", e.as_str()),
        None => "thinking left to the provider's default".to_string(),
    }
}

fn think_command<'a>(app: &mut App, agent: &Agent, parts: impl Iterator<Item = &'a str>) {
    let mode = agent.thinking_mode();
    let rest: Vec<&str> = parts.collect();
    // A budget is the setting between "as long as it likes" and "not at all":
    // the reasoning gets its own cap, so it can't eat the whole output budget
    // and leave nothing for an answer.
    let set = match rest.first().copied() {
        None | Some("on") => Some(Thinking::On),
        Some("off") => Some(Thinking::Off),
        Some("auto") => None,
        Some(n) => match crate::llm::Effort::parse(n) {
            Some(e) => Some(Thinking::Effort(e)),
            None => match parse_budget(n) {
                Some(n) => Some(Thinking::Budget(n)),
                None => {
                    app.push(Kind::Error, think_usage(n));
                    return;
                }
            },
        },
    };
    mode.set(set);
    app.think_label = mode.label();
    app.push(Kind::Notice, think_status(set));
}

fn think_status(thinking: Option<Thinking>) -> String {
    match thinking {
        Some(Thinking::Off) => "thinking off — answering directly".to_string(),
        Some(Thinking::On) => "thinking on, uncapped".to_string(),
        Some(Thinking::Budget(n)) => {
            format!("thinking capped at {n} tokens, leaving the rest of max-tokens for the answer")
        }
        Some(Thinking::Effort(e)) => {
            format!("thinking effort: {} (the provider's own scale)", e.as_str())
        }
        None => "thinking left to the provider's default".to_string(),
    }
}

fn think_usage(got: &str) -> String {
    format!(
        "usage: /think [on|off|auto|<effort>|<tokens>] (got {got}). \
         Efforts: minimal, low, medium, high, xhigh, max — though \
         servers differ on which they accept."
    )
}

fn trust_command<'a>(app: &mut App, cwd: &Path, parts: impl Iterator<Item = &'a str>) {
    let mut store = crate::trust::TrustStore::load();
    trust_command_with_store(app, cwd, &mut store, parts);
}

fn trust_command_with_store<'a>(
    app: &mut App,
    cwd: &Path,
    store: &mut crate::trust::TrustStore,
    mut parts: impl Iterator<Item = &'a str>,
) {
    let Some(prompt) = crate::trust::prompt_for(cwd, store) else {
        app.push(
            Kind::Notice,
            "this project has no .worksmith/config.toml".to_string(),
        );
        return;
    };

    match parts.next() {
        // Revoking is the point of having the command: a decision you cannot
        // revisit is one you will make carelessly.
        Some("revoke") | Some("forget") => {
            if store.revoke(cwd) {
                app.push(
                    Kind::Notice,
                    "forgot this project's trust decision — worksmith will ask again next start"
                        .to_string(),
                );
            } else {
                app.push(
                    Kind::Notice,
                    "(no decision recorded for this project)".to_string(),
                );
            }
        }
        Some(other) => app.push(Kind::Error, format!("usage: /trust [revoke] (got {other})")),
        None => {
            app.push(
                Kind::Notice,
                format!(
                    "{}\n{}",
                    prompt.config_path.display(),
                    trust_state(store.decision_for(cwd, &prompt.fingerprint)),
                ),
            );
            for (key, value, why) in &prompt.settings {
                app.push(
                    Kind::Notice,
                    match why {
                        Some(w) => format!("  ! {key} = {value}\n      {w}"),
                        None => format!("    {key} = {value}"),
                    },
                );
            }
            app.push(
                Kind::Notice,
                "/trust revoke to decide again on the next start".to_string(),
            );
        }
    }
}

fn trust_state(decision: Option<crate::trust::Decision>) -> &'static str {
    match decision {
        Some(crate::trust::Decision::Trust) => "trusted — its config is in effect",
        Some(crate::trust::Decision::Ignore) => "ignored — running on your global config",
        None => "undecided — its config is NOT in effect",
    }
}

fn mouse_command<'a>(
    app: &mut App,
    out: &mut impl io::Write,
    mut parts: impl Iterator<Item = &'a str>,
) {
    let want = match parts.next() {
        Some("on") => true,
        Some("off") => false,
        None => !app.mouse,
        Some(other) => {
            app.push(Kind::Error, format!("usage: /mouse [on|off] (got {other})"));
            return;
        }
    };

    let res = if want {
        execute!(out, EnableMouseCapture)
    } else {
        execute!(out, DisableMouseCapture)
    };
    match res {
        Ok(()) => {
            app.mouse = want;
            app.push(Kind::Notice, mouse_status(want));
        }
        Err(e) => app.push(Kind::Error, format!("mouse: {e}")),
    }
}

fn mouse_status(on: bool) -> &'static str {
    if on {
        "mouse capture on — the wheel scrolls the transcript. Shift+drag still selects text to copy."
    } else {
        "mouse capture off — the terminal owns the wheel again. In the alternate screen that usually means it sends Up/Down, which walks prompt history; PageUp/PageDown and Ctrl+U/Ctrl+D scroll."
    }
}

/// The lines for a bare `/model`: the configured `provider/model` entries,
/// sorted, with the one serving the session marked `*`.
///
/// `App.model` holds the bare model name (the part after the "/") — the
/// footer shows `resolved.model`, not the key — so an entry is marked when
/// its model part matches. If two providers shared a bare name both would be
/// marked: a degenerate config, and marking both is more honest than
/// guessing. Empty when nothing is configured; the caller then reports that.
fn model_list(current: &str, models: &std::collections::HashMap<String, crate::config::ModelSettings>) -> Vec<String> {
    let mut specs: Vec<&String> = models.keys().collect();
    specs.sort();
    specs
        .iter()
        .map(|spec| {
            let is_current = spec.split_once('/').map(|(_, m)| m) == Some(current);
            let mark = if is_current { "*" } else { " " };
            format!("{mark} {spec}")
        })
        .collect()
}

/// Switch the session's model to `spec` (`provider/model`, or a bare model
/// when one provider is configured). Session-scoped: it retargets the running
/// agent and the footer's model/window/prices, but never writes config.toml.
///
/// The model is swapped as a *set* (client + window + sampling) via
/// `ActiveModel::from`, so a new model never runs on the previous one's
/// numbers — the same half-swap `ModelOverride::resolve` guards against at
/// startup. `last_prompt_tokens` is zeroed with the model, both on the agent
/// (which `set_model` does) and on the footer's copy here: a 200k-model count
/// carried into a 32k model saturates the gauge and makes compaction fire
/// every step.
///
/// A resolve failure leaves the model unchanged and reports the error. A
/// missing API key is a warning, not a failure: `client_for` builds the client
/// anyway and the request simply goes out unauthenticated, exactly as at
/// startup.
async fn switch_model(
    app: &mut App,
    agent: &Arc<Agent>,
    session: &Arc<AsyncMutex<Session>>,
    config: &Config,
    spec: &str,
) {
    let over = match ModelOverride::resolve(config, spec) {
        Ok(o) => o,
        Err(e) => {
            app.push(Kind::Error, format!("model: {e:#}"));
            return;
        }
    };
    let from = agent.current().model;
    let to = over.model.clone();
    let context_limit = over.context_limit;
    let prices = over.settings.clone();
    let missing = over.missing_key_env.clone();
    agent.set_model(ActiveModel::from(over));
    // The footer's copy of the model's numbers: `set_model` retargets the
    // agent, but the footer reads from `app`, so both must move together.
    app.model = to.clone();
    app.context_limit = context_limit;
    app.prices = prices;
    app.last_prompt_tokens = 0;
    // Record the change in the session log and bus. The run loop drains the
    // bus right after a command, so `apply_event` renders the
    // "model changed: from → to" notice — this must not push its own.
    let mut s = session.lock().await;
    agent.note_model_change(&mut s, &from, &to);
    if let Some(var) = missing {
        app.push(
            Kind::Notice,
            format!("⚠ ${var} is not set, so requests to this model go out with no API key"),
        );
    }
}

/// Resolve a user-typed id, which is normally an 8-character prefix. Reports
/// the failure itself and returns None, so callers stay readable.
fn resolve_memory_id(app: &mut App, mem: &MemoryStore, typed: &str) -> Option<String> {
    match mem.resolve_id(typed) {
        Ok(IdMatch::Unique(id)) => Some(id),
        Ok(IdMatch::None) => {
            app.push(Kind::Notice, format!("(no memory matching `{typed}`)"));
            None
        }
        Ok(IdMatch::Ambiguous(ids)) => {
            let shown: Vec<&str> = ids.iter().map(|i| short_id(i)).collect();
            app.push(
                Kind::Notice,
                format!("`{typed}` matches {}: {}", ids.len(), shown.join(", ")),
            );
            None
        }
        Err(e) => {
            app.push(Kind::Error, format!("memory error: {e}"));
            None
        }
    }
}

/// `/memory [list|global|project | show <id> | forget <id> | add <scope> <kind> <subject> <content…>]`
fn memory_command<'a>(app: &mut App, mem: &MemoryStore, mut parts: impl Iterator<Item = &'a str>) {
    let sub = parts.next().unwrap_or("list");
    match sub {
        "list" | "" => memory_list(app, mem, None),
        "global" => memory_list(app, mem, Some(Scope::Global)),
        "project" => memory_list(app, mem, Some(Scope::Project)),
        "show" => {
            if let Some(id) = parts.next().and_then(|t| resolve_memory_id(app, mem, t)) {
                match mem.get(&id) {
                Ok(Some(r)) => app.push(
                    Kind::Notice,
                    format!(
                        "[{}/{}] {} (importance {}, {})\n{}",
                        r.scope, r.kind, r.subject, r.importance, r.status, r.content
                    ),
                ),
                    Ok(None) => app.push(Kind::Notice, format!("(no memory {id})")),
                    Err(e) => app.push(Kind::Error, format!("memory error: {e}")),
                }
            }
        }
        "forget" => {
            if let Some(id) = parts.next().and_then(|t| resolve_memory_id(app, mem, t)) {
                match mem.forget(&id) {
                    Ok(true) => app.push(Kind::Notice, format!("forgot {}", short_id(&id))),
                    Ok(false) => app.push(Kind::Notice, format!("(no memory {id})")),
                    Err(e) => app.push(Kind::Error, format!("memory error: {e}")),
                }
            }
        }
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
                                    h.score, short_id(&h.row.id), h.row.scope, h.row.kind,
                                    h.row.subject, h.row.content
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
                app.push(Kind::Notice, "(nothing pending)".to_string())
            }
            Ok(rows) => {
                let n = rows.len();
                for r in rows {
                    app.push(
                        Kind::Notice,
                        format!(
                            "{}  [{}/{}] {}: {}",
                            short_id(&r.id), r.scope, r.kind, r.subject, r.content
                        ),
                    );
                }
                // One hint for the batch. Repeating two full uuids per row made
                // the list unreadable and still left the id to be retyped.
                app.push(
                    Kind::Notice,
                    format!(
                        "{n} pending — /memory approve <id> (Tab completes) · \
                         /memory approve all · /memory forget <id>"
                    ),
                );
            }
            Err(e) => app.push(Kind::Error, format!("memory error: {e}")),
        },
        // `all` matters here: mining files several proposals at once, and
        // approving them one uuid at a time is the slowest part of the loop.
        "approve" => match parts.next() {
            Some("all") => match mem.pending_ids() {
                Ok(ids) if ids.is_empty() => {
                    app.push(Kind::Notice, "(nothing pending)".to_string())
                }
                Ok(ids) => {
                    let mut n = 0;
                    for id in &ids {
                        match mem.approve(id) {
                            Ok(true) => n += 1,
                            Ok(false) => {}
                            Err(e) => app.push(Kind::Error, format!("memory error: {e}")),
                        }
                    }
                    app.push(Kind::Notice, format!("approved {n} proposals"));
                }
                Err(e) => app.push(Kind::Error, format!("memory error: {e}")),
            },
            Some(t) => {
                if let Some(id) = resolve_memory_id(app, mem, t) {
                    match mem.approve(&id) {
                        Ok(true) => app.push(Kind::Notice, format!("approved {}", short_id(&id))),
                        Ok(false) => app
                            .push(Kind::Notice, format!("(not pending: {})", short_id(&id))),
                        Err(e) => app.push(Kind::Error, format!("memory error: {e}")),
                    }
                }
            }
            None => app.push(Kind::Notice, "usage: /memory approve <id|all>".to_string()),
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
        "help" | "?" => app.push(
            Kind::Notice,
            "\
  /memory                             list what is remembered
  /memory global | project            list one scope
  /memory search <query>              search it
  /memory extract                     distill this session into proposals
  /memory mine [n]                    mine past sessions of this project
  /memory pending                     review proposals
  /memory approve <id|all>            accept one, or all of them
  /memory forget <id>                 delete one
  /memory show <id>                   show one in full
  /memory add <scope> <kind> <subject> <content...>

Ids accept any unique prefix, and Tab completes them."
                .to_string(),
        ),
        other => app.push(
            Kind::Error,
            format!("unknown /memory subcommand: {other} — try /memory help"),
        ),
    }
}

fn memory_list(app: &mut App, mem: &MemoryStore, scope: Option<Scope>) {
    match mem.list(scope) {
        Ok(rows) if rows.is_empty() => app.push(Kind::Notice, "(no memories)".to_string()),
        Ok(rows) => {
            for r in rows {
                app.push(
                    Kind::Notice,
                    format!(
                        "{}  [{}/{}] {}: {}",
                        short_id(&r.id), r.scope, r.kind, r.subject, r.content
                    ),
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
    let memory_context = mem
        .turn_context(&message, app.context_limit)
        .unwrap_or_else(|e| {
            app.push(Kind::Error, format!("memory search failed: {e}"));
            None
        });
    *cancel = CancellationToken::new();
    let a = agent.clone();
    let s = session.clone();
    let tok = cancel.clone();
    let cmd = app.validate_cmd.clone();
    let cwd2 = cwd.to_path_buf();
    app.running = true;
    app.turn_start = Some(std::time::Instant::now());
    app.status = "working (Esc aborts)".into();
    app.transcript.follow = true;
    app.transcript.scroll_up = 0;
    *turn = Some(tokio::spawn(async move {
        let validator = cmd.map(|c| CommandValidator::new(c, cwd2.clone(), bash_timeout));
        let mut sess = s.lock().await;
        a.run_turn_with_context(
            &mut sess,
            &message,
            &sys,
            memory_context,
            validator.as_ref().map(|v| v as _),
            tok,
        )
        .await
    }));
}

/// Kick off an internal synthesis turn without rendering it as human input.
#[allow(clippy::too_many_arguments)]
fn start_synthetic_turn(
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
    app.synthetic_user_message = Some(message.clone());
    start_turn(message, app, agent, session, mem, cwd, bash_timeout, turn, cancel);
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
/// One line for a recorded event: what happened, not how it was rendered.
fn describe(ev: &Event) -> String {
    match ev {
        Event::UserMessage { text } => format!("you: {}", truncate(text.trim(), 60)),
        Event::AssistantMessage { text } => format!("said: {}", truncate(text.trim(), 60)),
        Event::ToolCall { name, arguments, .. } => {
            format!("⚙ {name} {}", truncate(arguments.trim(), 50))
        }
        Event::ToolResult { name, ok, output, .. } => format!(
            "  {} {name}: {}",
            if *ok { "→" } else { "✗" },
            truncate(output.trim(), 50)
        ),
        Event::ModelCallStarted => "model call →".to_string(),
        Event::ModelCallFinished => "model call ←".to_string(),
        Event::ModelChanged { from, to } => format!("model changed: {from} → {to}"),
        Event::Usage { completion_tokens, reasoning_tokens, finish_reason, .. } => format!(
            "usage: {completion_tokens} tok ({reasoning_tokens} reasoning), finish={}",
            finish_reason.as_deref().unwrap_or("none")
        ),
        Event::Checkpoint { kind, subject, .. } => {
            format!("◆ checkpoint ({kind}): {}", truncate(subject.trim(), 50))
        }
        Event::Nudge { reason } => format!("↻ nudge: {}", truncate(reason.trim(), 60)),
        Event::Validation { ok, detail } => {
            format!("{} validation: {detail}", if *ok { "✓" } else { "✗" })
        }
        Event::MemoryUsed { ids } => memory_used_summary(ids),
        Event::Compaction { tokens_before, tokens_after, .. } => {
            format!("⟲ compacted ~{tokens_before} → ~{tokens_after} tokens")
        }
        Event::Warning { message } => format!("⚠ {}", truncate(message.trim(), 60)),
        Event::Error { message } => format!("error: {}", truncate(message.trim(), 60)),
        Event::TurnComplete { outcome } => format!("turn complete: {outcome}"),
        Event::SessionStarted { id } => format!("session {id}"),
        Event::MessageDelta { .. } | Event::Thinking { .. } => String::new(),
    }
}

fn memory_used_summary(ids: &[String]) -> String {
    let shown = ids.iter().map(|id| short_id(id)).collect::<Vec<_>>().join(" ");
    if shown.is_empty() {
        "memory: using 0 item(s)".to_string()
    } else {
        format!("memory: using {} item(s): {shown}", ids.len())
    }
}

fn skill_command<'a>(app: &mut App, cwd: &Path, mut parts: impl Iterator<Item = &'a str>) {
    let catalog = crate::skill::SkillCatalog::discover(cwd);
    match parts.next() {
        None => {
            if catalog.is_empty() {
                app.push(Kind::Notice, "(no skills found). Looked in:".to_string());
                for (path, exists) in crate::skill::SkillCatalog::searched(cwd) {
                    let mark = if exists { "" } else { "  (missing)" };
                    app.push(Kind::Notice, format!("    {}{mark}", path.display()));
                }
                // The usual mistake, named rather than left to be guessed.
                for stray in crate::skill::SkillCatalog::misplaced(cwd) {
                    app.push(
                        Kind::Notice,
                        format!(
                            "  found {} but skills must live in <dir>/skills/<name>/SKILL.md",
                            stray.display()
                        ),
                    );
                }
                app.push(
                    Kind::Notice,
                    "  a skill is a directory containing SKILL.md; put it in one of the above"
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
/// When a worker started, and how long ago it ended.
///
/// A finished worker's line is otherwise identical whether it landed a second
/// ago or half an hour ago — which is the difference between "act on this" and
/// "this is history". Reported from use: a `[done]` worker sat in the list
/// looking current long after it had finished, and a nudge aimed at a worker
/// that had already stopped was accepted and did nothing.
///
/// Clock time for the start, because that is what you compare against your own
/// memory of the session; elapsed for the rest, because "4m ago" needs no
/// arithmetic.
fn worker_timing(w: &WorkerSummary) -> String {
    let clock = |t: SystemTime| {
        let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        // Local wall clock without pulling in a date library: the session is
        // today, so hh:mm:ss is all that is wanted.
        let day = secs % 86_400;
        format!("{:02}:{:02}:{:02}", day / 3600, (day % 3600) / 60, day % 60)
    };
    let ago = |t: SystemTime| match SystemTime::now().duration_since(t) {
        Ok(d) if d.as_secs() < 60 => format!("{}s", d.as_secs()),
        Ok(d) if d.as_secs() < 3600 => format!("{}m", d.as_secs() / 60),
        Ok(d) => format!("{}h{}m", d.as_secs() / 3600, (d.as_secs() % 3600) / 60),
        Err(_) => "?".to_string(),
    };
    match w.finished {
        Some(end) => format!(
            "{}→{} ({} ago)",
            clock(w.started),
            clock(end),
            ago(end)
        ),
        None => format!("{} (running {})", clock(w.started), ago(w.started)),
    }
}

fn agents_command<'a>(
    app: &mut App,
    workers: &mut WorkerManager,
    mut parts: impl Iterator<Item = &'a str>,
) {
    match parts.next().unwrap_or("list") {
        // The one thing `/agents` could not do: show what a worker is doing
        // *now*. `show` dumps a finished result; status is a single line.
        "tail" | "follow" | "watch" => match parts.next() {
            Some("off") | Some("stop") => {
                app.tail = None;
                app.push(Kind::Notice, "stopped following".to_string());
            }
            Some(id) => match workers.log_since(id, 0) {
                Some((lines, next, _)) => {
                    if lines.is_empty() {
                        app.push(Kind::Notice, format!("{id} hasn't done anything yet"));
                    }
                    for l in lines {
                        // `Kind::Tool` prepends its own "⚙ ", and the log
                        // line already carries one — which is where the
                        // "⚙ [w1] ⚙ bash" double glyph came from, and why the
                        // nesting looked like it meant parent/child when it was
                        // an accident. `Kind::Notice` adds no label, so the
                        // worker's own glyph stands alone, and the bar makes
                        // the nesting real.
                        app.push(Kind::Notice, format!("{id} │ {l}"));
                    }
                    app.tail = Some((id.to_string(), next));
                    app.push(
                        Kind::Notice,
                        format!("following {id} — /agents tail off to stop"),
                    );
                }
                None => app.push(Kind::Error, format!("no agent `{id}`")),
            },
            None => app.push(
                Kind::Notice,
                "usage: /agents tail <id> | /agents tail off".to_string(),
            ),
        },
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
                            "{} [{}] {} · {} tools · {} changed{}{} · {} — {}",
                            w.id,
                            w.status.label(),
                            worker_timing(&w),
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
                    match workers.nudge(&id, &message) {
                        Ok(()) => app.push(Kind::Notice, format!("nudged {id}")),
                        // Says which of the two it was. "(no agent w2)" for a
                        // worker that plainly exists on the list above reads as
                        // a bug in the lookup, when the real answer is that it
                        // already stopped.
                        Err(why) => app.push(Kind::Notice, format!("not nudged: {why}")),
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
fn complete(app: &mut App, cwd: &Path, mem: &MemoryStore, config: &Config) {
    if let Some(status) = app.composer.complete(cwd, mem, config) {
        app.status = status;
    }
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
    let input_height = app.composer.render_height();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .split(f.area());

    render_transcript(f, chunks[0], &app.transcript);
    let input_title = input_title(app);
    render_input(f, chunks[1], &app.composer, input_title.as_str());
    let footer_left = footer_string(app);
    let footer_status = footer_status(app);
    render_footer(f, chunks[2], footer_left.as_str(), footer_status.as_str());

    // The as-you-type hint sits directly above the composer, where you are
    // already looking, rather than in the middle of the screen.
    if let Some(hint) = &app.composer.hint {
        render_hint(f, chunks[1], hint);
    }

    // Last, so it composites over everything else.
    if let Some(ov) = &app.overlay {
        render_overlay(f, f.area(), ov);
    }
}

/// With the hint showing, does Enter take the highlighted row rather than run
/// what is typed? Yes — unless the typed text is already exactly a command, so
/// `/help<Enter>` still works the way the fingers expect.
fn hint_enter_accepts(input: &str) -> bool {
    !COMMANDS.iter().any(|(name, _)| *name == input.trim())
}

fn key_inserts_newline(key: &KeyEvent) -> bool {
    (matches!(key.code, KeyCode::Char('n')) && key.modifiers.contains(KeyModifiers::CONTROL))
        || (matches!(key.code, KeyCode::Enter)
            && key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT))
}

/// Draw the command hint anchored to the bottom of `above`, growing upward.
fn render_hint(f: &mut Frame, above: Rect, ov: &Overlay) {
    let matches = ov.matches();
    if matches.is_empty() {
        return;
    }
    let label_w = matches.iter().map(|(_, i)| i.label.chars().count()).max().unwrap_or(0);
    let desc_w = matches.iter().map(|(_, i)| i.description.chars().count()).max().unwrap_or(0);
    let width = (label_w + desc_w + 8).clamp(20, above.width.saturating_sub(2) as usize) as u16;
    let rows = (matches.len() as u16).min(8);
    let height = rows + 2;
    if above.y < height {
        return; // no room above the composer; the footer hint still applies
    }
    let rect = Rect { x: above.x, y: above.y - height, width, height };

    f.render_widget(ratatui::widgets::Clear, rect);
    // Say when the list is longer than the window; eight of fourteen looks
    // like all of them otherwise.
    let title = if matches.len() as u16 > rows {
        format!(" ↑↓ · Tab accepts · {rows}/{} ", matches.len())
    } else {
        " ↑↓ · Tab accepts ".to_string()
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let sel = ov.sel_index(matches.len());
    let first = sel.saturating_sub(inner.height.saturating_sub(1) as usize);
    let lines: Vec<Line> = matches
        .iter()
        .enumerate()
        .skip(first)
        .take(inner.height as usize)
        .map(|(i, (_, item))| {
            // Reverse video for the whole selected row: a solid highlight bar
            // that reads on a light or dark theme, unlike a named colour (see
            // the cursor row). Bold alone is invisible on a light background.
            let style = if i == sel {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            let marker = if i == sel { "▸ " } else { "  " };
            Line::from(vec![
                Span::styled(format!("{marker}{:<label_w$}  ", item.label), style),
                Span::styled(
                    item.description.clone(),
                    if i == sel { style } else { Style::default().fg(Color::DarkGray) },
                ),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

/// A centered floating list. `Clear` blanks what is underneath, which is what
/// makes it read as a window rather than as text drawn over the transcript.
fn render_overlay(f: &mut Frame, area: Rect, ov: &Overlay) {
    let matches = ov.matches();
    // Bounds are constant and ordered, so clamp cannot panic here.
    let width = area.width.saturating_sub(8).clamp(20, 76);
    let rows = (matches.len() as u16).clamp(1, 14);
    let height = (rows + 2).min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 3;
    let rect = Rect { x, y, width, height };

    f.render_widget(ratatui::widgets::Clear, rect);

    let title = if ov.filter.is_empty() {
        format!(" {} ", ov.title)
    } else {
        format!(" {} · {} ", ov.title, ov.filter)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_bottom(if ov.picking { " ↑↓ move · Enter pick · Esc close " } else { " ↑↓ move · Esc close " });
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    // Keep the selection on screen when the list is longer than the window.
    let visible = inner.height as usize;
    let sel = ov.sel_index(matches.len());
    let first = sel.saturating_sub(visible.saturating_sub(1));
    let label_w = matches.iter().map(|(_, i)| i.label.chars().count()).max().unwrap_or(0).min(20);

    let lines: Vec<Line> = matches
        .iter()
        .enumerate()
        .skip(first)
        .take(visible)
        .map(|(i, (_, item))| {
            let selected = i == sel;
            let marker = if selected { "▸ " } else { "  " };
            let label = format!("{:<label_w$}", item.label);
            // Reverse video for the whole selected row: a solid highlight bar
            // that reads on a light or dark theme, unlike a named colour (see
            // the cursor row). Bold alone is invisible on a light background.
            let style = if selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::styled(format!("{marker}{label}"), style),
                Span::styled(
                    format!("  {}", item.description),
                    if selected { style } else { Style::default().fg(Color::DarkGray) },
                ),
            ])
        })
        .collect();

    let body = if lines.is_empty() {
        vec![Line::from(Span::styled("(nothing matches)", Style::default().fg(Color::DarkGray)))]
    } else {
        lines
    };
    f.render_widget(Paragraph::new(body), inner);
}

fn render_transcript(f: &mut Frame, area: Rect, transcript: &Transcript) {
    // Rows are pre-wrapped and cached (see Transcript::ensure_rows); here we
    // just slice the tail (minus any manual scroll-up). Scrolling is cheap.
    let rows = &transcript.cached_rows;
    let h = area.height as usize;
    let total = rows.len();

    // In normal mode the window follows the cursor instead of the tail —
    // otherwise `k` would move a cursor you cannot see.
    let end = if transcript.mode == Mode::Normal {
        (transcript.cursor_row + 1).max(h.min(total)).min(total)
    } else {
        let up = (transcript.scroll_up as usize).min(total.saturating_sub(1));
        total.saturating_sub(up)
    };
    let start = end.saturating_sub(h);

    let hits: HashSet<usize> = if transcript.mode == Mode::Normal {
        transcript.search_hit_rows.iter().copied().collect()
    } else {
        HashSet::new()
    };
    let view: Vec<Line> = rows[start..end]
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let row = start + i;
            if transcript.mode == Mode::Normal && row == transcript.cursor_row {
                // Reverse video: readable in any theme, unlike a colour choice.
                let spans = line
                    .spans
                    .iter()
                    .map(|sp| {
                        Span::styled(
                            sp.content.clone(),
                            sp.style.add_modifier(Modifier::REVERSED),
                        )
                    })
                    .collect::<Vec<_>>();
                Line::from(spans)
            } else if hits.contains(&row) {
                Line::from(
                    line.spans
                        .iter()
                        .map(|sp| {
                            Span::styled(
                                sp.content.clone(),
                                sp.style.fg(Color::Black).bg(Color::Yellow),
                            )
                        })
                        .collect::<Vec<_>>(),
                )
            } else {
                line.clone()
            }
        })
        .collect();

    let para = Paragraph::new(view).block(Block::default().borders(Borders::NONE));
    f.render_widget(para, area);
}

fn input_title(app: &App) -> String {
    // A pending approval owns the keyboard and blocks the turn, so the composer
    // has to say that. It used to keep saying "working…" with the elapsed timer
    // climbing, which reads as "the model is busy" — observed costing 79 minutes
    // of waiting for a keypress nobody knew was wanted.
    if app.modals.approval_pending() {
        " APPROVE?  y = once · a = always this session · n = no ".to_string()
    } else if app.modals.ask_pending() {
        " CHECKPOINT  Enter answers · Esc skips ".to_string()
    } else if app.transcript.mode == Mode::Normal {
        // The single worst modal failure is not knowing which mode you are in.
        match &app.transcript.search {
            Some(s) if s.typing => format!(" NORMAL · /{} ", s.pattern),
            _ => " NORMAL · j k · /search · y yank · i insert ".to_string(),
        }
    } else if app.running {
        // Say that typing is useful right now. " working… " reads as "wait",
        // which is what made people assume input was ignored — and it was.
        " working… · Enter steers the running turn · Esc aborts ".to_string()
    } else {
        " message · Enter send · Ctrl+N newline ".to_string()
    }
}

fn render_input(f: &mut Frame, area: Rect, composer: &Composer, title: &str) {
    let inner_w = area.width.saturating_sub(2) as usize;
    let (rows, crow, ccol) = composer.wrapped_rows(inner_w);
    let inner_h = area.height.saturating_sub(2) as usize;
    // Vertical scroll so the cursor's row stays visible.
    let scroll = (crow + 1).saturating_sub(inner_h.max(1)) as u16;

    // Breaks are already in the text, so ratatui must not add its own.
    let para = Paragraph::new(rows.join("\n"))
        .block(Block::default().borders(Borders::ALL).title(title))
        .scroll((scroll, 0));
    f.render_widget(para, area);

    let x = area.x + 1 + (ccol as u16).min(inner_w.saturating_sub(1) as u16);
    let y = area.y + 1 + (crow as u16).saturating_sub(scroll);
    f.set_cursor_position((x, y));
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

/// Ring the terminal when the loop is blocked on the user.
///
/// The one event worth interrupting someone for is "I need you", and it is
/// exactly the one they miss: an approval prompt sat unanswered in a background
/// tab and stalled a run for half an hour, because nothing about a full-screen
/// TUI reaches you when you are looking at a different tab.
///
/// Two sequences, both cheap and both ignored by terminals that do not speak
/// them. BEL is universal and usually becomes a dock bounce or a tab badge.
/// OSC 9 is a real desktop notification in iTerm2, WezTerm, Kitty and Ghostty.
/// Neither moves the cursor, so neither disturbs the frame ratatui is drawing.
///
/// Deliberately not fired on "done": a notification you get for everything is
/// one you stop reading, and finishing is what the transcript is for.
fn alert(reason: &str) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = write!(out, "\x07\x1b]9;worksmith: {reason}\x07");
    let _ = out.flush();
}

/// Draw the command picker into an off-screen buffer and print it. A dev aid:
/// the layout is worth looking at, and a TUI is otherwise hard to inspect.
/// How long a full re-wrap takes as a transcript grows. Streaming sets `dirty`
/// on every token, so this cost is paid per token — the shape of that curve is
/// the question, not the absolute number.
pub fn bench_rows() {
    // Realistic: tool results are capped at 24k, and a chapter review reads
    // whole files. Rows, not items, are what the wrap loop walks.
    for turns in [5usize, 20, 60] {
        let mut app = App::new("m".into(), 128_000, None);
        for i in 0..turns {
            app.push(Kind::User, format!("question {i} about the chapter"));
            app.push(Kind::Assistant, "answer ".repeat(60));
            app.push(Kind::ToolResult, "x".repeat(12_000));
        }
        // The real streaming case: a token lands on the last item and the view
        // redraws. Previously this re-wrapped the whole transcript.
        app.push(Kind::Assistant, String::new());
        app.ensure_rows(100);
        let start = std::time::Instant::now();
        let n = 200;
        for _ in 0..n {
            app.apply_event(crate::event::Event::MessageDelta { text: "token ".into() });
            app.ensure_rows(100);
        }
        let each = start.elapsed() / n;
        println!(
            "{:>4} turns ({:>5} items, {:>6} rows): {:>8.3?} per re-wrap  → {:>7.1} re-wraps/sec",
            turns,
            app.transcript.items.len(),
            app.transcript.cached_rows.len(),
            each,
            1.0 / each.as_secs_f64()
        );
    }
}

/// Normal mode with a search active, drawn off-screen so the layout and the
/// mode indicator can be looked at rather than assumed.
pub fn print_normal_preview() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut app = App::new("qwen/qwen3.8-27b".into(), 128_000, None);
    app.push(Kind::User, "review chapter 9".to_string());
    app.push(Kind::Assistant, "The listing captions are missing for 9-3 and 9-14.".to_string());
    app.push(Kind::Tool, "⚙ read Chapter9.docx".to_string());
    app.push(Kind::ToolResult, "styles: Body, CodeAnnotated, ListPlain".to_string());
    app.ensure_rows(78);
    app.enter_normal();
    app.cursor_by(-4);
    app.set_search(Some(Search { pattern: "listing".into(), typing: false }));

    let mut term = Terminal::new(TestBackend::new(78, 14)).unwrap();
    term.draw(|f| ui(f, &app)).unwrap();
    let buf = term.backend().buffer().clone();
    for y in 0..buf.area.height {
        let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
        println!("{}", row.trim_end());
    }
}

pub fn print_hint_preview(typed: &str) {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut app = App::new("qwen/qwen3.8-27b".into(), 128_000, None);
    app.push(Kind::User, "review chapter 9".to_string());
    app.composer.set_input(typed.to_string());
    app.composer.refresh_hint();
    app.ensure_rows(80);

    let mut term = Terminal::new(TestBackend::new(80, 16)).unwrap();
    term.draw(|f| ui(f, &app)).unwrap();
    let buf = term.backend().buffer().clone();
    for y in 0..buf.area.height {
        let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
        println!("{}", row.trim_end());
    }
}

pub fn print_overlay_preview() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut app = App::new("qwen/qwen3.8-27b".into(), 128_000, None);
    app.push(Kind::User, "review chapter 9".to_string());
    app.push(Kind::Assistant, "Reading the docx…".to_string());
    let items = COMMANDS
        .iter()
        .map(|(name, desc)| OverlayItem {
            label: (*name).to_string(),
            description: (*desc).to_string(),
        })
        .collect();
    app.overlay = Some(Overlay::new("commands · type to filter", items));
    app.ensure_rows(80);

    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| ui(f, &app)).unwrap();
    let buf = term.backend().buffer().clone();
    for y in 0..buf.area.height {
        let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
        println!("{}", row.trim_end());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;

    fn app() -> App {
        App::new("m".into(), 1000, None)
    }

    fn project_with_config(config: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".worksmith")).unwrap();
        std::fs::write(dir.path().join(".worksmith/config.toml"), config).unwrap();
        dir
    }

    struct SilentClient;

    #[async_trait::async_trait]
    impl crate::llm::LlmClient for SilentClient {
        async fn stream(
            &self,
            _req: crate::llm::ChatRequest,
            _sink: tokio::sync::mpsc::Sender<crate::llm::StreamEvent>,
            _cancel: CancellationToken,
        ) -> Result<crate::llm::Completion> {
            Ok(crate::llm::Completion::default())
        }
    }

    fn test_agent() -> Agent {
        Agent::new(
            Arc::new(SilentClient),
            Arc::new(crate::tools::ToolRegistry::with_builtins()),
            EventBus::new(),
            "test/model".to_string(),
            None,
            None,
            8,
            1,
            3,
            32_000,
            6,
            crate::tools::ToolContext::default(),
        )
    }

    #[test]
    fn model_list_marks_the_entry_serving_the_session() {
        use std::collections::HashMap;
        let mut models = HashMap::new();
        models.insert("big/27b".to_string(), crate::config::ModelSettings::default());
        models.insert("cheap/7b".to_string(), crate::config::ModelSettings::default());

        // `App.model` is the bare name, so "big/27b" is marked, not "cheap/7b".
        // An unmarked line is the mark's space plus the separator's space.
        let lines = model_list("27b", &models);
        assert_eq!(lines, vec!["* big/27b".to_string(), "  cheap/7b".to_string()]);
    }

    #[test]
    fn model_list_marks_both_when_two_share_a_bare_name() {
        // Degenerate config: two providers, same bare model name. Marking both
        // is more honest than guessing which one the session is on.
        use std::collections::HashMap;
        let mut models = HashMap::new();
        models.insert("a/27b".to_string(), crate::config::ModelSettings::default());
        models.insert("b/27b".to_string(), crate::config::ModelSettings::default());

        let lines = model_list("27b", &models);
        assert_eq!(lines, vec!["* a/27b".to_string(), "* b/27b".to_string()]);
    }

    #[test]
    fn model_list_is_empty_when_nothing_is_configured() {
        use std::collections::HashMap;
        let models = HashMap::new();
        assert!(model_list("27b", &models).is_empty());
    }

    #[test]
    fn model_list_sorts_the_keys() {
        use std::collections::HashMap;
        let mut models = HashMap::new();
        models.insert("zeta/7b".to_string(), crate::config::ModelSettings::default());
        models.insert("alpha/27b".to_string(), crate::config::ModelSettings::default());

        let lines = model_list("27b", &models);
        assert_eq!(lines, vec!["* alpha/27b".to_string(), "  zeta/7b".to_string()]);
    }

    #[test]
    fn validate_command_reports_the_current_check() {
        let mut a = app();
        a.validate_cmd = Some("cargo test".to_string());

        validate_command(&mut a, std::iter::empty());

        assert_eq!(a.validate_cmd.as_deref(), Some("cargo test"));
        assert_eq!(
            a.transcript.items.last().unwrap().text,
            "validation: cargo test"
        );
    }

    #[test]
    fn validate_command_sets_and_clears_the_check() {
        let mut a = app();

        validate_command(
            &mut a,
            ["cargo", "test", "tui::tests", "--lib"].into_iter(),
        );
        assert_eq!(
            a.validate_cmd.as_deref(),
            Some("cargo test tui::tests --lib")
        );
        assert_eq!(
            a.transcript.items.last().unwrap().text,
            "validation: `cargo test tui::tests --lib`"
        );

        validate_command(&mut a, ["off"].into_iter());
        assert!(a.validate_cmd.is_none());
        assert_eq!(
            a.transcript.items.last().unwrap().text,
            "validation cleared"
        );
    }

    #[test]
    fn pair_command_reports_the_current_mode() {
        let mut a = app();
        let agent = test_agent();

        pair_command(&mut a, &agent, std::iter::empty());
        assert_eq!(
            a.transcript.items.last().unwrap().text,
            "pairing off — the checkpoint is no longer offered to the model"
        );

        agent.set_pairing(true);
        pair_command(&mut a, &agent, std::iter::empty());
        assert_eq!(a.transcript.items.last().unwrap().text, pair_status(true));
    }

    #[test]
    fn pair_command_sets_and_clears_pairing() {
        let mut a = app();
        let agent = test_agent();

        pair_command(&mut a, &agent, ["on"].into_iter());
        assert!(agent.pairing_on());
        assert_eq!(a.transcript.items.last().unwrap().text, pair_status(true));

        pair_command(&mut a, &agent, ["off"].into_iter());
        assert!(!agent.pairing_on());
        assert_eq!(a.transcript.items.last().unwrap().text, pair_status(false));
    }

    #[test]
    fn pair_command_rejects_unknown_args_without_changing_mode() {
        let mut a = app();
        let agent = test_agent().with_pairing(true);

        pair_command(&mut a, &agent, ["maybe"].into_iter());

        assert!(agent.pairing_on());
        assert!(matches!(
            a.transcript.items.last().unwrap().kind,
            Kind::Error
        ));
        assert_eq!(
            a.transcript.items.last().unwrap().text,
            "usage: /pair [on|off] (got maybe)"
        );
    }

    #[test]
    fn route_command_reports_the_current_route() {
        let mut a = app();
        let agent = test_agent();

        route_command(&mut a, &agent, std::iter::empty());
        assert_eq!(
            a.transcript.items.last().unwrap().text,
            "routing: the provider's default (OpenRouter sorts on price)"
        );

        a.route = Some("latency".to_string());
        agent.set_route(Some("latency".to_string()));
        route_command(&mut a, &agent, std::iter::empty());
        assert_eq!(
            a.transcript.items.last().unwrap().text,
            "routing: latency (OpenRouter only)"
        );
    }

    #[test]
    fn route_command_sets_and_clears_the_route() {
        let mut a = app();
        let agent = test_agent();

        route_command(&mut a, &agent, ["throughput"].into_iter());
        assert_eq!(a.route.as_deref(), Some("throughput"));
        assert_eq!(agent.route_for_test().as_deref(), Some("throughput"));
        assert_eq!(
            a.transcript.items.last().unwrap().text,
            "routing on throughput — takes effect on the next turn"
        );

        route_command(&mut a, &agent, ["default"].into_iter());
        assert!(a.route.is_none());
        assert!(agent.route_for_test().is_none());
        assert_eq!(
            a.transcript.items.last().unwrap().text,
            "routing left to the provider"
        );
    }

    #[test]
    fn route_command_rejects_unknown_args_without_changing_route() {
        let mut a = app();
        let agent = test_agent();
        a.route = Some("price".to_string());
        agent.set_route(Some("price".to_string()));

        route_command(&mut a, &agent, ["maybe"].into_iter());

        assert_eq!(a.route.as_deref(), Some("price"));
        assert_eq!(agent.route_for_test().as_deref(), Some("price"));
        assert!(matches!(
            a.transcript.items.last().unwrap().kind,
            Kind::Error
        ));
        assert_eq!(
            a.transcript.items.last().unwrap().text,
            "usage: /route [throughput|latency|price|auto] (got maybe)"
        );
    }

    #[test]
    fn fast_command_toggles_fast_mode() {
        let mut a = app();
        let agent = test_agent();

        fast_command(&mut a, &agent, std::iter::empty());
        assert_eq!(agent.thinking_mode().get(), Some(Thinking::Off));
        assert_eq!(a.think_label.as_deref(), Some("off"));
        assert_eq!(
            a.transcript.items.last().unwrap().text,
            "fast mode on — answering without thinking first"
        );

        fast_command(&mut a, &agent, std::iter::empty());
        assert_eq!(agent.thinking_mode().get(), Some(Thinking::On));
        assert_eq!(a.think_label.as_deref(), Some("on"));
        assert_eq!(
            a.transcript.items.last().unwrap().text,
            "fast mode off — thinking before answering"
        );
    }

    #[test]
    fn fast_command_sets_explicit_modes() {
        let mut a = app();
        let agent = test_agent().with_thinking(Some(Thinking::Budget(1200)));

        fast_command(&mut a, &agent, ["off"].into_iter());
        assert_eq!(agent.thinking_mode().get(), Some(Thinking::On));
        assert_eq!(a.think_label.as_deref(), Some("on"));
        assert_eq!(
            a.transcript.items.last().unwrap().text,
            "fast mode off — thinking before answering"
        );

        fast_command(&mut a, &agent, ["auto"].into_iter());
        assert_eq!(agent.thinking_mode().get(), None);
        assert!(a.think_label.is_none());
        assert_eq!(
            a.transcript.items.last().unwrap().text,
            "thinking left to the provider's default"
        );

        fast_command(&mut a, &agent, ["on"].into_iter());
        assert_eq!(agent.thinking_mode().get(), Some(Thinking::Off));
        assert_eq!(a.think_label.as_deref(), Some("off"));
    }

    #[test]
    fn fast_command_rejects_unknown_args_without_changing_thinking() {
        let mut a = app();
        let agent = test_agent().with_thinking(Some(Thinking::Budget(1200)));

        fast_command(&mut a, &agent, ["maybe"].into_iter());

        assert_eq!(agent.thinking_mode().get(), Some(Thinking::Budget(1200)));
        assert!(a.think_label.is_none());
        assert!(matches!(
            a.transcript.items.last().unwrap().kind,
            Kind::Error
        ));
        assert_eq!(
            a.transcript.items.last().unwrap().text,
            "usage: /fast [on|off|auto] (got maybe)"
        );
    }

    #[test]
    fn think_command_sets_modes_effort_and_budget() {
        let mut a = app();
        let agent = test_agent();

        think_command(&mut a, &agent, std::iter::empty());
        assert_eq!(agent.thinking_mode().get(), Some(Thinking::On));
        assert_eq!(a.think_label.as_deref(), Some("on"));
        assert_eq!(
            a.transcript.items.last().unwrap().text,
            "thinking on, uncapped"
        );

        think_command(&mut a, &agent, ["off"].into_iter());
        assert_eq!(agent.thinking_mode().get(), Some(Thinking::Off));
        assert_eq!(a.think_label.as_deref(), Some("off"));
        assert_eq!(
            a.transcript.items.last().unwrap().text,
            "thinking off — answering directly"
        );

        think_command(&mut a, &agent, ["low"].into_iter());
        assert_eq!(
            agent.thinking_mode().get(),
            Some(Thinking::Effort(crate::llm::Effort::Low))
        );
        assert_eq!(a.think_label.as_deref(), Some("low"));
        assert_eq!(
            a.transcript.items.last().unwrap().text,
            "thinking effort: low (the provider's own scale)"
        );

        think_command(&mut a, &agent, ["1200"].into_iter());
        assert_eq!(agent.thinking_mode().get(), Some(Thinking::Budget(1200)));
        assert_eq!(a.think_label.as_deref(), Some("1k"));
        assert_eq!(
            a.transcript.items.last().unwrap().text,
            "thinking capped at 1200 tokens, leaving the rest of max-tokens for the answer"
        );
    }

    #[test]
    fn think_command_clears_to_provider_default() {
        let mut a = app();
        let agent = test_agent().with_thinking(Some(Thinking::On));

        think_command(&mut a, &agent, ["auto"].into_iter());

        assert_eq!(agent.thinking_mode().get(), None);
        assert!(a.think_label.is_none());
        assert_eq!(
            a.transcript.items.last().unwrap().text,
            "thinking left to the provider's default"
        );
    }

    #[test]
    fn think_command_rejects_unknown_args_without_changing_thinking() {
        let mut a = app();
        let agent = test_agent().with_thinking(Some(Thinking::Budget(1200)));

        think_command(&mut a, &agent, ["maybe"].into_iter());

        assert_eq!(agent.thinking_mode().get(), Some(Thinking::Budget(1200)));
        assert!(a.think_label.is_none());
        assert!(matches!(
            a.transcript.items.last().unwrap().kind,
            Kind::Error
        ));
        assert_eq!(
            a.transcript.items.last().unwrap().text,
            "usage: /think [on|off|auto|<effort>|<tokens>] (got maybe). Efforts: minimal, low, medium, high, xhigh, max — though servers differ on which they accept."
        );
    }

    #[test]
    fn trust_command_reports_no_project_config() {
        let mut a = app();
        let dir = tempfile::tempdir().unwrap();
        let mut store = crate::trust::TrustStore::default();

        trust_command_with_store(&mut a, dir.path(), &mut store, std::iter::empty());

        assert_eq!(
            a.transcript.items.last().unwrap().text,
            "this project has no .worksmith/config.toml"
        );
    }

    #[test]
    fn trust_command_reports_the_project_trust_state() {
        let mut a = app();
        let dir = project_with_config("[agent]\nvalidate = \"cargo test\"\nmax-steps = 80\n");
        let mut store = crate::trust::TrustStore::default();

        trust_command_with_store(&mut a, dir.path(), &mut store, std::iter::empty());

        assert!(
            a.transcript.items[0]
                .text
                .contains(".worksmith/config.toml")
        );
        assert!(
            a.transcript.items[0]
                .text
                .contains("undecided — its config is NOT in effect")
        );
        assert!(
            a.transcript
                .items
                .iter()
                .any(|item| item.text.contains("! agent.validate = cargo test"))
        );
        assert_eq!(
            a.transcript.items.last().unwrap().text,
            "/trust revoke to decide again on the next start"
        );

        let prompt = crate::trust::prompt_for(dir.path(), &store).unwrap();
        store.record(
            dir.path(),
            &prompt.fingerprint,
            crate::trust::Decision::Trust,
        );
        trust_command_with_store(&mut a, dir.path(), &mut store, std::iter::empty());
        assert!(
            a.transcript
                .items
                .iter()
                .rev()
                .find(|item| item.text.contains(".worksmith/config.toml"))
                .unwrap()
                .text
                .contains("trusted — its config is in effect")
        );
    }

    #[test]
    fn trust_command_revokes_an_existing_decision() {
        let mut a = app();
        let dir = project_with_config("[agent]\nmax-steps = 80\n");
        let mut store = crate::trust::TrustStore::default();
        let prompt = crate::trust::prompt_for(dir.path(), &store).unwrap();
        store.record(
            dir.path(),
            &prompt.fingerprint,
            crate::trust::Decision::Trust,
        );

        trust_command_with_store(&mut a, dir.path(), &mut store, ["revoke"].into_iter());

        assert_eq!(store.decision_for(dir.path(), &prompt.fingerprint), None);
        assert_eq!(
            a.transcript.items.last().unwrap().text,
            "forgot this project's trust decision — worksmith will ask again next start"
        );
    }

    #[test]
    fn trust_command_reports_when_there_is_no_decision_to_revoke() {
        let mut a = app();
        let dir = project_with_config("[agent]\nmax-steps = 80\n");
        let mut store = crate::trust::TrustStore::default();

        trust_command_with_store(&mut a, dir.path(), &mut store, ["forget"].into_iter());

        assert_eq!(
            a.transcript.items.last().unwrap().text,
            "(no decision recorded for this project)"
        );
    }

    #[test]
    fn trust_command_rejects_unknown_args_without_revoking() {
        let mut a = app();
        let dir = project_with_config("[agent]\nmax-steps = 80\n");
        let mut store = crate::trust::TrustStore::default();
        let prompt = crate::trust::prompt_for(dir.path(), &store).unwrap();
        store.record(
            dir.path(),
            &prompt.fingerprint,
            crate::trust::Decision::Ignore,
        );

        trust_command_with_store(&mut a, dir.path(), &mut store, ["maybe"].into_iter());

        assert_eq!(
            store.decision_for(dir.path(), &prompt.fingerprint),
            Some(crate::trust::Decision::Ignore)
        );
        assert!(matches!(
            a.transcript.items.last().unwrap().kind,
            Kind::Error
        ));
        assert_eq!(
            a.transcript.items.last().unwrap().text,
            "usage: /trust [revoke] (got maybe)"
        );
    }

    #[test]
    fn mouse_command_toggles_the_current_mode() {
        let mut a = app();
        let mut out = Vec::new();
        assert!(a.mouse);

        mouse_command(&mut a, &mut out, std::iter::empty());

        assert!(!a.mouse);
        assert_eq!(a.transcript.items.last().unwrap().text, mouse_status(false));
        assert!(!out.is_empty(), "the terminal mode command is written");
    }

    #[test]
    fn mouse_command_sets_explicit_modes() {
        let mut a = app();
        let mut out = Vec::new();

        mouse_command(&mut a, &mut out, ["off"].into_iter());
        assert!(!a.mouse);
        assert_eq!(a.transcript.items.last().unwrap().text, mouse_status(false));

        mouse_command(&mut a, &mut out, ["on"].into_iter());
        assert!(a.mouse);
        assert_eq!(a.transcript.items.last().unwrap().text, mouse_status(true));
    }

    #[test]
    fn mouse_command_rejects_unknown_args_without_changing_mode() {
        let mut a = app();
        let mut out = Vec::new();

        mouse_command(&mut a, &mut out, ["maybe"].into_iter());

        assert!(a.mouse);
        assert!(out.is_empty(), "bad args do not write terminal escapes");
        assert!(matches!(
            a.transcript.items.last().unwrap().kind,
            Kind::Error
        ));
        assert_eq!(
            a.transcript.items.last().unwrap().text,
            "usage: /mouse [on|off] (got maybe)"
        );
    }

    #[test]
    fn streaming_deltas_coalesce_per_channel() {
        let mut a = app();
        a.apply_event(Event::UserMessage { text: "hi".into() });
        a.apply_event(Event::Thinking { text: "let me ".into() });
        a.apply_event(Event::Thinking { text: "think".into() });
        a.apply_event(Event::MessageDelta { text: "Hel".into() });
        a.apply_event(Event::MessageDelta { text: "lo".into() });

        assert_eq!(a.transcript.items.len(), 3);
        assert!(matches!(a.transcript.items[0].kind, Kind::User));
        assert!(matches!(a.transcript.items[1].kind, Kind::Thinking));
        assert_eq!(a.transcript.items[1].text, "let me think");
        assert!(matches!(a.transcript.items[2].kind, Kind::Assistant));
        assert_eq!(a.transcript.items[2].text, "Hello");
    }

    #[test]
    fn synthesis_prompt_is_not_rendered_as_human_input() {
        let mut a = app();
        let prompt =
            "Your 2 background workers just reported back (above). Combine their results.";
        a.synthetic_user_message = Some(prompt.to_string());

        a.apply_event(Event::UserMessage { text: prompt.to_string() });

        assert_eq!(a.transcript.items.len(), 1);
        assert!(matches!(a.transcript.items[0].kind, Kind::Notice));
        assert!(
            a.transcript.items[0].text.starts_with("synthesis"),
            "synthetic prompts need their own label: {:?}",
            a.transcript.items[0].text
        );
        assert!(a.synthetic_user_message.is_none());
    }

    #[test]
    fn tool_call_breaks_the_assistant_block() {
        let mut a = app();
        a.apply_event(Event::MessageDelta { text: "before".into() });
        a.apply_event(Event::ToolCall { id: "1".into(), name: "ls".into(), arguments: "{}".into() });
        a.apply_event(Event::MessageDelta { text: "after".into() });

        // before-assistant, tool, after-assistant → 3 separate items.
        assert_eq!(a.transcript.items.len(), 3);
        assert_eq!(a.transcript.items[0].text, "before");
        assert!(matches!(a.transcript.items[1].kind, Kind::Tool));
        assert_eq!(a.transcript.items[2].text, "after");
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
    fn session_started_is_visible_in_the_transcript() {
        let mut a = app();

        a.apply_event(Event::SessionStarted { id: "abc123".into() });

        assert_eq!(a.transcript.items.len(), 1);
        assert!(matches!(a.transcript.items[0].kind, Kind::Notice));
        assert_eq!(a.transcript.items[0].text, "session abc123");
    }

    #[test]
    fn a_new_session_resets_everything_the_footer_reports() {
        // The bug: /new emptied the transcript but left the counters, so the
        // footer went on reporting the previous session's context and cost over
        // an empty screen. Asserting the whole footer string rather than the
        // fields means a counter added later has to be reset to keep this green.
        let mut a = app();
        a.push(Kind::User, "hello");
        a.apply_event(Event::Thinking { text: "x".repeat(400) });
        a.apply_event(Event::Usage {
            prompt_tokens: 3055,
            completion_tokens: 75,
            total_tokens: 3130,
            reasoning_tokens: 52,
            finish_reason: Some("length".into()),
        });
        a.transcript.scroll_up = 7;
        let before = footer_string(&a);
        assert!(before.contains("3055"), "precondition: the footer reports the old session");

        a.reset_for_new_session(PathBuf::from("/tmp/new-session.jsonl"));

        assert_eq!(
            footer_string(&a),
            footer_string(&app()),
            "a new session's footer must read like a fresh one"
        );
        assert!(a.transcript.items.is_empty(), "the transcript is empty");
        assert_eq!(a.transcript.scroll_up, 0, "nothing to be scrolled back into");
        assert_eq!(a.session_path, PathBuf::from("/tmp/new-session.jsonl"));
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
    fn footer_legend_rows_are_well_formed() {
        let rows = footer_legend();
        assert_eq!(rows.len(), 8, "one row per footer glyph");
        for r in &rows {
            assert!(!r.label.is_empty(), "a row with no glyph");
            assert!(!r.description.trim().is_empty(), "{} has no meaning", r.label);
        }
    }

    #[test]
    fn footer_legend_explains_every_glyph_the_footer_shows() {
        // The drift guard: build an app with every conditional footer segment
        // forced on, and assert each legend row's glyph actually appears. Fails
        // if the legend describes a glyph the footer doesn't render, or if a
        // footer glyph is renamed and the legend goes stale.
        let mut a = app();
        a.last_prompt_tokens = 100;
        a.last_reasoning_tokens = 2000;
        a.last_finish_reason = Some("length".into());
        a.prices = crate::config::ModelSettings { input: Some(1.0), output: Some(2.0), ..Default::default() };
        a.total_in_tokens = 1_000_000;
        a.total_out_tokens = 1_000_000;
        a.think_label = Some("2k".into());
        a.agents_running = 2;
        a.agents_queued = 1;

        let s = footer_string(&a);
        for r in footer_legend() {
            // The label is a template (`↓N`, `think:<label>`); the glyph is the
            // literal part — drop the trailing `N` and any `<…>` placeholder.
            let glyph = match r.label.as_str() {
                "<model>" => a.model.as_str(),
                _ => {
                    let head = r.label.split_once(' ').map(|(g, _)| g).unwrap_or(&r.label);
                    let g = head.strip_suffix('N').unwrap_or(head);
                    g.split('<').next().unwrap_or(g)
                }
            };
            assert!(s.contains(glyph), "legend explains `{}` but the footer shows no such glyph in `{s}`", r.label);
        }
    }

    #[test]
    fn footer_omits_segments_it_would_not_show() {
        // The legend must not describe a glyph as always-present when the footer
        // hides it: no prices → no `$`, no cut-off → no `⚠cut`, no agents → no `↑`.
        let a = app(); // no prices, no reasoning, no cut, no agents
        let s = footer_string(&a);
        assert!(!s.contains('$'), "a free model shows no cost");
        assert!(!s.contains("⚠cut"), "no cut-off, no warning");
        assert!(!s.contains('↑'), "no workers, no agent count");
        assert!(!s.contains('↻'), "no reasoning, no reasoning spend");
    }

    #[tokio::test]
    async fn a_pending_approval_takes_over_the_composer() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (approver, mut rx) = crate::tools::approval::ChannelApprover::new();
        let h = tokio::spawn(async move {
            use crate::tools::approval::Approver;
            approver.ask("git push", "pushes commits to a remote").await
        });
        let req = rx.recv().await.unwrap();

        let mut a = app();
        a.running = true; // the turn is blocked on this, not finished
        a.modals.set_approval(req);
        a.ensure_rows(78);

        let mut term = Terminal::new(TestBackend::new(78, 12)).unwrap();
        term.draw(|f| ui(f, &a)).unwrap();
        let rows: Vec<String> = {
            let buf = term.backend().buffer().clone();
            (0..buf.area.height)
                .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect())
                .collect()
        };
        let screen = rows.join("\n");

        // The failure this exists for: the composer said "working…" with a
        // climbing timer while the turn sat waiting for a keypress.
        assert!(screen.contains("APPROVE?"), "the composer asks: {screen}");
        assert!(screen.contains("y = once"), "and says which keys: {screen}");
        assert!(!screen.contains("working…"), "and stops claiming to be busy");
        assert!(screen.contains("waiting for you"), "no spinner, no elapsed time");

        assert!(matches!(
            handle_approval_key(
                KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
                &mut a,
                false
            ),
            Some(Flow::Continue)
        ));
        assert_eq!(h.await.unwrap(), crate::tools::approval::Approval::Once);
    }

    #[tokio::test]
    async fn denying_an_approval_is_not_an_abort() {
        let (approver, mut rx) = crate::tools::approval::ChannelApprover::new();
        let h = tokio::spawn(async move {
            use crate::tools::approval::Approver;
            approver.ask("git push", "pushes commits to a remote").await
        });
        let req = rx.recv().await.unwrap();

        let mut a = app();
        a.running = true;
        a.modals.set_approval(req);

        let flow = handle_approval_key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            &mut a,
            false,
        );

        assert!(
            matches!(flow, Some(Flow::Continue)),
            "denial does not quit the TUI"
        );
        assert!(a.running, "denial answers the gate but does not abort the turn");
        assert_eq!(h.await.unwrap(), crate::tools::approval::Approval::Deny);
    }

    #[tokio::test]
    async fn escape_denies_an_approval() {
        let (approver, mut rx) = crate::tools::approval::ChannelApprover::new();
        let h = tokio::spawn(async move {
            use crate::tools::approval::Approver;
            approver.ask("git push", "pushes commits to a remote").await
        });
        let req = rx.recv().await.unwrap();

        let mut a = app();
        a.modals.set_approval(req);

        let flow =
            handle_approval_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut a, false);

        assert!(matches!(flow, Some(Flow::Continue)));
        assert_eq!(h.await.unwrap(), crate::tools::approval::Approval::Deny);
    }

    #[tokio::test]
    async fn a_checkpoint_answer_goes_to_the_question_not_into_a_new_turn() {
        let (asker, mut rx) = crate::tools::approval::ChannelAsker::new();
        let h = tokio::spawn(async move {
            use crate::tools::approval::Asker;
            asker.ask_text("Pin the worker model", "pin or retarget?").await
        });
        let req = rx.recv().await.unwrap();
        assert_eq!(req.subject, "Pin the worker model");

        let mut a = app();
        a.running = true; // the turn is blocked on the answer
        a.modals.set_ask(req);
        a.composer.insert_str("Pin it.");

        // Unlike an approval, the composer stays a composer — typing works,
        // and only Enter is routed somewhere else. This covers that routing
        // decision, not the key dispatch: the Enter handler needs the whole
        // turn's context (agent, session, workers) to call directly.
        assert_eq!(a.composer.input, "Pin it.");
        let input = a.composer.take_input().trim().to_string();
        assert!(answer_pending_ask(&mut a, Some(input)));

        assert_eq!(h.await.unwrap().as_deref(), Some("Pin it."));
        assert!(a.composer.input.is_empty(), "the answer left the composer");
    }

    #[tokio::test]
    async fn a_pending_checkpoint_stops_the_spinner_claiming_to_be_busy() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (asker, mut rx) = crate::tools::approval::ChannelAsker::new();
        let h = tokio::spawn(async move {
            use crate::tools::approval::Asker;
            asker.ask_text("Pin the worker model", "pin or retarget?").await
        });
        let req = rx.recv().await.unwrap();

        let mut a = app();
        a.running = true;
        a.status = "type your answer · Enter to send · Esc to skip".into();
        a.modals.set_ask(req);
        a.ensure_rows(78);

        let mut term = Terminal::new(TestBackend::new(78, 12)).unwrap();
        term.draw(|f| ui(f, &a)).unwrap();
        let buf = term.backend().buffer().clone();
        let screen: String = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(screen.contains("CHECKPOINT"), "the composer says what it wants: {screen}");
        assert!(screen.contains("Esc skips"), "and that ignoring it is allowed: {screen}");
        assert!(screen.contains("waiting for you"), "no spinner while it waits: {screen}");

        // Esc answers None, and the work carries on.
        assert!(answer_pending_ask(&mut a, None));
        assert_eq!(h.await.unwrap(), None);
    }

    #[test]
    fn the_end_of_a_long_paste_is_actually_on_screen() {
        // The rendered proof, not just the arithmetic: paste a long /spawn line
        // and the tail has to be visible somewhere in the composer.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut a = app();
        a.composer.insert_str(
            "/spawn -n 3 --until \"cd docs && zola check\" Write ONE Zola content page and \
             only that page, do not touch another worker's file. ENDMARKER",
        );
        a.ensure_rows(40);

        let mut term = Terminal::new(TestBackend::new(40, 14)).unwrap();
        term.draw(|f| ui(f, &a)).unwrap();
        let buf = term.backend().buffer().clone();
        let screen: String = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            screen.contains("ENDMARKER"),
            "the tail of a pasted line must be reachable on screen:\n{screen}"
        );
    }

    #[test]
    fn a_pasted_line_wraps_instead_of_vanishing_past_the_edge() {
        // The bug: a long `/spawn` line clipped at the right edge with no
        // horizontal scroll, so everything past ~76 columns was invisible and
        // the cursor pinned to the edge — typing blind.
        let long = "x".repeat(30);
        let (rows, crow, ccol) = wrap_input(&long, 10, 30);
        assert_eq!(rows, vec!["xxxxxxxxxx", "xxxxxxxxxx", "xxxxxxxxxx", ""]);
        assert_eq!((crow, ccol), (3, 0), "the cursor follows onto a fresh row");
    }

    #[test]
    fn the_cursor_and_the_breaks_cannot_disagree() {
        // They were separate calculations before, which is why the composer
        // clipped rather than wrapped.
        let text = "abcdefghij";
        // Sitting exactly on a break belongs to the next row, not to a column
        // off the edge.
        let (rows, crow, ccol) = wrap_input(text, 5, 5);
        assert_eq!(rows, vec!["abcde", "fghij"]);
        assert_eq!((crow, ccol), (1, 0));

        // And every earlier position lands where the character is drawn.
        for c in 0..5 {
            let (_, r, col) = wrap_input(text, 5, c);
            assert_eq!((r, col), (0, c), "cursor {c}");
        }
    }

    #[test]
    fn explicit_newlines_survive_wrapping() {
        // Alt+Enter inserts a newline; those are real rows, not wrap points.
        let (rows, crow, ccol) = wrap_input("ab\ncdefgh", 4, 3);
        assert_eq!(rows, vec!["ab", "cdef", "gh"]);
        assert_eq!((crow, ccol), (1, 0), "just after the newline");

        // An empty trailing line is a row of its own.
        let (rows, crow, _) = wrap_input("ab\n", 8, 3);
        assert_eq!(rows, vec!["ab", ""]);
        assert_eq!(crow, 1);
    }

    #[test]
    fn wrapping_counts_characters_not_bytes() {
        // Multibyte input must not split a char or miscount the column.
        let (rows, crow, ccol) = wrap_input("日本語テスト", 3, 4);
        assert_eq!(rows, vec!["日本語", "テスト"]);
        assert_eq!((crow, ccol), (1, 1));
    }

    #[test]
    fn an_empty_composer_has_one_row_and_a_cursor_at_the_start() {
        let (rows, crow, ccol) = wrap_input("", 20, 0);
        assert_eq!(rows, vec![""]);
        assert_eq!((crow, ccol), (0, 0));
    }

    #[test]
    fn model_completes_from_the_configured_models() {
        // Config-driven rather than a hardcoded list, so it cannot go stale the
        // way the other arg tables can.
        let cfg: Config = toml::from_str(
            r#"
            [models."openrouter/qwen/qwen3.8-27b"]
            input = 0.2
            [models."vllm/local-model"]
            temperature = 0.6
            "#,
        )
        .unwrap();

        let (_, all) =
            compute_completions("/model ", Path::new("."), &probe_store(), &cfg).unwrap();
        assert!(all.contains(&"openrouter/qwen/qwen3.8-27b".to_string()), "{all:?}");
        assert!(all.contains(&"vllm/local-model".to_string()), "{all:?}");
        assert!(all.contains(&"default".to_string()), "reverting is offered too: {all:?}");

        // And it filters on the prefix typed so far.
        let (_, some) =
            compute_completions("/model vllm/", Path::new("."), &probe_store(), &cfg).unwrap();
        assert_eq!(some, vec!["vllm/local-model".to_string()]);
    }

    #[test]
    fn a_checkpoint_is_its_own_channel_not_a_notice() {
        let mut a = app();
        a.apply_event(Event::Checkpoint {
            kind: "yours".into(),
            subject: "ActiveModel::from_override".into(),
            detail: "stubbed at llm/mod.rs:440 — must reset sampling".into(),
        });
        assert!(matches!(a.transcript.items[0].kind, Kind::Pair), "a checkpoint is not machinery chatter");
        assert!(a.transcript.items[0].text.contains("yours — ActiveModel::from_override"));

        // An `ask` renders when its answer lands, not when it is raised: the
        // question is already on screen in the composer's prompt.
        let mut b = app();
        b.apply_event(Event::Checkpoint {
            kind: "ask".into(),
            subject: "Pin the worker model".into(),
            detail: "pin or retarget?".into(),
        });
        assert!(b.transcript.items.is_empty(), "the question is not printed twice");
    }

    #[test]
    fn a_dropped_setting_is_shown_not_swallowed() {
        let mut a = app();
        a.apply_event(Event::Warning { message: "budget ignored".into() });
        assert!(matches!(a.transcript.items[0].kind, Kind::Notice));
        assert!(a.transcript.items[0].text.contains("budget ignored"));
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
            a.composer.insert_char(c);
        }
        assert_eq!(a.composer.input, "helo");
        assert_eq!(a.composer.cursor, 4);
        // Cursor before 'o'; insert the missing 'l' → "hello".
        a.composer.move_left();
        a.composer.insert_char('l');
        assert_eq!(a.composer.input, "hello");
        assert_eq!(a.composer.cursor, 4); // between the new 'l' and 'o'
        a.composer.move_end();
        a.composer.backspace();
        assert_eq!(a.composer.input, "hell");
    }

    #[test]
    fn composer_paste_is_multiline_at_cursor() {
        let mut a = app();
        a.composer.insert_str("line1\nline2\nline3");
        assert_eq!(a.composer.input.split('\n').count(), 3);
        assert_eq!(a.composer.cursor, a.composer.char_len());
        let (_, row, _col) = wrap_input(&a.composer.input, 80, a.composer.cursor);
        assert_eq!(row, 2, "cursor should be on the last pasted line");
    }

    #[test]
    fn readline_keys_move_the_cursor_in_the_composer() {
        // Ctrl+W was bound and Ctrl+A was not, so the hands went to a key that
        // did nothing. These are asserted on the App methods the key handler
        // calls; the handler itself needs the whole turn's context to invoke.
        let mut a = app();
        a.composer.insert_str("hello world");
        assert_eq!(a.composer.cursor, 11);

        a.composer.move_home(); // Ctrl+A
        assert_eq!(a.composer.cursor, 0);
        a.composer.move_end(); // Ctrl+E
        assert_eq!(a.composer.cursor, 11);

        // Home/End work per logical line, so a multi-line paste stays sane.
        a.composer.clear_input();
        a.composer.insert_str("one\ntwo");
        a.composer.move_home();
        assert_eq!(a.composer.cursor, 4, "start of the line the cursor is on, not of the buffer");
        a.composer.move_end();
        assert_eq!(a.composer.cursor, 7);
    }

    #[test]
    fn composer_delete_word_and_home_end() {
        let mut a = app();
        a.composer.insert_str("foo bar baz");
        a.composer.delete_word();
        assert_eq!(a.composer.input, "foo bar ");
        a.composer.move_home();
        assert_eq!(a.composer.cursor, 0);
        a.composer.move_end();
        assert_eq!(a.composer.cursor, a.composer.char_len());
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
        a.composer.insert_str("first");
        let _ = a.composer.take_input();
        a.composer.insert_str("second");
        let _ = a.composer.take_input();
        assert_eq!(a.composer.history.len(), 2);

        a.composer.insert_str("draft");
        a.composer.history_prev();
        assert_eq!(a.composer.input, "second");
        a.composer.history_prev();
        assert_eq!(a.composer.input, "first");
        a.composer.history_next();
        assert_eq!(a.composer.input, "second");
        a.composer.history_next();
        assert_eq!(a.composer.input, "draft", "past newest restores the draft");
    }

    /// Completion needs a store to offer memory ids; these tests are about the
    /// command grammar, so an empty one in a temp dir is the point.
    fn probe_store() -> crate::memory::MemoryStore {
        let dir = std::env::temp_dir().join(format!("ws-compl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        crate::memory::MemoryStore::open(Some(&dir)).unwrap()
    }

    /// The incremental cache must be indistinguishable from re-wrapping
    /// everything. It is a cache; if it can disagree with the truth it is a bug
    /// generator, and this one already silently dropped the transcript once.
    #[test]
    fn the_row_cache_matches_a_full_rebuild() {
        let mut a = app();
        let full = |a: &App| {
            build_rows(
                &a.transcript.items,
                a.transcript.collapse_tools,
                a.transcript.show_thinking,
                60,
            )
        };
        let text = |rows: &[Line]| -> Vec<String> {
            rows.iter()
                .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
                .collect()
        };

        a.push(Kind::User, "write the linter");
        a.push(Kind::ToolResult, "output ".repeat(80));
        a.ensure_rows(60);
        assert_eq!(text(&a.transcript.cached_rows), text(&full(&a)), "after appends");

        // Streaming: repeated appends to the last item, the hot path.
        for _ in 0..30 {
            a.apply_event(Event::MessageDelta { text: "token ".into() });
            a.ensure_rows(60);
        }
        assert_eq!(text(&a.transcript.cached_rows), text(&full(&a)), "after streaming");
        // The prefix is the thing at risk: a truncation bug loses it silently.
        assert!(
            text(&a.transcript.cached_rows)[0].contains("write the linter"),
            "the first item survived streaming: {:?}",
            &text(&a.transcript.cached_rows)[..2]
        );

        // An item pushed after streaming, then a toggle that changes how
        // *every* item renders, then a width change.
        a.push(Kind::Tool, "⚙ grep");
        a.ensure_rows(60);
        assert_eq!(text(&a.transcript.cached_rows), text(&full(&a)), "after a later push");

        a.transcript.collapse_tools = true;
        a.touch_all();
        a.ensure_rows(60);
        assert_eq!(text(&a.transcript.cached_rows), text(&full(&a)), "after a render toggle");

        a.ensure_rows(30);
        assert_eq!(
            text(&a.transcript.cached_rows),
            text(&build_rows(
                &a.transcript.items,
                a.transcript.collapse_tools,
                a.transcript.show_thinking,
                30,
            )),
            "after a width change"
        );
    }

    #[test]
    fn streaming_rewraps_only_the_item_being_written() {
        // The regression this exists for: every token re-wrapped the whole
        // transcript, which at 60 turns of real tool output was 15ms per token.
        let mut a = app();
        a.push(Kind::ToolResult, "x".repeat(5_000));
        a.ensure_rows(60);
        let prefix = a.transcript.cached_rows.len();

        a.apply_event(Event::MessageDelta { text: "hello".into() });
        assert_eq!(
            a.transcript.dirty_from,
            Some(a.transcript.items.len() - 1),
            "only the last item is stale"
        );
        a.ensure_rows(60);
        assert!(a.transcript.cached_rows.len() > prefix, "the prefix was kept, not rebuilt");
        assert_eq!(a.transcript.item_starts.len(), a.transcript.items.len(), "one start per item");
    }

    #[test]
    fn jj_in_quick_succession_leaves_the_composer() {
        let mut a = app();
        a.insert_escape = Some(('j', 'j', Duration::from_millis(300)));

        // A lone `j` is just a character.
        assert!(!a.escape_pair('j'));
        a.composer.insert_char('j');
        assert_eq!(a.composer.input, "j");
        assert_eq!(a.transcript.mode, Mode::Insert);

        // The second one completes the pair and takes the first `j` back with
        // it — otherwise you would land in normal mode with a stray character.
        assert!(a.escape_pair('j'));
        assert_eq!(a.composer.input, "", "the pending j is removed");
    }

    #[test]
    fn a_slow_jj_is_just_two_letters() {
        let mut a = app();
        a.insert_escape = Some(('j', 'j', Duration::from_millis(1)));
        assert!(!a.escape_pair('j'));
        a.composer.insert_char('j');
        std::thread::sleep(Duration::from_millis(5));
        assert!(!a.escape_pair('j'), "too slow to be the escape");
        a.composer.insert_char('j');
        assert_eq!(a.composer.input, "jj", "prose survives: this composer holds words");
    }

    #[test]
    fn jj_only_fires_at_the_end_of_what_you_typed() {
        let mut a = app();
        a.insert_escape = Some(('j', 'j', Duration::from_millis(300)));

        // A `j` typed mid-word, then the cursor moved: the pair must not fire
        // and quietly delete a character somewhere else.
        a.composer.set_input("hajj".into());
        a.composer.cursor = 2;
        assert!(!a.escape_pair('j'));
        assert!(!a.escape_pair('j'));
        assert_eq!(a.composer.input, "hajj", "nothing was removed");
    }

    #[test]
    fn the_escape_can_be_turned_off_or_rebound() {
        let mut a = app();
        a.insert_escape = None;
        assert!(!a.escape_pair('j'));
        a.composer.insert_char('j');
        assert!(!a.escape_pair('j'), "disabled means it is only ever a letter");

        // Rebinding to a different pair works the same way.
        let mut b = app();
        b.insert_escape = Some(('j', 'k', Duration::from_millis(300)));
        assert!(!b.escape_pair('j'));
        b.composer.insert_char('j');
        assert!(b.escape_pair('k'));
        assert_eq!(b.composer.input, "");
    }

    #[test]
    fn nothing_names_a_colour_that_is_the_background_on_a_light_theme() {
        // Found by running on a light Ghostty theme: the model answered, the
        // footer said done, and the transcript showed nothing. The reply was
        // there the whole time, white-on-white.
        //
        // Worksmith emits ANSI colour *names*, never RGB, so the terminal's
        // theme is worksmith's theme — which only holds while every name is a
        // hue. White is ANSI 7: a contrast extreme whose end of the axis flips
        // between light and dark. Emphasis belongs to modifiers, which no theme
        // can invert.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        fn assert_no_white(a: &App, what: &str) {
            let mut term = Terminal::new(TestBackend::new(78, 12)).unwrap();
            term.draw(|f| ui(f, a)).unwrap();
            let buf = term.backend().buffer().clone();
            for y in 0..buf.area.height {
                for x in 0..buf.area.width {
                    let cell = &buf[(x, y)];
                    assert_ne!(
                        cell.fg,
                        Color::White,
                        "{what}: ({x},{y}) {:?} names White as a foreground",
                        cell.symbol()
                    );
                }
            }
        }

        // Absence of White is only half the claim. Removing the highlight
        // entirely also leaves no white cell, and the selected row is then
        // unmarked — which is the bug this test was written after, one level
        // up. So assert the good thing is present, not just the bad thing gone.
        fn assert_row_is_marked(a: &App, needle: &str, what: &str) {
            let mut term = Terminal::new(TestBackend::new(78, 12)).unwrap();
            term.draw(|f| ui(f, a)).unwrap();
            let buf = term.backend().buffer().clone();
            let marked = (0..buf.area.height).any(|y| {
                let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
                row.contains(needle)
                    && (0..buf.area.width)
                        .any(|x| buf[(x, y)].modifier.contains(Modifier::REVERSED))
            });
            assert!(marked, "{what}: the selected row must be visibly highlighted");
        }

        // The overlay paints over the transcript, so they need separate draws —
        // checking them in one render silently skips whichever is underneath.
        let mut a = app();
        a.push(Kind::Assistant, "a reply");
        a.ensure_rows(78);
        assert!(!a.transcript.cached_rows.is_empty(), "the transcript must actually render");
        assert_no_white(&a, "transcript");

        a.overlay = Some(Overlay::new(
            "commands",
            vec![OverlayItem { label: "/help".into(), description: "keys".into() }],
        ));
        assert_no_white(&a, "picker");
        assert_row_is_marked(&a, "/help", "picker");

        a.overlay = Some(Overlay::reference(
            "footer",
            vec![OverlayItem { label: "\u{21bb}".into(), description: "thinking".into() }],
        ));
        assert_no_white(&a, "legend");
        assert_row_is_marked(&a, "\u{21bb}", "legend");
    }

    #[test]
    fn a_row_maps_back_to_the_item_it_came_from() {
        // `y` yanks the message, not the wrapped line under the cursor, so this
        // lookup is what makes the feature mean anything.
        let mut a = app();
        a.push(Kind::User, "short");
        a.push(Kind::Assistant, "long ".repeat(60));
        a.push(Kind::Tool, "⚙ grep");
        a.ensure_rows(40);

        assert_eq!(a.item_at_row(0), Some(0));
        let assistant_start = a.transcript.item_starts[1];
        assert_eq!(a.item_at_row(assistant_start), Some(1));
        assert_eq!(a.item_at_row(assistant_start + 1), Some(1), "a wrapped row is still item 1");
        assert_eq!(a.item_at_row(a.transcript.item_starts[2]), Some(2));
        assert_eq!(a.item_at_row(9_999), Some(2), "past the end clamps to the last item");
    }

    #[test]
    fn search_finds_wraps_and_reports_nothing_found() {
        let mut a = app();
        a.push(Kind::Assistant, "the listing caption is missing");
        a.push(Kind::Assistant, "unrelated");
        a.push(Kind::Assistant, "another listing here");
        a.ensure_rows(80);
        a.enter_normal();

        a.set_search(Some(Search { pattern: "LISTING".into(), typing: false }));
        let hits = a.transcript.search_hits().to_vec();
        assert_eq!(hits.len(), 2, "case-insensitive: {hits:?}");

        // Jumping repeatedly cycles the matches and comes back round, rather
        // than stopping at the last one.
        a.transcript.cursor_row = 0;
        let mut visited = Vec::new();
        for _ in 0..3 {
            assert!(a.jump_match(true));
            visited.push(a.transcript.cursor_row);
        }
        assert_eq!(visited, vec![hits[1], hits[0], hits[1]], "forward wraps: {visited:?}");

        // Backwards cycles the other way.
        assert!(a.jump_match(false));
        assert_eq!(a.transcript.cursor_row, hits[0]);

        // A pattern that matches nothing must say so, not move the cursor.
        a.set_search(Some(Search { pattern: "zzz".into(), typing: false }));
        let before = a.transcript.cursor_row;
        assert!(!a.jump_match(true));
        assert_eq!(a.transcript.cursor_row, before);
    }

    #[test]
    fn search_hits_refresh_when_rows_change() {
        let mut a = app();
        a.push(Kind::Assistant, "nothing yet");
        a.ensure_rows(80);
        a.enter_normal();
        a.set_search(Some(Search { pattern: "needle".into(), typing: false }));
        assert!(a.transcript.search_hits().is_empty());

        a.push(Kind::Assistant, "needle arrived later");
        assert!(a.transcript.search_hits_dirty, "new rows invalidate the cached hits");
        a.ensure_rows(80);

        let hits = a.transcript.search_hits().to_vec();
        assert_eq!(hits.len(), 1, "the appended row should be the only match: {hits:?}");
        assert!(
            row_text(&a.transcript.cached_rows[hits[0]]).contains("needle arrived later"),
            "the cached search result must include rows appended after the search started"
        );
    }

    #[test]
    fn leaving_normal_mode_resumes_following_the_tail() {
        let mut a = app();
        a.push(Kind::Assistant, "hello");
        a.ensure_rows(80);

        a.enter_normal();
        assert_eq!(a.transcript.mode, Mode::Normal);
        assert!(!a.transcript.follow, "reading should not jump to the bottom on new output");

        a.set_search(Some(Search { pattern: "x".into(), typing: true }));
        a.enter_insert();
        assert_eq!(a.transcript.mode, Mode::Insert);
        assert!(a.transcript.follow, "typing means you want to see what arrives");
        assert!(a.transcript.search.is_none(), "a stale search must not keep highlighting");
    }

    #[test]
    fn the_cursor_and_matches_are_visibly_marked() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut a = app();
        a.push(Kind::Assistant, "the listing caption");
        a.push(Kind::Assistant, "plain row");
        a.ensure_rows(78);
        a.enter_normal();
        a.transcript.cursor_row = 0;
        a.set_search(Some(Search { pattern: "listing".into(), typing: false }));

        let mut term = Terminal::new(TestBackend::new(78, 10)).unwrap();
        term.draw(|f| ui(f, &a)).unwrap();
        let buf = term.backend().buffer().clone();

        // Row 0 is both the cursor and a match; the cursor wins, and it is
        // reverse video so it reads in any theme rather than a colour that
        // might vanish.
        assert!(
            buf[(2, 0)].modifier.contains(Modifier::REVERSED),
            "the cursor row is marked: {:?}",
            buf[(2, 0)]
        );

        // And the mode is stated, because not knowing which mode you are in is
        // the failure people remember.
        let rows: Vec<String> = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect())
            .collect();
        assert!(rows.iter().any(|r| r.contains("NORMAL")), "mode is visible");
    }

    #[test]
    fn enter_takes_the_highlighted_command_unless_one_is_fully_typed() {
        // The reported bug: Enter fell through to the command handler and ran
        // the half-typed text — "unknown command: /agen" — while a highlighted
        // list was on screen implying it would pick that.
        assert!(hint_enter_accepts("/"));
        assert!(hint_enter_accepts("/agen"));
        assert!(hint_enter_accepts("/mem"));

        // Fully typed: run it, so muscle memory survives.
        assert!(!hint_enter_accepts("/help"));
        assert!(!hint_enter_accepts("  /quit  "));
    }

    #[test]
    fn the_hint_follows_a_command_being_typed() {
        let mut a = app();
        a.composer.set_input("/".into());
        a.composer.refresh_hint();
        assert_eq!(a.composer.hint.as_ref().unwrap().matches().len(), COMMANDS.len());

        a.composer.set_input("/me".into());
        a.composer.refresh_hint();
        let got: Vec<String> =
            a.composer.hint.as_ref().unwrap().matches().iter().map(|(_, i)| i.label.clone()).collect();
        assert_eq!(got, vec!["/memory"]);

        // Once the command is complete and arguments start, this is the wrong
        // list to be showing — argument completion is a different thing.
        a.composer.set_input("/memory ".into());
        a.composer.refresh_hint();
        assert!(a.composer.hint.is_none(), "a space ends it");

        // Nothing matches: no popup rather than an empty box.
        a.composer.set_input("/zzz".into());
        a.composer.refresh_hint();
        assert!(a.composer.hint.is_none());

        // Ordinary prose is not a command.
        a.composer.set_input("what does /memory do".into());
        a.composer.refresh_hint();
        assert!(a.composer.hint.is_none());
    }

    #[test]
    fn the_footer_shows_what_the_workers_spent() {
        // Reported with prices set and workers running: the cost never moved.
        // Worker events go to the worker's own bus and never reach the
        // parent's, so the footer — fed entirely from the parent — described a
        // session doing nothing next to a counter saying an agent was busy.
        //
        // Then, once it was counted, it still showed nothing: the cost was
        // looked up by model *name*, and the name is stored without its
        // provider prefix, so `qwen/qwen3.5-9b` never matched the config's
        // `openrouter/qwen/qwen3.5-9b`. It is now priced by the manager, using
        // each worker's own resolved settings — which is what ModelOverride
        // carries them for.
        let mut a = app();
        a.agent_spend = crate::worker::WorkerSpend {
            prompt: 1_000_000,
            completion: 1_000_000,
            cost: 0.30,
        };
        a.agents_running = 1;

        let f = footer_string(&a);
        assert!(f.contains("🤖 1 running"), "worker state is shown: {f}");
        assert!(f.contains("🪙 1000000"), "worker output is shown: {f}");
        assert!(f.contains("$0.30"), "and what it cost: {f}");
    }

    #[test]
    fn the_agent_count_comes_before_the_costs_it_used_to_hide_behind() {
        // It was last in a string that truncates at terminal width, so on an
        // 80-column terminal the one time-critical field was the first to be
        // cut — which is why it was reported as "no indication that there are
        // agents running".
        let mut a = app();
        a.agents_running = 2;
        let f = footer_string(&a);
        let agents = f.find("2 running").expect("shown at all");
        assert!(agents < 60, "near the front, not off the edge: {agents} in {f:?}");
    }

    #[test]
    fn empty_enter_with_nothing_pending_must_not_start_a_turn() {
        // Two halves, and only the first was ever specified. A worker asked to
        // make bare Enter skip a pending checkpoint delivered that correctly and
        // removed the guard behind it, so an empty composer with nothing pending
        // began falling through to `start_turn` — a model call on an empty
        // prompt for every stray Enter. `cargo test` passed, because nothing
        // covered the half nobody asked about.
        //
        // The first version of *this* test was no better: it looked for a
        // `return` anywhere in the block and found the one inside the skip
        // branch, so it passed with the guard deleted. It is checked against
        // that now — the empty-input block needs **two** returns, the skip and
        // the fallback.
        //
        // It reads the source because driving the key handler needs a live
        // agent, session and worker manager. Crude, and it fails when the guard
        // goes, which is the requirement.
        let src = include_str!("tui.rs");
        let enter_start = src.find("async fn handle_enter_key").expect("Enter handling stays named");
        let rest = &src[enter_start..];
        let block_start = rest
            .find("    if input.is_empty() {")
            .expect("the composer's empty-input guard");
        let rest = &rest[block_start..];

        // Walk to the matching close brace so the count cannot wander into the
        // command dispatch below.
        let open = rest.find('{').unwrap();
        let mut depth = 0usize;
        let mut end = open;
        for (i, c) in rest[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = open + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        let block = &rest[open..=end];
        let returns = block.matches("return Ok(Flow::Continue);").count();
        assert_eq!(
            returns, 2,
            "the empty-input block needs both returns — one to answer a pending \
             checkpoint with None, one so an empty composer with nothing pending \
             does not start a turn:\n{block}"
        );
    }

    #[test]
    fn submitting_a_command_leaves_no_stale_hint() {
        // `/agents<Enter>` ran the command and left the command list sitting on
        // screen until the next keystroke or an Esc, because the command branch
        // returns before the handler's single `refresh_hint()` — the one whose
        // comment says no edit path can forget it. Running a command is an edit
        // path: it empties the composer.
        //
        // This pins the invariant the fix restores rather than the call site:
        // whatever the composer holds, the hint must agree with it.
        let mut a = app();
        a.composer.set_input("/agents".into());
        a.composer.refresh_hint();
        assert!(a.composer.hint.is_some(), "the list is up while the command is typed");

        // What submitting does: the composer empties.
        a.composer.set_input(String::new());
        a.composer.refresh_hint();
        assert!(a.composer.hint.is_none(), "an empty composer must not still show a command list");
    }

    #[test]
    fn ctrl_n_is_a_portable_newline_chord() {
        assert!(key_inserts_newline(&KeyEvent::new(
            KeyCode::Char('n'),
            KeyModifiers::CONTROL
        )));
        assert!(key_inserts_newline(&KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)));
        assert!(key_inserts_newline(&KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::SHIFT
        )));

        assert!(!key_inserts_newline(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        assert!(!key_inserts_newline(&KeyEvent::new(
            KeyCode::Char('n'),
            KeyModifiers::NONE
        )));
    }

    #[test]
    fn pasting_a_command_refreshes_a_stale_hint() {
        // Bracketed paste bypasses `handle_key`, so it must maintain the same
        // hint invariant itself. Otherwise a stale `/help` hint can accept on
        // Enter and replace the pasted `/spawn ...` command.
        let mut a = app();
        a.composer.set_input("/h".into());
        a.composer.refresh_hint();
        assert_eq!(a.composer.hint.as_ref().unwrap().chosen().as_deref(), Some("/help"));

        a.composer.set_input(String::new());
        a.composer.paste("/spawn --model openrouter/qwen/qwen3.5-9b run pwd");

        assert!(
            a.composer.hint.is_none(),
            "a pasted command with arguments must not keep an old command hint"
        );
    }

    #[test]
    fn typing_further_does_not_jump_the_selection() {
        let mut a = app();
        a.composer.set_input("/m".into());
        a.composer.refresh_hint();
        a.composer.hint.as_mut().unwrap().move_by(1); // highlight /model
        assert_eq!(a.composer.hint.as_ref().unwrap().chosen().as_deref(), Some("/model"));

        // One more character narrows the list; the selection must stay valid
        // rather than pointing past the end or silently resetting.
        a.composer.set_input("/mo".into());
        a.composer.refresh_hint();
        let h = a.composer.hint.as_ref().unwrap();
        assert!(h.chosen().is_some(), "something is always selectable");
        assert!(h.matches().len() <= 2);
    }

    #[test]
    fn the_hint_draws_above_the_composer() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut a = app();
        a.composer.set_input("/me".into());
        a.composer.refresh_hint();
        a.ensure_rows(80);

        let mut term = Terminal::new(TestBackend::new(80, 16)).unwrap();
        term.draw(|f| ui(f, &a)).unwrap();
        let buf = term.backend().buffer().clone();
        let rows: Vec<String> = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect())
            .collect();

        let hint_row = rows.iter().position(|r| r.contains("/memory")).expect("hint is drawn");
        // No `unwrap_or` fallback here. Drawing the hint *below* the composer
        // covers the text being typed, so "/me " goes missing — and a fallback
        // of "assume it is the last row" turns that into a passing comparison.
        // The mutation that moves the hint down must fail this test, not sail
        // through it.
        let composer_row = rows
            .iter()
            .position(|r| r.contains("/me "))
            .expect("the composer still shows what is being typed");
        assert!(hint_row < composer_row, "the hint sits above what you are typing");
        assert!(rows.iter().any(|r| r.contains("Tab accepts")), "and says how to take it");
    }

    #[test]
    fn the_picker_filters_on_label_and_description() {
        let mut ov = Overlay::new(
            "commands",
            vec![
                OverlayItem { label: "/memory".into(), description: "what is remembered".into() },
                OverlayItem { label: "/mouse".into(), description: "wheel vs. selection".into() },
                OverlayItem { label: "/quit".into(), description: "exit".into() },
            ],
        );
        assert_eq!(ov.matches().len(), 3);

        ov.set_filter("mo");
        let got: Vec<&str> = ov.matches().iter().map(|(_, i)| i.label.as_str()).collect();
        assert_eq!(got, vec!["/memory", "/mouse"]);

        // Matching the description too is the point: you look for what a thing
        // *does* when you cannot remember what it is called.
        ov.set_filter("remember");
        assert_eq!(ov.matches().len(), 1);
        assert_eq!(ov.chosen().as_deref(), Some("/memory"));

        ov.set_filter("zzz");
        assert!(ov.matches().is_empty());
        assert_eq!(ov.chosen(), None, "an empty list must not yield a selection");
    }

    #[test]
    fn overlay_filter_edits_refresh_cached_matches() {
        let mut ov = Overlay::new(
            "commands",
            vec![
                OverlayItem { label: "/memory".into(), description: "what is remembered".into() },
                OverlayItem { label: "/mouse".into(), description: "wheel vs. selection".into() },
                OverlayItem { label: "/quit".into(), description: "exit".into() },
            ],
        );

        ov.push_filter('m');
        ov.push_filter('o');
        let got: Vec<&str> = ov.matches().iter().map(|(_, i)| i.label.as_str()).collect();
        assert_eq!(got, vec!["/memory", "/mouse"]);

        ov.pop_filter();
        assert_eq!(ov.matches().len(), 2);
        ov.set_filter("exit");
        assert_eq!(ov.chosen().as_deref(), Some("/quit"));
    }

    #[test]
    fn selection_wraps_and_survives_a_shrinking_list() {
        let mut ov = Overlay::new(
            "commands",
            vec![
                OverlayItem { label: "/a".into(), description: "one".into() },
                OverlayItem { label: "/b".into(), description: "two".into() },
            ],
        );
        ov.move_by(1);
        assert_eq!(ov.chosen().as_deref(), Some("/b"));
        ov.move_by(1);
        assert_eq!(ov.chosen().as_deref(), Some("/a"), "wraps to the top");
        ov.move_by(-1);
        assert_eq!(ov.chosen().as_deref(), Some("/b"), "and to the bottom");

        // Filtering to fewer items than the current index must not panic or
        // point past the end.
        ov.selected = 1;
        ov.set_filter("one");
        assert_eq!(ov.chosen().as_deref(), Some("/a"));
    }

    #[test]
    fn every_command_is_listed_with_a_description() {
        // The table backs completion, the picker and /help at once; an entry
        // without a description would show as a blank row.
        for (name, desc) in COMMANDS {
            assert!(name.starts_with('/'), "{name} should be a slash command");
            assert!(!desc.trim().is_empty(), "{name} has no description");
        }
    }

    #[test]
    fn the_picker_draws_over_the_transcript() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = app();
        app.push(Kind::Assistant, "x".repeat(400));
        app.overlay = Some(Overlay::new(
            "commands",
            vec![OverlayItem { label: "/memory".into(), description: "what is remembered".into() }],
        ));

        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| ui(f, &app)).unwrap();
        let rendered: String = term.backend().buffer().content().iter().map(|c| c.symbol()).collect();

        assert!(rendered.contains("/memory"), "the picker is on screen");
        assert!(rendered.contains("what is remembered"), "with its description");
        assert!(rendered.contains("Esc close"), "and how to get out of it");
    }

    #[test]
    fn the_footer_legend_renders_in_the_picker() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = app();
        app.overlay = Some(Overlay::reference("footer legend · Esc close", footer_legend()));

        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| ui(f, &app)).unwrap();
        let rendered: String = term.backend().buffer().content().iter().map(|c| c.symbol()).collect();

        assert!(rendered.contains("footer legend"), "the legend's title");
        assert!(rendered.contains("Esc close"), "and how to get out");
        assert!(!rendered.contains("Enter pick"), "a reference has nothing to pick");
        assert!(rendered.contains("reasoning tokens"), "the ↻ row's meaning");
        assert!(rendered.contains("max-tokens"), "the ⚠cut row's meaning");
    }

    #[test]
    fn completes_slash_commands() {
        let (start, c) = compute_completions("/me", Path::new("."), &probe_store(), &Config::default()).unwrap();
        assert_eq!(start, 0);
        assert_eq!(c, vec!["/memory ".to_string()]);

        let (_, all) = compute_completions("/", Path::new("."), &probe_store(), &Config::default()).unwrap();
        assert!(all.len() >= 5);

        // Not in command position → no command completion.
        assert!(compute_completions("hi /me", Path::new("."), &probe_store(), &Config::default()).is_none());
    }

    #[test]
    fn completes_subcommands_and_args() {
        // /agents subcommands
        let (_, c) = compute_completions("/agents ", Path::new("."), &probe_store(), &Config::default()).unwrap();
        assert!(c.contains(&"list ".to_string()) && c.contains(&"kill ".to_string()), "{c:?}");
        let (_, c) = compute_completions("/agents k", Path::new("."), &probe_store(), &Config::default()).unwrap();
        assert_eq!(c, vec!["kill ".to_string()]);

        // /memory subcommands, then add's scope + kind
        let (_, c) = compute_completions("/memory ", Path::new("."), &probe_store(), &Config::default()).unwrap();
        assert!(c.contains(&"forget ".to_string()) && c.contains(&"add ".to_string()), "{c:?}");
        let (_, c) = compute_completions("/memory add ", Path::new("."), &probe_store(), &Config::default()).unwrap();
        assert_eq!(c, vec!["global ".to_string(), "project ".to_string()]);
        let (_, c) = compute_completions("/memory add project ", Path::new("."), &probe_store(), &Config::default()).unwrap();
        assert!(c.contains(&"decision ".to_string()) && c.contains(&"lesson ".to_string()), "{c:?}");

        // /help has one subcommand: footer.
        let (_, c) = compute_completions("/help ", Path::new("."), &probe_store(), &Config::default()).unwrap();
        assert_eq!(c, vec!["footer ".to_string()]);
        let (_, c) = compute_completions("/help f", Path::new("."), &probe_store(), &Config::default()).unwrap();
        assert_eq!(c, vec!["footer ".to_string()]);
    }

    #[test]
    fn completes_at_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("main.rs"), "").unwrap();
        std::fs::write(dir.path().join("mod.rs"), "").unwrap();

        let (start, c) = compute_completions("@m", dir.path(), &probe_store(), &Config::default()).unwrap();
        assert_eq!(start, 0);
        assert!(c.contains(&"@main.rs".to_string()), "{c:?}");
        assert!(c.contains(&"@mod.rs".to_string()), "{c:?}");

        // Directories get a trailing slash.
        let (_, d) = compute_completions("@s", dir.path(), &probe_store(), &Config::default()).unwrap();
        assert!(d.contains(&"@src/".to_string()), "{d:?}");
    }

    #[test]
    fn build_rows_wraps_to_width_and_labels_channels() {
        let mut a = app();
        a.apply_event(Event::UserMessage { text: "hello world this is a long line".into() });
        let rows = build_rows(&a.transcript.items, a.transcript.collapse_tools, true, 16);
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
