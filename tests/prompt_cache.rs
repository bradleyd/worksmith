//! Opt-in synthetic provider probes. Normal tests never use the network.
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use worksmith::llm::{
    ChatRequest, Completion, LlmClient, Message, StreamEvent, Thinking, openai::OpenAiCompatClient,
};
// Live requests contain only these synthetic fixtures, never repository or user instructions.
const PROBE_SYSTEM: &str = "You are an assistant for a fictional project. Follow the current user request, use relevant current memory, and distinguish current preferences from superseded ones.";

fn synthetic_tools() -> Vec<worksmith::llm::ToolDef> {
    vec![worksmith::llm::ToolDef {
        name: "fixture_read".into(),
        description: "Read a fictional fixture by path.".into(),
        parameters: json!({"type":"object", "properties":{"path":{"type":"string"}}, "required":["path"]}),
    }]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Placement {
    Prefix,
    TurnStart,
}

impl Placement {
    fn label(self) -> &'static str {
        match self {
            Self::Prefix => "prefix",
            Self::TurnStart => "turn-start",
        }
    }
}

// Keep turn memory before the entire current turn, including later tool steps.
// This is an experimental layout, not a production setting.
fn messages(
    system: &str,
    history: &[Message],
    turn: &[Message],
    memory: &str,
    placement: Placement,
) -> Vec<Message> {
    let mut messages = vec![Message::system(system)];
    if placement == Placement::Prefix && !memory.is_empty() {
        messages.push(Message::user(memory));
    }
    messages.extend_from_slice(history);
    if placement == Placement::TurnStart && !memory.is_empty() {
        messages.push(Message::user(memory));
    }
    messages.extend_from_slice(turn);
    messages
}

fn client() -> (OpenAiCompatClient, String) {
    let url = std::env::var("WORKSMITH_CACHE_BASE_URL").expect("set an explicit provider URL");
    let model = std::env::var("WORKSMITH_CACHE_MODEL").expect("set an explicit model");
    let key = std::env::var("WORKSMITH_CACHE_API_KEY_ENV").ok().map(|name| {
        std::env::var(&name).expect("the specified API key environment variable must exist")
    });
    let mut headers = reqwest::header::HeaderMap::new();
    // Explicit OpenRouter session affinity avoids changing the inferred routing
    // key when this experiment changes the opening messages. It is best effort.
    if let Ok(id) = std::env::var("WORKSMITH_CACHE_SESSION_ID") {
        headers.insert("x-session-id", id.parse().expect("invalid session header"));
    }
    let http = reqwest::Client::builder()
        .default_headers(headers)
        .timeout(Duration::from_secs(90))
        .build()
        .unwrap();
    (OpenAiCompatClient::new(http, url, key), model)
}

async fn measure(
    client: &OpenAiCompatClient,
    model: &str,
    messages: Vec<Message>,
    max_tokens: u32,
    advertise_tools: bool,
) -> (Completion, Value) {
    let request = ChatRequest {
        model: model.into(),
        messages,
        tools: if advertise_tools { synthetic_tools() } else { vec![] },
        context_breakdown: None,
        temperature: Some(0.0),
        top_p: None,
        top_k: None,
        max_tokens: Some(max_tokens),
        thinking: Some(Thinking::Off),
        sort: None,
    };
    let (tx, mut rx) = mpsc::channel(64);
    let started = Instant::now();
    let collect = async {
        let mut first = None;
        while let Some(event) = rx.recv().await {
            if matches!(event, StreamEvent::TextDelta(ref s) | StreamEvent::ReasoningDelta(ref s) if !s.is_empty())
            {
                first.get_or_insert(started.elapsed().as_millis());
            }
        }
        first
    };
    let (completion, first) =
        tokio::join!(client.stream(request, tx, CancellationToken::new()), collect);
    let completion = completion.unwrap();
    let metrics = json!({"model":model, "prompt_tokens":completion.usage.prompt_tokens,
        "completion_tokens":completion.usage.completion_tokens, "cached_tokens":completion.usage.cached_tokens,
        "usage_reported":completion.usage.reported, "first_delta_ms":first,
        "total_ms":started.elapsed().as_millis(), "finish_reason":completion.finish_reason,
        "tools_advertised":advertise_tools, "tool_calls":completion.tool_calls});
    (completion, metrics)
}

