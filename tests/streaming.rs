//! End-to-end test of the OpenAI-compatible SSE parser + tool-call assembly,
//! against a tiny in-process mock server. No real model needed.

mod common;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use worksmith::llm::openai::OpenAiCompatClient;
use worksmith::llm::{ChatRequest, LlmClient, Message, StreamEvent};

/// Spawn a one-shot mock that returns the given SSE body, and return its base URL.
fn spawn_mock(sse_body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            // Read (and discard) the request; a single read is enough to unblock.
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);

            let response = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: text/event-stream\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\r\n{}",
                sse_body.len(),
                sse_body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    format!("http://127.0.0.1:{port}/v1")
}

#[tokio::test]
async fn streams_text_and_assembles_tool_call() {
    // Text delta, then a (chunked) tool call, then finish + usage + DONE.
    let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"Reading \"},\"finish_reason\":null}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\"file.\"},\"finish_reason\":null}]}\n\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\"}}]},\"finish_reason\":null}]}\n\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"a.txt\\\"}\"}}]},\"finish_reason\":null}]}\n\n\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n\
data: [DONE]\n\n";

    let base = spawn_mock(body);
    let client = OpenAiCompatClient::new(reqwest::Client::new(), base, None);

    let (tx, mut rx) = mpsc::channel::<StreamEvent>(64);
    let req = ChatRequest {
        model: "mock".into(),
        messages: vec![Message::user("read a.txt")],
        tools: vec![],
        temperature: None,
        max_tokens: None,
        thinking: None,
    };

    let completion = client.stream(req, tx, CancellationToken::new()).await.unwrap();

    // Assembled tool call: name + arguments joined across chunks.
    assert_eq!(completion.tool_calls.len(), 1);
    let call = &completion.tool_calls[0];
    assert_eq!(call.name, "read");
    assert_eq!(call.id, "call_1");
    assert_eq!(call.arguments, r#"{"path":"a.txt"}"#);
    assert_eq!(completion.content.as_deref(), Some("Reading file."));
    assert_eq!(completion.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(completion.usage.total_tokens, 15);

    // Streamed events include the two text deltas.
    let mut text = String::new();
    while let Ok(ev) = rx.try_recv() {
        if let StreamEvent::TextDelta(t) = ev {
            text.push_str(&t);
        }
    }
    assert_eq!(text, "Reading file.");
}

#[tokio::test]
async fn a_mid_stream_error_fails_instead_of_returning_nothing() {
    common::isolate_home();
    // Providers report failures in band: HTTP 200, then an error frame. Treating
    // that as an unparseable chunk produced an empty *successful* completion —
    // a rate-limited planner came back as "the model said nothing", and three
    // fan-out diagnoses were wrong because of it.
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n\
               data: {\"error\":{\"message\":\"rate-limited upstream\",\"code\":429}}\n\n\
               data: [DONE]\n\n";
    let base = spawn_mock(sse);
    let client = OpenAiCompatClient::new(reqwest::Client::new(), base, None);

    let (tx, mut rx) = mpsc::channel::<StreamEvent>(32);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let err = client
        .stream(
            ChatRequest {
                model: "m".into(),
                messages: vec![Message::user("hi")],
                tools: vec![],
                temperature: None,
                max_tokens: None,
                thinking: None,
            },
            tx,
            CancellationToken::new(),
        )
        .await
        .expect_err("an error frame must fail the call");
    let _ = drain.await;

    let msg = format!("{err:#}");
    assert!(msg.contains("rate-limited upstream"), "the provider's reason must survive: {msg}");
}
