//! Compact, word-wrapped checkpoint presentation. Session text remains unchanged.

use ratatui::prelude::*;
use ratatui::widgets::{Paragraph, Wrap};

pub(super) fn render(rows: &mut Vec<Line<'static>>, text: &str, expanded: bool, width: u16) {
    let (subject, body) = text.split_once('\n').unwrap_or((text, ""));
    let heading = Style::default()
        .fg(Color::Magenta)
        .add_modifier(Modifier::BOLD);
    let mut lines = vec![Line::styled(subject.to_string(), heading)];
    if expanded {
        lines.extend(body.lines().map(inline));
    } else {
        lines.push(Line::styled(
            "Enter to expand",
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    // Use the terminal's existing word wrapper and display-width handling.
    let content_width = width.saturating_sub(3).max(1);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let height = paragraph.line_count(content_width).min(u16::MAX as usize) as u16;
    let area = Rect::new(0, 0, content_width, height);
    let mut buffer = Buffer::empty(area);
    paragraph.render(area, &mut buffer);
    for y in 0..height {
        let mut spans = vec![Span::styled(
            if y == 0 {
                if expanded { "▼ " } else { "▶ " }
            } else {
                "  "
            },
            heading,
        )];
        let last = (0..content_width)
            .rev()
            .find(|&x| !buffer[(x, y)].symbol().trim().is_empty());
        let mut x = 0;
        while last.is_some_and(|last| x <= last) {
            let cell = &buffer[(x, y)];
            let span = Span::styled(cell.symbol().to_string(), cell.style());
            x += span.width().max(1) as u16;
            spans.push(span);
        }
        rows.push(Line::from(spans));
    }
    rows.push(Line::default());
}

/// The inline emphasis used by checkpoint prompts; identifiers keep underscores.
fn inline(text: &str) -> Line<'static> {
    let mut spans = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let marker = if rest.starts_with("**") {
            "**"
        } else if rest.starts_with('`') {
            "`"
        } else if rest.starts_with('*') {
            "*"
        } else {
            ""
        };
        if !marker.is_empty()
            && let Some(end) = rest[marker.len()..].find(marker)
        {
            let content = &rest[marker.len()..marker.len() + end];
            let style = match marker {
                "**" => Style::default().add_modifier(Modifier::BOLD),
                "*" => Style::default().add_modifier(Modifier::ITALIC),
                _ => Style::default().fg(Color::Cyan),
            };
            spans.push(Span::styled(content.to_string(), style));
            rest = &rest[marker.len() * 2 + end..];
        } else {
            let end = rest
                .char_indices()
                .skip(1)
                .find(|(_, c)| matches!(c, '*' | '`'))
                .map_or(rest.len(), |(i, _)| i);
            spans.push(Span::raw(rest[..end].to_string()));
            rest = &rest[end..];
        }
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(rows: &[Line<'_>]) -> String {
        rows.iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn checkpoints_wrap_words_and_render_emphasis_without_literal_markers() {
        let mut rows = Vec::new();
        render(
            &mut rows,
            "Timer choice\nUse **elapsed time** and `time.monotonic()` while *waiting*.",
            true,
            30,
        );
        let output = text(&rows);
        assert!(output.contains("elapsed time"));
        assert!(output.contains("time.monotonic()"));
        assert!(!output.contains("**"));
        assert!(!output.contains('`'));
        assert!(rows.iter().all(|l| l.width() <= 30));
        assert!(
            rows.iter()
                .flat_map(|l| &l.spans)
                .any(|s| s.style.add_modifier.contains(Modifier::ITALIC))
        );
    }

    #[test]
    fn collapsing_keeps_the_heading_and_hides_details() {
        let mut rows = Vec::new();
        render(
            &mut rows,
            "Timer choice\nDetailed alternatives\n\nYou: wall-clock",
            false,
            50,
        );
        let output = text(&rows);
        assert!(output.contains("▶ Timer choice"));
        assert!(output.contains("Enter to expand"));
        assert!(!output.contains("Detailed alternatives"));
    }

    #[test]
    fn narrow_unicode_and_unmatched_markup_stay_readable() {
        let mut rows = Vec::new();
        render(
            &mut rows,
            "計時\nKeep snake_case and unmatched * markers.",
            true,
            14,
        );
        assert!(rows.iter().all(|l| l.width() <= 14));
        let output = text(&rows);
        assert!(output.contains("計時"));
        assert!(output.contains("snake_case"));
        assert!(output.contains('*'));
    }
}
