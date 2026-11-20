//! Images reach a model that can see them, and only that model: attached with
//! `@path`, opened with `read`, and dropped to a note where they cannot go.

mod common;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use worksmith::agent::Agent;
use worksmith::config::ModelSettings;
use worksmith::event::EventBus;
use worksmith::llm::{ChatRequest, Completion, LlmClient, Message, Role, StreamEvent, ToolCall};
use worksmith::session::Session;
use worksmith::tools::{ToolContext, ToolRegistry};

/// Replays scripted completions and keeps every request it was sent.
struct Recorder {
    responses: Mutex<VecDeque<Completion>>,
    seen: Arc<Mutex<Vec<Vec<Message>>>>,
}

#[async_trait]
impl LlmClient for Recorder {
    async fn stream(
        &self,
        req: ChatRequest,
        _sink: mpsc::Sender<StreamEvent>,
        _cancel: CancellationToken,
    ) -> anyhow::Result<Completion> {
        self.seen.lock().unwrap().push(req.messages);
        Ok(self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default())
    }
}

fn png(path: &std::path::Path, w: u32, h: u32) {
    image::RgbImage::from_fn(w, h, |x, _| {
        if x < w / 2 {
            image::Rgb([255, 0, 0])
        } else {
            image::Rgb([0, 0, 255])
        }
    })
    .save(path)
    .unwrap();
}

fn read_call(path: &str) -> Completion {
    Completion {
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: "read".into(),
            arguments: format!("{{\"path\":\"{path}\"}}"),
        }],
        finish_reason: Some("tool_calls".into()),
        ..Default::default()
    }
}

fn agent(
    dir: &std::path::Path,
    responses: Vec<Completion>,
    vision: bool,
) -> (Agent, Arc<Mutex<Vec<Vec<Message>>>>, EventBus) {
    let bus = EventBus::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let client = Recorder {
        responses: Mutex::new(responses.into()),
        seen: seen.clone(),
    };
    let agent = Agent::new(
        Arc::new(client),
        Arc::new(ToolRegistry::with_builtins()),
        bus.clone(),
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
    .with_accounting(
        "local/vlm".into(),
        ModelSettings {
            vision: Some(vision),
            ..Default::default()
        },
    );
    (agent, seen, bus)
}

async fn run(agent: &Agent, session: &mut Session, text: &str) {
    agent
        .run_turn(session, text, "system", None, CancellationToken::new())
        .await
        .unwrap();
}

#[tokio::test]
async fn an_at_mention_sends_the_photo_to_a_model_that_can_see() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    png(&dir.path().join("closet.png"), 2000, 1500);
    let mut session = Session::create_at(&dir.path().join("s.jsonl"), dir.path()).unwrap();
    let (agent, seen, _) = agent(dir.path(), vec![], true);

    run(&agent, &mut session, "what goes with @closet.png today?").await;

    let requests = seen.lock().unwrap();
    let user = requests[0].iter().rfind(|m| m.role == Role::User).unwrap();
    assert_eq!(user.images.len(), 1);
    assert_eq!((user.images[0].width, user.images[0].height), (1024, 768));
    assert_eq!(
        user.content.as_deref(),
        Some("what goes with @closet.png today?")
    );
}

#[tokio::test]
async fn a_text_only_model_gets_a_note_and_never_the_image() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    png(&dir.path().join("closet.png"), 40, 40);
    let mut session = Session::create_at(&dir.path().join("s.jsonl"), dir.path()).unwrap();
    let (agent, seen, bus) = agent(dir.path(), vec![], false);
    let mut events = bus.subscribe();

    run(&agent, &mut session, "what goes with @closet.png?").await;

    let requests = seen.lock().unwrap();
    assert!(requests[0].iter().all(|m| m.images.is_empty()));
    let user = requests[0].iter().rfind(|m| m.role == Role::User).unwrap();
    assert!(
        user.content
            .as_deref()
            .unwrap()
            .contains("[image closet.png (40×40) not shown"),
        "{:?}",
        user.content
    );
    // The person is told too, with the line to add.
    let mut warned = false;
    while let Ok(ev) = events.try_recv() {
        if let worksmith::event::Event::Warning { message } = ev {
            warned |= message.contains("vision = true") && message.contains("local/vlm");
        }
    }
    assert!(warned);
    // The session keeps the image, so switching to a model that can see works.
    assert_eq!(session.messages()[0].images.len(), 1);
}

#[tokio::test]
async fn read_on_an_image_shows_it_after_the_step_results() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    png(&dir.path().join("screen.png"), 300, 600);
    let mut session = Session::create_at(&dir.path().join("s.jsonl"), dir.path()).unwrap();
    let (agent, seen, _) = agent(dir.path(), vec![read_call("screen.png")], true);

    run(&agent, &mut session, "check the login screen").await;

    let requests = seen.lock().unwrap();
    let second = &requests[1];
    let tool = second.iter().position(|m| m.role == Role::Tool).unwrap();
    assert!(
        second[tool].images.is_empty(),
        "a tool result is text on the wire"
    );
    assert!(second[tool].content.as_deref().unwrap().contains("300×600"));
    let next = &second[tool + 1];
    assert_eq!(next.role, Role::User);
    assert_eq!(next.images.len(), 1);
    assert_eq!(next.images[0].name, "screen.png");
}

#[tokio::test]
async fn only_the_newest_images_ride_each_request() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    for i in 1..=5 {
        png(&dir.path().join(format!("p{i}.png")), 20, 20);
    }
    let mut session = Session::create_at(&dir.path().join("s.jsonl"), dir.path()).unwrap();
    let (agent, seen, _) = agent(dir.path(), vec![], true);

    run(&agent, &mut session, "@p1.png @p2.png").await;
    run(&agent, &mut session, "@p3.png @p4.png @p5.png").await;

    let requests = seen.lock().unwrap();
    let last = requests.last().unwrap();
    let sent: Vec<&str> = last
        .iter()
        .flat_map(|m| m.images.iter().map(|i| i.name.as_str()))
        .collect();
    assert_eq!(sent, vec!["p3.png", "p4.png", "p5.png"]);
    let first_turn = last
        .iter()
        .find(|m| {
            m.content
                .as_deref()
                .is_some_and(|c| c.starts_with("@p1.png"))
        })
        .unwrap();
    assert!(
        first_turn
            .content
            .as_deref()
            .unwrap()
            .contains("[image p1.png (20×20) shown earlier"),
        "{:?}",
        first_turn.content
    );
}
