//! Seeding a new project's `.worksmith/`.
//!
//! Its own binary: `project_trust` points `WORKSMITH_HOME` at specific
//! directories, and that variable is process-wide. This test only needs the
//! shared scratch home, so it must not share a process with one that moves it.

mod common;

/// A new project should find out that a project config is possible, and what it
/// can set, without reading the source or the global example.
#[test]
fn a_new_project_gets_a_commented_sample_config() {
    common::isolate_home();

    let project = tempfile::tempdir().unwrap();
    let ws = project.path().join(".worksmith");
    assert!(!ws.exists());

    // Opening project memory is what creates the directory in practice.
    let _mem = worksmith::memory::MemoryStore::open(Some(project.path())).unwrap();

    let sample = ws.join(worksmith::config::EXAMPLE_CONFIG);
    assert!(sample.is_file(), "a sample lands beside the databases");
    let body = std::fs::read_to_string(&sample).unwrap();
    assert!(body.contains("[agents]"), "shows the worker settings: {body}");
    assert!(body.contains("base-url"), "shows how to add a local provider");
    assert!(body.contains("/trust"), "warns that a project config is asked about");

    // It is a sample, not a config: nothing is in effect until you copy it.
    assert!(!ws.join("config.toml").exists());
    let cfg = worksmith::config::Config::load(project.path()).unwrap();
    assert!(cfg.pending_trust.is_none(), "a sample must not trigger the trust prompt");

    // And it never overwrites a project that has already made its choices.
    std::fs::write(&sample, "# edited by hand\n").unwrap();
    let _mem2 = worksmith::memory::MemoryStore::open(Some(project.path())).unwrap();
    assert_eq!(std::fs::read_to_string(&sample).unwrap(), "# edited by hand\n");
}