fn history() -> Vec<Message> {
    let text = (0..160).map(|i| format!("Synthetic record {i}: keep stable instructions, validate changes, and report the result.\n")).collect::<String>();
    vec![Message::user(text), Message::assistant(Some("Acknowledged.".into()), vec![])]
}

#[tokio::test]
#[ignore = "requires explicit provider/model env; sends 21 short synthetic requests"]
async fn compare_prefix_reuse() {
    let (client, model) = client();
    let nonce = uuid::Uuid::new_v4();
    for round in 0..3 {
        let system = format!(
            "{}\nSynthetic cache probe {nonce}, round {round}. Reply OK. Do not use tools.",
            PROBE_SYSTEM
        );
        let order = if round % 2 == 0 {
            [Placement::Prefix, Placement::TurnStart]
        } else {
            [Placement::TurnStart, Placement::Prefix]
        };
        for placement in order {
            for (case, memory) in [("first", "A"), ("repeat", "A"), ("memory-change", "B")] {
                let memory = format!(
                    "Relevant memory for this turn:\n- [project/fact/probe] synthetic marker: {memory}"
                );
                let request = messages(
                    &system,
                    &history(),
                    &[Message::user("Reply OK.")],
                    &memory,
                    placement,
                );
                let (_, mut row) = measure(&client, &model, request, 8, true).await;
                row["experiment"] = json!("cache");
                row["round"] = json!(round);
                row["placement"] = json!(placement.label());
                row["case"] = json!(case);
                println!("{row}");
            }
        }
        let system = format!("{system}\n<SKILLS-LOADED>\nSynthetic rule\n</SKILLS-LOADED>");
        let request = messages(
            &system,
            &history(),
            &[Message::user("Reply OK.")],
            "Relevant memory for this turn:\n- [project/fact/probe] synthetic marker: B",
            Placement::TurnStart,
        );
        let (_, mut row) = measure(&client, &model, request, 8, true).await;
        row["experiment"] = json!("cache");
        row["round"] = json!(round);
        row["placement"] = json!("turn-start");
        row["case"] = json!("skill-change");
        println!("{row}");
    }
}

struct QualityCase {
    name: &'static str,
    memory: &'static str,
    prior: Vec<Message>,
    turn: Vec<Message>,
    expected: Value,
}

fn quality_cases() -> Vec<QualityCase> {
    let memory = "Relevant memory for this turn:\n- [project/preference/fixture] current status convention: Project status lines use bracketed lowercase labels: `[<name>] <value>`. This supersedes the previous colon format.";
    let task = "Format status name BUILD and value green using the current project convention. Reply only with JSON containing the key status. If no convention is available, use UNKNOWN as the status.";
    vec![
        QualityCase {
            name: "memory-convention",
            memory,
            prior: vec![],
            turn: vec![Message::user(task)],
            expected: json!({"status":"[build] green"}),
        },
        QualityCase {
            name: "superseded-history",
            memory,
            prior: vec![
                Message::user(
                    "Our old project convention was NAME: value. That was the rule for the previous version.",
                ),
                Message::assistant(Some("The previous version used NAME: value.".into()), vec![]),
            ],
            turn: vec![Message::user(task)],
            expected: json!({"status":"[build] green"}),
        },
        QualityCase {
            name: "explicit-user-override",
            memory,
            prior: vec![],
            turn: vec![Message::user(
                "For this reply only, override the saved convention: format BUILD and green as value / NAME, keeping NAME uppercase. Return only JSON with key status.",
            )],
            expected: json!({"status":"green / BUILD"}),
        },
        QualityCase {
            name: "irrelevant-memory",
            memory,
            prior: vec![],
            turn: vec![Message::user(
                "Return only JSON with key value set to the integer result of 19 + 23.",
            )],
            expected: json!({"value":42}),
        },
        QualityCase {
            name: "missing-memory",
            memory: "",
            prior: vec![],
            turn: vec![Message::user(task)],
            expected: json!({"status":"UNKNOWN"}),
        },
        QualityCase {
            name: "after-tool-result",
            memory,
            prior: vec![],
            turn: vec![
                Message::user(
                    "Migrate the formatter to the current saved convention. Inspect the old template, then return only JSON with key template containing the replacement template; retain the placeholders {name} and {value}.",
                ),
                Message::assistant(
                    None,
                    vec![worksmith::llm::ToolCall {
                        id: "read-fixture".into(),
                        name: "fixture_read".into(),
                        arguments: r#"{"path":"formatter.txt"}"#.into(),
                    }],
                ),
                Message::tool_result("read-fixture", "fixture_read", "1\t{name}: {value}"),
            ],
            expected: json!({"template":"[{name}] {value}"}),
        },
    ]
}

