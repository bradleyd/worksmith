//! Who a pairing checkpoint may interrupt.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use worksmith::agent::Agent;
use worksmith::event::EventBus;
use worksmith::llm::{ChatRequest, Completion, LlmClient, StreamEvent};
use worksmith::tools::{ToolContext, ToolRegistry};

/// Never called: these tests read what *would* be sent, not what comes back.
struct Silent;

#[async_trait]
impl LlmClient for Silent {
    async fn stream(
        &self,
        _req: ChatRequest,
        _sink: mpsc::Sender<StreamEvent>,
        _cancel: CancellationToken,
    ) -> anyhow::Result<Completion> {
        Ok(Completion::default())
    }
}

fn names(a: &Agent) -> Vec<String> {
    a.advertised_tools().into_iter().map(|d| d.name).collect()
}

/// Build a bare agent; the client is never called by these tests.
fn agent(ctx: ToolContext) -> Agent {
    Agent::new(
        Arc::new(Silent),
        Arc::new(ToolRegistry::with_builtins()),
        EventBus::new(),
        "test/model".to_string(),
        None,
        None,
        8,
        1,
        3,
        32_000,
        6,
        ctx,
    )
}

#[test]
fn pairing_off_does_not_even_advertise_the_checkpoint() {
    // Off has to mean "not in the payload". `defs()` rides every request, so a
    // schema for a tool that will never fire is real money in a 32k window.
    let a = agent(ToolContext::default());
    assert!(!names(&a).iter().any(|n| n == "checkpoint"));

    let a = a.with_pairing(true);
    assert!(names(&a).iter().any(|n| n == "checkpoint"));
}

#[test]
fn a_spawned_worker_never_inherits_pairing() {
    // Nobody is watching a background task, so a blocking question would stall
    // it against a user who does not know it was asked — and a fan-out of five
    // would queue five questions behind one composer.
    let parent = agent(ToolContext::default()).with_pairing(true);
    let worker = parent.fork(EventBus::new(), "w1".to_string());

    assert!(parent.pairing_on(), "the session is pairing");
    assert!(!worker.pairing_on(), "the worker is not");
    assert!(!names(&worker).iter().any(|n| n == "checkpoint"));
}

#[test]
fn turning_pairing_on_mid_session_does_not_reach_running_workers() {
    // `/pair` is a session switch, not a global one. Unlike `route`, the flag
    // is not shared with a fork.
    let parent = agent(ToolContext::default());
    let worker = parent.fork(EventBus::new(), "w1".to_string());
    parent.set_pairing(true);

    assert!(parent.pairing_on());
    assert!(
        !worker.pairing_on(),
        "a running worker must not start interrupting"
    );
}

mod common;

use std::sync::Mutex;
use worksmith::llm::{Message, Role};
use worksmith::prompt::PAIRING_PREAMBLE;
use worksmith::session::Session;

#[derive(Default)]
struct Recorder {
    requests: Mutex<Vec<ChatRequest>>,
}

#[async_trait]
impl LlmClient for Recorder {
    async fn stream(
        &self,
        req: ChatRequest,
        _sink: mpsc::Sender<StreamEvent>,
        _cancel: CancellationToken,
    ) -> anyhow::Result<Completion> {
        let compaction = req.tools.is_empty();
        self.requests.lock().unwrap().push(req);
        Ok(Completion {
            content: Some(if compaction {
                "## Goal\nKeep the user's settled design.\n\n## Locations\nsrc/connection.rs:1 — reconnect state and retry budget.\n\n## Established\nRetry state belongs to Connection; successful connections reset it. Preserve this decision when continuing the implementation.\n\n## Unfinished\nImplement and validate the reconnect change."
            } else { "Done." }.into()),
            ..Default::default()
        })
    }
}

