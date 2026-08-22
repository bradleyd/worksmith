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

/// No single tool result may take a fifth of the context window. A 25kB read
/// was 6300 tokens, five of them filled a 32k window, and compaction can only
/// drop such a message whole — after which the model reads it again.
#[tokio::test]
async fn an_oversized_result_is_capped_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let big = "x".repeat(40_000);
    std::fs::write(dir.path().join("big.txt"), &big).unwrap();

    let registry = worksmith::tools::ToolRegistry::with_builtins();
    let out = registry
        .run("read", serde_json::json!({"path": "big.txt"}), &ctx(dir.path()))
        .await;

    assert!(!out.is_error, "a big file is readable, just not all at once");
    assert!(
        out.content.len() < worksmith::tools::MAX_TOOL_RESULT_BYTES + 500,
        "capped: {} bytes",
        out.content.len()
    );
    // Silence would leave the model reasoning as if it had seen the end.
    assert!(out.content.contains("not shown"), "{}", &out.content[out.content.len() - 200..]);
    assert!(out.content.contains("offset"), "and points at the way to get the rest");
}

/// When capped content has headings, the notice names them — turning "read it
/// again" into "fetch the one section you need".
#[tokio::test]
async fn a_capped_read_of_structured_content_lists_its_headings() {
    let dir = tempfile::tempdir().unwrap();
    let mut doc = String::new();
    for i in 0..30 {
        doc.push_str(&format!("## Rule {i}\n{}\n", "prose ".repeat(120)));
    }
    std::fs::write(dir.path().join("rules.md"), &doc).unwrap();

    let registry = worksmith::tools::ToolRegistry::with_builtins();
    let out = registry
        .run("read", serde_json::json!({"path": "rules.md"}), &ctx(dir.path()))
        .await;

    assert!(out.content.contains("not shown"));
    assert!(out.content.contains("organized under these headings"), "{}", &out.content[out.content.len().saturating_sub(400)..]);
    assert!(out.content.contains("## Rule 0"));
}
