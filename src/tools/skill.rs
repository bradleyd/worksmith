//! `skill` — load an instruction pack's full text on demand.
//!
//! The catalog in the system prompt carries only names and descriptions; this
//! is how the model gets the rest. That split is the Agent Skills spec's
//! progressive disclosure, and it's why a project can have twenty skills
//! without paying for twenty skills every turn.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolContext, ToolOutput};
use crate::skill::SkillCatalog;

pub struct SkillTool;

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        "Load the full instructions for one of the skills listed in <SKILLS>. Call this before \
         doing work that a skill covers — it carries conventions, structure, and review rules \
         you would otherwise have to guess at. With `section`, fetch just one heading from a \
         loaded skill's reference files instead of reading the whole file. With no name, lists \
         what's available."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Skill name as shown in <SKILLS>. Omit to list them."
                },
                "section": {
                    "type": "string",
                    "description": "A heading from the skill's map (see <skill-map>). Returns \
                                    just that section — far cheaper than reading the file."
                }
            }
        })
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let catalog = SkillCatalog::discover(&ctx.cwd);
        let Some(name) = args.get("name").and_then(|v| v.as_str()) else {
            return ToolOutput::ok(list(&catalog));
        };
        let name = name.trim();
        if let Some(section) = args.get("section").and_then(|v| v.as_str()) {
            return match catalog.get(name) {
                Some(skill) => fetch_section(skill, section),
                None => ToolOutput::error(format!("no skill named `{name}`.\n{}", list(&catalog))),
            };
        }
        match catalog.get(name) {
            Some(skill) => match skill.body() {
                Ok(body) => {
                    // Already loaded: the text is pinned to the system prompt,
                    // so serving it again spends a thousand tokens to tell the
                    // model something it is already looking at.
                    let mut loaded = ctx.loaded_skills.lock().unwrap();
                    if loaded.iter().any(|(n, _)| n == &skill.name) {
                        return ToolOutput::ok(format!(
                            "skill `{}` is already loaded — its full instructions are in your \
                             system prompt under <SKILLS-LOADED>. Files live in {}. Fetch one \
                             reference section with skill(name, section). Get on with the work \
                             it describes.",
                            skill.name,
                            skill.dir.display()
                        ));
                    }
                    // The map is the second level of progressive disclosure:
                    // the model sees what sections exist without holding any of
                    // them, and fetches one when it needs it.
                    let map = skill.map();
                    let text = format!(
                        // Naming the directory is what makes the skill's own
                        // `references/...` paths resolvable with the read tool.
                        "skill `{}` (files live in {})\n\n{}{}",
                        skill.name,
                        skill.dir.display(),
                        body.trim(),
                        if map.is_empty() { String::new() } else { format!("\n\n{map}") }
                    );
                    loaded.push((skill.name.clone(), text.clone()));
                    ToolOutput::ok(text)
                }
                Err(e) => ToolOutput::error(format!("could not read skill `{name}`: {e}")),
            },
            None => ToolOutput::error(format!("no skill named `{name}`.\n{}", list(&catalog))),
        }
    }
}

fn fetch_section(skill: &crate::skill::Skill, query: &str) -> ToolOutput {
    use crate::skill::SectionMatch;
    match skill.find_section(query) {
        SectionMatch::One { file, heading, content } => ToolOutput::ok(format!(
            // Naming the file lets the model see where it landed and read
            // around it if the slice was not what it wanted.
            "{} § {heading} (from {})\n\n{content}",
            skill.name,
            file.display()
        )),
        // A guess would let the model write confidently from the wrong
        // section; candidates cost one ~50-token round trip.
        SectionMatch::Many(candidates) => {
            let mut out = format!("`{query}` matches several sections — pick one:\n");
            for (file, heading) in candidates {
                out.push_str(&format!("- {heading} ({})\n", file.display()));
            }
            ToolOutput::ok(out)
        }
        SectionMatch::None => {
            let map = skill.map();
            ToolOutput::error(if map.is_empty() {
                format!(
                    "skill `{}` has no reference sections to search; read files under {} directly",
                    skill.name,
                    skill.dir.display()
                )
            } else {
                format!("no section matching `{query}`. The map:\n{map}\nOr grep references/.")
            })
        }
    }
}

fn list(catalog: &SkillCatalog) -> String {
    if catalog.is_empty() {
        return "(no skills installed — add one under .worksmith/skills/<name>/SKILL.md)".into();
    }
    let mut out = String::from("available skills:\n");
    for s in catalog.skills() {
        out.push_str(&format!("- {}: {}\n", s.name, s.description));
    }
    out
}
