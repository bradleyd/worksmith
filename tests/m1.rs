//! M1 integration tests for the non-LLM logic: edit unique-match semantics,
//! memory CRUD/supersede, config merge, and session round-trip.

mod common;

use std::time::Duration;

use serde_json::json;
use worksmith::config::Config;
use worksmith::llm::Message;
use worksmith::memory::{MemoryStore, Scope};
use worksmith::session::Session;
use worksmith::tools::{ToolContext, ToolRegistry};

fn ctx(dir: &std::path::Path) -> ToolContext {
    ToolContext {
        cwd: dir.to_path_buf(),
        session_id: "test".to_string(),
        bash_timeout: Duration::from_secs(10),
        is_worker: false,
        ..Default::default()
    }
}

#[tokio::test]
async fn edit_unique_match_replaces() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    std::fs::write(&file, "hello world\n").unwrap();

    let reg = ToolRegistry::with_builtins();
    let out = reg
        .run(
            "edit",
            json!({ "path": "a.txt", "old_string": "world", "new_string": "there" }),
            &ctx(dir.path()),
        )
        .await;

    assert!(!out.is_error, "expected success: {}", out.content);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello there\n");
    // Output is a unified diff of the change.
    assert!(out.content.contains("edited"), "no summary: {}", out.content);
    assert!(out.content.contains("-hello world"), "no removed line: {}", out.content);
    assert!(out.content.contains("+hello there"), "no added line: {}", out.content);
}

#[tokio::test]
async fn edit_ambiguous_match_is_error() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    std::fs::write(&file, "x\nx\n").unwrap();

    let reg = ToolRegistry::with_builtins();
    let out = reg
        .run("edit", json!({ "path": "a.txt", "old_string": "x", "new_string": "y" }), &ctx(dir.path()))
        .await;

    assert!(out.is_error, "ambiguous match should error");
    // File is unchanged when an edit fails.
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "x\nx\n");
}

#[tokio::test]
async fn edit_not_found_is_error() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    std::fs::write(&file, "abc\n").unwrap();

    let reg = ToolRegistry::with_builtins();
    let out = reg
        .run("edit", json!({ "path": "a.txt", "old_string": "zzz", "new_string": "y" }), &ctx(dir.path()))
        .await;

    assert!(out.is_error, "missing old_string should error");
}

#[tokio::test]
async fn edit_replace_all() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    std::fs::write(&file, "x x x\n").unwrap();

    let reg = ToolRegistry::with_builtins();
    let out = reg
        .run(
            "edit",
            json!({ "path": "a.txt", "old_string": "x", "new_string": "y", "replace_all": true }),
            &ctx(dir.path()),
        )
        .await;

    assert!(!out.is_error, "{}", out.content);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "y y y\n");
}

#[tokio::test]
async fn edit_multiple_atomic() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    std::fs::write(&file, "one two three\n").unwrap();

    let reg = ToolRegistry::with_builtins();
    // Second edit is not found → whole call fails, file untouched.
    let out = reg
        .run(
            "edit",
            json!({ "path": "a.txt", "edits": [
                { "old_string": "one", "new_string": "1" },
                { "old_string": "MISSING", "new_string": "x" }
            ]}),
            &ctx(dir.path()),
        )
        .await;

    assert!(out.is_error);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "one two three\n");
}

#[test]
fn memory_crud_and_supersede() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("global.db");
    let project = dir.path().join("project.db");
    let store = MemoryStore::open_paths(&global, Some(&project)).unwrap();

    let row = store
        .remember(Scope::Project, "decision", "memory.storage", "Use SQLite for v1.", 80)
        .unwrap();
    assert_eq!(row.status, "active");

    let found = store.get_by_subject("memory.storage").unwrap();
    assert_eq!(found.len(), 1);

    // Supersede replaces meaning without dropping history.
    let new = store
        .supersede(
            Scope::Project,
            &row.id,
            "decision",
            "memory.storage",
            "Use SQLite with WAL for v1.",
            85,
        )
        .unwrap();
    assert_eq!(new.supersedes_id.as_deref(), Some(row.id.as_str()));

    let old = store.get(&row.id).unwrap().unwrap();
    assert_eq!(old.status, "superseded");

    // Only the active (new) row shows up for the subject.
    let active = store.get_by_subject("memory.storage").unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].id, new.id);

    assert!(store.forget(&new.id).unwrap());
    assert!(!store.forget("nonexistent").unwrap());
}

