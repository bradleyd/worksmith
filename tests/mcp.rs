mod common;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex, atomic::Ordering},
};
use worksmith::{
    mcp::{Manager, ServerConfig},
    tools::{
        ToolContext,
        approval::{Approval, Approver, RefuseWhenUnattended, RememberingApprover},
    },
};

struct Fixture {
    manager: Manager,
    ctx: ToolContext,
    log: PathBuf,
    _dir: tempfile::TempDir,
}
impl Fixture {
    fn new(mode: &str) -> Self {
        common::isolate_home();
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls.jsonl");
        let python = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|p| p.join("python3"))
            .find(|p| p.is_file())
            .expect("offline fixture requires python3")
            .canonicalize()
            .unwrap();
        let config = ServerConfig {
            enabled: true,
            command: python.to_string_lossy().into(),
            args: vec![
                format!(
                    "{}/tests/fixtures/mcp_server.py",
                    env!("CARGO_MANIFEST_DIR")
                ),
                log.to_string_lossy().into(),
                mode.into(),
            ],
            timeout_ms: 1000,
            ..ServerConfig::default()
        };
        let manager = Manager::new(BTreeMap::from([("fixture".into(), config)])).unwrap();
        let ctx = ToolContext {
            cwd: dir.path().to_path_buf(),
            session_id: uuid::Uuid::new_v4().to_string(),
            ..ToolContext::default()
        };
        Self {
            manager,
            ctx,
            log,
            _dir: dir,
        }
    }
    async fn action(&self, args: Value) -> worksmith::tools::ToolOutput {
        self.manager.run("mcp", args, &self.ctx, None).await
    }
    async fn start(&self, tool: &str) {
        let out = self
            .action(json!({"action":"refresh","server":"fixture"}))
            .await;
        assert!(!out.is_error, "{}", out.content);
        let out = self
            .action(json!({"action":"activate","tool":format!("mcp__fixture__{tool}")}))
            .await;
        assert!(!out.is_error, "{}", out.content);
    }
    async fn call(&self, tool: &str, args: Value) -> worksmith::tools::ToolOutput {
        let name = format!("mcp__fixture__{tool}");
        let snapshot = self.manager.definitions(&self.ctx);
        self.manager
            .run(
                &name,
                args,
                &self.ctx,
                snapshot.iter().find(|d| d.name == name),
            )
            .await
    }
    fn records(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

#[tokio::test]
async fn discovery_is_lazy_activation_changes_only_the_next_snapshot_and_workers_are_excluded() {
    let f = Fixture::new("normal");
    assert!(!f.log.exists());
    let initial = f.manager.definitions(&f.ctx);
    assert_eq!(initial.len(), 1);
    f.action(json!({"action":"list"})).await;
    assert!(!f.log.exists());
    f.start("echo").await;
    assert_eq!(initial.len(), 1);
    assert_eq!(f.manager.definitions(&f.ctx).len(), 2);
    assert!(!f.call("echo", json!({"value":"hello"})).await.is_error);
    let worker = ToolContext {
        is_worker: true,
        ..f.ctx.clone()
    };
    assert!(f.manager.definitions(&worker).is_empty());
    assert!(
        f.manager
            .run(
                "mcp",
                json!({"action":"refresh","server":"fixture"}),
                &worker,
                None
            )
            .await
            .is_error
    );
    assert!(
        f.manager
            .run("mcp__fixture__echo", json!({}), &worker, None)
            .await
            .is_error
    );
    assert_eq!(
        f.records()
            .iter()
            .filter(|v| v.get("call").is_some())
            .count(),
        1
    );
    assert_eq!(f.records()[0]["home_inherited"], false);
    f.manager.shutdown().await;
}

#[tokio::test]
async fn denied_launch_and_denied_operation_never_dispatch() {
    let mut f = Fixture::new("normal");
    f.ctx.approver = Arc::new(RefuseWhenUnattended);
    assert!(
        f.action(json!({"action":"refresh","server":"fixture"}))
            .await
            .is_error
    );
    assert!(!f.log.exists());
    f.ctx.approver = Arc::new(worksmith::tools::approval::AutoApprove);
    f.start("echo").await;
    f.ctx.approver = Arc::new(RefuseWhenUnattended);
    assert!(f.call("echo", json!({})).await.is_error);
    assert_eq!(f.records().len(), 1);
    f.manager.shutdown().await;
}

#[tokio::test]
async fn crash_after_mutation_is_uncertain_and_not_replayed_after_refresh() {
    let f = Fixture::new("normal");
    f.start("mutate").await;
    let out = f.call("mutate", json!({"value":"once"})).await;
    assert!(
        out.is_error && out.content.contains("uncertain"),
        "{}",
        out.content
    );
    f.start("mutate").await;
    let out = f.call("mutate", json!({"value":"once"})).await;
    assert!(out.is_error && out.content.contains("will not be replayed"));
    assert_eq!(
        f.records().iter().filter(|v| v["call"] == "mutate").count(),
        1
    );
    f.manager.shutdown().await;
}

#[tokio::test]
async fn oversized_output_is_paged_without_reexecuting_and_handles_are_session_scoped() {
    let mut f = Fixture::new("normal");
    f.start("large").await;
    let out = f.call("large", json!({})).await;
    assert!(!out.is_error && out.content.len() < 8000);
    let handle = out
        .content
        .split("session handle ")
        .nth(1)
        .unwrap()
        .split('.')
        .next()
        .unwrap();
    let mut offset = 0;
    loop {
        let page = f
            .action(json!({"action":"read","handle":handle,"offset":offset}))
            .await;
        assert!(!page.is_error, "{}", page.content);
        if page.content.contains("END_OF_RESULT") {
            break;
        }
        offset = page
            .content
            .rsplit("next_offset=")
            .next()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .parse::<usize>()
            .unwrap();
    }
    // Adjacent offsets exercise both alignments of the fixture's repeated é.
    for offset in [23600, 23601, 24000, 24001] {
        let page = f
            .action(json!({"action":"read","handle":handle,"offset":offset}))
            .await;
        assert!(!page.is_error, "{}", page.content);
        assert!(page.content.contains("END_OF_RESULT"));
        assert!(page.content.contains("next_offset=null; eof=true"));
    }
    assert_eq!(
        f.records().iter().filter(|v| v["call"] == "large").count(),
        1
    );
    f.ctx.session_id = uuid::Uuid::new_v4().to_string();
    assert!(
        f.action(json!({"action":"read","handle":handle}))
            .await
            .is_error
    );
    f.manager.shutdown().await;
}

#[tokio::test]
async fn bad_catalogs_fail_with_bounded_work() {
    for mode in ["pagination_loop", "oversized_frame", "init_hang"] {
        let f = Fixture::new(mode);
        let out = f
            .action(json!({"action":"refresh","server":"fixture"}))
            .await;
        assert!(out.is_error, "{mode}: {}", out.content);
        assert_eq!(f.manager.definitions(&f.ctx).len(), 1);
        f.manager.shutdown().await;
    }
    let f = Fixture::new("bad_schema");
    assert!(
        !f.action(json!({"action":"refresh","server":"fixture"}))
            .await
            .is_error
    );
    assert!(
        f.action(json!({"action":"activate","tool":"mcp__fixture__echo"}))
            .await
            .is_error
    );
    f.manager.shutdown().await;
}

#[tokio::test]
async fn execution_errors_unsupported_content_and_timeouts_do_not_report_success() {
    for tool in ["error", "image", "hang"] {
        let f = Fixture::new("normal");
        f.start(tool).await;
        let out = f.call(tool, json!({})).await;
        assert!(out.is_error, "{tool}: {}", out.content);
        f.manager.shutdown().await;
    }
}

#[tokio::test]
async fn approval_cancellation_clears_wait_and_does_not_launch() {
    let mut f = Fixture::new("normal");
    let (approver, mut rx) = worksmith::tools::approval::ChannelApprover::new();
    f.ctx.approver = Arc::new(approver);
    let cancel = f.ctx.cancel.clone();
    let waiter = async {
        let _request = rx.recv().await.unwrap();
        assert!(f.ctx.awaiting_approval.load(Ordering::Relaxed));
        cancel.cancel();
    };
    let (out, ()) = tokio::join!(
        f.action(json!({"action":"refresh","server":"fixture"})),
        waiter
    );
    assert!(out.is_error);
    assert!(!f.ctx.awaiting_approval.load(Ordering::Relaxed));
    assert!(!f.log.exists());
}

struct Always(Mutex<usize>);
#[async_trait::async_trait]
impl Approver for Always {
    async fn ask(&self, _: &str, _: &str) -> Approval {
        *self.0.lock().unwrap() += 1;
        Approval::AlwaysThisSession
    }
}
#[tokio::test]
async fn session_grants_match_exact_scope_and_never_match_a_generic_reason() {
    let inner = Arc::new(Always(Mutex::new(0)));
    let approver = RememberingApprover::new(inner.clone());
    for scope in [
        "server_a:read:schema1:parent",
        "server_a:read:schema1:parent",
        "server_a:write:schema1:parent",
        "server_b:read:schema1:parent",
        "server_a:read:schema2:parent",
        "server_a:read:schema1:worker",
    ] {
        approver
            .ask_scoped("args", "same prose reason", scope)
            .await;
    }
    assert_eq!(*inner.0.lock().unwrap(), 5);
    approver.ask("args", "same prose reason").await;
    assert_eq!(*inner.0.lock().unwrap(), 6);
}

#[tokio::test]
async fn stale_catalog_and_old_request_definitions_cannot_dispatch() {
    let f = Fixture::new("normal");
    f.start("echo").await;
    let mut old = f
        .manager
        .definitions(&f.ctx)
        .into_iter()
        .find(|d| d.name.ends_with("__echo"))
        .unwrap();
    old.parameters = json!({"type":"object","properties":{"different":{"type":"integer"}}});
    assert!(
        f.manager
            .run(&old.name, json!({}), &f.ctx, Some(&old))
            .await
            .is_error
    );
    assert_eq!(f.records().len(), 1);
    f.action(json!({"action":"activate","tool":"mcp__fixture__change"}))
        .await;
    f.call("change", json!({})).await;
    let out = f.call("echo", json!({})).await;
    assert!(
        out.is_error && out.content.contains("stale"),
        "{}",
        out.content
    );
    assert!(
        f.action(json!({"action":"activate","tool":"mcp__fixture__echo"}))
            .await
            .is_error
    );
    f.manager.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn shutdown_and_timeout_reap_the_child() {
    for timeout in [false, true] {
        let f = Fixture::new("normal");
        f.start("hang").await;
        let pid = f.records()[0]["start"].as_u64().unwrap() as i32;
        if timeout {
            f.call("hang", json!({})).await;
        }
        f.manager.shutdown().await;
        // SAFETY: signal zero only checks whether the fixture process exists.
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "fixture process {pid} survived"
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
}

#[tokio::test]
async fn a_resumed_session_retains_uncertain_operation_blocks() {
    let mut f = Fixture::new("normal");
    let mut session = worksmith::session::Session::create(&f.ctx.cwd).unwrap();
    f.ctx.session_id = session.id.clone();
    session
        .append_event(&worksmith::event::Event::McpOperation {
            server: "fixture".into(),
            tool: Some("mutate".into()),
            phase: "dispatched".into(),
            revision: None,
            elapsed_ms: 0,
            result_bytes: 0,
        })
        .unwrap();
    f.start("mutate").await;
    assert!(
        f.call("mutate", json!({"value":"changed arguments"}))
            .await
            .content
            .contains("will not be replayed")
    );
    assert_eq!(f.records().len(), 1);
    f.manager.shutdown().await;
}

struct Script {
    requests: Arc<Mutex<Vec<worksmith::llm::ChatRequest>>>,
    responses: Mutex<std::collections::VecDeque<worksmith::llm::Completion>>,
}
#[async_trait::async_trait]
impl worksmith::llm::LlmClient for Script {
    async fn stream(
        &self,
        request: worksmith::llm::ChatRequest,
        _: tokio::sync::mpsc::Sender<worksmith::llm::StreamEvent>,
        _: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<worksmith::llm::Completion> {
        self.requests.lock().unwrap().push(request);
        Ok(self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default())
    }
}

#[tokio::test]
async fn real_agent_loop_records_mcp_evidence_and_keeps_discovery_out_of_initial_schemas() {
    use worksmith::{
        agent::Agent,
        event::{Event, EventBus},
        llm::{Completion, ToolCall},
        session::Session,
        tools::ToolRegistry,
    };
    let f = Fixture::new("normal");
    let mut session = Session::create(&f.ctx.cwd).unwrap();
    let responses = [
        ("mcp", json!({"action":"refresh","server":"fixture"})),
        (
            "mcp",
            json!({"action":"activate","tool":"mcp__fixture__echo"}),
        ),
        ("mcp__fixture__echo", json!({"value":"verified"})),
    ]
    .into_iter()
    .enumerate()
    .map(|(id, (name, args))| Completion {
        tool_calls: vec![ToolCall {
            id: id.to_string(),
            name: name.into(),
            arguments: args.to_string(),
        }],
        ..Completion::default()
    })
    .collect();
    let requests = Arc::new(Mutex::new(vec![]));
    let manager = Arc::new(f.manager);
    let registry = Arc::new(ToolRegistry::with_builtins().with_mcp(manager.clone()));
    let agent = Agent::new(
        Arc::new(Script {
            requests: requests.clone(),
            responses: Mutex::new(responses),
        }),
        registry,
        EventBus::new(),
        "fixture-model".into(),
        None,
        None,
        10,
        1,
        3,
        100_000,
        1,
        f.ctx.clone(),
    );
    agent
        .run_turn(
            &mut session,
            "use fixture",
            "Test system prompt",
            None,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    {
        let requests = requests.lock().unwrap();
        assert!(
            !requests[0]
                .tools
                .iter()
                .any(|d| d.name.starts_with("mcp__"))
        );
        assert!(
            !requests[1]
                .tools
                .iter()
                .any(|d| d.name.starts_with("mcp__"))
        );
        assert!(
            requests[2]
                .tools
                .iter()
                .any(|d| d.name == "mcp__fixture__echo")
        );
        assert!(requests.iter().all(|request| {
            !serde_json::to_string(&request.messages)
                .unwrap()
                .contains("UNTRUSTED_SERVER_INSTRUCTIONS")
        }));
    }
    let events = worksmith::session::events(session.path()).unwrap();
    assert!(events.iter().any(
        |item| matches!(&item.event, Event::McpOperation { phase, .. } if phase == "dispatched")
    ));
    assert!(events.iter().any(
        |item| matches!(&item.event, Event::McpOperation { phase, .. } if phase == "completed")
    ));
    assert!(
        events
            .iter()
            .filter(|item| matches!(&item.event, Event::McpOperation { .. }))
            .all(|item| !serde_json::to_string(&item.event)
                .unwrap()
                .contains("verified"))
    );
    let worker = agent.fork(EventBus::new(), "worker".into());
    assert!(
        !worker
            .advertised_tools()
            .iter()
            .any(|d| d.name == "mcp" || d.name.starts_with("mcp__"))
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn refreshed_catalogs_retire_session_grants_and_invalid_params_are_recoverable() {
    let mut f = Fixture::new("normal");
    let inner = Arc::new(Always(Mutex::new(0)));
    f.ctx.approver = Arc::new(RememberingApprover::new(inner.clone()));
    f.start("echo").await;
    assert!(!f.call("echo", json!({"value":"first"})).await.is_error);
    assert!(!f.call("echo", json!({"value":"different"})).await.is_error);
    assert_eq!(*inner.0.lock().unwrap(), 2);
    assert!(
        f.manager
            .browser(&f.ctx)
            .iter()
            .any(|item| item.name.ends_with("__echo")
                && item.detail.contains("allowed this session"))
    );
    f.start("echo").await;
    let out = f.call("echo", json!({"value":"invalid"})).await;
    assert!(out.is_error && out.content.contains("invalid arguments"));
    assert!(!out.content.contains("secret error data"));
    assert_eq!(*inner.0.lock().unwrap(), 3);
    assert!(!f.call("echo", json!({"value":"valid"})).await.is_error);
    f.manager.shutdown().await;
}

#[tokio::test]
async fn cancelled_calls_are_uncertain_and_disconnect_clears_activation() {
    let f = Fixture::new("normal");
    f.start("hang").await;
    let cancel = f.ctx.cancel.clone();
    let trigger = async {
        for _ in 0..100 {
            if f.records().iter().any(|value| value["call"] == "hang") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        cancel.cancel();
    };
    let (out, ()) = tokio::join!(f.call("hang", json!({})), trigger);
    assert!(out.is_error && out.content.contains("uncertain"));
    f.manager.shutdown().await;

    let f = Fixture::new("normal");
    f.start("echo").await;
    assert!(!f.action(json!({"action":"disconnect"})).await.is_error);
    assert_eq!(f.manager.definitions(&f.ctx).len(), 1);
    assert!(f.call("echo", json!({})).await.is_error);
    f.manager.shutdown().await;
}
