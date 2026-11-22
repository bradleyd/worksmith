//! The end-of-turn review: an independent call on the request and the diff,
//! once per turn, that can send the agent back with concrete findings.

mod common;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use worksmith::agent::{Agent, TurnOutcome};
use worksmith::event::{Event, EventBus};
use worksmith::llm::{ChatRequest, Completion, LlmClient, Role, StreamEvent, ToolCall};
use worksmith::session::Session;
use worksmith::tools::{ToolContext, ToolRegistry};

/// Plays the agent from `steps` and the reviewer from `reviews`, telling them
/// apart by the reviewer's system prompt. Keeps what each reviewer saw.
struct Script {
    steps: Mutex<VecDeque<Completion>>,
    reviews: Mutex<VecDeque<String>>,
    reviewer_saw: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl LlmClient for Script {
    async fn stream(
        &self,
        req: ChatRequest,
        _sink: mpsc::Sender<StreamEvent>,
        _cancel: CancellationToken,
    ) -> anyhow::Result<Completion> {
        let system = req.messages[0].content.clone().unwrap_or_default();
        if system == worksmith::review::REVIEW_PROMPT {
            self.reviewer_saw
                .lock()
                .unwrap()
                .push(req.messages[1].content.clone().unwrap_or_default());
            let text = self.reviews.lock().unwrap().pop_front().unwrap_or_default();
            return Ok(Completion {
                content: Some(text),
                ..Default::default()
            });
        }
        Ok(self.steps.lock().unwrap().pop_front().unwrap_or_default())
    }
}

fn write(path: &str, content: &str) -> Completion {
    Completion {
        tool_calls: vec![ToolCall {
            id: format!("w-{path}"),
            name: "write".into(),
            arguments: serde_json::json!({ "path": path, "content": content }).to_string(),
        }],
        finish_reason: Some("tool_calls".into()),
        ..Default::default()
    }
}

fn done(text: &str) -> Completion {
    Completion {
        content: Some(text.into()),
        finish_reason: Some("stop".into()),
        ..Default::default()
    }
}

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let ok = std::process::Command::new("sh")
        .args([
            "-c",
            "git init -q && git config user.email t@t && git config user.name t \
             && echo '# app' > README.md && git add . && git commit -qm init",
        ])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success();
    assert!(ok);
    dir
}

struct Run {
    outcome: TurnOutcome,
    reviewer_saw: Vec<String>,
    session: Session,
    nudges: Vec<String>,
}

async fn run(dir: &std::path::Path, steps: Vec<Completion>, reviews: Vec<&str>, on: bool) -> Run {
    common::isolate_home();
    let saw = Arc::new(Mutex::new(Vec::new()));
    let client = Script {
        steps: Mutex::new(steps.into()),
        reviews: Mutex::new(reviews.into_iter().map(String::from).collect()),
        reviewer_saw: saw.clone(),
    };
    let bus = EventBus::new();
    let mut rx = bus.subscribe();
    let agent = Agent::new(
        Arc::new(client),
        Arc::new(ToolRegistry::with_builtins()),
        bus,
        "mock".into(),
        None,
        None,
        20,
        3,
        3,
        100_000,
        4,
        ToolContext {
            cwd: dir.to_path_buf(),
            session_id: "test".into(),
            bash_timeout: Duration::from_secs(10),
            ..Default::default()
        },
    )
    .with_review(on, None);
    // Outside the repo, where real sessions live (~/.worksmith/sessions).
    let logs = tempfile::tempdir().unwrap();
    let mut session = Session::create_at(&logs.path().join("s.jsonl"), dir).unwrap();
    let result = agent
        .run_turn(
            &mut session,
            "add a refund function to refunds.py",
            "system",
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut nudges = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let Event::Nudge { reason } = ev {
            nudges.push(reason);
        }
    }
    let reviewer_saw = saw.lock().unwrap().clone();
    Run {
        outcome: result.outcome,
        reviewer_saw,
        session,
        nudges,
    }
}