fn recording_agent(client: Arc<dyn LlmClient>, cwd: &std::path::Path) -> Agent {
    Agent::new(
        client,
        Arc::new(ToolRegistry::with_builtins()),
        EventBus::new(),
        "test/model".into(),
        None,
        Some(4096),
        8,
        1,
        3,
        32_000,
        6,
        ToolContext {
            cwd: cwd.to_path_buf(),
            ..Default::default()
        },
    )
}

fn assert_pairing_request(req: &ChatRequest, on: bool) {
    assert_eq!(req.messages[0].role, Role::System);
    assert_eq!(
        req.messages
            .iter()
            .filter(|m| m.role == Role::System)
            .count(),
        1
    );
    let system = req.messages[0].content.as_deref().unwrap();
    assert_eq!(system.matches(PAIRING_PREAMBLE).count(), usize::from(on));
    assert_eq!(req.tools.iter().any(|t| t.name == "checkpoint"), on);
    assert!(req.messages.iter().skip(1).all(|m| {
        !m.content
            .as_deref()
            .unwrap_or_default()
            .contains(PAIRING_PREAMBLE)
    }));
}

#[tokio::test]
async fn pairing_guidance_tracks_toggles_and_accounts_for_its_context() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let a = recording_agent(recorder.clone(), dir.path());
    let mut session = Session::create(dir.path()).unwrap();
    for on in [false, true, false] {
        a.set_pairing(on);
        a.run_turn(
            &mut session,
            "hello",
            "base",
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    }
    let requests = recorder.requests.lock().unwrap();
    for (req, on) in requests.iter().zip([false, true, false]) {
        assert_pairing_request(req, on);
    }
    assert_eq!(requests.len(), 3);
    let tokens = |i: usize| requests[i].context_breakdown.unwrap().system_tokens;
    assert!(tokens(1) > tokens(0));
    assert_eq!(tokens(0), tokens(2));
    assert!(
        !std::fs::read_to_string(session.path())
            .unwrap()
            .contains(PAIRING_PREAMBLE)
    );
}

