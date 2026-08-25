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

    // `discover` searches the *global* skills dir as well as the project, so
    // without this the assertion below reads whatever is in the developer's
    // own ~/.worksmith/skills — and passes or fails depending on whose machine
    // it runs on, and on which sibling test won the race to set the variable.
    common::isolate_home();

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

/// A skill is standing instruction, not conversation. Compaction deleted it and
/// the model reloaded the same 4kB pack eight times in one session, finally
/// tripping stuck detection on five identical `skill` calls in a row.
#[tokio::test]
async fn a_skill_loads_once_and_stays_loaded() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    write_skill(&skills, "book-writer", "how chapters are written", "ALWAYS use the outline");

    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext { cwd: dir.path().to_path_buf(), ..Default::default() };

    let first = registry.run("skill", json!({"name": "book-writer"}), &ctx).await;
    assert!(first.content.contains("ALWAYS use the outline"), "{}", first.content);

    let second = registry.run("skill", json!({"name": "book-writer"}), &ctx).await;
    assert!(!second.is_error, "asking twice is not an error");
    assert!(second.content.contains("already loaded"), "{}", second.content);
    assert!(
        !second.content.contains("ALWAYS use the outline"),
        "the body should not be served twice: {}",
        second.content
    );

    // Because it is pinned to the system prompt, where compaction cannot reach.
    let pinned = ctx.loaded_skills.lock().unwrap();
    assert_eq!(pinned.len(), 1);
    assert!(pinned[0].1.contains("ALWAYS use the outline"));
}

/// The two levels of progressive disclosure through the tool itself: loading
/// pins body + map, `section` fetches one slice as an ordinary (compactable)
/// result.
#[tokio::test]
async fn a_section_is_fetched_through_the_tool_without_loading_whole_files() {
    common::isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    let refs = skills.join("book-writer").join("references");
    std::fs::create_dir_all(&refs).unwrap();
    std::fs::write(
        skills.join("book-writer").join("SKILL.md"),
        "---\nname: book-writer\ndescription: chapters\n---\nRead rules before drafting.",
    )
    .unwrap();
    std::fs::write(
        refs.join("writing-rules.md"),
        "# Writing rules\n\n## 8. Writing Style Rules\nShort sentences win.\n\n\
         ## 9. Handling Special Characters\nEscape angle brackets.\n",
    )
    .unwrap();

    let registry = ToolRegistry::with_builtins();
    let ctx = ToolContext { cwd: dir.path().to_path_buf(), ..Default::default() };

    // Load: the pinned text carries the map, so the model knows what exists.
    let load = registry.run("skill", json!({"name": "book-writer"}), &ctx).await;
    assert!(load.content.contains("<skill-map"), "{}", load.content);
    assert!(load.content.contains("## 8. Writing Style Rules"));
    assert!(
        !load.content.contains("Short sentences win"),
        "the map lists sections; it must not carry their content"
    );

    // Fetch: one section, named by file, tiny.
    let one = registry
        .run("skill", json!({"name": "book-writer", "section": "writing style"}), &ctx)
        .await;
    assert!(!one.is_error, "{}", one.content);
    assert!(one.content.contains("Short sentences win"));
    assert!(one.content.contains("writing-rules.md"), "names where it landed");
    assert!(!one.content.contains("Escape angle brackets"), "and only that section");

    // A miss teaches, not just fails: the map and the grep fallback.
    let miss = registry
        .run("skill", json!({"name": "book-writer", "section": "nonexistent"}), &ctx)
        .await;
    assert!(miss.is_error);
    assert!(miss.content.contains("<skill-map"), "{}", miss.content);
}

/// The two real installed skills are the fixtures the plan calls for: their
/// references are heading-structured markdown, so both must produce a sane,
/// bounded map. Runs only where they exist (a dev machine, not CI), and builds
/// the `Skill` by hand — calling discovery here would touch the process-wide
/// config cache before other tests isolate HOME.
#[test]
fn the_installed_skills_produce_sane_maps() {
    for name in ["book-writer", "chapter-editor"] {
        let dir = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join(".worksmith/skills")
            .join(name);
        if !dir.exists() {
            continue;
        }
        let skill = worksmith::skill::Skill {
            name: name.into(),
            description: String::new(),
            path: dir.join("SKILL.md"),
            dir,
            allowed_tools: None,
        };
        let map = skill.map();
        assert!(!map.is_empty(), "{name} has heading-structured references");
        assert!(
            map.len() <= worksmith::skill::MAX_MAP_CHARS + 200,
            "{name} map is bounded: {} chars",
            map.len()
        );
        assert!(map.contains("references/"), "{name}: {map}");
    }
}
