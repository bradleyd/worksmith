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

    /// The prose reference files, sorted. `scripts/` and `assets/` are not
    /// prose and are deliberately not walked.
    pub fn reference_files(&self) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(self.dir.join("references"))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "md"))
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        files
    }

    /// The generated map of this skill's reference headings — the second level
    /// of progressive disclosure. Nobody authors this; it is read off the
    /// headings that already exist, so it cannot drift stale. Empty string if
    /// there is nothing to map.
    pub fn map(&self) -> String {
        let per_file: Vec<(PathBuf, Vec<(usize, String)>)> = self
            .reference_files()
            .into_iter()
            .filter_map(|p| {
                let text = std::fs::read_to_string(&p).ok()?;
                Some((p, headings(&text)))
            })
            .collect();
        if per_file.iter().all(|(_, h)| h.is_empty()) {
            return String::new();
        }

        // Bounded: a skill with two hundred headings gets its deepest level
        // dropped first, then an explicit "…more" line — never a silent cut.
        // Depth is measured *per file*, relative to its shallowest heading: a
        // file that jumps straight from `#` to `###` (writing-rules.md does)
        // would otherwise lose its entire structure while a sibling file that
        // uses `##` kept all of its own.
        for max_depth in (1..=4).rev() {
            let mut out = format!("<skill-map name=\"{}\">\n", self.name);
            for (path, hs) in &per_file {
                let rel = path.strip_prefix(&self.dir).unwrap_or(path);
                out.push_str(&format!("{}\n", rel.display()));
                let base = hs.iter().map(|(l, _)| *l).min().unwrap_or(1);
                let shown: Vec<_> =
                    hs.iter().filter(|(l, _)| *l - base < max_depth).collect();
                let dropped = hs.len() - shown.len();
                for (level, title) in shown {
                    out.push_str(&format!("  {} {}\n", "#".repeat(*level), title));
                }
                if dropped > 0 {
                    out.push_str(&format!("  …{dropped} more, grep the file\n"));
                }
            }
            out.push_str(&format!(
                "Fetch one section: skill(name: \"{}\", section: \"<heading>\")\n</skill-map>",
                self.name
            ));
            if out.len() <= MAX_MAP_CHARS || max_depth == 1 {
                return out;
            }
        }
        unreachable!("the loop returns at max_depth == 1")
    }

    /// Find the reference section matching `query`, case-insensitively, with
    /// leading list numbers stripped ("writing style" hits
    /// "## 8. Writing Style Rules").
    ///
    /// A query that names a reference *file* is answered with that file's
    /// headings. The first live run (think:off) did exactly this — the file
    /// paths are the most prominent lines of the map, so a weak config grabbed
    /// one — and the miss sent it back to whole-file reads, the exact behavior
    /// sections exist to replace. Meet the model where it reached.
    pub fn find_section(&self, query: &str) -> SectionMatch {
        let want = query.trim().to_lowercase();
        if want.is_empty() {
            return SectionMatch::None;
        }
        for path in self.reference_files() {
            let name = path.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
            if want.ends_with(&*name) || name == want {
                let rel = path.strip_prefix(&self.dir).map(PathBuf::from).unwrap_or(path.clone());
                let hs = std::fs::read_to_string(&path).map(|t| headings(&t)).unwrap_or_default();
                return SectionMatch::File { file: rel, headings: hs };
            }
        }
        let mut hits: Vec<(PathBuf, String, String)> = Vec::new();
        for path in self.reference_files() {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            for (level, title) in headings(&text) {
                if normalize_heading(&title).contains(&want) {
                    hits.push((path.clone(), title.clone(), section_slice(&text, level, &title)));
                }
            }
        }
        match hits.len() {
            0 => SectionMatch::None,
            1 => {
                let (path, heading, content) = hits.remove(0);
                let rel = path.strip_prefix(&self.dir).map(PathBuf::from).unwrap_or(path);
                SectionMatch::One { file: rel, heading, content }
            }
            _ => SectionMatch::Many(
                hits.into_iter()
                    .map(|(p, h, _)| {
                        (p.strip_prefix(&self.dir).map(PathBuf::from).unwrap_or(p), h)
                    })
                    .collect(),
            ),
        }
    }
}

