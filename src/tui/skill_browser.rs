//! Browsing skill text never loads instructions into the agent.
use super::overlay::{Overlay, ReferenceInput, SkillFocus};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

const EXPLANATION: &str = "Catalog discovered automatically; full instructions activate on demand.";
const CONTROLS: &str = "Enter activate · u deactivate · Tab panes · Esc back/close\nj/k scroll · gg/G · / filter skills · wheel scrolls focused pane";

fn wrapped(text: String) -> Paragraph<'static> {
    Paragraph::new(text).wrap(Wrap { trim: false })
}

fn panes(area: Rect, status: &str, heading: &str, mcp: bool) -> [Rect; 4] {
    let inner = area.inner(Margin::new(1, 0));
    let header_rows = wrapped(heading.into()).line_count(inner.width) as u16;
    let footer_rows = wrapped(footer_text(status, mcp)).line_count(inner.width) as u16;
    let sections = Layout::vertical([
        Constraint::Length(header_rows),
        Constraint::Min(4),
        Constraint::Length(footer_rows),
    ])
    .split(inner);
    let layout = if inner.width >= 90 {
        Layout::horizontal([Constraint::Percentage(38), Constraint::Percentage(62)])
    } else {
        Layout::vertical([Constraint::Percentage(45), Constraint::Percentage(55)])
    };
    let pair = layout.split(sections[1]);
    [pair[0], pair[1], sections[0], sections[2]]
}

fn heading_text(ov: &Overlay) -> String {
    let explanation = if ov.is_mcp_catalog() {
        "MCP: parent only. Connection, activation and authorization are separate."
    } else {
        EXPLANATION
    };
    let noun = if ov.is_mcp_catalog() {
        "tools"
    } else {
        "skills"
    };
    if ov.filter.is_empty() && ov.reference_input != ReferenceInput::Filter {
        explanation.into()
    } else {
        format!("{explanation}\nFilter {noun}: /{}", ov.filter)
    }
}

fn footer_text(status: &str, mcp: bool) -> String {
    let controls = if mcp {
        "Enter activate / refresh server · u deactivate · r refresh\nTab panes · / filter · j/k scroll · Esc back/close"
    } else {
        CONTROLS
    };
    if status.is_empty() {
        controls.into()
    } else {
        format!("{controls}\n{status}")
    }
}

fn preview_text(ov: &Overlay) -> String {
    let text = match &ov.preview {
        Some(preview) => {
            let description = ov
                .items
                .iter()
                .find(|item| item.label == preview.name)
                .map(|item| item.description.as_str())
                .unwrap_or("");
            format!("{description}\n\n{}", preview.text)
        }
        None if ov.is_mcp_catalog() => {
            "No configured MCP servers. Add an explicitly enabled server to trusted configuration."
                .into()
        }
        None => "Highlight a skill to preview its instructions.".into(),
    };
    text.replace('\t', "    ")
}

pub(super) fn prepare(ov: &mut Overlay, area: Rect, status: &str) {
    let [_, preview, _, _] = panes(area, status, &heading_text(ov), ov.is_mcp_catalog());
    let rows = wrapped(preview_text(ov)).line_count(preview.width.saturating_sub(2));
    ov.preview_max_scroll =
        rows.saturating_sub(preview.height.saturating_sub(2) as usize).min(u16::MAX as usize);
    ov.preview_scroll = ov.preview_scroll.min(ov.preview_max_scroll);
}

fn pane_title(title: impl Into<String>, focused: bool) -> Line<'static> {
    let title = title.into();
    if focused {
        Line::styled(
            format!("> {title}"),
            Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )
    } else {
        Line::from(title)
    }
}

