use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use super::App;
use super::overlay::OverlayItem;

/// `7900` -> `7.9k`. The footer has room for a number, not a paragraph.
pub(super) fn compact_tokens(n: u32) -> String {
    if n >= 1000 { format!("{:.1}k", n as f32 / 1000.0) } else { n.to_string() }
}

/// The left half of the footer: model, context, and the token/cost/thinking/
/// agent counters. Factored out of `render_footer` so it can be asserted on
/// directly — the footer-legend drift test checks every glyph it explains
/// against this string.
pub(super) fn footer_string(app: &App) -> String {
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
    // Worker spend, priced per model. Kept separate from the session's own
    // rather than blended: "what did this session cost" and "what did the
    // fan-out cost" are different questions, and the second is the one the
    // cost-per-solved-task work actually measures.
    // Output tokens are what the footer shows; input is folded into the cost
    // below, where it belongs — on a billed API it is normally the larger half,
    // but as a bare number it says less than the money does.
    let agent_out = app.agent_spend.completion;
    let agent_cost = app.agent_spend.cost;
    // Three tiers, mirroring the session's own cost above. Two tiers dropped
    // anything under a cent entirely — which is most of a short worker run, and
    // reproduced the exact complaint this field was added to answer: prices
    // configured, workers running, no cost on screen.
    let money = match agent_cost {
        c if c >= 0.01 => format!(" (${c:.2})"),
        c if c > 0.0 => format!(" (${c:.3})"),
        _ => String::new(),
    };
    // Ahead of cost and think, because "work is happening in the background"
    // outranks a running total — and because this used to sit last in an
    // 89-character string that truncates at terminal width, so the one
    // time-critical field was the first to fall off the edge.
    // One field, spelled out. It carried a glyph — `⧉` — chosen because every
    // other symbol was taken, which is collision-avoidance rather than design;
    // and it butted straight against the digits, so `⧉1196 tok` read as one
    // token. The word "agents" was already doing the work the symbol was
    // supposed to do.
    //
    // Running count and spend are merged because they are one thought, and
    // because the spend has to outlive the count: workers finish, their cost
    // does not stop having been paid.
    let mut bits: Vec<String> = Vec::new();
    if app.agents_running > 0 {
        bits.push(format!("{} running", app.agents_running));
    }
    if app.agents_queued > 0 {
        bits.push(format!("{} queued", app.agents_queued));
    }
    if agent_out > 0 {
        bits.push(format!("🪙 {agent_out}{money}"));
    }
    // A space after the glyph, which is the whole complaint the previous one
    // earned: `⧉1196 tok` ran the symbol into the digits and read as one token.
    let agents =
        if bits.is_empty() { String::new() } else { format!("  🤖 {}", bits.join(" · ")) };
    let tail = format!("{agents}{reasoning}{cut}{cost}{fast}");
    format!(
        " {}  ctx {}% ({}/{})  ↓{}{}",
        app.model, pct, app.last_prompt_tokens, app.context_limit, app.total_out_tokens, tail
    )
}

pub(super) fn footer_status(app: &App) -> String {
    // While a turn runs, show an animated spinner + elapsed seconds.
    if app.modals.approval_pending() || app.modals.ask_pending() {
        // No spinner: nothing is happening, and an animation would say it is.
        format!("⏸ waiting for you  {}", app.status)
    } else if app.running || app.compacting {
        const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let start = if app.running { app.turn_start } else { app.compact_start };
        let elapsed = start.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        format!("{} {elapsed}s  {}", SPIN[app.spinner % SPIN.len()], app.status)
    } else {
        app.status.clone()
    }
}

pub(super) fn render_footer(f: &mut Frame, area: Rect, left: &str, status: &str) {
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
pub(super) fn footer_legend() -> Vec<OverlayItem> {
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
        ("🤖 … 🪙 N", "background workers: how many are running, how many are queued, and the output tokens and cost they have spent — kept separate from this session's own ↓ and $, and priced per worker model, since --worker-model exists so a cheap fan-out can run under an expensive judge. Placed before the token and cost fields because it used to sit last and was the first thing an 80-column terminal truncated away."),
    ]
    .into_iter()
    .map(|(label, description)| OverlayItem {
        label: label.to_string(),
        description: description.to_string(),
    })
    .collect()
}