#[tokio::test]
async fn guidance_survives_compacted_history_and_resume_uses_current_mode() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let a = recording_agent(recorder.clone(), dir.path()).with_pairing(true);
    let mut session = Session::create(dir.path()).unwrap();
    for _ in 0..12 {
        session
            .append_message(Message::user("old request"))
            .unwrap();
        session
            .append_message(Message::assistant(
                Some("old evidence ".repeat(1000)),
                vec![],
            ))
            .unwrap();
    }
    let before = session.messages().len();
    a.compact(&mut session).await.unwrap();
    assert!(
        session.messages().len() < before,
        "the fixture must really compact"
    );
    a.run_turn(
        &mut session,
        "continue",
        "base",
        None,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let path = session.path().to_path_buf();
    drop(session);
    let mut resumed = Session::open(&path).unwrap();
    // The CLI constructs a new agent from current config; mode isn't replayed.
    let fresh = recording_agent(recorder.clone(), dir.path());
    fresh
        .run_turn(
            &mut resumed,
            "continue",
            "base",
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let worker = a.fork(EventBus::new(), "w1".into());
    let mut child = Session::create(dir.path()).unwrap();
    worker
        .run_turn(
            &mut child,
            "work",
            "worker base",
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let requests = recorder.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    assert!(requests[0].tools.is_empty());
    assert!(
        !requests[0].messages[0]
            .content
            .as_deref()
            .unwrap()
            .contains(PAIRING_PREAMBLE)
    );
    assert_pairing_request(&requests[1], true);
    assert!(requests[1].messages.iter().any(|m| {
        m.content
            .as_deref()
            .is_some_and(|t| t.contains("Keep the user's settled design."))
    }));
    assert_pairing_request(&requests[2], false);
    assert_pairing_request(&requests[3], false);
}

struct PausedRequest {
    requests: Mutex<Vec<ChatRequest>>,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
    reject_first: bool,
}

#[async_trait]
impl LlmClient for PausedRequest {
    async fn stream(
        &self,
        req: ChatRequest,
        _sink: mpsc::Sender<StreamEvent>,
        _cancel: CancellationToken,
    ) -> anyhow::Result<Completion> {
        let first = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(req);
            requests.len() == 1
        };
        if first {
            self.started.notify_one();
            self.release.notified().await;
            if self.reject_first {
                anyhow::bail!(
                    "This model's maximum context length is 32768 tokens. However, you requested 4096 output tokens and your prompt contains at least 29000 input tokens, for a total of at least 33096 tokens."
                );
            }
            return Ok(Completion {
                tool_calls: vec![worksmith::llm::ToolCall {
                    id: "read1".into(),
                    name: "read".into(),
                    arguments: r#"{"path":"file.txt"}"#.into(),
                }],
                ..Default::default()
            });
        }
        Ok(Completion {
            content: Some("Done.".into()),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn toggling_during_a_request_changes_only_the_next_step() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file.txt"), "evidence").unwrap();
    let client = Arc::new(PausedRequest {
        requests: Mutex::new(Vec::new()),
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        reject_first: false,
    });
    let a = Arc::new(recording_agent(client.clone(), dir.path()).with_pairing(true));
    let mut session = Session::create(dir.path()).unwrap();
    let running = a.clone();
    let task = tokio::spawn(async move {
        running
            .run_turn(
                &mut session,
                "read file.txt",
                "base",
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap()
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), client.started.notified())
        .await
        .unwrap();
    a.set_pairing(false);
    client.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    let requests = client.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_pairing_request(&requests[0], true);
    assert_pairing_request(&requests[1], false);
}

struct DialogueClient {
    requests: Mutex<Vec<ChatRequest>>,
}

#[async_trait]
impl LlmClient for DialogueClient {
    async fn stream(
        &self,
        req: ChatRequest,
        _sink: mpsc::Sender<StreamEvent>,
        _cancel: CancellationToken,
    ) -> anyhow::Result<Completion> {
        let helper = req.tools.is_empty();
        let first = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(req);
            requests.len() == 1
        };
        if first {
            return Ok(Completion {
                tool_calls: vec![
                    worksmith::llm::ToolCall {
                        id: "decision".into(), name: "checkpoint".into(),
                        arguments: r#"{"kind":"ask","subject":"Retry ownership","detail":"Keep retry state on Connection?"}"#.into(),
                    },
                    worksmith::llm::ToolCall {
                        id: "edit".into(), name: "write".into(),
                        arguments: r#"{"path":"result.txt","content":"implemented"}"#.into(),
                    },
                ],
                ..Default::default()
            });
        }
        Ok(Completion {
            content: Some(
                if helper {
                    "It preserves the budget across reconnects."
                } else {
                    "Done."
                }
                .into(),
            ),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn a_tool_checkpoint_answers_questions_before_filing_or_running_the_next_tool() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success()
    );
    let client = Arc::new(DialogueClient {
        requests: Mutex::new(Vec::new()),
    });
    let (asker, mut questions) = worksmith::tools::approval::ChannelAsker::new();
    let mut ctx = ToolContext {
        cwd: dir.path().to_path_buf(),
        ..Default::default()
    };
    ctx.asker = Arc::new(asker);
    ctx.decisions_dir = "decisions".into();
    let a = Arc::new(
        Agent::new(
            client.clone(),
            Arc::new(ToolRegistry::with_builtins()),
            EventBus::new(),
            "mock".into(),
            None,
            None,
            8,
            1,
            3,
            32_000,
            6,
            ctx,
        )
        .with_pairing(true),
    );
    let mut session = Session::create(dir.path()).unwrap();
    let runner = a.clone();
    let task = tokio::spawn(async move {
        let out = runner
            .run_turn(
                &mut session,
                "fix it",
                "base",
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        (out, session)
    });
    let question = next_question(&mut questions).await;
    question.answer(Some("Why put it there?".into()));
    let followup = next_question(&mut questions).await;
    assert!(followup.question.contains("preserves the budget"));
    assert!(!dir.path().join("decisions").exists());
    assert!(!dir.path().join("result.txt").exists());
    // A mode switch must not dismiss a question already awaiting direction.
    a.set_pairing(false);
    assert!(!task.is_finished());
    followup.answer(Some("Yes, keep it there.".into()));
    let (out, session) = task.await.unwrap();
    assert!(matches!(out.outcome, worksmith::agent::TurnOutcome::Done));
    assert!(
        !dir.path().join("result.txt").exists(),
        "pre-decision batched edits must be reconsidered"
    );
    let decision =
        std::fs::read_to_string(dir.path().join("decisions/0001-retry-ownership.md")).unwrap();
    assert!(decision.contains("Yes, keep it there."));
    assert!(!decision.contains("Why put it there?"));
    let requests = client.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_pairing_request(&requests[0], true);
    assert!(
        requests[1].tools.is_empty(),
        "the discussion reply cannot edit"
    );
    assert!(
        !requests[1].messages[0]
            .content
            .as_deref()
            .unwrap()
            .contains(PAIRING_PREAMBLE)
    );
    assert_pairing_request(&requests[2], false);
    assert!(requests[2].messages.iter().any(|m| {
        m.content
            .as_deref()
            .is_some_and(|t| t.contains("Not executed: a pairing checkpoint intervened"))
    }));
    let log = std::fs::read_to_string(session.path()).unwrap();
    assert!(
        log.contains("Why put it there?"),
        "discussion remains in structural history"
    );
}

#[tokio::test]
async fn unanswered_or_cancelled_tool_discussions_cannot_run_batched_edits() {
    common::isolate_home();
    for cancel_early in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let client = Arc::new(DialogueClient {
            requests: Mutex::new(Vec::new()),
        });
        let (asker, mut questions) = worksmith::tools::approval::ChannelAsker::new();
        let ctx = ToolContext {
            cwd: dir.path().to_path_buf(),
            asker: Arc::new(asker),
            decisions_dir: "decisions".into(),
            ..Default::default()
        };
        let a = Agent::new(
            client.clone(),
            Arc::new(ToolRegistry::with_builtins()),
            EventBus::new(),
            "mock".into(),
            None,
            None,
            8,
            1,
            3,
            32_000,
            6,
            ctx,
        )
        .with_pairing(true);
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let mut session = Session::create(dir.path()).unwrap();
        let task = tokio::spawn(async move {
            a.run_turn(&mut session, "fix it", "base", None, token)
                .await
                .unwrap()
        });
        if cancel_early {
            let _pending = next_question(&mut questions).await;
            cancel.cancel();
        } else {
            for _ in 0..4 {
                next_question(&mut questions)
                    .await
                    .answer(Some("Why?".into()));
            }
        }
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            out.outcome,
            worksmith::agent::TurnOutcome::Aborted
        ));
        assert!(!dir.path().join("result.txt").exists());
        assert!(!dir.path().join("decisions").exists());
        assert_eq!(
            client.requests.lock().unwrap().len(),
            if cancel_early { 1 } else { 5 }
        );
    }
}

async fn next_question(
    questions: &mut mpsc::Receiver<worksmith::tools::approval::TextRequest>,
) -> worksmith::tools::approval::TextRequest {
    tokio::time::timeout(std::time::Duration::from_secs(5), questions.recv())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn fit_retries_keep_the_original_pairing_snapshot() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let client = Arc::new(PausedRequest {
        requests: Mutex::new(Vec::new()),
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        reject_first: true,
    });
    let a = Arc::new(recording_agent(client.clone(), dir.path()).with_pairing(true));
    let mut session = Session::create(dir.path()).unwrap();
    let running = a.clone();
    let task = tokio::spawn(async move {
        running
            .run_turn(
                &mut session,
                "hello",
                "base",
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap()
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), client.started.notified())
        .await
        .unwrap();
    a.set_pairing(false);
    client.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    let requests = client.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_pairing_request(&requests[0], true);
    assert_pairing_request(&requests[1], true);
    assert!(requests[1].max_tokens < requests[0].max_tokens);
}
