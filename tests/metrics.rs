mod common;

use worksmith::event::Event;
use worksmith::metrics::{Accounting, load, session_report};
use worksmith::session::{Session, TimedEvent, WorkerLink, link_worker};

fn sample(model: &str, cost: Option<f64>, cached: Option<u32>) -> Event {
    Event::ModelMetrics {
        session_id: None,
        model: model.into(),
        purpose: "agent".into(),
        cached_tokens: cached,
        cache_write_tokens: None,
        cost_usd: cost,
        prompt_tokens: 1000,
        completion_tokens: 100,
        reasoning_tokens: 40,
        context_breakdown: None,
        total_ms: 2000,
        first_output_ms: Some(500),
        prompt_tokens_per_second: 2000.0,
        completion_tokens_per_second: 66.7,
    }
}

fn timed(event: Event) -> TimedEvent {
    TimedEvent { ts: 1, event }
}

#[test]
fn separates_turns_prices_cache_coverage_and_validation() {
    let events = vec![
        timed(Event::UserMessage {
            text: "first".into(),
        }),
        timed(sample("local/a", Some(0.0), Some(800))),
        // Usage is a UI event, not a second bill for the same completion.
        timed(Event::Usage {
            prompt_tokens: 1000,
            completion_tokens: 100,
            reasoning_tokens: 40,
            total_tokens: 1100,
            finish_reason: None,
        }),
        timed(Event::ToolCall {
            id: "t".into(),
            name: "read".into(),
            arguments: "{}".into(),
        }),
        timed(Event::Nudge {
            reason: "retry".into(),
        }),
        timed(Event::Validation {
            ok: true,
            detail: "passed".into(),
        }),
        timed(Event::TurnComplete {
            outcome: "done".into(),
        }),
        timed(Event::UserMessage {
            text: "second".into(),
        }),
        timed(sample("hosted/b", Some(0.2), None)),
        timed(sample("unpriced/c", None, Some(0))),
        timed(Event::TurnComplete {
            outcome: "done".into(),
        }),
    ];
    let stats = Accounting::from_events(&events);
    assert_eq!(stats.totals.calls, 3);
    assert_eq!(stats.totals.completion_tokens, 300);
    assert_eq!(stats.totals.reasoning_tokens, 120);
    assert_eq!(stats.totals.model_ms, 6000);
    assert_eq!(stats.totals.cache_reported_calls, 2);
    assert_eq!(stats.totals.cache_prompt_tokens, 2000);
    assert_eq!(stats.totals.cached_tokens, 800);
    assert_eq!(stats.totals.unpriced_calls, 1);
    assert_eq!(stats.totals.known_cost_usd, 0.2);
    assert_eq!(stats.turns.len(), 2);
    assert_eq!(stats.turns[0].totals.tool_calls, 1);
    assert_eq!(stats.turns[0].validation, Some(true));
    assert_eq!(stats.turns[1].validation, None);
    assert_eq!(stats.models["hosted/b"].known_cost_usd, 0.2);
    let report = stats.report().join("\n");
    assert!(report.contains("40.0%; 2/3 calls reported"));
    assert!(report.contains("+ 1 unpriced calls"));
    assert!(report.contains("#2 $0.200000"));
}

#[test]
fn helpers_contribute_spend_without_inflating_context_or_creating_turns() {
    let mut helper = sample("local/a", Some(0.0), None);
    if let Event::ModelMetrics {
        purpose,
        prompt_tokens,
        ..
    } = &mut helper
    {
        *purpose = "helper".into();
        *prompt_tokens = 99999;
    }
    let events = vec![
        timed(sample("local/a", Some(0.0), None)),
        timed(Event::TurnComplete {
            outcome: "done".into(),
        }),
        timed(helper),
        timed(Event::UserMessage {
            text: "next".into(),
        }),
    ];
    let stats = Accounting::from_events(&events);
    assert_eq!(stats.totals.calls, 2);
    assert_eq!(stats.turns.len(), 2);
    assert_eq!(stats.turns[1].totals.calls, 0);
    assert_eq!(stats.turns[0].peak_context, 1000);
    let report = worksmith::metrics::report(&events).join("\n");
    assert!(report.contains("peak ctx 1.0k"));
    assert!(!report.contains("peak ctx 100.0k"));
}

