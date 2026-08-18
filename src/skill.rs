//! Skills: markdown instruction packs the model can pull in on demand.
//!
//! We implement the Agent Skills format as published rather than inventing one
//! — the same `SKILL.md` that Claude Code, Codex, Cursor, Gemini CLI and ~30
//! other tools read. A skill you already wrote works here with no porting, and
//! a skill written here works everywhere else.
//!
//! Loading follows the spec's progressive disclosure: only `name` and
//! `description` reach the prompt (§[`SkillCatalog::prompt_section`]); the body
//! arrives when the model calls the `skill` tool; `references/`, `scripts/` and
//! `assets/` need no machinery at all, because the body points at them and the
//! model reads them like any other file.
//!
//! What the spec deliberately leaves out — multi-step orchestration,
//! determinism, validation — is *not* bolted on here. That belongs in a
//! separate artifact (PLAN.md, workflows), because forking a format 30 tools
//! agree on to smuggle in a state machine would cost the interop and gain
//! nothing.

use std::path::{Path, PathBuf};

use crate::config;

/// Spec limits. They exist to keep the always-loaded catalog cheap, so we
/// enforce them rather than letting one verbose skill crowd out the rest.
const MAX_NAME: usize = 64;
const MAX_DESCRIPTION: usize = 1024;

/// Cap on the whole `<SKILLS>` block. Descriptions ride in the stable prompt
/// prefix on every turn, so this is a per-turn tax on every session.
pub const MAX_CATALOG_CHARS: usize = 4_000;

/// One discovered skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// The skill's directory — `references/` and friends resolve against it.
    pub dir: PathBuf,
    /// The `SKILL.md` itself.
    pub path: PathBuf,
    /// Spec-experimental; parsed so it round-trips, not enforced yet.
    pub allowed_tools: Option<String>,
}

impl Skill {
    /// The instructions below the frontmatter.
    pub fn body(&self) -> std::io::Result<String> {
        let text = std::fs::read_to_string(&self.path)?;
        Ok(strip_frontmatter(&text).to_string())
    }
}

/// Every skill visible from a project, nearest definition winning.
#[derive(Debug, Default)]
pub struct SkillCatalog {
    skills: Vec<Skill>,
    /// Things worth telling the user at startup: malformed skills, and skills
    /// shadowed by a higher-precedence copy. A skill that silently fails to
    /// load is the same trap as a config key that silently does nothing.
    notes: Vec<String>,
}

impl SkillCatalog {
    /// Search every location, lowest precedence first.
    pub fn discover(project_dir: &Path) -> SkillCatalog {
        let mut cat = SkillCatalog::default();
        for dir in search_paths(project_dir) {
            cat.load_dir(&dir);
        }
        cat.skills.sort_by(|a, b| a.name.cmp(&b.name));
        cat
    }

    /// Load every `<dir>/*/SKILL.md`. Later calls override earlier ones by name.
    fn load_dir(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return; // absent search paths are normal, not an error
        };
        let mut found: Vec<Skill> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path().join("SKILL.md");
            if !path.is_file() {
                continue;
            }
            match parse_skill(&path) {
                Ok(s) => found.push(s),
                Err(e) => self.notes.push(format!("skipped {}: {e}", path.display())),
            }
        }
        found.sort_by(|a, b| a.name.cmp(&b.name));
        for skill in found {
            if let Some(existing) = self.skills.iter_mut().find(|s| s.name == skill.name) {
                self.notes.push(format!(
                    "skill `{}` from {} overrides {}",
                    skill.name,
                    skill.dir.display(),
                    existing.dir.display()
                ));
                *existing = skill;
            } else {
                self.skills.push(skill);
            }
        }
    }

    pub fn skills(&self) -> &[Skill] {
        &self.skills
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// Warnings and shadowing notices, for the startup header.
    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    /// The always-loaded tier: one line per skill, and how to get the rest.
    /// Empty when there are no skills, so a session without any pays nothing.
    pub fn prompt_section(&self, max_chars: usize) -> String {
        if self.skills.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "<SKILLS>\nInstruction packs available for this project. Call the `skill` tool with \
             a name to load one's full instructions before doing that kind of work.\n",
        );
        let mut dropped = 0;
        for s in &self.skills {
            let line = format!("- {}: {}\n", s.name, s.description);
            if out.len() + line.len() > max_chars {
                dropped += 1;
                continue;
            }
            out.push_str(&line);
        }
        if dropped > 0 {
            out.push_str(&format!(
                "({dropped} more skills not listed — the catalog hit its size cap)\n"
            ));
        }
        out.push_str("</SKILLS>\n");
        out
    }
}

