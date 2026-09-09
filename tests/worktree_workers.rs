mod common;
use async_trait::async_trait;
use std::{path::Path, sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use worksmith::{
    agent::Agent,
    event::EventBus,
    llm::{ChatRequest, Completion, LlmClient, Role, StreamEvent, ToolCall},
    tools::{ToolContext, ToolRegistry},
    worker::WorkerManager,
};

struct Writer;
#[async_trait]
impl LlmClient for Writer {
    async fn stream(
        &self,
        req: ChatRequest,
        _: mpsc::Sender<StreamEvent>,
        _: CancellationToken,
    ) -> anyhow::Result<Completion> {
        if req.messages.iter().any(|m| m.role == Role::Tool) {
            return Ok(Completion {
                content: Some("Wrote the requested worker result and finished the task.".into()),
                ..Default::default()
            });
        }
        let content = req
            .messages
            .iter()
            .find(|m| m.role == Role::User)
            .and_then(|m| m.content.as_deref())
            .unwrap();
        let text = if content.contains("second") {
            "second"
        } else {
            "first"
        };
        Ok(Completion {
            tool_calls: vec![ToolCall {
                id: "write".into(),
                name: "bash".into(),
                arguments:
                    serde_json::json!({"command":format!("printf '{text}' > file; pwd > location")})
                        .to_string(),
            }],
            ..Default::default()
        })
    }
}
async fn git(root: &Path, args: &[&str]) {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
async fn repo() -> tempfile::TempDir {
    common::isolate_home();
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-q"]).await;
    git(
        root.path(),
        &["config", "user.email", "test@example.invalid"],
    )
    .await;
    git(root.path(), &["config", "user.name", "Test"]).await;
    git(root.path(), &["config", "commit.gpgsign", "false"]).await;
    std::fs::create_dir(root.path().join("sub")).unwrap();
    std::fs::write(root.path().join("sub/file"), "base").unwrap();
    git(root.path(), &["add", "."]).await;
    git(root.path(), &["commit", "-qm", "base"]).await;
    root
}
fn manager(cwd: &Path, count: usize) -> WorkerManager {
    let agent = Agent::new(
        Arc::new(Writer),
        Arc::new(ToolRegistry::with_builtins()),
        EventBus::new(),
        "mock".into(),
        None,
        None,
        8,
        1,
        3,
        100000,
        6,
        ToolContext {
            cwd: cwd.into(),
            ..Default::default()
        },
    );
    WorkerManager::new(Arc::new(agent), cwd.into(), count)
}
async fn wait(manager: &WorkerManager) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while manager.running_count() > 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn parallel_workers_and_checks_use_separate_subdirectories() {
    let root = repo().await;
    let cwd = root.path().join("sub");
    let mut manager = manager(&cwd, 2);
    manager.prepare_spawn(false).await.unwrap();
    manager
        .spawn_checked(
            "first".into(),
            "system".into(),
            None,
            Some("test \"$(cat file)\" = first".into()),
        )
        .unwrap();
    manager
        .spawn_checked(
            "second".into(),
            "system".into(),
            None,
            Some("test \"$(cat file)\" = second && echo VALIDATED_SECOND".into()),
        )
        .unwrap();
    wait(&manager).await;
    assert_eq!(std::fs::read_to_string(cwd.join("file")).unwrap(), "base");
    assert!(!cwd.join("location").exists());
    let records = worksmith::workspace::list(&cwd).await.unwrap();
    assert_eq!(records.len(), 2);
    for worker in manager.list() {
        assert_eq!(worker.check_passed, Some(true), "{}", worker.result);
        let check = worker.validation.as_ref().unwrap();
        assert!(check.log_path.as_ref().unwrap().exists());
        let record = records
            .iter()
            .find(|record| record.id == worker.session_id)
            .unwrap();
        assert_eq!(
            record.validation.as_ref().unwrap().display(),
            check.display()
        );
        let path = worksmith::session::Session::path_for_id(&worker.session_id).unwrap();
        let events = worksmith::session::events(&path).unwrap();
        assert!(events.iter().any(|entry| matches!(&entry.event, worksmith::event::Event::Validation { report: Some(report), .. } if report.log_path == check.log_path)));
        assert!(worksmith::report::worker_detail(&worker).contains("Validation · Passed · exit 0"));

        assert!(worker.changed.contains(&"sub/file".to_owned()));
        let diff = manager
            .review("diff", Some(&worker.id), false)
            .await
            .unwrap();
        assert!(diff.contains(&format!("+{}", worker.task)));
        assert!(diff.contains("Validation · Passed · exit 0"));
        assert!(!diff.contains("Some("));
        if worker.task == "second" {
            assert!(diff.contains("VALIDATED_SECOND"));
        }
        let mut legacy = serde_json::to_value(record).unwrap();
        legacy.as_object_mut().unwrap().remove("validation");
        let legacy: worksmith::workspace::Workspace = serde_json::from_value(legacy).unwrap();
        assert!(legacy.validation.is_none());
        manager
            .review("discard", Some(&worker.id), true)
            .await
            .unwrap();
    }
    manager.shutdown().await;
}
#[tokio::test]
async fn queued_workers_keep_the_captured_base_and_parent_edits() {
    let root = repo().await;
    let cwd = root.path().join("sub");
    let mut manager = manager(&cwd, 1);
    manager.prepare_spawn(false).await.unwrap();
    manager.spawn_many(
        vec!["first".into(), "second".into()],
        "system".into(),
        "group".into(),
    );
    wait(&manager).await;
    std::fs::write(cwd.join("file"), "parent changed").unwrap();
    git(root.path(), &["add", "."]).await;
    git(root.path(), &["commit", "-qm", "parent advanced"]).await;
    assert_eq!(manager.pump().len(), 1);
    wait(&manager).await;
    assert_eq!(
        std::fs::read_to_string(cwd.join("file")).unwrap(),
        "parent changed"
    );
    let second = manager.review("diff", Some("w2"), false).await.unwrap();
    assert!(second.contains("-base") && second.contains("+second"));
    assert!(manager.review("apply", Some("w2"), false).await.is_err());
    for worker in manager.list() {
        manager
            .review("discard", Some(&worker.id), true)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn cancelling_a_check_retains_edits_and_shutdown_is_repeatable() {
    let root = repo().await;
    let cwd = root.path().join("sub");
    let mut manager = manager(&cwd, 1);
    manager.prepare_spawn(false).await.unwrap();
    manager
        .spawn_checked(
            "first".into(),
            "system".into(),
            None,
            Some("echo ready > checking; sleep 10".into()),
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let records = worksmith::workspace::list(&cwd).await.unwrap();
            if records
                .iter()
                .any(|r| r.cwd().unwrap().join("checking").exists())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    manager.shutdown().await;
    manager.shutdown().await;
    assert!(!manager.get("w1").unwrap().status.is_running());
    assert_eq!(std::fs::read_to_string(cwd.join("file")).unwrap(), "base");
    let diff = manager.review("diff", Some("w1"), false).await.unwrap();
    assert!(diff.contains("+first"));
    manager.review("discard", Some("w1"), true).await.unwrap();
}

#[tokio::test]
async fn isolation_failure_does_not_dispatch_a_shared_worker() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let mut manager = manager(dir.path(), 1);
    assert!(manager.prepare_spawn(false).await.is_err());
    assert!(manager.spawn("first".into(), "system".into()).is_err());
    assert!(!dir.path().join("file").exists());
    manager.prepare_spawn(true).await.unwrap();
    manager.spawn("first".into(), "system".into()).unwrap();
    wait(&manager).await;
    assert_eq!(
        std::fs::read_to_string(dir.path().join("file")).unwrap(),
        "first"
    );
}

#[tokio::test]
async fn worker_knowledge_is_local_and_does_not_become_a_source_change() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("architecture.md"),
        "The worker-local architecture uses copper gears.",
    )
    .unwrap();
    let ctx = ToolContext {
        cwd: dir.path().to_owned(),
        session_id: uuid::Uuid::new_v4().to_string(),
        is_worker: true,
        ..Default::default()
    };
    let out = ToolRegistry::with_builtins()
        .run(
            "knowledge",
            serde_json::json!({"action":"search", "query":"copper gears"}),
            &ctx,
        )
        .await;
    assert!(
        !out.is_error && out.content.contains("copper"),
        "{}",
        out.content
    );
    assert!(!dir.path().join(".worksmith").exists());
}
