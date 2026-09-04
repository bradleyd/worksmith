//! Memory retrieval: FTS search with hybrid ranking, write-time dedup, and the
//! worker proposal flow (workers propose, they don't persist).

use worksmith::memory::{MemoryCaps, MemoryStore, Scope};

fn store(dir: &std::path::Path) -> MemoryStore {
    MemoryStore::open_paths(&dir.join("global.db"), Some(&dir.join("project.db"))).unwrap()
}

fn named_store(dir: &std::path::Path, project_name: &str) -> MemoryStore {
    MemoryStore::open_paths_with_project_name(
        &dir.join("global.db"),
        Some(&dir.join("project.db")),
        Some(vec![project_name.to_string()]),
    )
    .unwrap()
}

#[test]
fn search_finds_memories_by_words_not_just_subjects() {
    let dir = tempfile::tempdir().unwrap();
    let m = store(dir.path());
    m.remember(Scope::Global, "preference", "infrastructure", "Prefer simple local Rust components over distributed infrastructure.", 70).unwrap();
    m.remember(Scope::Project, "decision", "durable memory", "Use separate SQLite databases for global and project memory.", 80).unwrap();
    m.remember(Scope::Project, "lesson", "pdf extraction", "poppler beats pandoc for scanned PDFs.", 40).unwrap();

    let hits = m.search("sqlite databases", 5).unwrap();
    assert!(!hits.is_empty(), "should match on content words");
    assert_eq!(hits[0].row.subject, "durable memory");
    assert!(hits.iter().all(|h| h.score > 0.0));

    // Unrelated query doesn't drag everything back.
    let hits = m.search("kubernetes helm charts", 5).unwrap();
    assert!(hits.is_empty(), "no spurious matches, got {hits:?}");
}

#[test]
fn explicit_search_does_not_drop_turn_prompt_noise_words() {
    let dir = tempfile::tempdir().unwrap();
    let m = store(dir.path());
    m.remember(
        Scope::Project,
        "decision",
        "durable memory",
        "Use separate SQLite databases for global and project memory.",
        80,
    )
    .unwrap();

    let hits = m.search("memory", 5).unwrap();
    assert_eq!(
        hits.len(),
        1,
        "explicit search should honor the word memory"
    );
}

#[test]
fn an_exact_subject_hit_outranks_a_body_mention() {
    let dir = tempfile::tempdir().unwrap();
    let m = store(dir.path());
    m.remember(Scope::Project, "fact", "worker supervision", "The supervisor nudges then escalates.", 50).unwrap();
    m.remember(Scope::Project, "fact", "unrelated note", "Something about worker supervision in passing.", 50).unwrap();

    let hits = m.search("worker supervision", 5).unwrap();
    assert_eq!(hits[0].row.subject, "worker supervision", "exact subject wins");
}

#[test]
fn turn_context_uses_relevant_memory_with_context_scaled_caps() {
    let dir = tempfile::tempdir().unwrap();
    let m = store(dir.path());
    let rustfmt = m
        .remember(
            Scope::Project,
            "preference",
            "rust formatting",
            "Use targeted rustfmt, not broad cargo fmt, in worksmith.",
            80,
        )
        .unwrap();
    let commit = m
        .remember(
            Scope::Global,
            "preference",
            "commits",
            "Provide git commands when it is time to commit.",
            70,
        )
        .unwrap();
    m.remember(
        Scope::Project,
        "fact",
        "mud game",
        "Rooms have exits and inventory items.",
        90,
    )
    .unwrap();

    assert_eq!(
        MemoryCaps::for_context(8_192),
        MemoryCaps {
            max_items: 2,
            max_chars: 600,
            max_item_chars: 320,
        }
    );
    assert_eq!(MemoryCaps::for_context(128_000).max_items, 8);

    let ctx = m
        .turn_context(
            "make a rust formatting change and provide git commands",
            8_192,
        )
        .unwrap()
        .expect("matching memories");
    assert!(ctx.text.starts_with("Relevant memory for this turn:"));
    assert!(ctx.text.contains(&format!("project/preference/{}", &rustfmt.id[..8])));
    assert!(ctx.text.contains(&format!("global/preference/{}", &commit.id[..8])));
    assert!(ctx.ids.contains(&rustfmt.id));
    assert!(ctx.ids.contains(&commit.id));
    assert!(
        !ctx.text.contains("Rooms have exits"),
        "unrelated memory should not ride along: {}",
        ctx.text
    );
    assert!(ctx.text.len() <= 600, "small-window cap applies");
}

