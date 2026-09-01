use ratatui::prelude::*;

/// Typing, or reading. See `App::mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    Insert,
    Normal,
}

/// A `/` search over the transcript.
#[derive(Debug, Clone, Default)]
pub(super) struct Search {
    pub(super) pattern: String,
    /// True while the pattern is being typed; Enter commits it.
    pub(super) typing: bool,
}

/// The plain text of a rendered row, for searching.
pub(super) fn row_text(line: &Line) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Which channel a transcript line belongs to — drives its color/gutter.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
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

pub(super) struct Item {
    pub(super) kind: Kind,
    pub(super) text: String,
}

/// How many lines of a long tool result to show before capping (Ctrl+O expands).
const TOOL_RESULT_PREVIEW_LINES: usize = 15;

pub(super) struct Transcript {
    pub(super) items: Vec<Item>,
    /// Lines scrolled up from the bottom; 0 = following the tail.
    pub(super) scroll_up: u16,
    pub(super) follow: bool,
    pub(super) collapse_tools: bool,
    // Cached wrapped rows so scrolling doesn't rebuild the whole transcript.
    pub(super) cached_rows: Vec<Line<'static>>,
    pub(super) cache_width: u16,
    pub(super) dirty: bool,
    /// Index of the first item whose cached rows are stale, and where each
    /// item's rows start in `cached_rows`. Streaming appends to the *last* item
    /// and set `dirty` for the whole transcript, so every token re-wrapped
    /// everything — 15ms per token at 60 turns of real tool output, in a debug
    /// build. Now only the tail is rebuilt.
    pub(super) dirty_from: Option<usize>,
    pub(super) item_starts: Vec<usize>,
    pub(super) show_thinking: bool,
    /// Insert (typing) or normal (reading). Normal mode exists to reclaim the
    /// alphabet: `j`, `k`, `/`, `y` cannot coexist with a composer that eats
    /// every character. Nothing is mode-*only* — every insert-mode key still
    /// works — so a mode you never enter cannot trap you.
    pub(super) mode: Mode,
    /// Row the cursor sits on in normal mode, an index into `cached_rows`.
    pub(super) cursor_row: usize,
    /// The active `/` search: the pattern, and whether it is still being typed.
    pub(super) search: Option<Search>,
    /// Cached row indexes matching `search`, invalidated with the row cache or
    /// when the search pattern changes.
    pub(super) search_hit_rows: Vec<usize>,
    pub(super) search_hits_dirty: bool,
}

impl Default for Transcript {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            scroll_up: 0,
            follow: true,
            collapse_tools: false,
            cached_rows: Vec::new(),
            cache_width: 0,
            dirty: true,
            dirty_from: None,
            item_starts: Vec::new(),
            show_thinking: true,
            mode: Mode::Insert,
            cursor_row: 0,
            search: None,
            search_hit_rows: Vec::new(),
            search_hits_dirty: false,
        }
    }
}

impl Transcript {
    pub(super) fn clear_for_new_session(&mut self) {
        self.items.clear();
        // A fresh transcript has nothing to be scrolled back into.
        self.scroll_up = 0;
        self.dirty = true;
        self.search_hits_dirty = true;
    }

    pub(super) fn push(&mut self, kind: Kind, text: impl Into<String>) {
        let at = self.items.len();
        self.items.push(Item { kind, text: text.into() });
        self.touch(at);
    }

    /// Enter reading mode, putting the cursor on the last visible row.
    pub(super) fn enter_normal(&mut self) {
        self.mode = Mode::Normal;
        self.cursor_row = self.cached_rows.len().saturating_sub(1);
        self.follow = false; // reading, not tailing
    }

    pub(super) fn enter_insert(&mut self) {
        self.mode = Mode::Insert;
        self.set_search(None);
        self.follow = true;
    }

    /// Move the cursor by `delta` rows, clamped, keeping it on screen.
    pub(super) fn cursor_by(&mut self, delta: isize) {
        let last = self.cached_rows.len().saturating_sub(1);
        let next =
            (self.cursor_row as isize + delta).clamp(0, last as isize) as usize;
        self.cursor_row = next;
    }

