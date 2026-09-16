//! Turn grouping: one root row per user turn, everything the turn produced
//! nested under it. Grouping is by position, never by timing: an item belongs
//! to the most recent root before it, which is exactly the order the bus
//! delivered it in. Status comes from the loop's own start/finish, not from
//! guessing at a half-written model response.
use super::transcript::{Item, Kind, Transcript, row_text};
use ratatui::prelude::*;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Status {
    Working,
    /// Blocked on the user: a checkpoint or an approval.
    Waiting,
    Done,
    /// Ended without success; the root text carries the outcome label.
    Failed,
}

/// The text of the label the root row shows. First line of the user's text,
/// nothing more: labels are never generated, only quoted.
pub(super) fn label(text: &str) -> &str {
    text.lines().next().unwrap_or("").trim()
}

/// `1m 05s` reads at a glance; `65s` needs arithmetic.
pub(super) fn elapsed_label(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {:02}s", secs / 60, secs % 60)
    }
}

/// A loop outcome is appended to the root's text as its last paragraph, in
/// brackets, so `y` and search see it and the renderer can style it apart
/// from what the user typed.
const NOTE_SEP: &str = "\n\n[";

/// Split a trailing outcome note off the user's text.
fn split_note(text: &str) -> (&str, Option<&str>) {
    match text.rsplit_once(NOTE_SEP) {
        Some((body, note)) if note.ends_with(']') => (body, Some(&note[..note.len() - 1])),
        _ => (text, None),
    }
}

impl Transcript {
    /// Start a turn group. Text is the user's message (or the synthesis
    /// prompt, labeled as such) so the label is theirs and `y` copies it whole.
    pub(super) fn start_turn(&mut self, text: String) {
        // Any earlier turn still marked as running was never finished by the
        // loop — e.g. its join was lost. Stop it claiming to work.
        if let Some(i) = self.live_turn.take().map(|(i, _)| i) {
            self.set_turn_status(i, Status::Failed, Some("unfinished"));
        }
        let index = self.items.len();
        self.push(
            Kind::Turn { expanded: true, status: Status::Working, elapsed_ms: None },
            text,
        );
        self.live_turn = Some((index, Instant::now()));
    }

    pub(super) fn current_turn(&self) -> Option<usize> {
        self.live_turn.map(|(i, _)| i)
    }

    /// Finish the live turn with the loop's outcome label. `done` is success;
    /// anything else is shown verbatim on the root row.
    pub(super) fn finish_turn(&mut self, outcome: &str) {
        let Some((index, started)) = self.live_turn.take() else { return; };
        let ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let status = if outcome == "done" { Status::Done } else { Status::Failed };
        let note = (status == Status::Failed).then_some(outcome);
        self.set_turn_status(index, status, note);
        if let Kind::Turn { elapsed_ms, .. } = &mut self.items[index].kind {
            *elapsed_ms = Some(ms);
        }
        self.touch(index);
    }

    /// Working ⇄ waiting for the live turn. Called every draw from the modal
    /// state, so every path that answers a prompt is covered without each one
    /// remembering to say so.
    pub(super) fn set_turn_waiting(&mut self, waiting: bool) {
        let Some(index) = self.current_turn() else { return; };
        let want = if waiting { Status::Waiting } else { Status::Working };
        if let Kind::Turn { status, .. } = &self.items[index].kind
            && *status != want
        {
            self.set_turn_status(index, want, None);
        }
    }

    fn set_turn_status(&mut self, index: usize, status: Status, note: Option<&str>) {
        let item = &mut self.items[index];
        if let Kind::Turn { status: s, .. } = &mut item.kind {
            *s = status;
        }
        if let Some(note) = note {
            item.text = format!("{}{NOTE_SEP}{note}]", item.text.trim_end());
        }
        self.touch(index);
    }

