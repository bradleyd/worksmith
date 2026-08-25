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
use std::time::{Duration, Instant};

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
use crate::memory::{IdMatch, MemoryStore, Scope, short_id};
use crate::prompt::{build_system_prompt, build_worker_prompt};
use crate::session::Session;
use crate::validation::CommandValidator;
use crate::fanout::{
    FanOut, PendingFanOut, assign, fanout_notice, matching_files, parse_spawn, plan_fanout,
    spawn_notice,
};
use crate::report::{
    GroupAcc, group_report, record_in_group, single_report, truncate, truncate_chars,
    worker_headline,
};
use crate::config::Config;
use crate::llm::ModelOverride;
use crate::supervisor::SupervisorConfig;
use crate::worker::WorkerManager;

/// A planner call in flight: the task producing subtasks, plus everything the
/// resulting spawn needs — system prompt, the original request, and the model
/// the workers will run on.
type PlannedFanOut = (
    JoinHandle<crate::fanout::FanOutPlan>,
    String,
    String,
    Option<ModelOverride>,
    // The per-worker check, held across planning so a planned fan-out is
    // validated the same as an explicit one.
    Option<String>,
);

/// Typing, or reading. See `App::mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Insert,
    Normal,
}

/// A `/` search over the transcript.
#[derive(Debug, Clone, Default)]
struct Search {
    pattern: String,
    /// True while the pattern is being typed; Enter commits it.
    typing: bool,
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

/// The plain text of a rendered row, for searching.
fn row_text(line: &Line) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

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
    /// A pairing checkpoint. Its own channel on purpose: rendered as a notice
    /// it reads as machinery chatter, and machinery chatter is what teaches
    /// someone to stop reading the transcript.
    Pair,
}

struct Item {
    kind: Kind,
    text: String,
}

/// How many lines of a long tool result to show before capping (Ctrl+O expands).
const TOOL_RESULT_PREVIEW_LINES: usize = 15;

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
    ("/pair", "stop at decisions so you learn the code being written"),
    ("/mouse", "wheel scrolls the transcript (off: the terminal keeps the wheel)"),
    ("/trust", "is this project's own config in effect?"),
    ("/history", "what the loop did, and when"),
    ("/quit", "exit"),
];

/// A floating list: a filter line and a scrollable set of choices. One
/// component, because everything awkward in this UI is picking an opaque thing
/// — a command, a model, a session, a worker id.
struct Overlay {
    title: String,
    items: Vec<OverlayItem>,
    filter: String,
    selected: usize,
    /// A picker lets you select a row (Enter puts it in the composer). A
    /// reference — like the footer legend — has nothing to pick: Enter just
    /// closes, and the footer bar says so.
    picking: bool,
}

#[derive(Clone)]
struct OverlayItem {
    label: String,
    description: String,
}

impl Overlay {
    fn new(title: impl Into<String>, items: Vec<OverlayItem>) -> Self {
        Self { title: title.into(), items, filter: String::new(), selected: 0, picking: true }
    }

    /// A read-only list: rows can be scrolled and filtered, but there is
    /// nothing to select. Enter closes rather than putting a row in the
    /// composer, which would be nonsense for a legend.
    fn reference(title: impl Into<String>, items: Vec<OverlayItem>) -> Self {
        Self { title: title.into(), items, filter: String::new(), selected: 0, picking: false }
    }

    /// Items matching the filter, as `(original index, item)`.
    fn matches(&self) -> Vec<(usize, &OverlayItem)> {
        let f = self.filter.trim().to_ascii_lowercase();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, i)| {
                f.is_empty()
                    || i.label.to_ascii_lowercase().contains(&f)
                    || i.description.to_ascii_lowercase().contains(&f)
            })
            .collect()
    }

    fn move_by(&mut self, delta: isize) {
        let n = self.matches().len();
        if n == 0 {
            self.selected = 0;
            return;
        }
        let cur = self.selected.min(n - 1) as isize;
        self.selected = (cur + delta).rem_euclid(n as isize) as usize;
    }

    /// Which row is highlighted, clamped to the current matches. Typing narrows
    /// the list under the cursor, so the stored index can point past the end;
    /// clamping in one place keeps what is drawn and what Enter picks in
    /// agreement, instead of highlighting a row that selects nothing.
    fn sel_index(&self, matches: usize) -> usize {
        self.selected.min(matches.saturating_sub(1))
    }

    /// The label of the highlighted row, if the filter matched anything.
    fn chosen(&self) -> Option<String> {
        let m = self.matches();
        m.get(self.sel_index(m.len())).map(|(_, i)| i.label.clone())
    }
}

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
    /// Where the current session lives, cached so read-only commands
    /// (/history) never touch the session lock — the agent holds that lock for
    /// the whole turn, and awaiting it from the event loop froze the TUI.
    session_path: std::path::PathBuf,
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
    /// Every prompt token billed this session. Each request re-sends the
    /// history, so this is a running total of what was charged, not the size of
    /// the conversation.
    total_in_tokens: u64,
    /// Prices for the session's model, when the config gives them. A local
    /// model has none, and showing $0.00 would be a claim rather than a fact.
    prices: crate::config::ModelSettings,
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
    /// Index of the first item whose cached rows are stale, and where each
    /// item's rows start in `cached_rows`. Streaming appends to the *last* item
    /// and set `dirty` for the whole transcript, so every token re-wrapped
    /// everything — 15ms per token at 60 turns of real tool output, in a debug
    /// build. Now only the tail is rebuilt.
    dirty_from: Option<usize>,
    item_starts: Vec<usize>,
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
    /// The `jj`-style escape: the pair, how fast it must be typed, and the
    /// pending first key. `None` disables it.
    insert_escape: Option<(char, char, Duration)>,
    pending_escape: Option<Instant>,
    /// Insert (typing) or normal (reading). Normal mode exists to reclaim the
    /// alphabet: `j`, `k`, `/`, `y` cannot coexist with a composer that eats
    /// every character. Nothing is mode-*only* — every insert-mode key still
    /// works — so a mode you never enter cannot trap you.
    mode: Mode,
    /// Row the cursor sits on in normal mode, an index into `cached_rows`.
    cursor_row: usize,
    /// The active `/` search: the pattern, and whether it is still being typed.
    search: Option<Search>,
    /// A floating picker, when one is open. It owns the keyboard while up.
    overlay: Option<Overlay>,
    /// The as-you-type command hint. Unlike `overlay` it is *not* modal: the
    /// composer keeps the keyboard and this just follows what is typed, the way
    /// a shell completion menu does.
    hint: Option<Overlay>,
    /// Which worker we're following, and how far we've printed. A worker's
    /// events go to its own bus, so this polls its recorded log instead.
    tail: Option<(String, usize)>,
    /// A command waiting on the user's yes/no. While this is set, keys answer
    /// the question instead of editing the composer — the agent's task is
    /// blocked until it gets a reply.
    pending_approval: Option<crate::tools::approval::ApprovalRequest>,
    /// A pairing checkpoint waiting on an answer. Unlike an approval it does
    /// *not* seize the keyboard: the answer is prose, so the composer stays a
    /// composer and only Enter is routed somewhere else.
    pending_ask: Option<crate::tools::approval::TextRequest>,
    /// OpenRouter provider routing, when set live with `/route`.
    route: Option<String>,
    /// Is mouse capture on? On by default, so the wheel scrolls the transcript
    /// you are looking at. Shift+drag still selects text — see `setup_terminal`.
    mouse: bool,
}

