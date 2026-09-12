//! Tool call/result projection; presentation state never enters model history.
use super::transcript::{Item, Kind, Transcript, item_rows};
use ratatui::prelude::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Status {
    Running,
    Passed,
    Failed,
}

impl Transcript {
    pub(super) fn start_tool(&mut self, id: String, summary: String) {
        let index = self.items.len();
        self.push(
            Kind::ToolActivity {
                expanded: false,
                chosen: false,
                status: Status::Running,
                diff: false,
            },
            summary.replace('\n', " "),
        );
        self.pending_tools.insert(id, index);
    }

    pub(super) fn finish_tool(
        &mut self,
        id: &str,
        name: &str,
        ok: bool,
        output: &str,
        elapsed_ms: Option<u64>,
    ) -> bool {
        let Some(index) = self.pending_tools.remove(id) else {
            return false;
        };
        let item = &mut self.items[index];
        let Kind::ToolActivity {
            expanded,
            chosen,
            status,
            diff,
        } = &mut item.kind
        else {
            return false;
        };
        *status = if ok { Status::Passed } else { Status::Failed };
        *diff = ok && matches!(name, "edit" | "write");
        if !*chosen {
            *expanded = !ok;
        }
        if let Some(ms) = elapsed_ms {
            item.text.insert_str(0, &format!("{ms}ms · "));
        }
        item.text.push('\n');
        item.text.push_str(output);
        self.touch(index);
        true
    }

    pub(super) fn toggle_tools(&mut self) {
        self.collapse_tools = !self.collapse_tools;
        for item in &mut self.items {
            if let Kind::ToolActivity {
                expanded, chosen, ..
            } = &mut item.kind
            {
                *expanded = !self.collapse_tools;
                *chosen = true;
            }
        }
        self.touch_all();
    }
}

pub(super) fn render(
    rows: &mut Vec<Line<'static>>,
    text: &str,
    expanded: bool,
    status: Status,
    diff: bool,
    width: u16,
) {
    let (summary, output) = text.split_once('\n').unwrap_or((text, ""));
    let (mark, color) = match status {
        Status::Running => ("…", Color::Yellow),
        Status::Passed => ("✓", Color::Green),
        Status::Failed => ("✗", Color::Red),
    };
    let arrow = if expanded { "▼" } else { "▶" };
    let heading = format!("{arrow} {mark} {summary}");
    // The full command remains available when expanded and when copying.
    let room = width.saturating_sub(1) as usize;
    let mut visible = String::new();
    let mut used = 0;
    for c in heading.chars() {
        used += Span::raw(c.to_string()).width();
        if used >= room {
            break;
        }
        visible.push(c);
    }
    if visible != heading && room > 0 {
        visible.push('…');
    }
    rows.push(Line::styled(visible, Style::default().fg(color)));
    if expanded {
        let body = if Span::raw(&heading).width() >= room {
            format!("{summary}\n{output}")
        } else {
            output.to_string()
        };
        item_rows(
            rows,
            &Item {
                kind: if diff {
                    Kind::ReviewDiff
                } else {
                    Kind::ToolResult
                },
                text: body,
                checkpoint: Vec::new(),
            },
            false,
            true,
            width,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::transcript::Search;

    #[test]
    fn results_join_by_id_and_failures_open_without_moving_later_selection() {
        let mut t = Transcript::default();
        t.start_tool("a".into(), "read main.py".into());
        t.start_tool("b".into(), "bash tests".into());
        t.ensure_rows(60);
        t.enter_normal();
        t.cursor_row = t.item_starts[1];
        assert!(t.finish_tool("a", "read", false, &"error details\n".repeat(10), Some(25)));
        t.ensure_rows(60);
        assert_eq!(t.item_at_row(t.cursor_row), Some(1));
        assert!(t.finish_tool("b", "bash", true, "140 passed", Some(100)));
        assert_eq!(t.items.len(), 2);
        assert!(t.items[0].text.starts_with("25ms · read main.py\n"));
        assert!(matches!(
            t.items[0].kind,
            Kind::ToolActivity {
                expanded: true,
                status: Status::Failed,
                ..
            }
        ));
        assert!(matches!(
            t.items[1].kind,
            Kind::ToolActivity {
                expanded: false,
                status: Status::Passed,
                ..
            }
        ));
        assert!(!t.finish_tool("missing", "read", true, "legacy", None));
        assert!(t.items[1].text.contains("140 passed"));
    }

    #[test]
    fn explicit_expansion_survives_completion_and_search_reveals_hidden_output() {
        let mut t = Transcript::default();
        t.start_tool("a".into(), "edit main.py".into());
        t.ensure_rows(60);
        t.enter_normal();
        assert!(t.toggle_entry());
        t.finish_tool("a", "edit", true, "+new timer", None);
        assert!(matches!(
            t.items[0].kind,
            Kind::ToolActivity {
                expanded: true,
                diff: true,
                ..
            }
        ));
        t.toggle_tools();
        assert!(matches!(
            t.items[0].kind,
            Kind::ToolActivity {
                expanded: false,
                ..
            }
        ));
        t.set_search(Some(Search {
            pattern: "new timer".into(),
            typing: false,
        }));
        assert!(matches!(
            t.items[0].kind,
            Kind::ToolActivity { expanded: true, .. }
        ));
        assert!(!t.search_hits().is_empty());
        t.clear_for_new_session();
        assert!(!t.finish_tool("a", "edit", true, "stale", None));
    }

    #[test]
    fn rendering_caps_headers_and_preserves_diff_colors_and_full_output() {
        let mut rows = Vec::new();
        render(
            &mut rows,
            "edit 計時 main.py\n+new timer\n-old timer",
            false,
            Status::Passed,
            true,
            15,
        );
        assert_eq!(rows.len(), 1);
        assert!(rows[0].width() <= 15);
        assert!(!super::super::transcript::row_text(&rows[0]).contains("new timer"));
        rows.clear();
        render(
            &mut rows,
            "edit main.py\nupdated main.py\n--- before\n+++ after\n+new timer\n-old timer",
            true,
            Status::Passed,
            true,
            60,
        );
        assert!(
            rows.iter()
                .flat_map(|r| &r.spans)
                .any(|s| s.style.fg == Some(Color::Green) && s.content.contains("new timer"))
        );
    }
}