    /// Repaint the live root row's elapsed time without re-wrapping the turn
    /// beneath it. A touch would rebuild every row from the root down, on
    /// every spinner tick, for the whole turn — which is the quadratic churn
    /// the streaming cache exists to avoid.
    pub(super) fn refresh_live_turn(&mut self) {
        let Some((index, started)) = self.live_turn else { return; };
        if self.dirty || self.cache_width == 0 { return; }
        let Some(&row) = self.item_starts.get(index) else { return; };
        if row >= self.cached_rows.len() { return; }
        let Kind::Turn { expanded, status, .. } = self.items[index].kind else { return; };
        let heading = heading_row(
            label(&self.items[index].text),
            expanded,
            status,
            Some(started.elapsed().as_millis() as u64),
            self.cache_width,
        );
        if row_text(&heading) != row_text(&self.cached_rows[row]) {
            self.cached_rows[row] = heading;
            self.search_hits_dirty = true;
        }
    }
}

/// Index of the turn root `index` sits under, if any. Roots are not under
/// themselves. Scans back to the nearest root, which is short: a turn's
/// worth of items, and only the tail is ever rebuilt.
pub(super) fn enclosing(items: &[Item], index: usize) -> Option<usize> {
    items[..index]
        .iter()
        .rposition(|item| matches!(item.kind, Kind::Turn { .. }))
}

pub(super) fn is_collapsed(items: &[Item], root: Option<usize>) -> bool {
    matches!(root.map(|r| items[r].kind), Some(Kind::Turn { expanded: false, .. }))
}

/// Root row: `▼ label` with the status on the right when it fits, after a
/// separator when it does not. Words, not colour, carry the state.
pub(super) fn heading_row(
    label: &str,
    expanded: bool,
    status: Status,
    elapsed_ms: Option<u64>,
    width: u16,
) -> Line<'static> {
    let (word, color) = match status {
        Status::Working => ("working", Color::Yellow),
        Status::Waiting => ("waiting for you", Color::Magenta),
        Status::Done => ("done", Color::Green),
        Status::Failed => ("ended", Color::Red),
    };
    let state = match elapsed_ms {
        Some(ms) if status != Status::Waiting => format!("{word} · {}", elapsed_label(ms)),
        _ => word.to_string(),
    };
    let arrow = if expanded { "▼ " } else { "▶ " };
    let room = (width.max(12) as usize).saturating_sub(1);
    let state_w = state.chars().count() + 2;
    let label_room = room.saturating_sub(arrow.chars().count() + state_w).max(4);
    let mut shown: String = label.chars().take(label_room).collect();
    if shown.chars().count() < label.chars().count() {
        shown.pop();
        shown.push('…');
    }
    let used = arrow.chars().count() + shown.chars().count();
    let pad = room.saturating_sub(used + state.chars().count()).max(2);
    let title = Style::default().add_modifier(Modifier::BOLD);
    Line::from(vec![
        Span::styled(arrow.to_string(), title),
        Span::styled(shown, title),
        Span::raw(" ".repeat(pad)),
        Span::styled(state, Style::default().fg(color)),
    ])
}

pub(super) fn render(
    rows: &mut Vec<Line<'static>>,
    item: &Item,
    expanded: bool,
    status: Status,
    elapsed_ms: Option<u64>,
    width: u16,
) {
    let (text, note) = split_note(&item.text);
    let (head, rest) = text.split_once('\n').unwrap_or((text, ""));
    let head = head.trim();
    let heading = heading_row(head, expanded, status, elapsed_ms, width);
    let fits = !row_text(&heading).contains('…');
    rows.push(heading);
    if !expanded {
        return;
    }
    // A message that did not fit on the root row, or ran to several lines,
    // is still shown whole beneath it. Nothing the user typed is hidden.
    let body = if fits { rest.trim() } else { text.trim() };
    let mut inner = Vec::new();
    for (kind, text) in [(Kind::User, body), (Kind::Error, note.unwrap_or(""))] {
        if text.is_empty() {
            continue;
        }
        super::transcript::item_rows(
            &mut inner,
            &Item { kind, text: text.to_string(), checkpoint: Vec::new() },
            false,
            true,
            width.saturating_sub(2),
        );
    }
    indent(&mut inner, 0);
    rows.extend(inner);
}

