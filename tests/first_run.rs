//! First-run behaviour, in its own test binary on purpose.
//!
//! This is the one test that must NOT share a `WORKSMITH_HOME` with anything
//! else: it points the variable at a directory that does not exist yet, and the
//! rest of the suite pins one shared scratch home per process via `OnceLock`.
//! Cargo gives each integration test file its own process, which is the
//! isolation this needs.

/// First run: `~/.worksmith` does not exist yet. Every downstream error names a
/// `config.toml` in that directory, so the directory has to exist and the
/// annotated reference has to be sitting in it.
#[test]
fn a_fresh_global_home_is_created_with_a_reference_config() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("never-created");
    assert!(!home.exists(), "precondition: the global dir is missing");
    unsafe { std::env::set_var(worksmith::config::GLOBAL_DIR_ENV, &home) };

    let project = tempfile::tempdir().unwrap();
    let cfg = worksmith::config::Config::load(project.path()).unwrap();

    assert!(home.is_dir(), "the global dir must be created on first load");
    let example = home.join(worksmith::config::EXAMPLE_CONFIG);
    assert!(example.is_file(), "an annotated example must be seeded beside it");
    let body = std::fs::read_to_string(&example).unwrap();
    assert!(body.contains("[providers."), "the example must show a provider section");

    // Seeding a reference is not the same as choosing a model: writing
    // config.toml would pick a provider on the user's behalf.
    assert!(!home.join("config.toml").exists(), "no config.toml is invented");

    // And the error a fresh user hits must name both paths, not just "config.toml".
    let err = format!("{:#}", cfg.resolve_model(None).unwrap_err());
    assert!(err.contains(&home.display().to_string()), "error names the real path: {err}");
    assert!(err.contains("config.example.toml"), "error points at the example: {err}");
}
