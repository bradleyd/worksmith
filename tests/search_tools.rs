//! The `grep` tool on ripgrep's walker: .gitignore, glob, case, and output
//! that is the same on every run.

use serde_json::json;
use worksmith::tools::{ToolContext, ToolRegistry};

fn ctx(dir: &std::path::Path) -> ToolContext {
    ToolContext {
        cwd: dir.to_path_buf(),
        session_id: "t".into(),
        ..Default::default()
    }
}

fn write(dir: &std::path::Path, name: &str, body: &str) {
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

#[tokio::test]
async fn grep_honours_gitignore_outside_a_git_repo() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), ".gitignore", "generated/\n");
    write(dir.path(), "src/lib.rs", "fn needle() {}\n");
    write(dir.path(), "generated/out.rs", "fn needle() {}\n");
    write(dir.path(), "target/debug/x.rs", "fn needle() {}\n");

    let out = ToolRegistry::with_builtins()
        .run("grep", json!({ "pattern": "needle" }), &ctx(dir.path()))
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(out.content.trim(), "src/lib.rs:1:fn needle() {}");
}

#[tokio::test]
async fn grep_filters_by_glob_and_case() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "a.rs", "let Refund = 1;\n");
    write(dir.path(), "b.py", "refund = 1\n");
    let reg = ToolRegistry::with_builtins();

    let out = reg
        .run(
            "grep",
            json!({ "pattern": "refund", "glob": "*.rs", "ignore_case": true }),
            &ctx(dir.path()),
        )
        .await;
    assert_eq!(out.content.trim(), "a.rs:1:let Refund = 1;");

    let out = reg
        .run("grep", json!({ "pattern": "refund" }), &ctx(dir.path()))
        .await;
    assert_eq!(
        out.content.trim(),
        "b.py:1:refund = 1",
        "case-sensitive by default"
    );

    let out = reg
        .run(
            "grep",
            json!({ "pattern": "x", "glob": "[" }),
            &ctx(dir.path()),
        )
        .await;
    assert!(
        out.is_error && out.content.contains("invalid glob"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn grep_output_is_ordered_the_same_every_time() {
    // The walk is parallel; the model must still see a stable listing, or a
    // repeated search looks like a changed tree.
    let dir = tempfile::tempdir().unwrap();
    for i in 0..40 {
        write(
            dir.path(),
            &format!("d{}/f{i:02}.txt", i % 7),
            "hit\nmiss\nhit\n",
        );
    }
    let reg = ToolRegistry::with_builtins();
    let first = reg
        .run("grep", json!({ "pattern": "hit" }), &ctx(dir.path()))
        .await
        .content;
    for _ in 0..5 {
        let again = reg
            .run("grep", json!({ "pattern": "hit" }), &ctx(dir.path()))
            .await
            .content;
        assert_eq!(first, again);
    }
    let lines: Vec<&str> = first.lines().collect();
    assert_eq!(lines.len(), 80);
    let mut sorted = lines.clone();
    sorted.sort();
    assert_eq!(lines, sorted);
}

#[tokio::test]
async fn grep_on_a_single_file_searches_that_file() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "notes.md", "one\ntwo needle\n");
    let out = ToolRegistry::with_builtins()
        .run(
            "grep",
            json!({ "pattern": "needle", "path": "notes.md" }),
            &ctx(dir.path()),
        )
        .await;
    assert_eq!(out.content.trim(), "notes.md:2:two needle");
}