/// Prefix every row from `from` with two columns, so the turn's children
/// read as its children.
pub(super) fn indent(rows: &mut [Line<'static>], from: usize) {
    for row in &mut rows[from..] {
        if row.spans.is_empty() {
            continue; // a blank spacer stays blank
        }
        row.spans.insert(0, Span::raw("  "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::activity;
    use crate::tui::transcript::{Search, build_rows};

    fn texts(rows: &[Line<'static>]) -> Vec<String> {
        rows.iter().map(row_text).collect()
    }

    #[test]
    fn children_nest_under_the_root_and_a_collapsed_turn_hides_them() {
        let mut t = Transcript::default();
        t.push(Kind::Notice, "worksmith · m");
        t.start_turn("Fix reconnect handling".into());
        t.push(Kind::Assistant, "I'll inspect the reconnect path.");
        t.start_tool("a".into(), "read connection.rs".into());
        t.finish_tool("a", "read", true, "fn reconnect()", Some(200));
        t.ensure_rows(60);
        let rows = texts(&t.cached_rows);
        assert!(rows[0].starts_with("worksmith"), "pre-turn items stay at the margin");
        assert!(rows.iter().any(|r| r.starts_with("▼ Fix reconnect handling") && r.ends_with("working · 0s")), "{rows:?}");
        assert!(rows.iter().any(|r| r.starts_with("  I'll inspect")), "prose is indented: {rows:?}");
        assert!(rows.iter().any(|r| r.starts_with("  ▶ ✓ 200ms · read connection.rs")));
        // The reference has no clock; compare once the duration is frozen.
        t.finish_turn("done");
        t.ensure_rows(60);
        let rows = texts(&t.cached_rows);
        assert_eq!(texts(&build_rows(&t.items, false, true, 60)), rows, "cache matches the reference");

        t.enter_normal();
        t.cursor_row = t.item_starts[1];
        assert!(t.toggle_entry());
        t.ensure_rows(60);
        let rows = texts(&t.cached_rows);
        assert!(rows.iter().any(|r| r.starts_with("▶ Fix reconnect handling")));
        assert!(!rows.iter().any(|r| r.contains("I'll inspect") || r.contains("connection.rs")));
        assert_eq!(texts(&build_rows(&t.items, false, true, 60)), rows);
        // Hidden children resolve to the row that hides them, so `y` on the
        // root yanks the user's own text and the cursor cannot vanish.
        assert_eq!(t.item_at_row(t.item_starts[1]), Some(1));
        assert_eq!(t.item_starts[2], t.item_starts[3]);
    }

    #[test]
    fn finishing_freezes_elapsed_and_names_a_bad_outcome() {
        let mut t = Transcript::default();
        t.start_turn("first".into());
        t.finish_turn("done");
        t.start_turn("second".into());
        t.finish_turn("hit step limit (30)");
        t.ensure_rows(60);
        let rows = texts(&t.cached_rows);
        let first = rows.iter().find(|r| r.starts_with("▼ first")).unwrap_or_else(|| panic!("{rows:?}"));
        assert!(first.ends_with("done · 0s"), "{first}");
        let second = rows.iter().find(|r| r.starts_with("▼ second")).unwrap_or_else(|| panic!("{rows:?}"));
        assert!(second.ends_with("ended · 0s"), "{second}");
        assert!(rows.iter().any(|r| r == "  ! hit step limit (30)"), "{rows:?}");
        assert!(t.items[1].text.ends_with("[hit step limit (30)]"), "yank and search still see it");
        assert_eq!(t.current_turn(), None);
        assert!(matches!(t.items[0].kind, Kind::Turn { status: Status::Done, elapsed_ms: Some(_), .. }));
        t.finish_turn("done"); // nothing live: a no-op, not a panic
    }

    #[test]
    fn waiting_replaces_working_and_never_shows_a_duration() {
        let mut t = Transcript::default();
        t.start_turn("choose".into());
        t.set_turn_waiting(true);
        t.ensure_rows(50);
        let rows = texts(&t.cached_rows);
        assert!(rows[0].ends_with("waiting for you"), "{}", rows[0]);
        t.set_turn_waiting(false);
        t.ensure_rows(50);
        assert!(texts(&t.cached_rows)[0].ends_with("working · 0s"));
        t.finish_turn("done");
        t.set_turn_waiting(true); // no live turn: nothing to change
        assert!(matches!(t.items[0].kind, Kind::Turn { status: Status::Done, .. }));
    }

    #[test]
    fn a_new_turn_closes_one_the_loop_never_finished() {
        let mut t = Transcript::default();
        t.start_turn("lost".into());
        t.start_turn("next".into());
        assert!(matches!(t.items[0].kind, Kind::Turn { status: Status::Failed, elapsed_ms: None, .. }));
        assert!(t.items[0].text.ends_with("[unfinished]"));
        assert_eq!(split_note(&t.items[0].text), ("lost", Some("unfinished")));
        assert_eq!(split_note("plain [bracketed] text"), ("plain [bracketed] text", None));
        assert_eq!(t.current_turn(), Some(1));
    }

    #[test]
    fn live_refresh_repaints_one_row_and_keeps_the_cache_clean() {
        let mut t = Transcript::default();
        t.start_turn("tick".into());
        t.push(Kind::Assistant, "streaming");
        t.ensure_rows(40);
        let before = t.cached_rows.len();
        if let Some((_, started)) = &mut t.live_turn {
            *started = Instant::now() - std::time::Duration::from_secs(75);
        }
        t.refresh_live_turn();
        assert!(!t.dirty);
        assert_eq!(t.cached_rows.len(), before);
        assert!(texts(&t.cached_rows)[0].ends_with("working · 1m 15s"));
        t.push(Kind::Assistant, "more");
        t.refresh_live_turn(); // dirty: leave it to ensure_rows
        assert!(t.dirty);
    }

    #[test]
    fn long_and_multiline_messages_stay_whole_beneath_the_root() {
        let mut t = Transcript::default();
        t.start_turn("Fix the reconnect handling so that the retry budget survives failures".into());
        t.ensure_rows(40);
        let rows = texts(&t.cached_rows);
        assert!(rows[0].contains('…'), "{}", rows[0]);
        assert!(rows[0].chars().count() <= 40);
        assert!(rows.iter().any(|r| r.contains("you ▸ Fix the reconnect")), "{rows:?}");
        let joined = rows.join("");
        assert!(joined.contains("survives"), "nothing typed is lost");

        let mut t = Transcript::default();
        t.start_turn("short\nwith a second line".into());
        t.ensure_rows(60);
        let rows = texts(&t.cached_rows);
        assert!(rows[0].starts_with("▼ short "));
        assert!(rows.iter().any(|r| r.contains("with a second line")));
    }

    #[test]
    fn search_opens_a_collapsed_turn_around_its_match() {
        let mut t = Transcript::default();
        t.start_turn("one".into());
        t.start_tool("a".into(), "bash cargo test".into());
        t.finish_tool("a", "bash", true, "140 passed", None);
        t.finish_turn("done");
        t.ensure_rows(60);
        t.enter_normal();
        t.cursor_row = 0;
        assert!(t.toggle_entry());
        t.ensure_rows(60);
        assert!(!texts(&t.cached_rows).iter().any(|r| r.contains("140 passed")));
        t.set_search(Some(Search { pattern: "140 passed".into(), typing: false }));
        t.ensure_rows(60);
        assert!(matches!(t.items[0].kind, Kind::Turn { expanded: true, .. }));
        assert!(matches!(t.items[1].kind, Kind::ToolActivity { expanded: true, .. }));
        assert!(!t.search_hits().is_empty());
    }

    #[test]
    fn bulk_tool_toggle_leaves_turns_alone() {
        let mut t = Transcript::default();
        t.start_turn("one".into());
        t.start_tool("a".into(), "read x".into());
        t.finish_tool("a", "read", true, "body", None);
        t.toggle_tools(); // Ctrl+O folds every tool entry; turns are not tools
        assert!(matches!(t.items[0].kind, Kind::Turn { expanded: true, .. }));
        assert!(matches!(t.items[1].kind, Kind::ToolActivity { expanded: false, status: activity::Status::Passed, .. }));
        t.toggle_tools();
        assert!(matches!(t.items[1].kind, Kind::ToolActivity { expanded: true, .. }));
    }

    #[test]
    fn narrow_headings_keep_the_status_word() {
        for width in [12u16, 20, 30] {
            let row = heading_row("a rather long label for a turn", true, Status::Waiting, None, width);
            let text = row_text(&row);
            assert!(text.ends_with("waiting for you"), "{width}: {text}");
            assert!(text.starts_with("▼ a"), "{width}: {text}");
        }
        assert_eq!(elapsed_label(59_999), "59s");
        assert_eq!(elapsed_label(60_000), "1m 00s");
        assert_eq!(elapsed_label(3_725_000), "62m 05s");
    }
}
