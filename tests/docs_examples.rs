//! Every config example in the docs is held to the real parser.
//!
//! Examples in docs are the first thing to drift: a key is renamed in the code
//! and the page keeps the old spelling, and the reader finds out when their
//! copied config refuses to load. `deny_unknown_fields` already makes a stale
//! key an error, so the check is cheap: pull out every ```toml block and parse
//! it, plus the two semantic checks parsing cannot do (a `thinking` word that
//! resolves to nothing is silently ignored, and a `model` can name a provider
//! the block never defines). Skill examples are installed and discovered.

mod common;

use std::path::{Path, PathBuf};

use worksmith::config::Config;
use worksmith::skill::SkillCatalog;

/// A fenced block: where it came from, its info string, and its body.
struct Block {
    origin: String,
    lang: String,
    body: String,
}

fn markdown_files() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = vec![root.join("README.md")];
    let mut dirs = vec![root.join("docs")];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            // `public/` is Zola's build output, a copy of the same pages.
            if path.is_dir() && path.file_name().is_some_and(|n| n != "public") {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "md") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn blocks(path: &Path) -> Vec<Block> {
    let text = std::fs::read_to_string(path).unwrap();
    let mut out = Vec::new();
    let mut open: Option<(usize, String, String)> = None;
    for (i, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        match &mut open {
            None => {
                if let Some(lang) = trimmed.strip_prefix("```")
                    && !lang.trim().is_empty()
                {
                    open = Some((i + 1, lang.trim().to_string(), String::new()));
                }
            }
            Some((start, lang, body)) => {
                if trimmed.starts_with("```") {
                    out.push(Block {
                        origin: format!("{}:{start}", path.display()),
                        lang: std::mem::take(lang),
                        body: std::mem::take(body),
                    });
                    open = None;
                } else {
                    body.push_str(line);
                    body.push('\n');
                }
            }
        }
    }
    out
}

fn all_blocks(lang: &str) -> Vec<Block> {
    markdown_files()
        .iter()
        .flat_map(|f| blocks(f))
        .filter(|b| b.lang == lang)
        .collect()
}

#[test]
fn every_toml_example_in_the_docs_loads() {
    let toml_blocks = all_blocks("toml");
    // The recipes page alone has seven; fewer than that means the scan broke,
    // not that the docs got shorter.
    assert!(
        toml_blocks.len() >= 7,
        "found only {} toml blocks",
        toml_blocks.len()
    );

    let mut failures = Vec::new();
    for b in &toml_blocks {
        let cfg: Config = match toml::from_str(&b.body) {
            Ok(c) => c,
            Err(e) => {
                failures.push(format!("{}: does not parse: {e}", b.origin));
                continue;
            }
        };
        if cfg.agent.thinking.is_some() && cfg.thinking().is_none() {
            failures.push(format!(
                "{}: `thinking` is set but resolves to nothing (an unknown word is ignored)",
                b.origin
            ));
        }
        if cfg.model.is_some()
            && let Err(e) = cfg.resolve_model(None)
        {
            failures.push(format!("{}: `model` does not resolve: {e:#}", b.origin));
        }
        if let Some(worker) = cfg.agents.model.as_deref()
            && let Err(e) = cfg.resolve_model(Some(worker))
        {
            failures.push(format!(
                "{}: `[agents] model` does not resolve: {e:#}",
                b.origin
            ));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
fn every_skill_example_in_the_docs_is_discovered() {
    common::isolate_home();
    let skills: Vec<Block> = all_blocks("markdown")
        .into_iter()
        .filter(|b| b.body.starts_with("---\n"))
        .collect();
    assert!(!skills.is_empty(), "the recipes page has a skill example");

    for b in &skills {
        let name = b
            .body
            .lines()
            .find_map(|l| l.strip_prefix("name:"))
            .map(|n| n.trim().to_string())
            .unwrap_or_else(|| panic!("{}: skill example has no name", b.origin));
        // The page says where to save it; that path has to agree with the
        // name, or the reader's copy is found by nothing.
        let file = b.origin.rsplit_once(':').unwrap().0;
        let page = std::fs::read_to_string(file).unwrap();
        assert!(
            page.contains(&format!("skills/{name}/SKILL.md")),
            "{}: the page never says to save `{name}` as skills/{name}/SKILL.md",
            b.origin
        );

        let project = tempfile::tempdir().unwrap();
        let dir = project.path().join(".worksmith/skills").join(&name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), &b.body).unwrap();

        let catalog = SkillCatalog::discover(project.path());
        assert!(
            catalog.get(&name).is_some(),
            "{}: `{name}` was not discovered; notes: {:?}",
            b.origin,
            catalog.notes()
        );
        let complaints: Vec<_> = catalog
            .notes()
            .iter()
            .filter(|n| n.contains(&name))
            .collect();
        assert!(complaints.is_empty(), "{}: {complaints:?}", b.origin);
    }
}