pub(super) fn render(f: &mut Frame, area: Rect, ov: &Overlay, status: &str) {
    let [list, preview, header, footer] =
        panes(area, status, &heading_text(ov), ov.is_mcp_catalog());
    f.render_widget(Clear, area);
    f.render_widget(wrapped(heading_text(ov)), header);
    let border = |focused| {
        if focused { Style::default().add_modifier(Modifier::BOLD) } else { Style::default() }
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(pane_title(ov.title.clone(), ov.skill_focus == SkillFocus::List))
        .border_style(border(ov.skill_focus == SkillFocus::List));
    let inner = block.inner(list);
    f.render_widget(block, list);
    let matches = ov.matches();
    let selected = ov.sel_index(matches.len());
    let first = selected.saturating_sub((inner.height as usize).saturating_sub(1));
    let mut rows: Vec<Line> = matches
        .iter()
        .enumerate()
        .skip(first)
        .take(inner.height as usize)
        .map(|(i, (_, item))| {
            let text = match ov.skill_loaded(&item.label) {
                Some(loaded) => {
                    format!(
                        "[{}] {}",
                        if loaded {
                            "active"
                        } else if ov.is_mcp_catalog() {
                            "available"
                        } else {
                            "catalog"
                        },
                        item.label
                    )
                }
                None => item.label.clone(),
            };
            Line::styled(
                text,
                if i == selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                },
            )
        })
        .collect();
    if rows.is_empty() {
        rows.push(Line::from("(nothing matches)"));
    }
    f.render_widget(Paragraph::new(rows), inner);
    let title = if ov.is_mcp_catalog() {
        "Tool details and permission"
    } else if ov.preview.as_ref().is_some_and(|p| p.loaded) {
        "Active instructions"
    } else {
        "Preview — catalog only"
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(pane_title(title, ov.skill_focus == SkillFocus::Preview))
        .border_style(border(ov.skill_focus == SkillFocus::Preview));
    let inner = block.inner(preview);
    f.render_widget(block, preview);
    f.render_widget(wrapped(preview_text(ov)).scroll((ov.preview_scroll as u16, 0)), inner);
    f.render_widget(wrapped(footer_text(status, ov.is_mcp_catalog())), footer);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_moves_the_title_marker_and_underline_between_panes() {
        use ratatui::{Terminal, backend::TestBackend};
        for width in [70, 120] {
            let mut ov = Overlay::skills(vec![], Default::default());
            let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
            let [list, preview, _, _] = panes(Rect::new(0, 0, width, 30), "", EXPLANATION, false);
            for focus in [SkillFocus::List, SkillFocus::Preview] {
                ov.skill_focus = focus;
                terminal.draw(|f| render(f, f.area(), &ov, "")).unwrap();
                for (pane, kind) in [(list, SkillFocus::List), (preview, SkillFocus::Preview)] {
                    let cell = &terminal.backend().buffer()[(pane.x + 1, pane.y)];
                    assert_eq!(cell.symbol() == ">", kind == focus);
                    assert_eq!(cell.modifier.contains(Modifier::UNDERLINED), kind == focus);
                }
            }
        }
    }

    #[test]
    fn long_filter_is_visible_above_both_panes() {
        use ratatui::{Terminal, backend::TestBackend};
        for width in [40, 70, 120] {
            let mut ov = Overlay::skills(vec![], Default::default());
            ov.set_filter(format!("{}FILTER_END", "long query ".repeat(8)));
            let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
            terminal.draw(|f| render(f, f.area(), &ov, "")).unwrap();
            let [list, preview, header, _] =
                panes(Rect::new(0, 0, width, 30), "", &heading_text(&ov), false);
            assert!(header.bottom() <= list.y && header.bottom() <= preview.y);
            let text: String = (header.y..header.bottom())
                .flat_map(|y| (header.x..header.right()).map(move |x| (x, y)))
                .map(|pos| terminal.backend().buffer()[pos].symbol())
                .collect();
            assert!(text.contains("Filter skills:"));
            assert!(text.contains("FILTER_END"), "query clipped at width {width}");
        }
    }

    #[test]
    fn narrow_terminals_stack_preview_below_list() {
        let [list, preview, _, _] = panes(Rect::new(0, 0, 120, 30), "", EXPLANATION, false);
        assert_eq!(list.y, preview.y);
        assert_eq!(list.right(), preview.x);
        let [list, preview, _, _] = panes(Rect::new(0, 0, 70, 30), "", EXPLANATION, false);
        assert_eq!(list.x, preview.x);
        assert_eq!(list.bottom(), preview.y);
    }
}