#[test]
fn memory_rejects_bad_kind() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let store =
        MemoryStore::open_paths(&dir.path().join("g.db"), Some(&dir.path().join("p.db"))).unwrap();
    assert!(store.remember(Scope::Global, "notakind", "s", "c", 50).is_err());
}

#[test]
fn config_project_overrides_global() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    // A project config that sets a model + provider.
    let wsdir = dir.path().join(".worksmith");
    std::fs::create_dir_all(&wsdir).unwrap();
    std::fs::write(
        wsdir.join("config.toml"),
        r#"
model = "vllm/qwen3"
temperature = 0.3

[providers.vllm]
type = "openai-compat"
base-url = "http://localhost:8000/v1"
"#,
    )
    .unwrap();

    // `load_trusted`: these exercise config merging, not the trust prompt.
    let cfg = Config::load_trusted(dir.path()).unwrap();
    let resolved = cfg.resolve_model(None).unwrap();
    assert_eq!(resolved.model, "qwen3");
    assert_eq!(resolved.provider.base_url, "http://localhost:8000/v1");
    assert_eq!(cfg.temperature, Some(0.3));

    // CLI override wins over config.
    let resolved2 = cfg.resolve_model(Some("vllm/other-model")).unwrap();
    assert_eq!(resolved2.model, "other-model");
}

#[test]
fn session_round_trip() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");

    {
        let mut s = Session::create_at(&path, dir.path()).unwrap();
        s.append_message(Message::user("hello")).unwrap();
        s.append_message(Message::assistant(Some("hi there".into()), vec![])).unwrap();
        assert_eq!(s.messages().len(), 2);
    }

    let reopened = Session::open(&path).unwrap();
    assert_eq!(reopened.messages().len(), 2);
    assert_eq!(reopened.messages()[0].content.as_deref(), Some("hello"));
    assert_eq!(reopened.cwd(), dir.path().display().to_string());
}

#[test]
fn a_mistyped_config_section_fails_loudly() {
    common::isolate_home();
    // `[agent]` (the loop) vs `[agents]` (the workers) is one character apart,
    // and silently ignoring the wrong one cost a whole dogfooding session:
    // every supervisor setting was dropped without a word.
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join(".worksmith");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("config.toml"),
        "model = \"p/m\"\n[providers.p]\nbase-url = \"http://h\"\n\
         [agent]\nsupervisor = \"rules\"\n",
    )
    .unwrap();

    let err = worksmith::config::Config::load_trusted(dir.path()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("supervisor"), "the error must name the offending key: {msg}");
}

#[test]
fn a_correct_config_still_loads() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join(".worksmith");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("config.toml"),
        "model = \"p/m\"\n[providers.p]\nbase-url = \"http://h\"\n\
         [agents]\nsupervisor = \"rules\"\nmax-nudges = 2\nmodel = \"p/small\"\n",
    )
    .unwrap();

    let c = worksmith::config::Config::load_trusted(dir.path()).unwrap();
    assert_eq!(c.supervisor().max_nudges, 2);
    assert_eq!(c.agents_model(), Some("p/small"));
}

/// `thinking` takes a mode or a token budget. TOML hands us a string in one case
/// and an integer in the other, and a budget that only worked when quoted would
/// be a trap.
#[test]
fn thinking_accepts_a_mode_or_a_budget() {
    use worksmith::llm::Thinking;

    let load = |line: &str| {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".worksmith");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("config.toml"),
            format!("model = \"p/m\"\n[providers.p]\nbase-url = \"http://h\"\n[agent]\n{line}\n"),
        )
        .unwrap();
        worksmith::config::Config::load_trusted(dir.path()).unwrap().thinking()
    };

    common::isolate_home();
    assert_eq!(load("thinking = \"off\""), Some(Thinking::Off));
    assert_eq!(load("thinking = \"on\""), Some(Thinking::On));
    assert_eq!(load("thinking = 2000"), Some(Thinking::Budget(2000)));
    assert_eq!(load("thinking = \"2000\""), Some(Thinking::Budget(2000)));
    // Unset means "send nothing at all", which is what keeps strict providers
    // working — not a silent default of on.
    assert_eq!(load("max-steps = 5"), None);
}