#[test]
fn turn_context_does_not_pad_weak_matches_to_fill_the_cap() {
    let dir = tempfile::tempdir().unwrap();
    let m = store(dir.path());
    let formatting = m
        .remember(
            Scope::Project,
            "preference",
            "formatting",
            "Use targeted rustfmt, not broad cargo fmt, in worksmith.",
            80,
        )
        .unwrap();
    m.remember(
        Scope::Project,
        "decision",
        "worksmith docs",
        "Docs live under docs/ as a Zola static site with templates and mermaid visuals.",
        95,
    )
    .unwrap();
    m.remember(
        Scope::Project,
        "lesson",
        "worksmith validation timeout",
        "If cargo appears stuck, check for orphaned cargo processes holding the build lock.",
        95,
    )
    .unwrap();

    let generic = m
        .turn_context("testing memory, make a small change in main.rs", 8_192)
        .unwrap();
    assert!(
        generic.is_none(),
        "a generic test prompt should not inject weak matches: {generic:?}"
    );

    let ctx = m
        .turn_context(
            "testing memory, make a small formatting change in main.rs",
            8_192,
        )
        .unwrap()
        .expect("the formatting preference is relevant");

    assert_eq!(ctx.ids, vec![formatting.id.clone()]);
    assert!(
        !ctx.text.contains("Zola") && !ctx.text.contains("orphaned cargo"),
        "weak high-importance matches should not fill unused memory slots: {}",
        ctx.text
    );
}

#[test]
fn turn_context_treats_the_project_name_as_noise() {
    let dir = tempfile::tempdir().unwrap();
    let m = named_store(dir.path(), "worksmith");
    let formatting = m
        .remember(
            Scope::Project,
            "preference",
            "formatting",
            "Use targeted rustfmt, not broad cargo fmt, in worksmith.",
            60,
        )
        .unwrap();
    m.remember(
        Scope::Project,
        "lesson",
        "worksmith validation 120s timeout root cause",
        "Worksmith validation is cargo test and cargo clippy; timeouts usually mean orphaned cargo processes.",
        75,
    )
    .unwrap();
    m.remember(
        Scope::Project,
        "decision",
        "worksmith docs: location and format",
        "Docs live in THIS repo under docs/ as markdown rendered by Zola.",
        75,
    )
    .unwrap();
    m.remember(
        Scope::Project,
        "decision",
        "worksmith docs: Zola site on GitHub Pages",
        "Docs live in THIS repo under docs/ as a Zola static site.",
        70,
    )
    .unwrap();
    m.remember(
        Scope::Project,
        "preference",
        "clippy-zero-warnings",
        "The repo standard is zero clippy warnings; keep cargo clippy clean.",
        70,
    )
    .unwrap();
    m.remember(
        Scope::Project,
        "fact",
        "TUI idle redraw",
        "The TUI event loop redraws too often while idle.",
        60,
    )
    .unwrap();

    let ctx = m
        .turn_context(
            "worksmith test: run a formatting check on src/main.rs",
            98_304,
        )
        .unwrap()
        .expect("the formatting preference should still match");

    assert_eq!(ctx.ids, vec![formatting.id.clone()]);
    assert!(
        !ctx.text.contains("Zola")
            && !ctx.text.contains("orphaned cargo")
            && !ctx.text.contains("clippy warnings")
            && !ctx.text.contains("TUI event loop"),
        "project-name-only matches should not be injected: {}",
        ctx.text
    );

    let search = m
        .search("worksmith test: run a formatting check on src/main.rs", 10)
        .unwrap();
    assert_eq!(
        search.first().map(|hit| hit.row.id.as_str()),
        Some(formatting.id.as_str()),
        "explicit search should rank the exact formatting hit first: {search:?}"
    );

    let hits = m.search("worksmith", 10).unwrap();
    assert!(
        hits.len() > 1,
        "explicit memory search should still honor the project name"
    );
}