#[tokio::test]
async fn findings_send_the_agent_back_once_and_the_reviewer_sees_only_the_change() {
    let dir = repo();
    let r = run(
        dir.path(),
        vec![
            write("refunds.py", "def refund(order):\n    pass  # TODO\n"),
            done("Added refund."),
            write(
                "refunds.py",
                "def refund(order):\n    order.refunded = True\n",
            ),
            done("Implemented it."),
        ],
        vec!["VERDICT: revise\n- refunds.py:2: refund is a stub that does nothing"],
        true,
    )
    .await;

    assert!(matches!(r.outcome, TurnOutcome::Done), "{:?}", r.outcome);
    assert_eq!(
        r.reviewer_saw.len(),
        1,
        "once per turn, even after a revision"
    );
    let saw = &r.reviewer_saw[0];
    assert!(saw.contains("add a refund function"), "{saw}");
    assert!(saw.contains("+    pass  # TODO"), "{saw}");
    assert!(
        !saw.contains("Added refund."),
        "never the agent's account of itself"
    );
    assert!(!saw.contains("s.jsonl"), "{saw}");

    let directive = r
        .session
        .messages()
        .iter()
        .find(|m| {
            m.role == Role::User
                && m.content
                    .as_deref()
                    .is_some_and(|c| c.contains("independent reviewer"))
        })
        .expect("findings reach the agent");
    assert!(
        directive
            .content
            .as_deref()
            .unwrap()
            .contains("refund is a stub")
    );
    assert!(
        r.nudges
            .iter()
            .any(|n| n.starts_with("review (mock): 1 finding")),
        "{:?}",
        r.nudges
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("refunds.py")).unwrap(),
        "def refund(order):\n    order.refunded = True\n"
    );
}

#[tokio::test]
async fn a_pass_finishes_the_turn_as_before() {
    let dir = repo();
    let r = run(
        dir.path(),
        vec![
            write("refunds.py", "def refund(o):\n    o.refunded = True\n"),
            done("Done."),
        ],
        vec!["VERDICT: pass"],
        true,
    )
    .await;
    assert!(matches!(r.outcome, TurnOutcome::Done), "{:?}", r.outcome);
    assert_eq!(r.reviewer_saw.len(), 1);
    assert!(
        r.nudges.iter().any(|n| n == "review (mock): pass"),
        "{:?}",
        r.nudges
    );
    assert!(!r.session.messages().iter().any(|m| {
        m.content
            .as_deref()
            .is_some_and(|c| c.contains("independent reviewer"))
    }));
}

#[tokio::test]
async fn nothing_changed_or_review_off_means_no_review() {
    let dir = repo();
    let r = run(
        dir.path(),
        vec![done("It is in README.md.")],
        vec!["VERDICT: revise\n- x:1: y"],
        true,
    )
    .await;
    assert!(matches!(r.outcome, TurnOutcome::Done), "{:?}", r.outcome);
    assert!(
        r.reviewer_saw.is_empty(),
        "a question that changed nothing is not reviewed"
    );
    // ...but it says so, or a skip looks like review being off.
    assert!(
        r.nudges
            .iter()
            .any(|n| n == "review: nothing to check — this turn changed no project files"),
        "{:?}",
        r.nudges
    );

    let dir = repo();
    let r = run(
        dir.path(),
        vec![write("a.py", "x = 1\n"), done("Done.")],
        vec!["VERDICT: revise\n- a.py:1: y"],
        false,
    )
    .await;
    assert!(r.reviewer_saw.is_empty());
    assert!(
        r.nudges.iter().all(|n| !n.starts_with("review")),
        "off is silent: {:?}",
        r.nudges
    );
}

#[tokio::test]
async fn a_broken_reviewer_never_holds_the_turn() {
    let dir = repo();
    // An empty answer is an error from the helper call; it must end in Done.
    let r = run(
        dir.path(),
        vec![write("a.py", "x = 1\n"), done("Done.")],
        vec![""],
        true,
    )
    .await;
    assert!(matches!(r.outcome, TurnOutcome::Done), "{:?}", r.outcome);
}

#[test]
fn the_person_is_told_who_reviews() {
    let dir = tempfile::tempdir().unwrap();
    let agent = Agent::new(
        Arc::new(Script {
            steps: Mutex::new(VecDeque::new()),
            reviews: Mutex::new(VecDeque::new()),
            reviewer_saw: Arc::new(Mutex::new(Vec::new())),
        }),
        Arc::new(ToolRegistry::with_builtins()),
        EventBus::new(),
        "mock".into(),
        None,
        None,
        20,
        3,
        3,
        100_000,
        4,
        ToolContext {
            cwd: dir.path().to_path_buf(),
            ..Default::default()
        },
    )
    .with_accounting("vllm/qwen38".into(), Default::default())
    .with_review(true, None);
    let note = agent.reviewer_note();
    assert!(note.contains("vllm/qwen38"), "{note}");
    assert!(
        note.contains("session's own model in a fresh context"),
        "{note}"
    );
    assert!(note.contains("[agents] model"), "{note}");
}

#[tokio::test]
async fn outside_git_the_skip_is_explained() {
    let dir = tempfile::tempdir().unwrap();
    let r = run(
        dir.path(),
        vec![write("a.py", "x = 1\n"), done("Done.")],
        vec![],
        true,
    )
    .await;
    assert!(matches!(r.outcome, TurnOutcome::Done), "{:?}", r.outcome);
    assert!(r.reviewer_saw.is_empty());
    assert!(
        r.nudges
            .iter()
            .any(|n| n == "review: skipped — not a git repository"),
        "{:?}",
        r.nudges
    );
}