/// Cap on a generated skill map. It rides in the pinned prompt for the rest of
/// the session, so it obeys the same rule as the catalog: a per-turn tax.
/// Sized against the two real installed skills: at 1kB the map cut
/// "Writing Style Rules" — the heading whose 8× re-read motivated the feature.
/// 2kB (~500 tokens) shows both skills whole.
pub const MAX_MAP_CHARS: usize = 2_048;

/// Result of a section lookup.
#[derive(Debug)]
pub enum SectionMatch {
    None,
    One { file: PathBuf, heading: String, content: String },
    /// Ambiguity returns the candidates, not a guess: a model writing
    /// confidently from the wrong section damages the work invisibly.
    Many(Vec<(PathBuf, String)>),
    /// The query named a whole reference file; answer with its headings so the
    /// next call can name one.
    File { file: PathBuf, headings: Vec<(usize, String)> },
}

/// Markdown headings (level, title) of `text`, `#` through `####`, skipping
/// fenced code blocks — a `# comment` inside a ```bash fence is not a heading.
pub fn headings(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut fence: Option<&str> = None;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(open) = fence {
            if trimmed.starts_with(open) {
                fence = None;
            }
            continue;
        }
        if trimmed.starts_with("```") {
            fence = Some("```");
            continue;
        }
        if trimmed.starts_with("~~~") {
            fence = Some("~~~");
            continue;
        }
        let hashes = line.chars().take_while(|c| *c == '#').count();
        if (1..=4).contains(&hashes)
            && let Some(title) = line[hashes..].strip_prefix(' ')
        {
            out.push((hashes, title.trim().to_string()));
        }
    }
    out
}