#[test]
fn legacy_events_remain_readable_and_unpriced() {
    let old = serde_json::json!({"type":"model_metrics", "prompt_tokens":100,
        "completion_tokens":10,"reasoning_tokens":0,"total_ms":500,
        "first_output_ms":null,"prompt_tokens_per_second":0.0,
        "completion_tokens_per_second":20.0});
    let event: Event = serde_json::from_value(old).unwrap();
    let stats = Accounting::from_events(&[timed(event)]);
    assert_eq!(stats.models["unknown (legacy)"].unpriced_calls, 1);
    assert_eq!(stats.totals.cache_reported_calls, 0);
}

#[test]
fn reopened_reports_include_workers_once_and_show_missing_workers() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let parent_path = dir.path().join("parent.jsonl");
    let mut parent = Session::create_at(&parent_path, dir.path()).unwrap();
    parent
        .append_event(&sample("parent", Some(0.1), None))
        .unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let mut worker =
        Session::create_at(&dir.path().join(format!("{id}.jsonl")), dir.path()).unwrap();
    worker
        .append_event(&sample("worker", Some(0.2), Some(100)))
        .unwrap();
    let link = WorkerLink {
        id: "w1".into(),
        session_id: id,
    };
    link_worker(&parent_path, &link).unwrap();
    link_worker(&parent_path, &link).unwrap();
    link_worker(
        &parent_path,
        &WorkerLink {
            id: "w2".into(),
            session_id: uuid::Uuid::new_v4().to_string(),
        },
    )
    .unwrap();
    drop(parent);
    drop(worker);
    let stats = load(&parent_path).unwrap();
    assert_eq!(stats.workers.len(), 1);
    assert_eq!(stats.parent.totals.calls, 1);
    assert_eq!(stats.combined.calls, 2);
    assert!((stats.combined.known_cost_usd - 0.3).abs() < 1e-12);
    assert_eq!(stats.warnings.len(), 1);
    let report = session_report(&parent_path).unwrap().join("\n");
    assert!(report.contains("w2: unavailable"));
    assert!(report.contains("Parent + Available Workers"));
    assert_eq!(Session::open(&parent_path).unwrap().messages().len(), 0);
}

#[test]
fn stats_cli_needs_no_model_configuration_or_network() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create(dir.path()).unwrap();
    session
        .append_event(&sample("test/model", Some(0.5), None))
        .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_worksmith"))
        .current_dir(dir.path())
        .args(["stats", &session.id, "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let data: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(data["parent"]["totals"]["calls"], 1);
    assert_eq!(data["combined"]["known_cost_usd"], 0.5);
}

#[test]
fn cache_creation_is_not_a_cache_hit_and_missing_is_not_zero() {
    let mut creation = sample("another/model", Some(0.01), None);
    if let Event::ModelMetrics {
        cache_write_tokens, ..
    } = &mut creation
    {
        *cache_write_tokens = Some(400);
    }
    let mut no_creation = sample("another/model", None, Some(0));
    if let Event::ModelMetrics {
        cache_write_tokens, ..
    } = &mut no_creation
    {
        *cache_write_tokens = Some(0);
    }
    let stats = Accounting::from_events(&[
        timed(creation),
        timed(no_creation),
        timed(sample("old/model", None, None)),
    ]);
    assert_eq!(stats.totals.cache_write_tokens, 400);
    assert_eq!(stats.totals.cache_write_reported_calls, 2);
    assert_eq!(stats.totals.cache_reported_calls, 1);
    assert_eq!(stats.totals.cached_tokens, 0);
    assert_eq!(
        stats.totals.prompt_tokens, 3000,
        "cache details do not add input twice"
    );
}

#[test]
fn normalization_preserves_absent_cache_telemetry_and_legacy_usage() {
    use worksmith::llm::{InputTokens, OutputTokens, Usage};
    let usage = Usage::from_parts(
        InputTokens {
            uncached: 10,
            ..Default::default()
        },
        OutputTokens {
            non_reasoning: 5,
            ..Default::default()
        },
    );
    assert_eq!(usage.prompt_tokens, 10);
    assert_eq!(usage.completion_tokens, 5);
    assert_eq!(usage.cached_tokens, None);
    assert_eq!(usage.cache_write_tokens, None);
    let old: Usage = serde_json::from_value(serde_json::json!({
        "prompt_tokens":10,"completion_tokens":5,"reasoning_tokens":0,"total_tokens":15
    }))
    .unwrap();
    assert_eq!(old.cache_write_tokens, None);
    assert_eq!(old.cached_tokens, None);
}
