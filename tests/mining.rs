//! Memory mining: which past sessions get read, and what happens to what the
//! classifier returns.

mod common;

use worksmith::llm::Message;
use worksmith::memory::{MemoryStore, Scope};
use worksmith::mining::{MineReport, plan, record};
use worksmith::session::Session;

/// A session in `cwd` with `turns` user messages, written where the miner looks.
fn seed_session(cwd: &std::path::Path, turns: usize) -> String {
    let mut s = Session::create(cwd).unwrap();
    for i in 0..turns {
        s.append_message(Message::user(format!("question {i}"))).unwrap();
        s.append_message(Message::assistant(Some(format!("answer {i}")), vec![])).unwrap();
    }
    s.id.clone()
}

fn store(cwd: &std::path::Path) -> MemoryStore {
    MemoryStore::open(Some(cwd)).unwrap()
}

#[test]
fn only_this_projects_sessions_are_mined() {
    common::isolate_home();
    let mine_dir = tempfile::tempdir().unwrap();
    let other_dir = tempfile::tempdir().unwrap();

    seed_session(mine_dir.path(), 12);
    seed_session(other_dir.path(), 12);

    // A lesson learned in another repo is not this project's memory, and the
    // sessions directory is global — so the cwd filter is the whole safeguard.
    let p = plan(&store(mine_dir.path()), mine_dir.path(), 10).unwrap();
    assert_eq!(p.report.found, 1, "the other project's session must not appear");
    assert_eq!(p.items.len(), 1);
}

#[test]
fn short_sessions_are_skipped_and_never_reconsidered() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    seed_session(dir.path(), 2); // below the 10-user-message floor
    let mem = store(dir.path());

    let p = plan(&mem, dir.path(), 10).unwrap();
    assert!(p.items.is_empty(), "too short to be worth a model call");
    assert_eq!(p.report.too_short, 1);

    // A finished short session will not grow. Re-reading it every run is the
    // same wasted work forever, so it is marked as seen.
    let again = plan(&mem, dir.path(), 10).unwrap();
    assert_eq!(again.report.already_mined, 1);
    assert_eq!(again.report.too_short, 0);
}

#[test]
fn a_mined_session_is_not_mined_twice() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let id = seed_session(dir.path(), 12);
    let mem = store(dir.path());

    let p = plan(&mem, dir.path(), 10).unwrap();
    assert_eq!(p.items.len(), 1);

    let results = vec![(
        id,
        Ok("project|lesson|pandoc|Round-trip docx through pandoc, never edit XML by hand|70"
            .to_string()),
    )];
    let report = record(&mem, results, p.report);
    assert_eq!(report.proposed, 1);

    // Without this, every run re-proposes what you already rejected.
    let again = plan(&mem, dir.path(), 10).unwrap();
    assert!(again.items.is_empty());
    assert_eq!(again.report.already_mined, 1);
}

#[test]
fn mined_memories_are_proposals_in_project_scope() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let id = seed_session(dir.path(), 12);
    let mem = store(dir.path());
    let p = plan(&mem, dir.path(), 10).unwrap();

    // The classifier says "global"; mining overrides it. A session ran in this
    // project, so what it taught is this project's — a wrong guess here would
    // pollute every other repo.
    let results =
        vec![(id, Ok("global|preference|style|Prefers small commits|60".to_string()))];
    record(&mem, results, p.report);

    let pending = mem.pending().unwrap();
    assert_eq!(pending.len(), 1, "mined memories await approval, never go straight in");
    assert_eq!(pending[0].scope, Scope::Project.as_str());
    assert!(mem.list(Some(Scope::Global)).unwrap().is_empty(), "nothing leaked to global");
}

#[test]
fn a_failed_classification_is_reported_not_swallowed() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let id = seed_session(dir.path(), 12);
    let mem = store(dir.path());
    let p = plan(&mem, dir.path(), 10).unwrap();

    let results = vec![(id, Err("model returned no content".to_string()))];
    let report = record(&mem, results, p.report);

    assert_eq!(report.proposed, 0);
    assert_eq!(report.failed.len(), 1, "a silent failure is how an empty store looks healthy");
    assert!(report.failed[0].contains("no content"));
}

#[test]
fn the_limit_caps_how_many_sessions_one_run_reads() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    for _ in 0..5 {
        seed_session(dir.path(), 12);
    }

    // Each session read is a model call; an archive of a thousand must not be
    // one blocking command.
    let p = plan(&store(dir.path()), dir.path(), 2).unwrap();
    assert_eq!(p.report.found, 5);
    assert_eq!(p.items.len(), 2);
    assert_eq!(p.report.read, 2);
}

#[test]
fn an_empty_archive_says_so() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let p = plan(&store(dir.path()), dir.path(), 10).unwrap();
    assert_eq!(p.report, MineReport::default());
    assert!(p.report.summary().contains("no past sessions"));
}
