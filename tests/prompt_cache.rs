//! Opt-in synthetic provider probe. Normal tests never use the network.
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use worksmith::llm::{
    ChatRequest, LlmClient, Message, StreamEvent, Thinking, openai::OpenAiCompatClient,
};
use worksmith::tools::ToolRegistry;

#[tokio::test]
#[ignore = "requires WORKSMITH_CACHE_BASE_URL and WORKSMITH_CACHE_MODEL; makes small live requests"]
async fn compare_prefix_reuse() {
    let url =
        std::env::var("WORKSMITH_CACHE_BASE_URL").expect("set an explicit local provider URL");
    let model = std::env::var("WORKSMITH_CACHE_MODEL").expect("set an explicit model");
    let http = reqwest::Client::builder().timeout(Duration::from_secs(90)).build().unwrap();
    let client = OpenAiCompatClient::new(http, url, None);
    let nonce = uuid::Uuid::new_v4();
    let system = format!(
        "{}\nSynthetic cache probe {nonce}. Reply OK. Do not use tools.",
        worksmith::prompt::BASE_SYSTEM_PROMPT
    );
    let history = (0..160).map(|i| format!("Synthetic record {i}: keep stable instructions, validate changes, and report the result.\n")).collect::<String>();
    for (label, tail, memory, skill) in [
        ("prefix-first", false, "A", false),
        ("prefix-repeat", false, "A", false),
        ("prefix-memory-change", false, "B", false),
        ("tail-first", true, "A", false),
        ("tail-repeat", true, "A", false),
        ("tail-memory-change", true, "B", false),
        ("skill-change", true, "B", true),
    ] {
        let mut messages = vec![Message::system(if skill {
            format!("{system}\n<SKILLS-LOADED>\nSynthetic rule\n</SKILLS-LOADED>")
        } else {
            system.clone()
        })];
        let memory = Message::user(format!("Synthetic turn memory: {memory}"));
        if !tail {
            messages.push(memory.clone());
        }
        messages.push(Message::user(history.clone()));
        messages.push(Message::assistant(Some("Acknowledged.".into()), vec![]));
        if tail {
            messages.push(memory);
        }
        messages.push(Message::user("Reply OK."));
        let request = ChatRequest {
            model: model.clone(),
            messages,
            tools: ToolRegistry::with_builtins().defs(),
            context_breakdown: None,
            temperature: Some(0.0),
            top_p: None,
            top_k: None,
            max_tokens: Some(8),
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
        println!(
            "{}",
            serde_json::json!({"case":label, "model":model, "prompt_tokens":completion.usage.prompt_tokens,
            "cached_tokens":completion.usage.cached_tokens, "usage_reported":completion.usage.reported,
            "first_delta_ms":first, "total_ms":started.elapsed().as_millis()})
        );
    }
}
