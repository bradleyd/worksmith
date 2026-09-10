//! Reports derived from persisted events; shared by the TUI, plain mode and evals.

use crate::event::Event;
use crate::session::TimedEvent;
use serde::Serialize;
use std::collections::BTreeMap;

pub fn report(evs: &[crate::session::TimedEvent]) -> Vec<String> {
    #[derive(Clone, Copy)]
    struct Sample {
        idx: usize,
        ts: u64,
        prompt_tokens: u32,
        completion_tokens: u32,
        reasoning_tokens: u32,
        context_breakdown: Option<crate::llm::ContextBreakdown>,
        total_ms: u64,
        first_output_ms: Option<u64>,
        prompt_tokens_per_second: f64,
        completion_tokens_per_second: f64,
    }

    let start = evs.first().map(|e| e.ts).unwrap_or(0);
    let mut samples = Vec::new();
    let mut compactions = Vec::new();
    for ev in evs {
        match &ev.event {
            Event::ModelMetrics {
                purpose,
                prompt_tokens,
                completion_tokens,
                reasoning_tokens,
                context_breakdown,
                total_ms,
                first_output_ms,
                prompt_tokens_per_second,
                completion_tokens_per_second,
                ..
            } if purpose != "helper" => samples.push(Sample {
                idx: samples.len() + 1,
                ts: ev.ts,
                prompt_tokens: *prompt_tokens,
                completion_tokens: *completion_tokens,
                reasoning_tokens: *reasoning_tokens,
                context_breakdown: *context_breakdown,
                total_ms: *total_ms,
                first_output_ms: *first_output_ms,
                prompt_tokens_per_second: *prompt_tokens_per_second,
                completion_tokens_per_second: *completion_tokens_per_second,
            }),
            Event::Compaction {
                tokens_before,
                tokens_after,
                ..
            } => compactions.push((*tokens_before, *tokens_after)),
            _ => {}
        }
    }
    if samples.is_empty() {
        let mut lines = vec!["metrics: no model metrics recorded yet".to_string()];
        lines.extend(Accounting::from_events(evs).report());
        return lines;
    }

    let last = *samples.last().expect("checked non-empty");
    let peak = samples
        .iter()
        .max_by_key(|s| s.prompt_tokens)
        .copied()
        .expect("checked non-empty");
    let avg_first = average_ms(samples.iter().filter_map(|s| s.first_output_ms));
    let avg_total = average_ms(samples.iter().map(|s| s.total_ms));
    let avg_decode = average_f64(samples.iter().map(|s| s.completion_tokens_per_second));
    let avg_prompt = average_f64(samples.iter().map(|s| s.prompt_tokens_per_second));

    let mut lines = vec![
        "Summary".to_string(),
        format!(
            "  calls {:<5} compactions {:<5} peak ctx {}",
            samples.len(),
            compactions.len(),
            compact_count(peak.prompt_tokens as u64),
        ),
        String::new(),
        "Latest Call".to_string(),
        metric_pair_line(
            "ctx",
            compact_count(last.prompt_tokens as u64),
            "output",
            last.completion_tokens.to_string(),
        ),
        metric_pair_line(
            "reasoning",
            last.reasoning_tokens.to_string(),
            "first",
            fmt_ms_opt(last.first_output_ms),
        ),
        metric_pair_line(
            "total",
            fmt_ms(last.total_ms),
            "prompt/s",
            format!("{:.1}", last.prompt_tokens_per_second),
        ),
        format!(
            "  {:<11} {:.1}",
            "decode/s", last.completion_tokens_per_second
        ),
        String::new(),
        "Averages".to_string(),
        metric_pair_line("first", fmt_ms(avg_first), "total", fmt_ms(avg_total)),
        metric_pair_line(
            "prompt/s",
            format!("{avg_prompt:.1}"),
            "decode/s",
            format!("{avg_decode:.1}"),
        ),
    ];
    if let Some(breakdown) = last.context_breakdown {
        lines.push(String::new());
        lines.push("Context Breakdown".to_string());
        lines.push(metric_pair_line(
            "system",
            compact_count(breakdown.system_tokens as u64),
            "tools",
            compact_count(breakdown.tool_schema_tokens as u64),
        ));
        lines.push(metric_pair_line(
            "skills",
            compact_count(breakdown.loaded_skill_tokens as u64),
            "memory",
            compact_count(breakdown.memory_tokens as u64),
        ));
        lines.push(metric_pair_line(
            "history",
            compact_count(breakdown.history_tokens as u64),
            "latest user",
            compact_count(breakdown.latest_user_tokens as u64),
        ));
        lines.push(metric_pair_line(
            "est sum",
            compact_count(context_breakdown_total(breakdown) as u64),
            "provider",
            compact_count(last.prompt_tokens as u64),
        ));
    }
    if let Some((before, after)) = compactions.last() {
        lines.push(String::new());
        lines.push("Compaction".to_string());
        lines.push(format!("  latest      ~{before} -> ~{after} tokens"));
    }
    lines.push(String::new());
    lines.push("Recent Calls".to_string());
    lines.push("  #    age    ctx      first    total    decode/s".to_string());
    let first = samples.len().saturating_sub(8);
    for s in &samples[first..] {
        lines.push(format!(
            "  {:<4} {:>4}s  {:<7} {:<8} {:<8} {:.1}",
            s.idx,
            s.ts.saturating_sub(start),
            compact_count(s.prompt_tokens as u64),
            fmt_ms_opt(s.first_output_ms),
            fmt_ms(s.total_ms),
            s.completion_tokens_per_second
        ));
    }
    lines.extend(Accounting::from_events(evs).report());
    lines
}

