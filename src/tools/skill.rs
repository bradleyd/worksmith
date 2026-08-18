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
         you would otherwise have to guess at. With no name, lists what's available."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Skill name as shown in <SKILLS>. Omit to list them."
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
        match catalog.get(name) {
            Some(skill) => match skill.body() {
                Ok(body) => ToolOutput::ok(format!(
                    // Naming the directory is what makes the skill's own
                    // `references/...` paths resolvable with the read tool.
                    "skill `{}` (files live in {})\n\n{}",
                    skill.name,
                    skill.dir.display(),
                    body.trim()
                )),
                Err(e) => ToolOutput::error(format!("could not read skill `{name}`: {e}")),
            },
            None => ToolOutput::error(format!("no skill named `{name}`.\n{}", list(&catalog))),
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