#[test]
fn turn_context_is_deterministically_capped() {
    let dir = tempfile::tempdir().unwrap();
    let m = store(dir.path());
    for i in 0..20 {
        m.remember(
            Scope::Project,
            "lesson",
            &format!("rust lesson {i:02}"),
            "Rust changes should keep validation focused and avoid unrelated formatting churn.",
            50 + i,
        )
        .unwrap();
    }

    let caps = MemoryCaps {
        max_items: 3,
        max_chars: 260,
        max_item_chars: 80,
    };
    let first = m
        .turn_context_with_caps("rust validation formatting", caps)
        .unwrap()
        .expect("matching memories");
    let second = m
        .turn_context_with_caps("rust validation formatting", caps)
        .unwrap()
        .expect("matching memories");

    assert_eq!(first, second);
    assert!(first.ids.len() <= 3);
    assert!(first.text.len() <= 260);
}

#[test]
fn unrelated_turn_context_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let m = store(dir.path());
    m.remember(
        Scope::Project,
        "preference",
        "rust formatting",
        "Use targeted rustfmt.",
        80,
    )
    .unwrap();

    let ctx = m.turn_context("plan a hiking itinerary", 32_768).unwrap();
    assert!(ctx.is_none(), "unrelated memories should not be injected");
}

#[test]
fn stable_system_prompt_does_not_embed_memory() {
    let dir = tempfile::tempdir().unwrap();
    let m = store(dir.path());
    m.remember(
        Scope::Project,
        "preference",
        "formatting",
        "Use targeted rustfmt.",
        80,
    )
    .unwrap();

    let prompt = worksmith::prompt::build_system_prompt(dir.path(), &m);
    assert!(
        !prompt.contains("Use targeted rustfmt"),
        "memory belongs in the dynamic turn context, not the stable system prompt"
    );
    assert!(
        prompt.contains("MEMORY AND KNOWLEDGE"),
        "the stable prompt should still explain how to use memory"
    );
}

#[test]
fn duplicate_writes_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let m = store(dir.path());
    let (first, wrote) = m.remember_deduped(Scope::Project, "decision", "db", "Use SQLite.", 60).unwrap();
    assert!(wrote);
    // Same thing, different whitespace/case → not a new row.
    let (again, wrote) = m.remember_deduped(Scope::Project, "decision", "db", "use   sqlite.", 60).unwrap();
    assert!(!wrote, "a restatement must not grow the store");
    assert_eq!(first.id, again.id);
    assert_eq!(m.list(Some(Scope::Project)).unwrap().len(), 1);
}

#[test]
fn worker_proposals_wait_for_approval() {
    let dir = tempfile::tempdir().unwrap();
    let m = store(dir.path());
    let (row, wrote) = m.propose(Scope::Project, "lesson", "vllm", "Set max-tokens >= 4096.", 60).unwrap();
    assert!(wrote);
    assert_eq!(row.status, "proposed");

    // A proposal is not yet part of the agent's working memory.
    assert!(m.list(None).unwrap().is_empty(), "proposals are not active");
    assert!(m.search("vllm", 5).unwrap().is_empty(), "proposals are not searchable");
    assert_eq!(m.pending().unwrap().len(), 1);

    assert!(m.approve(&row.id).unwrap());
    assert_eq!(m.list(None).unwrap().len(), 1);
    assert!(m.pending().unwrap().is_empty());
    assert!(!m.approve("nope").unwrap(), "unknown id");

    // Rejection is just forgetting it.
    let (row2, _) = m.propose(Scope::Global, "fact", "x", "y", 50).unwrap();
    assert!(m.forget(&row2.id).unwrap());
    assert!(m.pending().unwrap().is_empty());
}

#[test]
fn extraction_output_parses_and_junk_is_dropped() {
    use worksmith::memory::parse_candidates;

    let text = "\
project|decision|knowledge index|Keep the knowledge index separate because it is rebuildable.|80
global|preference|formatting|Run cargo fmt before calling a task done.|70
not-a-scope|decision|x|y|50
project|nonsense-kind|x|y|50
project|fact||missing subject|50
this line is not a candidate at all
project|lesson|vllm max-tokens|A low max-tokens truncates file-writing tool calls.";

    let got = parse_candidates(text);
    assert_eq!(got.len(), 3, "only well-formed lines survive: {got:?}");
    assert_eq!(got[0].subject, "knowledge index");
    assert_eq!(got[0].importance, 80);
    assert_eq!(got[1].scope, Scope::Global);
    // Missing importance falls back to a sane default rather than dropping the row.
    assert_eq!(got[2].importance, 60);
    assert_eq!(got[2].kind, "lesson");

    // "Prefer zero memories" has to actually work.
    assert!(parse_candidates("NONE").is_empty());
    assert!(parse_candidates("").is_empty());
}