/// Lowercased, with a leading list number dropped: "8. Writing Style Rules" →
/// "writing style rules".
fn normalize_heading(title: &str) -> String {
    let t = title.trim();
    let rest = t
        .split_once(". ")
        .filter(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        .map(|(_, rest)| rest)
        .unwrap_or(t);
    rest.to_lowercase()
}

/// The slice from the heading `title` (at `level`) to the next heading of equal
/// or shallower depth, fence-aware for the same reason [`headings`] is.
fn section_slice(text: &str, level: usize, title: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut fence: Option<&str> = None;
    let mut inside = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        let in_fence = fence.is_some();
        if let Some(open) = fence {
            if trimmed.starts_with(open) {
                fence = None;
            }
        } else if trimmed.starts_with("```") {
            fence = Some("```");
        } else if trimmed.starts_with("~~~") {
            fence = Some("~~~");
        }
        if !in_fence && fence.is_none() {
            let hashes = line.chars().take_while(|c| *c == '#').count();
            if (1..=4).contains(&hashes)
                && let Some(t) = line[hashes..].strip_prefix(' ')
            {
                if inside && hashes <= level {
                    break;
                }
                if !inside && hashes == level && t.trim() == title {
                    inside = true;
                }
            }
        }
        if inside {
            out.push(line);
        }
    }
    out.join("\n")
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
    /// Directories that *would* hold a skill, and whether each exists. A skill
    /// in the wrong place is found by nothing and reported by nothing, so the
    /// empty case has to be able to say where it looked.
    pub fn searched(project_dir: &Path) -> Vec<(PathBuf, bool)> {
        search_paths(project_dir).into_iter().map(|p| (p.exists(), p)).map(|(e, p)| (p, e)).collect()
    }

    /// Look for a stray `SKILL.md` near the project that discovery cannot see.
    /// The common mistake is a skill directory sitting beside the project
    /// instead of under `skills/`, which is exactly what a newcomer does.
    pub fn misplaced(project_dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(entries) = std::fs::read_dir(project_dir) else {
            return found;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() && p.file_name().is_some_and(|n| n != "skills") {
                let candidate = p.join("SKILL.md");
                if candidate.is_file() {
                    found.push(candidate);
                }
            }
        }
        found.sort();
        found
    }

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
pub fn search_paths(project_dir: &Path) -> Vec<PathBuf> {
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

    const DOC: &str = "# Writing rules\n\nintro\n\n```bash\n# not a heading\necho hi\n```\n\n## 8. Writing Style Rules\nshort sentences\n\n### Detail\nnested detail\n\n## 9. Handling Special Characters\nescape them\n";

    #[test]
    fn headings_skip_code_fences() {
        let hs = headings(DOC);
        let titles: Vec<&str> = hs.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                "Writing rules",
                "8. Writing Style Rules",
                "Detail",
                "9. Handling Special Characters"
            ],
            "the # inside the bash fence must not appear"
        );
        assert_eq!(hs[0].0, 1);
        assert_eq!(hs[2].0, 3);
    }

    #[test]
    fn a_section_slice_keeps_its_subsections_and_stops_at_a_peer() {
        let slice = section_slice(DOC, 2, "8. Writing Style Rules");
        assert!(slice.contains("short sentences"));
        assert!(slice.contains("### Detail"), "nested stays inside its parent");
        assert!(!slice.contains("escape them"), "the next peer heading ends it");
    }

    #[test]
    fn the_last_section_runs_to_end_of_file() {
        let slice = section_slice(DOC, 2, "9. Handling Special Characters");
        assert!(slice.contains("escape them"));
    }

    #[test]
    fn numbered_headings_match_without_their_numbers() {
        assert_eq!(normalize_heading("8. Writing Style Rules"), "writing style rules");
        assert_eq!(normalize_heading("Plain Title"), "plain title");
        // "v2. something" is not a list number; leave it alone.
        assert_eq!(normalize_heading("v2. Something"), "v2. something");
    }

    fn skill_with_references(files: &[(&str, &str)]) -> (tempfile::TempDir, Skill) {
        let dir = tempfile::tempdir().unwrap();
        let refs = dir.path().join("references");
        std::fs::create_dir_all(&refs).unwrap();
        for (name, content) in files {
            std::fs::write(refs.join(name), content).unwrap();
        }
        let path = dir.path().join("SKILL.md");
        std::fs::write(&path, "---\nname: t\ndescription: d\n---\nbody").unwrap();
        let skill = Skill {
            name: "t".into(),
            description: "d".into(),
            dir: dir.path().to_path_buf(),
            path,
            allowed_tools: None,
        };
        (dir, skill)
    }

    #[test]
    fn the_map_lists_files_and_headings_and_teaches_the_fetch() {
        let (_g, skill) = skill_with_references(&[("rules.md", DOC)]);
        let map = skill.map();
        assert!(map.contains("references/rules.md"), "{map}");
        assert!(map.contains("## 8. Writing Style Rules"));
        assert!(map.contains("skill(name: \"t\", section:"), "the usage line is the teaching");
    }

    #[test]
    fn a_bloated_map_drops_depth_before_width() {
        let mut big = String::from("# Top\n");
        for i in 0..120 {
            big.push_str(&format!("### Deep section number {i} with a long title\n"));
        }
        let (_g, skill) = skill_with_references(&[("big.md", &big)]);
        let map = skill.map();
        assert!(map.len() <= MAX_MAP_CHARS + 200, "bounded: {} chars", map.len());
        assert!(map.contains("# Top"), "shallow survives");
        assert!(map.contains("more, grep the file"), "the cut is named, never silent");
    }

    #[test]
    fn headingless_references_mean_no_map_not_an_error() {
        let (_g, skill) = skill_with_references(&[("notes.md", "just prose, no structure")]);
        assert_eq!(skill.map(), "");
        assert!(matches!(skill.find_section("anything"), SectionMatch::None));
    }

    #[test]
    fn a_file_path_query_returns_that_files_headings() {
        // The first live run (think:off 27B) passed the map's file paths as
        // `section`. The miss sent it straight back to whole-file reads.
        let (_g, skill) = skill_with_references(&[("rules.md", DOC)]);
        match skill.find_section("references/rules.md") {
            SectionMatch::File { file, headings } => {
                assert_eq!(file, std::path::PathBuf::from("references/rules.md"));
                assert!(headings.iter().any(|(_, t)| t.contains("Writing Style Rules")));
            }
            other => panic!("expected File, got {other:?}"),
        }
        // Bare filename works too.
        assert!(matches!(skill.find_section("rules.md"), SectionMatch::File { .. }));
    }

    #[test]
    fn an_ambiguous_query_returns_candidates_not_a_guess() {
        let (_g, skill) = skill_with_references(&[
            ("a.md", "## Style Rules\nfrom a\n"),
            ("b.md", "## Style Rules for Tables\nfrom b\n"),
        ]);
        match skill.find_section("style rules") {
            SectionMatch::Many(c) => assert_eq!(c.len(), 2),
            other => panic!("expected Many, got {other:?}"),
        }
        // A narrower query resolves.
        match skill.find_section("for tables") {
            SectionMatch::One { content, .. } => assert!(content.contains("from b")),
            other => panic!("expected One, got {other:?}"),
        }
    }

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