/// Where skills live, lowest precedence first. Project beats global, and the
/// worksmith-specific directory beats the shared one — so a skill can be
/// tailored here without editing the copy other tools read.
fn search_paths(project_dir: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    // Shipped with the source tree (this repo's own `skills/`).
    paths.push(project_dir.join("skills"));
    // The shared location every other tool reads. When `WORKSMITH_HOME` is set
    // it moves too, so a test or a reproducible eval run doesn't quietly
    // inherit whatever skills happen to be installed on the machine.
    if let Some(home) = home_root() {
        paths.push(home.join(".claude").join("skills"));
    }
    if let Some(global) = config::global_dir() {
        paths.push(global.join("skills"));
    }
    paths.push(project_dir.join(".claude").join("skills"));
    paths.push(project_dir.join(".worksmith").join("skills"));
    paths
}

/// Where `~/.claude` is rooted: the real home, or `WORKSMITH_HOME` when it's
/// set so isolation covers both skill locations rather than only ours.
fn home_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(config::GLOBAL_DIR_ENV) {
        return Some(PathBuf::from(dir));
    }
    dirs::home_dir()
}

/// Read and validate one `SKILL.md`.
fn parse_skill(path: &Path) -> Result<Skill, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("unreadable: {e}"))?;
    let fields = parse_frontmatter(&text)?;

    let name = fields
        .iter()
        .find(|(k, _)| k == "name")
        .map(|(_, v)| v.clone())
        .ok_or("no `name` in frontmatter")?;
    let description = fields
        .iter()
        .find(|(k, _)| k == "description")
        .map(|(_, v)| v.clone())
        .ok_or("no `description` in frontmatter")?;

    validate_name(&name)?;
    if description.trim().is_empty() {
        return Err("`description` is empty (it's the only thing the model sees)".into());
    }
    if description.chars().count() > MAX_DESCRIPTION {
        return Err(format!("`description` is over {MAX_DESCRIPTION} characters"));
    }

    let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    // The spec ties the name to the directory; a mismatch means `/skill <name>`
    // and the folder on disk disagree, which is worse than a hard error here.
    if let Some(folder) = dir.file_name().and_then(|f| f.to_str())
        && folder != name
    {
        return Err(format!("`name: {name}` does not match its directory `{folder}`"));
    }

    Ok(Skill {
        name,
        description: description.trim().to_string(),
        dir,
        path: path.to_path_buf(),
        allowed_tools: fields
            .iter()
            .find(|(k, _)| k == "allowed-tools")
            .map(|(_, v)| v.clone()),
    })
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.chars().count() > MAX_NAME {
        return Err(format!("`name` must be 1..={MAX_NAME} characters"));
    }
    if name.contains("--") || name.starts_with('-') || name.ends_with('-') {
        return Err("`name` cannot start or end with `-`, or contain `--`".into());
    }
    if !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
        return Err("`name` must be lowercase letters, digits, and hyphens".into());
    }
    Ok(())
}

/// Everything after the frontmatter block.
fn strip_frontmatter(text: &str) -> &str {
    let t = text.trim_start_matches('\u{feff}');
    let Some(rest) = t.strip_prefix("---") else {
        return text;
    };
    let rest = rest.trim_start_matches(['\r', '\n']);
    match find_closing_fence(rest) {
        Some((_, after)) => after,
        None => text,
    }
}

