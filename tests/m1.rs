//! M1 integration tests for the non-LLM logic: edit unique-match semantics,
//! memory CRUD/supersede, config merge, and session round-trip.

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
    }
}

#[tokio::test]
async fn edit_unique_match_replaces() {
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
}

#[tokio::test]
async fn edit_ambiguous_match_is_error() {
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
    let dir = tempfile::tempdir().unwrap();
    let store =
        MemoryStore::open_paths(&dir.path().join("g.db"), Some(&dir.path().join("p.db"))).unwrap();
    assert!(store.remember(Scope::Global, "notakind", "s", "c", 50).is_err());
}

#[test]
fn config_project_overrides_global() {
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

    let cfg = Config::load(dir.path()).unwrap();
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