/// `api-key-env` naming a variable nobody exported sends the request with no
/// Authorization header. The server answers 401 and the cause looks like
/// anything except "you forgot to export it".
#[test]
fn a_named_but_unset_api_key_env_is_reported() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join(".worksmith");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("config.toml"),
        "model = \"local/m\"\n[providers.local]\nbase-url = \"http://127.0.0.1:8000/v1\"\n\
         api-key-env = \"WS_TEST_KEY_DEFINITELY_UNSET\"\n",
    )
    .unwrap();

    let c = worksmith::config::Config::load_trusted(dir.path()).unwrap();
    let resolved = c.resolve_model(None).unwrap();
    assert!(resolved.api_key.is_none());
    assert_eq!(
        resolved.missing_key_env.as_deref(),
        Some("WS_TEST_KEY_DEFINITELY_UNSET"),
        "the caller needs to be able to say which variable"
    );

    // Omitting api-key-env entirely is the normal local-server case and must
    // not warn about anything.
    std::fs::write(
        cfg.join("config.toml"),
        "model = \"local/m\"\n[providers.local]\nbase-url = \"http://127.0.0.1:8000/v1\"\n",
    )
    .unwrap();
    let c = worksmith::config::Config::load_trusted(dir.path()).unwrap();
    let resolved = c.resolve_model(None).unwrap();
    assert!(resolved.missing_key_env.is_none());
}

/// `sort` is OpenRouter's provider routing. A typo there would silently route
/// on price forever, so it fails at config time instead.
#[test]
fn an_unknown_provider_sort_is_rejected() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join(".worksmith");
    std::fs::create_dir_all(&cfg).unwrap();
    let write = |sort: &str| {
        std::fs::write(
            cfg.join("config.toml"),
            format!(
                "model = \"p/m\"\n[providers.p]\nbase-url = \"http://h\"\nsort = \"{sort}\"\n"
            ),
        )
        .unwrap();
    };

    write("fastest");
    let c = worksmith::config::Config::load_trusted(dir.path()).unwrap();
    let err = format!("{:#}", c.resolve_model(None).unwrap_err());
    assert!(err.contains("throughput"), "names the valid values: {err}");

    for ok in ["throughput", "latency", "price"] {
        write(ok);
        let c = worksmith::config::Config::load_trusted(dir.path()).unwrap();
        assert!(c.resolve_model(None).is_ok(), "{ok} should be accepted");
    }
}

/// One table keyed by `provider/model`, because three things want the same key:
/// what a model costs, how it wants to be sampled, and (later) which models
/// `/model` offers.
#[test]
fn per_model_settings_carry_prices_and_sampling() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join(".worksmith");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("config.toml"),
        "model = \"openrouter/qwen/qwen3.8-27b\"\ntemperature = 0.7\n\
         [providers.openrouter]\nbase-url = \"https://openrouter.ai/api/v1\"\n\
         [models.\"openrouter/qwen/qwen3.8-27b\"]\n\
         input = 0.20\noutput = 0.80\ntemperature = 0.6\ntop-k = 20\n",
    )
    .unwrap();

    let c = worksmith::config::Config::load_trusted(dir.path()).unwrap();
    let r = c.resolve_model(None).unwrap();
    assert_eq!(r.settings.temperature, Some(0.6), "the model's number beats the global one");
    assert_eq!(r.settings.top_k, Some(20));
    assert_eq!(r.settings.top_p, None, "unset stays unset, leaving the server's default alone");

    // 1M in + 1M out at 0.20/0.80.
    assert_eq!(r.settings.cost(1_000_000, 1_000_000), Some(1.0));
    assert_eq!(r.settings.cost(500_000, 0), Some(0.10));

    // A model with no entry is not free, it is unknown: saying $0.00 would be a
    // claim rather than a fact.
    let unknown = c.resolve_model(Some("openrouter/something/else")).unwrap();
    assert_eq!(unknown.settings, worksmith::config::ModelSettings::default());
    assert_eq!(unknown.settings.cost(1_000_000, 1_000_000), None);
}

/// `deny_unknown_fields` catches typos, and reads identically when the config is
/// simply newer than the binary. That happens on every upgrade where the config
/// moves first, and the message must not send people to edit a correct file.
#[test]
fn an_unknown_field_suggests_a_stale_binary() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join(".worksmith");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("config.toml"),
        "model = \"p/m\"\n[providers.p]\nbase-url = \"http://h\"\n\
         [some_future_section]\nwhatever = 1\n",
    )
    .unwrap();

    let err = format!("{:#}", worksmith::config::Config::load_trusted(dir.path()).unwrap_err());
    assert!(err.contains("unknown field"), "still says what TOML said: {err}");
    assert!(err.contains("ahead of the"), "offers the other explanation: {err}");
    assert!(
        err.contains(env!("CARGO_PKG_VERSION")),
        "names this build's version so the skew is checkable: {err}"
    );
    assert!(err.contains("which -a worksmith"), "and how to find the stale copy: {err}");
}