    /// Which item a row belongs to. `item_starts` already records where each
    /// item begins, so this is the lookup that makes "yank what I'm looking at"
    /// mean the whole message rather than one wrapped line.
    pub(super) fn item_at_row(&self, row: usize) -> Option<usize> {
        if self.item_starts.is_empty() {
            return None;
        }
        Some(match self.item_starts.binary_search(&row) {
            Ok(i) => i,
            Err(0) => 0,
            Err(i) => i - 1,
        })
    }

    pub(super) fn set_search(&mut self, search: Option<Search>) {
        self.search = search;
        self.search_hits_dirty = true;
        self.rebuild_search_hits();
    }

    pub(super) fn mutate_search(&mut self, f: impl FnOnce(&mut Search)) {
        if let Some(search) = &mut self.search {
            f(search);
        }
        self.search_hits_dirty = true;
        self.rebuild_search_hits();
    }

    /// Rows matching the active search pattern, in order.
    fn rebuild_search_hits(&mut self) {
        self.search_hit_rows.clear();
        let Some(s) = &self.search else {
            self.search_hits_dirty = false;
            return;
        };
        if s.pattern.is_empty() {
            self.search_hits_dirty = false;
            return;
        }
        let needle = s.pattern.to_ascii_lowercase();
        self.search_hit_rows = self
            .cached_rows
            .iter()
            .enumerate()
            .filter(|(_, l)| row_text(l).to_ascii_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
        self.search_hits_dirty = false;
    }

    pub(super) fn search_hits(&mut self) -> &[usize] {
        if self.search_hits_dirty {
            self.rebuild_search_hits();
        }
        &self.search_hit_rows
    }

    /// Jump to the next match after the cursor, wrapping. Returns false when
    /// nothing matches, so the caller can say so instead of moving silently.
    pub(super) fn jump_match(&mut self, forward: bool) -> bool {
        let cur = self.cursor_row;
        let hits = self.search_hits();
        if hits.is_empty() {
            return false;
        }
        let next = if forward {
            hits.iter().find(|&&r| r > cur).copied().unwrap_or(hits[0])
        } else {
            hits.iter()
                .rev()
                .find(|&&r| r < cur)
                .copied()
                .unwrap_or(*hits.last().unwrap())
        };
        self.cursor_row = next;
        true
    }

    /// Mark item `index` (and everything after it) as needing re-wrapping.
    pub(super) fn touch(&mut self, index: usize) {
        self.dirty = true;
        self.dirty_from =
            Some(self.dirty_from.map_or(index, |d| d.min(index)));
        self.search_hits_dirty = true;
    }

    /// Everything needs re-wrapping — a width change, or a toggle that changes
    /// how items render.
    pub(super) fn touch_all(&mut self) {
        self.dirty = true;
        self.dirty_from = Some(0);
        self.search_hits_dirty = true;
    }

    /// Rebuild the wrapped-row cache, doing only the work that changed.
    pub(super) fn ensure_rows(&mut self, width: u16) {
        let width_changed = self.cache_width != width;
        if !self.dirty && !width_changed {
            if self.search_hits_dirty {
                self.rebuild_search_hits();
            }
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
        let keep_rows =
            self.item_starts.get(from).copied().unwrap_or(self.cached_rows.len());
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
        self.search_hits_dirty = true;
        self.rebuild_search_hits();
    }

    /// Scroll toward older content.
    pub(super) fn scroll_up(&mut self, n: u16) {
        self.follow = false;
        self.scroll_up = self.scroll_up.saturating_add(n);
    }

    /// Scroll toward the newest content; re-enable follow at the bottom.
    pub(super) fn scroll_down(&mut self, n: u16) {
        self.scroll_up = self.scroll_up.saturating_sub(n);
        if self.scroll_up == 0 {
            self.follow = true;
        }
    }
}

/// Build the fully-wrapped, styled rows for the transcript (each row already
/// fits `width`). Tabs are expanded so widths are predictable.
/// Wrap every item. The incremental cache in `ensure_rows` is checked against
/// this, so it stays as the obvious-and-correct reference implementation.
#[cfg(test)]
pub(super) fn build_rows(
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
