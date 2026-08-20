//! Tests for the `doc` tool. Engine-free paths run everywhere; the pandoc
//! round-trip is skipped when pandoc isn't installed.

use std::time::Duration;

use serde_json::json;
use worksmith::tools::{ToolContext, ToolRegistry};

fn ctx(dir: &std::path::Path) -> ToolContext {
    ToolContext {
        cwd: dir.to_path_buf(),
        session_id: "t".into(),
        bash_timeout: Duration::from_secs(60),
        is_worker: false,
        ..Default::default()
    }
}

fn have(bin: &str) -> bool {
    std::env::var("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

#[tokio::test]
async fn doc_read_plaintext_needs_no_engine() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.md"), "# Hi\n\nplain markdown\n").unwrap();

    let reg = ToolRegistry::with_builtins();
    let out = reg.run("doc", json!({ "action": "read", "path": "notes.md" }), &ctx(dir.path())).await;

    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("plain markdown"));
}

#[tokio::test]
async fn doc_read_offset_limit_pages_through_text() {
    let dir = tempfile::tempdir().unwrap();
    let body: String = (1..=100).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
    std::fs::write(dir.path().join("big.md"), &body).unwrap();

    let reg = ToolRegistry::with_builtins();
    let out = reg
        .run("doc", json!({ "action": "read", "path": "big.md", "offset": 10, "limit": 3 }), &ctx(dir.path()))
        .await;

    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("lines 10-12 of 100"), "header missing: {}", out.content);
    assert!(out.content.contains("line 10") && out.content.contains("line 12"));
    assert!(!out.content.contains("line 13"), "limit not respected");
    assert!(!out.content.contains("line 9"), "offset not respected");
}

#[tokio::test]
async fn doc_missing_action_errors() {
    let dir = tempfile::tempdir().unwrap();
    let reg = ToolRegistry::with_builtins();
    let out = reg.run("doc", json!({ "path": "x" }), &ctx(dir.path())).await;
    assert!(out.is_error);
    assert!(out.content.contains("action"));
}

#[tokio::test]
async fn doc_read_pdf_without_engine_gives_install_hint() {
    if have("pdftotext") || have("mutool") {
        eprintln!("skip: a PDF engine is installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.pdf"), b"%PDF-1.4 not really").unwrap();

    let reg = ToolRegistry::with_builtins();
    let out = reg.run("doc", json!({ "action": "read", "path": "f.pdf" }), &ctx(dir.path())).await;

    assert!(out.is_error);
    assert!(out.content.contains("poppler"), "should hint the install: {}", out.content);
}

#[tokio::test]
async fn doc_pandoc_round_trip_md_to_docx_and_back() {
    if !have("pandoc") {
        eprintln!("skip: pandoc not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("src.md"), "# Title\n\nHello from pandoc.\n").unwrap();

    let reg = ToolRegistry::with_builtins();

    // md -> docx (via `create`, which is convert)
    let out = reg
        .run("doc", json!({ "action": "create", "path": "src.md", "out": "out.docx" }), &ctx(dir.path()))
        .await;
    assert!(!out.is_error, "convert failed: {}", out.content);
    assert!(dir.path().join("out.docx").exists(), "docx not created");

    // docx -> text
    let back = reg
        .run("doc", json!({ "action": "read", "path": "out.docx", "format": "text" }), &ctx(dir.path()))
        .await;
    assert!(!back.is_error, "read failed: {}", back.content);
    assert!(back.content.contains("Hello from pandoc"), "round-trip lost text: {}", back.content);
}