fn passes(completion: &Completion, expected: &Value) -> bool {
    completion.tool_calls.is_empty()
        && completion.finish_reason.as_deref() != Some("length")
        && completion
            .content
            .as_deref()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .as_ref()
            == Some(expected)
}

#[tokio::test]
#[ignore = "requires explicit provider/model env; sends 24 objectively scored synthetic requests"]
async fn compare_memory_quality() {
    let (client, model) = client();
    let nonce = uuid::Uuid::new_v4();
    for round in 0..2 {
        for case in quality_cases() {
            let system = format!(
                "{}\nSynthetic memory evaluation {nonce}, round {round}. Answer the current user request using relevant current memory. Return only the requested JSON object; no markdown or additional tool calls.",
                PROBE_SYSTEM
            );
            let mut prior = history();
            prior.extend(case.prior.clone());
            let order = if round % 2 == 0 {
                [Placement::Prefix, Placement::TurnStart]
            } else {
                [Placement::TurnStart, Placement::Prefix]
            };
            for placement in order {
                let request = messages(&system, &prior, &case.turn, case.memory, placement);
                let (completion, mut row) = measure(&client, &model, request, 256, false).await;
                row["experiment"] = json!("quality");
                row["round"] = json!(round);
                row["placement"] = json!(placement.label());
                row["case"] = json!(case.name);
                row["passed"] = json!(passes(&completion, &case.expected));
                row["expected"] = case.expected.clone();
                row["actual"] = json!(completion.content);
                println!("{row}");
            }
        }
    }
}

#[test]
fn turn_start_memory_keeps_tool_steps_append_only() {
    let case = quality_cases().into_iter().find(|c| c.name == "after-tool-result").unwrap();
    let prior = history();
    let first = messages("system", &prior, &case.turn[..1], case.memory, Placement::TurnStart);
    let next = messages("system", &prior, &case.turn, case.memory, Placement::TurnStart);
    assert_eq!(
        serde_json::to_value(&first).unwrap(),
        serde_json::to_value(&next[..first.len()]).unwrap()
    );
    let changed = messages("system", &prior, &case.turn, "changed memory", Placement::TurnStart);
    assert_eq!(
        serde_json::to_value(&next[..prior.len() + 1]).unwrap(),
        serde_json::to_value(&changed[..prior.len() + 1]).unwrap()
    );
    assert_eq!(next.iter().filter(|m| m.content.as_deref() == Some(case.memory)).count(), 1);
    assert_eq!(
        serde_json::to_value(messages("system", &prior, &case.turn, "", Placement::Prefix))
            .unwrap(),
        serde_json::to_value(messages("system", &prior, &case.turn, "", Placement::TurnStart))
            .unwrap(),
        "without memory the two layouts must be identical"
    );
    let prefix = messages("system", &prior, &case.turn, case.memory, Placement::Prefix);
    assert_eq!(prefix[1].content.as_deref(), Some(case.memory));
    assert_eq!(next[prior.len() + 1].content.as_deref(), Some(case.memory));
}

#[test]
fn quality_oracle_rejects_wrong_values_extra_fields_and_truncation() {
    let expected = json!({"status":"[build] green"});
    let mut completion = Completion { content: Some(expected.to_string()), ..Default::default() };
    assert!(passes(&completion, &expected));
    completion.tool_calls.push(worksmith::llm::ToolCall {
        id: "unexpected".into(),
        name: "fixture_read".into(),
        arguments: "{}".into(),
    });
    assert!(!passes(&completion, &expected));
    completion.tool_calls.clear();
    completion.finish_reason = Some("length".into());
    assert!(!passes(&completion, &expected));
    for text in
        [r#"{"status":"BUILD: green"}"#, r#"{"status":"[build] green","extra":true}"#, "not json"]
    {
        completion = Completion { content: Some(text.into()), ..Default::default() };
        assert!(!passes(&completion, &expected));
    }
}
