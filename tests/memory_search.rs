//! Memory retrieval: FTS search with hybrid ranking, write-time dedup, and the
//! worker proposal flow (workers propose, they don't persist).

use worksmith::memory::{MemoryStore, Scope};

fn store(dir: &std::path::Path) -> MemoryStore {
    MemoryStore::open_paths(&dir.join("global.db"), Some(&dir.join("project.db"))).unwrap()
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
fn an_exact_subject_hit_outranks_a_body_mention() {
    let dir = tempfile::tempdir().unwrap();
    let m = store(dir.path());
    m.remember(Scope::Project, "fact", "worker supervision", "The supervisor nudges then escalates.", 50).unwrap();
    m.remember(Scope::Project, "fact", "unrelated note", "Something about worker supervision in passing.", 50).unwrap();

    let hits = m.search("worker supervision", 5).unwrap();
    assert_eq!(hits[0].row.subject, "worker supervision", "exact subject wins");
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