/// Split the frontmatter block from the body. Returns (block, body).
fn find_closing_fence(rest: &str) -> Option<(&str, &str)> {
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return Some((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

/// A deliberately narrow YAML subset: the two fields the spec requires, plus
/// whatever else is a plain scalar. Adding a YAML crate for `name` and
/// `description` isn't a trade worth making, but pretending to parse YAML we
/// don't understand is worse — anything unexpected is an error naming the file.
fn parse_frontmatter(text: &str) -> Result<Vec<(String, String)>, String> {
    let t = text.trim_start_matches('\u{feff}');
    let rest = t
        .strip_prefix("---")
        .ok_or("missing `---` frontmatter block")?
        .trim_start_matches(['\r', '\n']);
    let (block, _) = find_closing_fence(rest).ok_or("frontmatter block is not closed by `---`")?;

    let mut fields: Vec<(String, String)> = Vec::new();
    let lines: Vec<&str> = block.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // Indented lines belong to whatever came before; the top-level loop
        // consumes those itself, so seeing one here means we already skipped it.
        if line.starts_with(' ') || line.starts_with('\t') || trimmed.starts_with('-') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            return Err(format!("frontmatter line is not `key: value`: {trimmed}"));
        };
        let key = key.trim().to_string();
        let value = value.trim();

        // Block scalars (`|`, `>-`, …) take the indented lines that follow.
        if value.starts_with('|') || value.starts_with('>') {
            let fold = value.starts_with('>');
            let mut parts: Vec<String> = Vec::new();
            while i < lines.len() && (lines[i].starts_with(' ') || lines[i].trim().is_empty()) {
                parts.push(lines[i].trim().to_string());
                i += 1;
            }
            let joined =
                if fold { parts.join(" ") } else { parts.join("\n") }.trim().to_string();
            fields.push((key, joined));
            continue;
        }
        // A bare key introduces a nested map or list (the spec's `metadata:`).
        // We don't need it; skip its body rather than choking on it.
        if value.is_empty() {
            while i < lines.len() && (lines[i].starts_with(' ') || lines[i].trim().is_empty()) {
                i += 1;
            }
            continue;
        }
        fields.push((key, unquote(value).to_string()));
    }
    Ok(fields)
}

fn unquote(s: &str) -> &str {
    let s = s.trim();
    for q in ['"', '\''] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            return &s[1..s.len() - 1];
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let d = dir.join(name);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("SKILL.md");
        std::fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn parses_the_required_fields_and_body() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            dir.path(),
            "docs",
            "---\nname: docs\ndescription: Working with PDFs and DOCX.\n---\n\n# Docs\n\nBody here.\n",
        );
        let s = parse_skill(&p).unwrap();
        assert_eq!(s.name, "docs");
        assert_eq!(s.description, "Working with PDFs and DOCX.");
        assert!(s.body().unwrap().contains("Body here."));
        assert!(!s.body().unwrap().contains("description:"), "frontmatter is stripped");
    }

    #[test]
    fn handles_quotes_folded_blocks_and_nested_maps() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            dir.path(),
            "fancy",
            "---\nname: \"fancy\"\ndescription: >-\n  A folded description\n  across two lines.\n\
             metadata:\n  author: someone\n  tags: a,b\nallowed-tools: Bash(git:*) Read\n---\nbody\n",
        );
        let s = parse_skill(&p).unwrap();
        assert_eq!(s.name, "fancy", "quotes stripped");
        assert_eq!(s.description, "A folded description across two lines.");
        assert_eq!(s.allowed_tools.as_deref(), Some("Bash(git:*) Read"));
    }

    #[test]
    fn rejects_what_the_spec_forbids() {
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            ("nodesc", "---\nname: nodesc\n---\nbody\n", "description"),
            ("noname", "---\ndescription: x\n---\nbody\n", "name"),
            ("unclosed", "---\nname: unclosed\ndescription: x\nbody\n", "not closed"),
            ("nofence", "name: nofence\ndescription: x\n", "missing `---`"),
            ("Bad-Case", "---\nname: Bad-Case\ndescription: x\n---\n", "lowercase"),
        ];
        for (dirname, text, needle) in cases {
            let p = write(dir.path(), dirname, text);
            let err = parse_skill(&p).unwrap_err();
            assert!(err.contains(needle), "{dirname}: expected {needle:?}, got {err:?}");
        }
    }

    #[test]
    fn name_must_match_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        // Otherwise `/skill <name>` and the folder on disk disagree.
        let p = write(dir.path(), "folder-name", "---\nname: other-name\ndescription: x\n---\n");
        assert!(parse_skill(&p).unwrap_err().contains("does not match"));
    }

    #[test]
    fn nearer_definitions_win_and_say_so() {
        let dir = tempfile::tempdir().unwrap();
        let low = dir.path().join("low");
        let high = dir.path().join("high");
        write(&low, "dup", "---\nname: dup\ndescription: the low one\n---\n");
        write(&high, "dup", "---\nname: dup\ndescription: the high one\n---\n");

        let mut cat = SkillCatalog::default();
        cat.load_dir(&low);
        cat.load_dir(&high);

        assert_eq!(cat.skills().len(), 1, "one name, one entry");
        assert_eq!(cat.get("dup").unwrap().description, "the high one");
        assert!(
            cat.notes().iter().any(|n| n.contains("overrides")),
            "shadowing must be announced, not silent: {:?}",
            cat.notes()
        );
    }

    #[test]
    fn a_malformed_skill_is_reported_not_swallowed() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "good", "---\nname: good\ndescription: fine\n---\n");
        write(dir.path(), "broken", "---\nname: broken\n---\n");
        let mut cat = SkillCatalog::default();
        cat.load_dir(dir.path());
        assert_eq!(cat.skills().len(), 1, "the good one still loads");
        assert!(cat.notes().iter().any(|n| n.contains("broken")), "{:?}", cat.notes());
    }

    #[test]
    fn the_catalog_block_is_bounded() {
        let mut cat = SkillCatalog::default();
        for i in 0..40 {
            cat.skills.push(Skill {
                name: format!("skill-{i}"),
                description: "x".repeat(400),
                dir: PathBuf::from("/tmp"),
                path: PathBuf::from("/tmp/SKILL.md"),
                allowed_tools: None,
            });
        }
        let section = cat.prompt_section(MAX_CATALOG_CHARS);
        assert!(section.len() <= MAX_CATALOG_CHARS + 200, "len {}", section.len());
        assert!(section.contains("not listed"), "and it says what it dropped");
        assert!(SkillCatalog::default().prompt_section(MAX_CATALOG_CHARS).is_empty());
    }
}
