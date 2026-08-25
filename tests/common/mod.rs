//! Shared test setup.
//!
//! Sessions and global memory live under `~/.worksmith` by default, so a test
//! run would otherwise write scratch sessions into the developer's real one.
//! Pointing `WORKSMITH_HOME` at a per-process scratch dir keeps the suite
//! self-contained. Every test that touches anything resolved against that
//! directory calls [`isolate_home`] first — sessions and global memory, but
//! also the global skills dir, which `SkillCatalog::discover` searches
//! alongside the project. Reading that one unisolated made a test's result
//! depend on whose machine it ran on.

use std::path::PathBuf;
use std::sync::OnceLock;

static HOME: OnceLock<PathBuf> = OnceLock::new();

/// Point `WORKSMITH_HOME` at a scratch directory for this test process.
/// Idempotent, and safe to call from every test: `OnceLock` makes later
/// callers wait until the variable is actually set.
pub fn isolate_home() -> &'static PathBuf {
    HOME.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("worksmith-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creating the test home directory");
        // SAFETY: the OnceLock serializes this, and tests call it before
        // touching anything that reads the variable.
        unsafe { std::env::set_var(worksmith::config::GLOBAL_DIR_ENV, &dir) };
        dir
    })
}