impl App {
    fn new(model: String, context_limit: usize, validate_cmd: Option<String>) -> Self {
        App {
            items: Vec::new(),
            input: String::new(),
            cursor: 0,
            session_path: std::path::PathBuf::new(),
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
            total_in_tokens: 0,
            prices: crate::config::ModelSettings::default(),
            step_reasoning_chars: 0,
            last_finish_reason: None,
            total_out_tokens: 0,
            validate_cmd,
            status: "/help for keys and commands".into(),
            cur_assistant: None,
            cur_thinking: None,
            cached_rows: Vec::new(),
            cache_width: 0,
            dirty_from: None,
            item_starts: Vec::new(),
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
            insert_escape: Some(('j', 'j', Duration::from_millis(300))),
            pending_escape: None,
            mode: Mode::Insert,
            cursor_row: 0,
            search: None,
            overlay: None,
            hint: None,
            tail: None,
            pending_approval: None,
            pending_ask: None,
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
        self.items.clear();
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
        // A fresh transcript has nothing to be scrolled back into.
        self.scroll_up = 0;
        self.dirty = true;
    }

    fn push(&mut self, kind: Kind, text: impl Into<String>) {
        let at = self.items.len();
        self.items.push(Item { kind, text: text.into() });
        self.touch(at);
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
            && self.input.ends_with(first)
            && self.cursor == self.char_len()
        {
            self.backspace(); // remove the first key, which was already inserted
            self.pending_escape = None;
            return true;
        }
        self.pending_escape = (c == first).then_some(now);
        false
    }

    /// Enter reading mode, putting the cursor on the last visible row.
    fn enter_normal(&mut self) {
        self.mode = Mode::Normal;
        self.cursor_row = self.cached_rows.len().saturating_sub(1);
        self.follow = false; // reading, not tailing
    }

    fn enter_insert(&mut self) {
        self.mode = Mode::Insert;
        self.search = None;
        self.follow = true;
    }

    /// Move the cursor by `delta` rows, clamped, keeping it on screen.
    fn cursor_by(&mut self, delta: isize) {
        let last = self.cached_rows.len().saturating_sub(1);
        let next = (self.cursor_row as isize + delta).clamp(0, last as isize) as usize;
        self.cursor_row = next;
    }

    /// Which item a row belongs to. `item_starts` already records where each
    /// item begins, so this is the lookup that makes "yank what I'm looking at"
    /// mean the whole message rather than one wrapped line.
    fn item_at_row(&self, row: usize) -> Option<usize> {
        if self.item_starts.is_empty() {
            return None;
        }
        Some(match self.item_starts.binary_search(&row) {
            Ok(i) => i,
            Err(0) => 0,
            Err(i) => i - 1,
        })
    }

    /// Rows matching the committed search pattern, in order.
    fn search_hits(&self) -> Vec<usize> {
        let Some(s) = &self.search else { return Vec::new() };
        if s.pattern.is_empty() {
            return Vec::new();
        }
        let needle = s.pattern.to_ascii_lowercase();
        self.cached_rows
            .iter()
            .enumerate()
            .filter(|(_, l)| row_text(l).to_ascii_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect()
    }

    /// Jump to the next match after the cursor, wrapping. Returns false when
    /// nothing matches, so the caller can say so instead of moving silently.
    fn jump_match(&mut self, forward: bool) -> bool {
        let hits = self.search_hits();
        if hits.is_empty() {
            return false;
        }
        let cur = self.cursor_row;
        let next = if forward {
            hits.iter().find(|&&r| r > cur).copied().unwrap_or(hits[0])
        } else {
            hits.iter().rev().find(|&&r| r < cur).copied().unwrap_or(*hits.last().unwrap())
        };
        self.cursor_row = next;
        true
    }

    /// Mark item `index` (and everything after it) as needing re-wrapping.
    fn touch(&mut self, index: usize) {
        self.dirty = true;
        self.dirty_from = Some(self.dirty_from.map_or(index, |d| d.min(index)));
    }

    /// Everything needs re-wrapping — a width change, or a toggle that changes
    /// how items render.
    fn touch_all(&mut self) {
        self.dirty = true;
        self.dirty_from = Some(0);
    }

    /// Rebuild the wrapped-row cache, doing only the work that changed.
    fn ensure_rows(&mut self, width: u16) {
        let width_changed = self.cache_width != width;
        if !self.dirty && !width_changed {
            return;
        }
        let from = if width_changed { 0 } else { self.dirty_from.unwrap_or(0) };
        // Never start past the last item with recorded rows: `item_starts` is
        // what says where a rebuild may resume, and indexing past it would
        // truncate the cache to nothing and silently lose the transcript.
        let from = from.min(self.item_starts.len());

        // Drop the stale tail, keep the prefix, and re-wrap only from `from`.
        // A missing start means "nothing recorded yet for this item", i.e. keep
        // every cached row and append.
        let keep_rows = self.item_starts.get(from).copied().unwrap_or(self.cached_rows.len());
        self.cached_rows.truncate(keep_rows);
        self.item_starts.truncate(from);
        for item in &self.items[from..] {
            self.item_starts.push(self.cached_rows.len());
            item_rows(
                &mut self.cached_rows,
                item,
                self.collapse_tools,
                self.show_thinking,
                width,
            );
        }
        self.cache_width = width;
        self.dirty = false;
        self.dirty_from = None;
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

    /// Show the command list while a `/command` is being typed, and hide it
    /// once the command is complete (a space means arguments now, and the
    /// argument completions are a different thing).
    fn refresh_hint(&mut self) {
        let typed = self.input.trim_start();
        let showing = typed.starts_with('/')
            && !typed.contains(char::is_whitespace)
            && !self.input.contains('\n');
        if !showing {
            self.hint = None;
            return;
        }
        let items: Vec<OverlayItem> = COMMANDS
            .iter()
            .filter(|(name, _)| name.starts_with(typed))
            .map(|(name, desc)| OverlayItem {
                label: (*name).to_string(),
                description: (*desc).to_string(),
            })
            .collect();
        if items.is_empty() {
            self.hint = None;
            return;
        }
        // Keep the highlighted row where it was if it is still in range, so
        // typing one more character doesn't jump the selection around.
        let selected = self.hint.as_ref().map(|h| h.selected).unwrap_or(0).min(items.len() - 1);
        let mut ov = Overlay::new("commands", items);
        ov.selected = selected;
        self.hint = Some(ov);
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
                // Only the item being written to is stale — that is the whole
                // point of tracking a start index per item.
                let at = match self.cur_thinking {
                    Some(i) => {
                        self.items[i].text.push_str(&text);
                        i
                    }
                    None => {
                        self.items.push(Item { kind: Kind::Thinking, text });
                        self.cur_thinking = Some(self.items.len() - 1);
                        self.items.len() - 1
                    }
                };
                self.touch(at);
            }
            Event::MessageDelta { text } => {
                let at = match self.cur_assistant {
                    Some(i) => {
                        self.items[i].text.push_str(&text);
                        i
                    }
                    None => {
                        self.items.push(Item { kind: Kind::Assistant, text });
                        self.cur_assistant = Some(self.items.len() - 1);
                        self.items.len() - 1
                    }
                };
                self.touch(at);
            }
            // Bookkeeping for the supervisor's idle rule; nothing to draw.
            Event::ModelCallStarted | Event::ModelCallFinished => {}
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
                    "ask" => return,
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
    app.session_path = session.lock().await.path().to_path_buf();
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
            if app.follow {
                app.scroll_up = 0;
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
                        app.push(Kind::Tool, format!("[{id}] {l}"));
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
                                pf.validate,
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

            // The agent is asking whether it may do something outward-facing.
            // It is blocked until this loop answers, so nothing else matters
            // until the user decides.
            Some(req) = approvals.recv(), if app.pending_approval.is_none() => {
                app.push(
                    Kind::Error,
                    format!("⚠ approve? {}\n  {}", req.reason, req.command),
                );
                app.status = "y = once · a = always this session · n = no".into();
                app.pending_approval = Some(req);
                if app.follow { app.scroll_up = 0; }
                app.dirty = true;
            }

            // A pairing checkpoint. The turn is blocked on it, but the user is
            // free to ignore it — Esc skips, and the work carries on without
            // their answer rather than stalling.
            Some(req) = asks.recv(), if app.pending_ask.is_none() => {
                app.push(
                    Kind::Pair,
                    format!("{}\n  {}", req.subject, req.question),
                );
                app.status = "type your answer · Enter to send · Esc to skip".into();
                app.pending_ask = Some(req);
                if app.follow { app.scroll_up = 0; }
                app.dirty = true;
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
                let (_, system, request, over, validate) = fanout.take().unwrap();
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
                        let report =
                            workers.spawn_many_checked(plan.tasks, system, request, over, validate);
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

    // A pending approval owns the keyboard. The agent's task is blocked waiting
    // for the answer, so typing into the composer here would look like a hang;
    // and an approval answered by accident is the failure this exists to stop.
    if let Some(req) = app.pending_approval.take() {
        use crate::tools::approval::Approval;
        let (answer, note) = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => (Approval::Once, "approved once"),
            KeyCode::Char('a') | KeyCode::Char('A') => (
                Approval::AlwaysThisSession,
                "approved — and not asking again this session for this kind of command",
            ),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => (Approval::Deny, "denied"),
            KeyCode::Char('c') if ctrl => (Approval::Deny, "denied (quitting)"),
            // Anything else is not an answer. Put the question back rather than
            // guessing, because both guesses are bad.
            _ => {
                app.pending_approval = Some(req);
                app.status = "y = once · a = always this session · n = no".into();
                return Ok(Flow::Continue);
            }
        };
        req.answer(answer);
        app.push(Kind::Notice, note.to_string());
        app.status = "/help for keys and commands".into();
        app.dirty = true;
        if key.code == KeyCode::Char('c') && ctrl {
            return Ok(Flow::Quit);
        }
        return Ok(Flow::Continue);
    }

    // A picker owns the keyboard while it is up. Esc always returns to the
    // composer with whatever was typed still there — a modal you can get stuck
    // in is worse than no modal.
    if let Some(ov) = &mut app.overlay {
        match key.code {
            KeyCode::Esc => app.overlay = None,
            KeyCode::Up => ov.move_by(-1),
            KeyCode::Down => ov.move_by(1),
            KeyCode::Char('p') if ctrl => ov.move_by(-1),
            KeyCode::Char('n') if ctrl => ov.move_by(1),
            KeyCode::Char('c') if ctrl => return Ok(Flow::Quit),
            KeyCode::Backspace => {
                ov.filter.pop();
                ov.selected = 0;
            }
            KeyCode::Enter => {
                let (chosen, picking) = (ov.chosen(), ov.picking);
                app.overlay = None;
                // A reference has nothing to pick, so Enter just closes.
                if picking && let Some(label) = chosen {
                    // Put it in the composer rather than running it: a picker
                    // that fires commands on Enter is a picker you cannot use
                    // to *look* at something.
                    app.set_input(format!("{label} "));
                }
            }
            KeyCode::Char(c) if !ctrl => {
                ov.filter.push(c);
                ov.selected = 0;
            }
            _ => {}
        }
        app.dirty = true;
        return Ok(Flow::Continue);
    }

    // Normal mode owns the alphabet. Nothing here is reachable unless you
    // deliberately entered it, and every route out is one key.
    if app.mode == Mode::Normal {
        // A search being typed takes precedence: it is a prompt, not a mode.
        if let Some(s) = &mut app.search
            && s.typing
        {
            match key.code {
                KeyCode::Esc => app.search = None,
                KeyCode::Enter => {
                    s.typing = false;
                    if !app.jump_match(true) {
                        let p = app.search.as_ref().map(|s| s.pattern.clone()).unwrap_or_default();
                        app.status = format!("no match for `{p}`");
                        app.search = None;
                    }
                }
                KeyCode::Backspace => {
                    s.pattern.pop();
                }
                KeyCode::Char(c) if !ctrl => s.pattern.push(c),
                _ => {}
            }
            return Ok(Flow::Continue);
        }

        match key.code {
            KeyCode::Char('c') if ctrl => return Ok(Flow::Quit),
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
                app.cursor_row = 0;
            }
            KeyCode::Char('/') => app.search = Some(Search { pattern: String::new(), typing: true }),
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
            // Yank the whole item under the cursor, not the wrapped row: what
            // you want is the message, the tool output, the code block.
            KeyCode::Char('y') => match app.item_at_row(app.cursor_row) {
                Some(i) => {
                    let text = app.items[i].text.clone();
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
        return Ok(Flow::Continue);
    }

    // The as-you-type hint is not modal — it only claims the keys it needs, and
    // only while it is visible.
    if app.hint.is_some() {
        match key.code {
            // Up/Down browse the list rather than input history: while typing a
            // command, the list is what you are looking at.
            KeyCode::Up => {
                app.hint.as_mut().unwrap().move_by(-1);
                app.dirty = true;
                return Ok(Flow::Continue);
            }
            KeyCode::Down => {
                app.hint.as_mut().unwrap().move_by(1);
                app.dirty = true;
                return Ok(Flow::Continue);
            }
            // Enter takes the highlighted row too. Falling through to the
            // command handler ran the half-typed text and answered "unknown
            // command: /agen", which is the opposite of what a visible,
            // highlighted list implies pressing Enter will do. An exactly-typed
            // command still runs, so muscle memory for `/help<Enter>` survives.
            KeyCode::Enter if hint_enter_accepts(&app.input) => {
                if let Some(label) = app.hint.as_ref().and_then(|h| h.chosen()) {
                    app.set_input(format!("{label} "));
                    app.hint = None;
                    app.dirty = true;
                    return Ok(Flow::Continue);
                }
            }
            // Tab accepts the highlighted command. This is better than the old
            // blind prefix-cycling: you can see what you are accepting.
            KeyCode::Tab => {
                if let Some(label) = app.hint.as_ref().and_then(|h| h.chosen()) {
                    app.set_input(format!("{label} "));
                    app.hint = None;
                    app.dirty = true;
                    return Ok(Flow::Continue);
                }
            }
            // First Esc dismisses the list; a second one clears the composer, so
            // Esc never does two things at once.
            KeyCode::Esc => {
                app.hint = None;
                app.dirty = true;
                return Ok(Flow::Continue);
            }
            _ => {}
        }
    }

    // Any key other than Tab ends an in-progress completion cycle.
    if key.code != KeyCode::Tab {
        app.completion = None;
    }

    match key.code {
        KeyCode::Char('c') if ctrl => return Ok(Flow::Quit),
        KeyCode::Tab => complete(app, cwd, mem),
        KeyCode::Char('o') if ctrl => {
            app.collapse_tools = !app.collapse_tools;
            // Changes how *every* item renders, not just the tail.
            app.touch_all();
            app.status = format!("tool output {}", if app.collapse_tools { "collapsed" } else { "expanded" });
        }
        KeyCode::Char('t') if ctrl => {
            app.show_thinking = !app.show_thinking;
            app.touch_all();
            app.status = format!("thinking {}", if app.show_thinking { "shown" } else { "hidden" });
        }
        KeyCode::Char('p') if ctrl => {
            app.status = "model cycling: configure multiple models (coming soon)".into();
        }
        KeyCode::Char('g') if ctrl => return Ok(Flow::ExternalEdit),
        KeyCode::Esc => {
            // Skipping a checkpoint outranks the other Esc meanings: the turn
            // is blocked on it, and "abort the turn" is not what someone who
            // just wants to move on is reaching for.
            if let Some(req) = app.pending_ask.take() {
                app.push(Kind::Pair, "skipped".to_string());
                app.status = "/help for keys and commands".into();
                req.answer(None);
            } else if app.running {
                cancel.cancel();
                app.status = "aborting…".into();
            } else if app.input.is_empty() {
                // Nothing to clear, so Esc means "stop typing, start reading".
                // Never steals an Esc that had a job to do.
                app.enter_normal();
                app.status = "normal · j k /search n N · y yank · i insert".into();
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

            // Answering a checkpoint, not starting a turn. Checked before the
            // command dispatch below so an answer that happens to start with a
            // slash is still an answer.
            if let Some(req) = app.pending_ask.take() {
                app.push(Kind::Pair, format!("you ▸ {input}"));
                app.status = "/help for keys and commands".into();
                req.answer(Some(input));
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

            let message = expand_file_mentions(&input, cwd);

            // Mid-turn, this is *steering*: the agent drains its mailbox at the
            // top of every step, so the message lands before the next model
            // call. Previously the composer was cleared and the text thrown
            // away with a "a turn is already running" notice — typing a
            // correction while the model worked simply destroyed it, in a
            // harness whose stated bet is human-in-the-loop.
            if app.running {
                agent.steering().push(message);
                app.push(Kind::User, format!("↳ {input}"));
                app.status = "sent — the model sees it at its next step".into();
                if app.follow {
                    app.scroll_up = 0;
                }
                return Ok(Flow::Continue);
            }

            start_turn(message, app, agent, session, mem, cwd, bash_timeout, turn, cancel);
        }
        // Ignore control-chords; accept normal (and shifted) chars at the cursor.
        KeyCode::Char(c) if !ctrl => {
            if app.escape_pair(c) {
                app.enter_normal();
                app.status = "normal · j k /search n N · y yank · i insert".into();
                app.refresh_hint();
                return Ok(Flow::Continue);
            }
            app.insert_char(c);
        }
        _ => {}
    }
    // One place, so no edit path can forget it.
    app.refresh_hint();
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
  Alt+Enter      newline
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
                Some(Thinking::Effort(e)) => format!("thinking effort: {}", e.as_str()),
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
                Some(n) if crate::llm::Effort::parse(n).is_some() => {
                    Some(Thinking::Effort(crate::llm::Effort::parse(n).unwrap()))
                }
                Some(n) => match parse_budget(n) {
                    Some(n) => Some(Thinking::Budget(n)),
                    None => {
                        app.push(
                            Kind::Error,
                            format!(
                                "usage: /think [on|off|auto|<effort>|<tokens>] (got {n}). \
                                 Efforts: minimal, low, medium, high, xhigh, max — though \
                                 servers differ on which they accept."
                            ),
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
                    "thinking capped at {n} tokens, leaving the rest of max-tokens for the answer"
                ),
                Some(Thinking::Effort(e)) => {
                    format!("thinking effort: {} (the provider's own scale)", e.as_str())
                }
                None => "thinking left to the provider's default".to_string(),
            };
            app.push(Kind::Notice, msg);
        }
        "trust" => {
            use crate::trust::{Decision, TrustStore, prompt_for};
            let mut store = TrustStore::load();
            let Some(p) = prompt_for(cwd, &store) else {
                app.push(Kind::Notice, "this project has no .worksmith/config.toml".to_string());
                return Ok(true);
            };
            match parts.next() {
                // Revoking is the point of having the command: a decision you
                // cannot revisit is one you will make carelessly.
                Some("revoke") | Some("forget") => {
                    if store.revoke(cwd) {
                        app.push(
                            Kind::Notice,
                            "forgot this project's trust decision — worksmith will ask again \
                             next start"
                                .to_string(),
                        );
                    } else {
                        app.push(Kind::Notice, "(no decision recorded for this project)".to_string());
                    }
                }
                Some(other) => app.push(
                    Kind::Error,
                    format!("usage: /trust [revoke] (got {other})"),
                ),
                None => {
                    let state = match store.decision_for(cwd, &p.fingerprint) {
                        Some(Decision::Trust) => "trusted — its config is in effect",
                        Some(Decision::Ignore) => "ignored — running on your global config",
                        None => "undecided — its config is NOT in effect",
                    };
                    app.push(Kind::Notice, format!("{}\n{state}", p.config_path.display()));
                    for (key, value, why) in &p.settings {
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
        "pair" => {
            let on = match parts.next() {
                None => !agent.pairing_on(),
                Some("on") => true,
                Some("off") => false,
                Some(other) => {
                    app.push(Kind::Error, format!("usage: /pair [on|off] (got {other})"));
                    return Ok(true);
                }
            };
            agent.set_pairing(on);
            app.push(
                Kind::Notice,
                if on {
                    "pairing on — the loop will stop at decisions worth your say. Spawned \
                     workers never will."
                        .to_string()
                } else {
                    "pairing off — the checkpoint is no longer offered to the model".to_string()
                },
            );
        }
        "route" => {
            // Deliberately not folded into /fast. `sort` changes *which
            // provider* serves the request, and OpenRouter endpoints differ in
            // quantization and price. A speed button that silently swaps your
            // backend is a surprise, not a feature.
            let cur = app.route.clone();
            match parts.next() {
                None => app.push(
                    Kind::Notice,
                    match &cur {
                        Some(s) => format!("routing: {s} (OpenRouter only)"),
                        None => "routing: the provider's default (OpenRouter sorts on price)"
                            .to_string(),
                    },
                ),
                Some("auto") | Some("default") => {
                    app.route = None;
                    agent.set_route(None);
                    app.push(Kind::Notice, "routing left to the provider".to_string());
                }
                Some(v @ ("throughput" | "latency" | "price")) => {
                    app.route = Some(v.to_string());
                    agent.set_route(Some(v.to_string()));
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
        "mouse" => {
            let want = match parts.next() {
                Some("on") => true,
                Some("off") => false,
                None => !app.mouse,
                Some(other) => {
                    app.push(Kind::Error, format!("usage: /mouse [on|off] (got {other})"));
                    return Ok(true);
                }
            };
            let mut out = io::stdout();
            let res = if want {
                execute!(out, EnableMouseCapture)
            } else {
                execute!(out, DisableMouseCapture)
            };
            match res {
                Ok(()) => {
                    app.mouse = want;
                    app.push(
                        Kind::Notice,
                        if want {
                            "mouse capture on — the wheel scrolls the transcript. Shift+drag \
                             still selects text to copy."
                                .to_string()
                        } else {
                            "mouse capture off — the terminal owns the wheel again. In the \
                             alternate screen that usually means it sends Up/Down, which walks \
                             prompt history; PageUp/PageDown and Ctrl+U/Ctrl+D scroll."
                                .to_string()
                        },
                    );
                }
                Err(e) => app.push(Kind::Error, format!("mouse: {e}")),
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
                        app.push(Kind::Tool, format!("[{id}] {l}"));
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
fn complete(app: &mut App, cwd: &Path, mem: &MemoryStore) {
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

    let Some((start, candidates)) = compute_completions(&app.input, cwd, mem) else {
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
fn compute_completions(input: &str, cwd: &Path, mem: &MemoryStore) -> Option<(usize, Vec<String>)> {
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
            .filter(|(c, _)| c[1..].starts_with(rest))
            .map(|(c, _)| format!("{c} "))
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
    let cands = arg_completions(first, prev, token, &tokens, cwd, mem)?;
    (!cands.is_empty()).then_some((token_start, cands))
}

/// Complete a subcommand or argument for a `/command`.
fn arg_completions(
    first: &str,
    prev: usize,
    token: &str,
    tokens: &[&str],
    cwd: &Path,
    mem: &MemoryStore,
) -> Option<Vec<String>> {
    let opts: &[&str] = match first.trim_start_matches('/') {
        "agents" | "workers" if prev == 1 => {
            &["list", "show", "tail", "kill", "nudge", "drop-queued"]
        }
        "spawn" if prev == 1 => &["-n", "--each-files", "--model"],
        "knowledge" | "know" if prev == 1 => &["index", "search", "status"],
        "skill" | "skills" if prev == 1 => return Some(skill_names(token, cwd)),
        "fast" | "lucky" if prev == 1 => &["on", "off", "auto"],
        "help" | "h" if prev == 1 => &["footer"],
        "think" if prev == 1 => {
            // Servers disagree about which levels exist: OpenRouter documents
            // minimal..max, and some vLLM builds accept only xhigh/medium/low.
            &["on", "off", "auto", "minimal", "low", "medium", "high", "xhigh", "max", "2000"]
        }
        "mouse" if prev == 1 => &["on", "off"],
        "route" if prev == 1 => &["throughput", "latency", "price", "auto"],
        "trust" if prev == 1 => &["revoke"],
        "validate" if prev == 1 => &["off"],
        "memory" | "mem" => match prev {
            1 => &[
                "list", "global", "project", "search", "show", "pending", "approve",
                "extract", "mine", "forget", "add", "help",
            ],
            // Ids are UUIDs. Completing them is the difference between the
            // review loop being usable and the user retyping 36 characters from
            // a terminal they cannot even select text in.
            2 if matches!(tokens.get(1), Some(&"approve")) => {
                let mut out = memory_id_candidates(mem, token, true);
                if "all".starts_with(token) {
                    out.insert(0, "all ".to_string());
                }
                return Some(out);
            }
            2 if matches!(tokens.get(1), Some(&"forget") | Some(&"show")) => {
                return Some(memory_id_candidates(mem, token, false));
            }
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

/// Short ids matching `token`. `pending_only` narrows to proposals, which is
/// what `approve` can act on — offering ids it would reject is worse than
/// offering none.
fn memory_id_candidates(mem: &MemoryStore, token: &str, pending_only: bool) -> Vec<String> {
    let ids = if pending_only { mem.pending_ids() } else { mem.list(None).map(|rows| rows.into_iter().map(|r| r.id).collect()) };
    ids.unwrap_or_default()
        .iter()
        .map(|id| short_id(id).to_string())
        .filter(|id| id.starts_with(token))
        .map(|id| format!("{id} "))
        .collect()
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

    // The as-you-type hint sits directly above the composer, where you are
    // already looking, rather than in the middle of the screen.
    if let Some(hint) = &app.hint {
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

fn render_transcript(f: &mut Frame, area: Rect, app: &App) {
    // Rows are pre-wrapped and cached (see App::ensure_rows); here we just slice
    // the tail (minus any manual scroll-up). Scrolling is therefore cheap.
    let rows = &app.cached_rows;
    let h = area.height as usize;
    let total = rows.len();

    // In normal mode the window follows the cursor instead of the tail —
    // otherwise `k` would move a cursor you cannot see.
    let end = if app.mode == Mode::Normal {
        (app.cursor_row + 1).max(h.min(total)).min(total)
    } else {
        let up = (app.scroll_up as usize).min(total.saturating_sub(1));
        total.saturating_sub(up)
    };
    let start = end.saturating_sub(h);

    let hits: Vec<usize> = if app.mode == Mode::Normal { app.search_hits() } else { Vec::new() };
    let view: Vec<Line> = rows[start..end]
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let row = start + i;
            if app.mode == Mode::Normal && row == app.cursor_row {
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

/// Build the fully-wrapped, styled rows for the transcript (each row already
/// fits `width`). Tabs are expanded so widths are predictable.
/// Wrap every item. The incremental cache in `ensure_rows` is checked against
/// this, so it stays as the obvious-and-correct reference implementation.
#[cfg(test)]
fn build_rows(
    items: &[Item],
    collapse_tools: bool,
    show_thinking: bool,
    width: u16,
) -> Vec<Line<'static>> {
    let mut rows: Vec<Line> = Vec::new();
    for item in items {
        item_rows(&mut rows, item, collapse_tools, show_thinking, width);
    }
    rows
}

/// Append one item's wrapped rows. Split out of `build_rows` so the cache can
/// rebuild a single item instead of the whole transcript: streaming mutates
/// only the last item, and re-wrapping everything per token is what made a long
/// session crawl (measured: 15ms per token at 60 turns of real tool output).
fn item_rows(
    rows: &mut Vec<Line<'static>>,
    item: &Item,
    collapse_tools: bool,
    show_thinking: bool,
    width: u16,
) {
    let w = (width.max(12) as usize).saturating_sub(1);
    {
        if item.kind == Kind::Thinking && !show_thinking {
            return;
        }
        if item.kind == Kind::Diff {
            render_diff(rows, &item.text, collapse_tools, width);
            rows.push(Line::from(""));
            return;
        }
        // Colours 1-6 are hues: every theme keeps its red red, so naming one is
        // portable. White is ANSI 7 — a contrast extreme, and the *background*
        // on a light theme. Assistant text is the app's default text, so it
        // names no colour at all and inherits the terminal's foreground.
        let (style, label): (Style, &str) = match item.kind {
            Kind::User => (Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD), "you ▸ "),
            Kind::Assistant => (Style::default(), ""),
            Kind::Thinking => {
                (Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC), "thinking ")
            }
            Kind::Tool => (Style::default().fg(Color::Yellow), "⚙ "),
            Kind::ToolResult => (Style::default().fg(Color::DarkGray), "→ "),
            Kind::Notice => (Style::default().fg(Color::Blue), ""),
            Kind::Pair => {
                (Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD), "◆ ")
            }
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
}

fn render_input(f: &mut Frame, area: Rect, app: &App) {
    // A pending approval owns the keyboard and blocks the turn, so the composer
    // has to say that. It used to keep saying "working…" with the elapsed timer
    // climbing, which reads as "the model is busy" — observed costing 79 minutes
    // of waiting for a keypress nobody knew was wanted.
    let title = if app.pending_approval.is_some() {
        " APPROVE?  y = once · a = always this session · n = no ".to_string()
    } else if app.pending_ask.is_some() {
        " CHECKPOINT  Enter answers · Esc skips ".to_string()
    } else if app.mode == Mode::Normal {
        // The single worst modal failure is not knowing which mode you are in.
        match &app.search {
            Some(s) if s.typing => format!(" NORMAL · /{} ", s.pattern),
            _ => " NORMAL · j k · /search · y yank · i insert ".to_string(),
        }
    } else if app.running {
        // Say that typing is useful right now. " working… " reads as "wait",
        // which is what made people assume input was ignored — and it was.
        " working… · Enter steers the running turn · Esc aborts ".to_string()
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

/// The left half of the footer: model, context, and the token/cost/thinking/
/// agent counters. Factored out of `render_footer` so it can be asserted on
/// directly — the footer-legend drift test checks every glyph it explains
/// against this string.
fn footer_string(app: &App) -> String {
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
    // Only when the model has prices. A local model is free, and $0.00 would be
    // a claim rather than a fact.
    let cost = match app.prices.cost(app.total_in_tokens, app.total_out_tokens) {
        Some(c) if c >= 0.01 => format!("  ${c:.2}"),
        Some(c) if c > 0.0 => format!("  ${c:.3}"),
        _ => String::new(),
    };
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
    let tail = format!("{reasoning}{cut}{cost}{fast}{agents}");
    format!(
        " {}  ctx {}% ({}/{})  ↓{}{}",
        app.model, pct, app.last_prompt_tokens, app.context_limit, app.total_out_tokens, tail
    )
}

fn render_footer(f: &mut Frame, area: Rect, app: &App) {
    let left = footer_string(app);
    // While a turn runs, show an animated spinner + elapsed seconds.
    let status = if app.pending_approval.is_some() || app.pending_ask.is_some() {
        // No spinner: nothing is happening, and an animation would say it is.
        format!("⏸ waiting for you  {}", app.status)
    } else if app.running {
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

/// What the footer's glyphs mean, as a legend. A strict glyph→meaning table:
/// the left column is the token as it appears in `footer_string`, the right is
/// what it is. `/help footer` opens this in the picker.
fn footer_legend() -> Vec<OverlayItem> {
    [
        ("<model>", "the model serving this session"),
        ("ctx N% (a/b)", "last prompt size vs the model's context window"),
        ("↓N", "output tokens generated this session (answers, not reasoning)"),
        (
            "↻N",
            "reasoning tokens on the last step — a live estimate while it streams, the provider's number once it lands. In the transcript ↻ marks a nudge — same glyph, different place.",
        ),
        ("⚠cut", "the last answer was cut off at max-tokens (finish reason `length`) — truncated, not finished"),
        ("$N", "cost this session — only shown when the model has prices; a free/local model shows nothing"),
        ("think:<label>", "current thinking mode (off / on / a budget like 2k / an effort)"),
        ("↑N agents", "workers running (plus · M queued when any are waiting)"),
    ]
    .into_iter()
    .map(|(label, description)| OverlayItem {
        label: label.to_string(),
        description: description.to_string(),
    })
    .collect()
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
            app.items.len(),
            app.cached_rows.len(),
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
    app.search = Some(Search { pattern: "listing".into(), typing: false });

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
    app.set_input(typed.to_string());
    app.refresh_hint();
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
        a.scroll_up = 7;
        let before = footer_string(&a);
        assert!(before.contains("3055"), "precondition: the footer reports the old session");

        a.reset_for_new_session(PathBuf::from("/tmp/new-session.jsonl"));

        assert_eq!(
            footer_string(&a),
            footer_string(&app()),
            "a new session's footer must read like a fresh one"
        );
        assert!(a.items.is_empty(), "the transcript is empty");
        assert_eq!(a.scroll_up, 0, "nothing to be scrolled back into");
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
        a.pending_approval = Some(req);
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

        a.pending_approval.take().unwrap().answer(crate::tools::approval::Approval::Once);
        assert_eq!(h.await.unwrap(), crate::tools::approval::Approval::Once);
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
        a.pending_ask = Some(req);
        a.insert_str("Pin it.");

        // Unlike an approval, the composer stays a composer — typing works,
        // and only Enter is routed somewhere else. This covers that routing
        // decision, not the key dispatch: the Enter handler needs the whole
        // turn's context (agent, session, workers) to call directly.
        assert_eq!(a.input, "Pin it.");
        let input = a.take_input().trim().to_string();
        a.pending_ask.take().unwrap().answer(Some(input));

        assert_eq!(h.await.unwrap().as_deref(), Some("Pin it."));
        assert!(a.input.is_empty(), "the answer left the composer");
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
        a.pending_ask = Some(req);
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
        a.pending_ask.take().unwrap().answer(None);
        assert_eq!(h.await.unwrap(), None);
    }

    #[test]
    fn a_checkpoint_is_its_own_channel_not_a_notice() {
        let mut a = app();
        a.apply_event(Event::Checkpoint {
            kind: "yours".into(),
            subject: "ActiveModel::from_override".into(),
            detail: "stubbed at llm/mod.rs:440 — must reset sampling".into(),
        });
        assert!(matches!(a.items[0].kind, Kind::Pair), "a checkpoint is not machinery chatter");
        assert!(a.items[0].text.contains("yours — ActiveModel::from_override"));

        // An `ask` renders when its answer lands, not when it is raised: the
        // question is already on screen in the composer's prompt.
        let mut b = app();
        b.apply_event(Event::Checkpoint {
            kind: "ask".into(),
            subject: "Pin the worker model".into(),
            detail: "pin or retarget?".into(),
        });
        assert!(b.items.is_empty(), "the question is not printed twice");
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
        let full = |a: &App| build_rows(&a.items, a.collapse_tools, a.show_thinking, 60);
        let text = |rows: &[Line]| -> Vec<String> {
            rows.iter()
                .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
                .collect()
        };

        a.push(Kind::User, "write the linter");
        a.push(Kind::ToolResult, "output ".repeat(80));
        a.ensure_rows(60);
        assert_eq!(text(&a.cached_rows), text(&full(&a)), "after appends");

        // Streaming: repeated appends to the last item, the hot path.
        for _ in 0..30 {
            a.apply_event(Event::MessageDelta { text: "token ".into() });
            a.ensure_rows(60);
        }
        assert_eq!(text(&a.cached_rows), text(&full(&a)), "after streaming");
        // The prefix is the thing at risk: a truncation bug loses it silently.
        assert!(
            text(&a.cached_rows)[0].contains("write the linter"),
            "the first item survived streaming: {:?}",
            &text(&a.cached_rows)[..2]
        );

        // An item pushed after streaming, then a toggle that changes how
        // *every* item renders, then a width change.
        a.push(Kind::Tool, "⚙ grep");
        a.ensure_rows(60);
        assert_eq!(text(&a.cached_rows), text(&full(&a)), "after a later push");

        a.collapse_tools = true;
        a.touch_all();
        a.ensure_rows(60);
        assert_eq!(text(&a.cached_rows), text(&full(&a)), "after a render toggle");

        a.ensure_rows(30);
        assert_eq!(
            text(&a.cached_rows),
            text(&build_rows(&a.items, a.collapse_tools, a.show_thinking, 30)),
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
        let prefix = a.cached_rows.len();

        a.apply_event(Event::MessageDelta { text: "hello".into() });
        assert_eq!(a.dirty_from, Some(a.items.len() - 1), "only the last item is stale");
        a.ensure_rows(60);
        assert!(a.cached_rows.len() > prefix, "the prefix was kept, not rebuilt");
        assert_eq!(a.item_starts.len(), a.items.len(), "one start per item");
    }

    #[test]
    fn jj_in_quick_succession_leaves_the_composer() {
        let mut a = app();
        a.insert_escape = Some(('j', 'j', Duration::from_millis(300)));

        // A lone `j` is just a character.
        assert!(!a.escape_pair('j'));
        a.insert_char('j');
        assert_eq!(a.input, "j");
        assert_eq!(a.mode, Mode::Insert);

        // The second one completes the pair and takes the first `j` back with
        // it — otherwise you would land in normal mode with a stray character.
        assert!(a.escape_pair('j'));
        assert_eq!(a.input, "", "the pending j is removed");
    }

    #[test]
    fn a_slow_jj_is_just_two_letters() {
        let mut a = app();
        a.insert_escape = Some(('j', 'j', Duration::from_millis(1)));
        assert!(!a.escape_pair('j'));
        a.insert_char('j');
        std::thread::sleep(Duration::from_millis(5));
        assert!(!a.escape_pair('j'), "too slow to be the escape");
        a.insert_char('j');
        assert_eq!(a.input, "jj", "prose survives: this composer holds words");
    }

    #[test]
    fn jj_only_fires_at_the_end_of_what_you_typed() {
        let mut a = app();
        a.insert_escape = Some(('j', 'j', Duration::from_millis(300)));

        // A `j` typed mid-word, then the cursor moved: the pair must not fire
        // and quietly delete a character somewhere else.
        a.set_input("hajj".into());
        a.cursor = 2;
        assert!(!a.escape_pair('j'));
        assert!(!a.escape_pair('j'));
        assert_eq!(a.input, "hajj", "nothing was removed");
    }

    #[test]
    fn the_escape_can_be_turned_off_or_rebound() {
        let mut a = app();
        a.insert_escape = None;
        assert!(!a.escape_pair('j'));
        a.insert_char('j');
        assert!(!a.escape_pair('j'), "disabled means it is only ever a letter");

        // Rebinding to a different pair works the same way.
        let mut b = app();
        b.insert_escape = Some(('j', 'k', Duration::from_millis(300)));
        assert!(!b.escape_pair('j'));
        b.insert_char('j');
        assert!(b.escape_pair('k'));
        assert_eq!(b.input, "");
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
        assert!(!a.cached_rows.is_empty(), "the transcript must actually render");
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
        let assistant_start = a.item_starts[1];
        assert_eq!(a.item_at_row(assistant_start), Some(1));
        assert_eq!(a.item_at_row(assistant_start + 1), Some(1), "a wrapped row is still item 1");
        assert_eq!(a.item_at_row(a.item_starts[2]), Some(2));
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

        a.search = Some(Search { pattern: "LISTING".into(), typing: false });
        let hits = a.search_hits();
        assert_eq!(hits.len(), 2, "case-insensitive: {hits:?}");

        // Jumping repeatedly cycles the matches and comes back round, rather
        // than stopping at the last one.
        a.cursor_row = 0;
        let mut visited = Vec::new();
        for _ in 0..3 {
            assert!(a.jump_match(true));
            visited.push(a.cursor_row);
        }
        assert_eq!(visited, vec![hits[1], hits[0], hits[1]], "forward wraps: {visited:?}");

        // Backwards cycles the other way.
        assert!(a.jump_match(false));
        assert_eq!(a.cursor_row, hits[0]);

        // A pattern that matches nothing must say so, not move the cursor.
        a.search = Some(Search { pattern: "zzz".into(), typing: false });
        let before = a.cursor_row;
        assert!(!a.jump_match(true));
        assert_eq!(a.cursor_row, before);
    }

    #[test]
    fn leaving_normal_mode_resumes_following_the_tail() {
        let mut a = app();
        a.push(Kind::Assistant, "hello");
        a.ensure_rows(80);

        a.enter_normal();
        assert_eq!(a.mode, Mode::Normal);
        assert!(!a.follow, "reading should not jump to the bottom on new output");

        a.search = Some(Search { pattern: "x".into(), typing: true });
        a.enter_insert();
        assert_eq!(a.mode, Mode::Insert);
        assert!(a.follow, "typing means you want to see what arrives");
        assert!(a.search.is_none(), "a stale search must not keep highlighting");
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
        a.cursor_row = 0;
        a.search = Some(Search { pattern: "listing".into(), typing: false });

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
        a.set_input("/".into());
        a.refresh_hint();
        assert_eq!(a.hint.as_ref().unwrap().matches().len(), COMMANDS.len());

        a.set_input("/me".into());
        a.refresh_hint();
        let got: Vec<String> =
            a.hint.as_ref().unwrap().matches().iter().map(|(_, i)| i.label.clone()).collect();
        assert_eq!(got, vec!["/memory"]);

        // Once the command is complete and arguments start, this is the wrong
        // list to be showing — argument completion is a different thing.
        a.set_input("/memory ".into());
        a.refresh_hint();
        assert!(a.hint.is_none(), "a space ends it");

        // Nothing matches: no popup rather than an empty box.
        a.set_input("/zzz".into());
        a.refresh_hint();
        assert!(a.hint.is_none());

        // Ordinary prose is not a command.
        a.set_input("what does /memory do".into());
        a.refresh_hint();
        assert!(a.hint.is_none());
    }

    #[test]
    fn typing_further_does_not_jump_the_selection() {
        let mut a = app();
        a.set_input("/m".into());
        a.refresh_hint();
        a.hint.as_mut().unwrap().move_by(1); // highlight /mouse
        assert_eq!(a.hint.as_ref().unwrap().chosen().as_deref(), Some("/mouse"));

        // One more character narrows the list; the selection must stay valid
        // rather than pointing past the end or silently resetting.
        a.set_input("/mo".into());
        a.refresh_hint();
        let h = a.hint.as_ref().unwrap();
        assert!(h.chosen().is_some(), "something is always selectable");
        assert!(h.matches().len() <= 2);
    }

    #[test]
    fn the_hint_draws_above_the_composer() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut a = app();
        a.set_input("/me".into());
        a.refresh_hint();
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

        ov.filter = "mo".into();
        let got: Vec<&str> = ov.matches().iter().map(|(_, i)| i.label.as_str()).collect();
        assert_eq!(got, vec!["/memory", "/mouse"]);

        // Matching the description too is the point: you look for what a thing
        // *does* when you cannot remember what it is called.
        ov.filter = "remember".into();
        assert_eq!(ov.matches().len(), 1);
        assert_eq!(ov.chosen().as_deref(), Some("/memory"));

        ov.filter = "zzz".into();
        assert!(ov.matches().is_empty());
        assert_eq!(ov.chosen(), None, "an empty list must not yield a selection");
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
        ov.filter = "one".into();
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
        let (start, c) = compute_completions("/me", Path::new("."), &probe_store()).unwrap();
        assert_eq!(start, 0);
        assert_eq!(c, vec!["/memory ".to_string()]);

        let (_, all) = compute_completions("/", Path::new("."), &probe_store()).unwrap();
        assert!(all.len() >= 5);

        // Not in command position → no command completion.
        assert!(compute_completions("hi /me", Path::new("."), &probe_store()).is_none());
    }

    #[test]
    fn completes_subcommands_and_args() {
        // /agents subcommands
        let (_, c) = compute_completions("/agents ", Path::new("."), &probe_store()).unwrap();
        assert!(c.contains(&"list ".to_string()) && c.contains(&"kill ".to_string()), "{c:?}");
        let (_, c) = compute_completions("/agents k", Path::new("."), &probe_store()).unwrap();
        assert_eq!(c, vec!["kill ".to_string()]);

        // /memory subcommands, then add's scope + kind
        let (_, c) = compute_completions("/memory ", Path::new("."), &probe_store()).unwrap();
        assert!(c.contains(&"forget ".to_string()) && c.contains(&"add ".to_string()), "{c:?}");
        let (_, c) = compute_completions("/memory add ", Path::new("."), &probe_store()).unwrap();
        assert_eq!(c, vec!["global ".to_string(), "project ".to_string()]);
        let (_, c) = compute_completions("/memory add project ", Path::new("."), &probe_store()).unwrap();
        assert!(c.contains(&"decision ".to_string()) && c.contains(&"lesson ".to_string()), "{c:?}");

        // /help has one subcommand: footer.
        let (_, c) = compute_completions("/help ", Path::new("."), &probe_store()).unwrap();
        assert_eq!(c, vec!["footer ".to_string()]);
        let (_, c) = compute_completions("/help f", Path::new("."), &probe_store()).unwrap();
        assert_eq!(c, vec!["footer ".to_string()]);
    }

    #[test]
    fn completes_at_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("main.rs"), "").unwrap();
        std::fs::write(dir.path().join("mod.rs"), "").unwrap();

        let (start, c) = compute_completions("@m", dir.path(), &probe_store()).unwrap();
        assert_eq!(start, 0);
        assert!(c.contains(&"@main.rs".to_string()), "{c:?}");
        assert!(c.contains(&"@mod.rs".to_string()), "{c:?}");

        // Directories get a trailing slash.
        let (_, d) = compute_completions("@s", dir.path(), &probe_store()).unwrap();
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