fn metric_pair_line(
    left_label: &str,
    left_value: String,
    right_label: &str,
    right_value: String,
) -> String {
    format!("  {left_label:<11} {left_value:<8}  {right_label:<11} {right_value}")
}

fn average_ms(values: impl Iterator<Item = u64>) -> u64 {
    let mut n = 0u64;
    let mut sum = 0u64;
    for v in values {
        n += 1;
        sum = sum.saturating_add(v);
    }
    sum.checked_div(n).unwrap_or(0)
}

fn context_breakdown_total(b: crate::llm::ContextBreakdown) -> u32 {
    b.system_tokens
        .saturating_add(b.loaded_skill_tokens)
        .saturating_add(b.memory_tokens)
        .saturating_add(b.history_tokens)
        .saturating_add(b.latest_user_tokens)
        .saturating_add(b.tool_schema_tokens)
}

fn average_f64(values: impl Iterator<Item = f64>) -> f64 {
    let mut n = 0.0;
    let mut sum = 0.0;
    for v in values {
        n += 1.0;
        sum += v;
    }
    if n == 0.0 { 0.0 } else { sum / n }
}

pub(crate) fn fmt_ms(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

pub(crate) fn fmt_ms_opt(ms: Option<u64>) -> String {
    ms.map(fmt_ms).unwrap_or_else(|| "n/a".to_string())
}

pub(crate) fn compact_count(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// Unknown prices and absent cache telemetry remain visible in every roll-up.
#[derive(Debug, Default, Clone, Serialize)]
pub struct Totals {
    pub calls: u64,
    pub tool_calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub reasoning_tokens: u64,
    pub model_ms: u64,
    pub compactions: u64,
    pub nudges: u64,
    pub cached_tokens: u64,
    pub cache_prompt_tokens: u64,
    pub cache_reported_calls: u64,
    pub cache_write_tokens: u64,
    pub cache_write_reported_calls: u64,
    pub known_cost_usd: f64,
    pub unpriced_calls: u64,
}

impl Totals {
    pub(crate) fn observe(&mut self, event: &Event) {
        match event {
            Event::ModelMetrics {
                prompt_tokens,
                completion_tokens,
                reasoning_tokens,
                cached_tokens,
                cache_write_tokens,
                cost_usd,
                total_ms,
                ..
            } => {
                self.calls += 1;
                self.prompt_tokens += *prompt_tokens as u64;
                self.completion_tokens += *completion_tokens as u64;
                self.reasoning_tokens += *reasoning_tokens as u64;
                self.model_ms += total_ms;
                if let Some(cached) = cached_tokens {
                    self.cached_tokens += (*cached).min(*prompt_tokens) as u64;
                    self.cache_prompt_tokens += *prompt_tokens as u64;
                    self.cache_reported_calls += 1;
                }
                if let Some(written) = cache_write_tokens {
                    self.cache_write_tokens += (*written).min(*prompt_tokens) as u64;
                    self.cache_write_reported_calls += 1;
                }
                if let Some(cost) = cost_usd.filter(|c| c.is_finite() && *c >= 0.0) {
                    self.known_cost_usd += cost;
                } else {
                    self.unpriced_calls += 1;
                }
            }
            Event::ToolCall { .. } => self.tool_calls += 1,
            Event::Compaction { .. } => self.compactions += 1,
            Event::Nudge { .. } => self.nudges += 1,
            _ => {}
        }
    }

    pub fn cost_label(&self) -> String {
        if self.unpriced_calls == 0 {
            format!("${:.6}", self.known_cost_usd)
        } else {
            format!(
                "${:.6} + {} unpriced calls",
                self.known_cost_usd, self.unpriced_calls
            )
        }
    }

    fn report_lines(&self) -> Vec<String> {
        vec![
            format!(
                "  calls {} tools {} in {} out {}",
                self.calls, self.tool_calls, self.prompt_tokens, self.completion_tokens
            ),
            format!(
                "  reasoning {} model {} cost {}",
                self.reasoning_tokens,
                fmt_ms(self.model_ms),
                self.cost_label()
            ),
        ]
    }
}

#[derive(Debug, Default, Serialize)]
pub struct Turn {
    pub number: usize,
    pub totals: Totals,
    pub outcome: Option<String>,
    pub validation: Option<bool>,
    pub peak_context: u32,
    pub last_context: u32,
    pub last_context_breakdown: Option<crate::llm::ContextBreakdown>,
}

#[derive(Debug, Default, Serialize)]
pub struct Accounting {
    pub totals: Totals,
    pub turns: Vec<Turn>,
    pub models: BTreeMap<String, Totals>,
}

impl Accounting {
    pub fn from_events(events: &[TimedEvent]) -> Self {
        let mut out = Self::default();
        let mut turn = Turn {
            number: 1,
            ..Default::default()
        };
        let mut has_turn = false;
        for entry in events {
            let event = &entry.event;
            out.totals.observe(event);
            if !matches!(event, Event::ModelMetrics { purpose, .. } if purpose == "helper" && !has_turn)
            {
                turn.totals.observe(event);
            }
            match event {
                Event::UserMessage { .. } | Event::ModelCallStarted | Event::ToolCall { .. } => {
                    has_turn = true
                }
                Event::ModelMetrics {
                    model,
                    purpose,
                    prompt_tokens,
                    context_breakdown,
                    ..
                } => {
                    if purpose != "helper" {
                        has_turn = true;
                        turn.peak_context = turn.peak_context.max(*prompt_tokens);
                        turn.last_context = *prompt_tokens;
                        turn.last_context_breakdown = *context_breakdown;
                    }
                    let model = if model.is_empty() {
                        "unknown (legacy)"
                    } else {
                        model
                    };
                    out.models
                        .entry(model.to_string())
                        .or_default()
                        .observe(event);
                }
                Event::Validation { ok, .. } => turn.validation = Some(*ok),
                Event::TurnComplete { outcome } => {
                    turn.outcome = Some(outcome.clone());
                    out.turns.push(turn);
                    turn = Turn {
                        number: out.turns.len() + 1,
                        ..Default::default()
                    };
                    has_turn = false;
                }
                _ => {}
            }
        }
        if has_turn {
            out.turns.push(turn);
        }
        out
    }

    pub fn report(&self) -> Vec<String> {
        let t = &self.totals;
        let mut lines = vec![
            String::new(),
            "Session Totals".into(),
            format!(
                "  turns {} compactions {} nudges {}",
                self.turns.len(),
                t.compactions,
                t.nudges
            ),
        ];
        lines.extend(t.report_lines());
        if t.cache_reported_calls == 0 {
            lines.push("  cache n/a (provider did not report it)".into());
        } else {
            let rate = if t.cache_prompt_tokens == 0 {
                0.0
            } else {
                100.0 * t.cached_tokens as f64 / t.cache_prompt_tokens as f64
            };
            lines.push(format!(
                "  cache {} / {} tokens ({rate:.1}%; {}/{} calls reported)",
                t.cached_tokens, t.cache_prompt_tokens, t.cache_reported_calls, t.calls
            ));
        }
        lines.push(String::new());
        if t.cache_write_reported_calls > 0 {
            lines.push(format!(
                "  cache writes {} tokens ({}/{} calls reported)",
                t.cache_write_tokens, t.cache_write_reported_calls, t.calls
            ));
            lines.push(String::new());
        }
        lines.push("Cost by Model (estimate at configured standard rates)".into());
        for (model, totals) in &self.models {
            lines.push(format!(
                "  {model}: {} ({} calls)",
                totals.cost_label(),
                totals.calls
            ));
        }
        lines.push(String::new());
        lines.push("Per-Turn History".into());
        for turn in &self.turns {
            lines.push(format!(
                "  #{} {}{}",
                turn.number,
                turn.outcome
                    .as_deref()
                    .unwrap_or("in progress / incomplete"),
                match turn.validation {
                    Some(true) => " (validated)",
                    Some(false) => " (validation failed)",
                    None => "",
                }
            ));
            lines.extend(turn.totals.report_lines());
            lines.push(format!(
                "    context last {} peak {}",
                turn.last_context, turn.peak_context
            ));
            if let Some(b) = turn.last_context_breakdown {
                lines.push(format!(
                    "    estimated: system {} tools {} skills {}",
                    b.system_tokens, b.tool_schema_tokens, b.loaded_skill_tokens
                ));
                lines.push(format!(
                    "    memory {} history {} user {}",
                    b.memory_tokens, b.history_tokens, b.latest_user_tokens
                ));
            }
        }
        let mut ranked: Vec<_> = self.turns.iter().filter(|t| t.totals.calls > 0).collect();
        ranked.sort_by(|a, b| b.totals.known_cost_usd.total_cmp(&a.totals.known_cost_usd));
        if !ranked.is_empty() {
            lines.push("Highest Known Turn Costs".into());
            for turn in ranked.into_iter().take(5) {
                lines.push(format!("  #{} {}", turn.number, turn.totals.cost_label()));
            }
        }
        lines
    }
}

#[derive(Debug, Serialize)]
pub struct WorkerMetrics {
    pub id: String,
    pub session_id: String,
    pub accounting: Accounting,
}

#[derive(Debug, Serialize)]
pub struct SessionMetrics {
    pub parent: Accounting,
    pub workers: Vec<WorkerMetrics>,
    pub combined: Totals,
    pub warnings: Vec<String>,
}

/// Load only explicitly linked workers, never scan the global session store.
pub fn load(path: &std::path::Path) -> anyhow::Result<SessionMetrics> {
    let events = crate::session::events(path)?;
    load_with_events(path, &events)
}

fn load_with_events(
    path: &std::path::Path,
    events: &[TimedEvent],
) -> anyhow::Result<SessionMetrics> {
    let parent = Accounting::from_events(events);
    let mut combined = parent.totals.clone();
    let mut workers = Vec::new();
    let mut warnings = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for link in crate::session::worker_links(path)? {
        if !seen.insert(link.session_id.clone()) {
            continue;
        }
        if uuid::Uuid::parse_str(&link.session_id).is_err() {
            warnings.push(format!("{}: invalid worker session id", link.id));
            continue;
        }
        let sibling = path.with_file_name(format!("{}.jsonl", link.session_id));
        let worker_path = if sibling.is_file() { sibling } else { crate::session::Session::path_for_id(&link.session_id)? };
        if worker_path == path {
            warnings.push(format!("{}: worker links to parent", link.id));
            continue;
        }
        match crate::session::events(&worker_path) {
            Ok(events) => {
                for event in &events {
                    combined.observe(&event.event);
                }
                workers.push(WorkerMetrics {
                    id: link.id,
                    session_id: link.session_id,
                    accounting: Accounting::from_events(&events),
                });
            }
            Err(e) => warnings.push(format!("{}: unavailable ({e})", link.id)),
        }
    }
    Ok(SessionMetrics {
        parent,
        workers,
        combined,
        warnings,
    })
}

pub fn session_report(path: &std::path::Path) -> anyhow::Result<Vec<String>> {
    let events = crate::session::events(path)?;
    let metrics = load_with_events(path, &events)?;
    let mut lines = report(&events);
    if !metrics.workers.is_empty() || !metrics.warnings.is_empty() {
        lines.push(String::new());
        lines.push("Workers (separate sessions)".into());
        for worker in metrics.workers {
            lines.push(format!("  {} [{}]", worker.id, worker.session_id));
            lines.extend(worker.accounting.totals.report_lines());
            for (model, totals) in worker.accounting.models {
                lines.push(format!("    {model}: {}", totals.cost_label()));
            }
        }
        for warning in metrics.warnings {
            lines.push(format!("  {warning}"));
        }
        lines.push("Parent + Available Workers".into());
        lines.extend(metrics.combined.report_lines());
    }
    Ok(lines)
}
