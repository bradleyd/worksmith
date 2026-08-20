//! Skills end to end: discovery across the standard and worksmith locations,
//! what reaches the prompt, and what the `skill` tool hands back.

mod common;

use std::path::Path;

use serde_json::json;
use worksmith::memory::MemoryStore;
use worksmith::prompt::build_system_prompt;
use worksmith::skill::SkillCatalog;
use worksmith::tools::{ToolContext, ToolRegistry};

fn write_skill(root: &Path, name: &str, description: &str, body: &str) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"),
    )
    .unwrap();
}

fn store(dir: &Path) -> MemoryStore {
    MemoryStore::open_paths(&dir.join("g.db"), Some(&dir.join("p.db"))).unwrap()
}

#[test]
fn only_the_description_reaches_the_prompt() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    write_skill(
        &dir.path().join(".worksmith").join("skills"),
        "newsletter",
        "Writing and reviewing the dispatch newsletter.",
        "# Newsletter\n\nSECRET_BODY_MARKER: the full instructions.",
    );

    let prompt = build_system_prompt(dir.path(), &store(dir.path()));
    assert!(prompt.contains("<SKILLS>"), "catalog block missing:\n{prompt}");
    assert!(prompt.contains("newsletter: Writing and reviewing the dispatch newsletter."));
    assert!(
        !prompt.contains("SECRET_BODY_MARKER"),
        "the body must stay out of the prompt — that's the whole point of the catalog"
    );
}

#[test]
fn no_skills_means_no_block_at_all() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let prompt = build_system_prompt(dir.path(), &store(dir.path()));
    assert!(!prompt.contains("<SKILLS>"), "a project with no skills should pay nothing");
}

#[test]
fn the_standard_location_is_searched_and_the_project_wins() {
    common::isolate_home();
    let home = common::isolate_home().clone();
    let dir = tempfile::tempdir().unwrap();

    // A skill shared with every other tool that reads ~/.claude/skills.
    // `isolate_home` only moves WORKSMITH_HOME, so write into the real layout
    // under a temp HOME-like root by using the project-level standard path too.
    write_skill(&dir.path().join(".claude").join("skills"), "shared", "the shared one", "body A");
    let cat = SkillCatalog::discover(dir.path());
    assert!(cat.get("shared").is_some(), "a .claude/skills skill must be found");

    // The worksmith-specific copy overrides it, and says so.
    write_skill(
        &dir.path().join(".worksmith").join("skills"),
        "shared",
        "the worksmith one",
        "body B",
    );
    let cat = SkillCatalog::discover(dir.path());
    assert_eq!(cat.get("shared").unwrap().description, "the worksmith one");
    assert!(cat.notes().iter().any(|n| n.contains("overrides")), "{:?}", cat.notes());
    assert!(home.exists());
}

#[tokio::test]
async fn the_tool_returns_the_body_and_where_it_lives() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    write_skill(
        &dir.path().join(".worksmith").join("skills"),
        "newsletter",
        "Writing the dispatch.",
        "Read references/style-guide.md before drafting.",
    );

    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext {
        cwd: dir.path().to_path_buf(),
        session_id: "s".into(),
        bash_timeout: std::time::Duration::from_secs(5),
        is_worker: false,
        ..Default::default()
    };

    let out = registry.run("skill", json!({"name": "newsletter"}), &ctx).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("Read references/style-guide.md"), "body: {}", out.content);
    assert!(
        out.content.contains(".worksmith/skills/newsletter"),
        "must name the directory so references/ resolves: {}",
        out.content
    );

    // Listing works with no name.
    let out = registry.run("skill", json!({}), &ctx).await;
    assert!(out.content.contains("newsletter"), "{}", out.content);

    // An unknown name is an error with the list, not a panic.
    let out = registry.run("skill", json!({"name": "nope"}), &ctx).await;
    assert!(out.is_error);
    assert!(out.content.contains("no skill named"), "{}", out.content);
}

#[test]
fn the_bundled_docs_skill_satisfies_our_own_loader() {
    common::isolate_home();
    // It shipped without frontmatter and would have failed to load in the very
    // first release of the loader.
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let cat = SkillCatalog::discover(repo);
    let docs = cat.get("docs").expect("bundled docs skill should load");
    assert!(docs.description.to_lowercase().contains("doc"));
    assert!(docs.body().unwrap().contains("doc read"));
}

/// A skill in the wrong place is found by nothing and reported by nothing. The
/// common mistake is a skill directory sitting beside the project rather than
/// under `skills/`, which is what a newcomer does.
#[test]
fn a_misplaced_skill_can_be_pointed_at() {
    use worksmith::skill::SkillCatalog;

    let dir = tempfile::tempdir().unwrap();
    write_skill(dir.path(), "bluecollar-newsletter", "house style", "the guide");
    let stray = dir.path().join("bluecollar-newsletter");

    assert!(SkillCatalog::discover(dir.path()).is_empty(), "not in a skills/ dir, so not loaded");

    let found = SkillCatalog::misplaced(dir.path());
    assert_eq!(found.len(), 1, "but it can be spotted: {found:?}");
    assert!(found[0].ends_with("bluecollar-newsletter/SKILL.md"));

    // Every search path is reportable, so the empty case can say where it looked.
    let searched = SkillCatalog::searched(dir.path());
    assert!(searched.iter().any(|(p, _)| p.ends_with("skills")));
    assert!(searched.iter().all(|(p, exists)| *exists == p.exists()));

    // And once it is in the right place, it loads and is no longer "misplaced".
    let proper = dir.path().join("skills");
    std::fs::create_dir_all(&proper).unwrap();
    std::fs::rename(&stray, proper.join("bluecollar-newsletter")).unwrap();
    assert!(!SkillCatalog::discover(dir.path()).is_empty());
    assert!(SkillCatalog::misplaced(dir.path()).is_empty());
}
