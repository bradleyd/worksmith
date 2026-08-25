//! `checkpoint` — the pairing tool: stop and bring the user in.
//!
//! Worksmith writes code its user does not end up knowing. Reading the diff
//! afterwards does not fix that: retention comes from *deciding*, not from
//! reading. So a checkpoint is not a report — it is a point where the user gets
//! a say, is told one thing worth knowing, or writes the hard part themselves.
//!
//! **Selection is not this tool's job.** Judging "was that load-bearing" is the
//! kind of meta-reasoning a small model cannot do reliably, and it is exactly
//! the load the harness is supposed to carry (see `PAIR_PLAN.md`). The plan doc
//! being implemented names the decisions, usually with the question already
//! written out; this tool only carries them to the user. What *is* enforced
//! here is the cap, because a model can ignore a paragraph asking it to be
//! sparing and cannot ignore a tool that declines.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolContext, ToolOutput};

pub struct CheckpointTool;

/// Filed decisions are numbered so they read in the order they were taken.
fn next_number(dir: &Path) -> u32 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 1;
    };
    let highest = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            name.split_once('-')?.0.parse::<u32>().ok()
        })
        .max()
        .unwrap_or(0);
    highest + 1
}

/// `Pin the worker model` -> `pin-the-worker-model`, capped so a long subject
/// does not become a long filename.
fn slugify(subject: &str) -> String {
    let mut out = String::new();
    for c in subject.chars() {
        if c.is_ascii_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
        if out.len() >= 48 {
            break;
        }
    }
    let s = out.trim_matches('-').to_string();
    if s.is_empty() { "decision".to_string() } else { s }
}

/// Is this path inside something git is ignoring? A decision filed into an
/// ignored directory is not in the history it was written for, and finding that
/// out six months later is worse than a line of output now.
fn is_git_ignored(cwd: &Path, path: &Path) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .arg("check-ignore")
        .arg("-q")
        .arg(path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn decisions_dir(ctx: &ToolContext) -> PathBuf {
    if ctx.decisions_dir.is_absolute() {
        ctx.decisions_dir.clone()
    } else {
        ctx.cwd.join(&ctx.decisions_dir)
    }
}

/// File the decision. The user's answer is the body: they decided it, so the
/// record is their words, not a paraphrase of them.
fn write_decision(ctx: &ToolContext, subject: &str, question: &str, answer: &str) -> String {
    let dir = decisions_dir(ctx);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return format!("(could not create {}: {e})", dir.display());
    }
    let path = dir.join(format!("{:04}-{}.md", next_number(&dir), slugify(subject)));
    let body = format!(
        "# {subject}\n\n## Question\n\n{question}\n\n## Decision\n\n{answer}\n",
    );
    match std::fs::write(&path, body) {
        Err(e) => format!("(could not write {}: {e})", path.display()),
        Ok(()) => {
            let rel = super::display_rel(&path, &ctx.cwd);
            if is_git_ignored(&ctx.cwd, &path) {
                format!(
                    "filed at {rel} — but git is ignoring that path, so this decision will \
                     not reach the history it was written for. Tell the user to un-ignore it \
                     or set `decisions-dir` to somewhere tracked."
                )
            } else {
                format!("filed at {rel}")
            }
        }
    }
}

#[async_trait]
impl Tool for CheckpointTool {
    fn name(&self) -> &str {
        "checkpoint"
    }

    fn description(&self) -> &str {
        "Bring the user into the work at a point that matters. `ask` puts a real question to \
         them and waits — use it BEFORE writing code, when the plan names a judgment call or \
         two approaches are genuinely defensible; their answer is filed as a decision record. \
         `note` tells them one thing worth knowing about what you just wrote — why, never \
         what, since they can read the code. `yours` hands them the hard part: write the \
         surrounding wiring, leave the function as todo!() with a comment stating the \
         contract, and move on. Do not checkpoint on wiring, boilerplate, or a change the \
         plan did not flag."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["ask", "note", "yours"],
                    "description": "ask = a question, blocks for their answer, filed as a \
                                    decision. note = one line of why, does not block. \
                                    yours = you stubbed it, they write it."
                },
                "subject": {
                    "type": "string",
                    "description": "What it is about, short: a symbol, a file, or the \
                                    decision's name. Becomes the decision record's title."
                },
                "detail": {
                    "type": "string",
                    "description": "For `ask`, the question, with the options you are \
                                    choosing between and what each costs. For `note`, the \
                                    why. For `yours`, where the stub is and what it must do."
                }
            },
            "required": ["kind", "subject", "detail"]
        })
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let kind = args.get("kind").and_then(|v| v.as_str()).unwrap_or("note");
        let subject = args.get("subject").and_then(|v| v.as_str()).unwrap_or("").trim();
        let detail = args.get("detail").and_then(|v| v.as_str()).unwrap_or("").trim();
        if subject.is_empty() || detail.is_empty() {
            return ToolOutput::error("checkpoint needs both `subject` and `detail`");
        }

        // The cap is spent by every kind, not just the blocking one: three
        // notes in a turn is the same chattiness that teaches the user to stop
        // reading them.
        if ctx.checkpoints_left.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
            n.checked_sub(1)
        }).is_err()
        {
            return ToolOutput::error(
                "no checkpoints left this turn — carry on with the work and raise it in your \
                 summary at the end instead.",
            );
        }

        match kind {
            "ask" => match ctx.asker.ask_text(subject, detail).await {
                Some(answer) if !answer.trim().is_empty() => {
                    let filed = write_decision(ctx, subject, detail, answer.trim());
                    ToolOutput::ok(format!(
                        "The user answered: {}\n\nBuild that. Decision {filed}.",
                        answer.trim()
                    ))
                }
                // Skipped, or nobody there to ask. Not a failure: a checkpoint
                // is pedagogy, and the work still has to get done.
                _ => ToolOutput::ok(
                    "No answer — nobody watching, or they skipped it. Decide it yourself, \
                     say which way you went and why, and carry on.",
                ),
            },
            "note" | "yours" => ToolOutput::ok("Shown to the user. Carry on."),
            other => ToolOutput::error(format!(
                "unknown checkpoint kind `{other}` (use ask, note, or yours)"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::approval::Asker;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    fn ctx(dir: &Path, asker: Arc<dyn Asker>) -> ToolContext {
        ToolContext {
            cwd: dir.to_path_buf(),
            asker,
            decisions_dir: PathBuf::from("decisions"),
            checkpoints_left: Arc::new(AtomicUsize::new(super::super::CHECKPOINTS_PER_TURN)),
            ..Default::default()
        }
    }

    struct Answers(&'static str);

    #[async_trait]
    impl Asker for Answers {
        async fn ask_text(&self, _s: &str, _q: &str) -> Option<String> {
            Some(self.0.to_string())
        }
    }

    fn args(kind: &str) -> Value {
        json!({"kind": kind, "subject": "Pin the worker model", "detail": "pin or retarget?"})
    }

    #[tokio::test]
    async fn an_answer_becomes_a_decision_record_in_the_users_own_words() {
        let tmp = tempfile::tempdir().unwrap();
        let c = ctx(tmp.path(), Arc::new(Answers("Pin it. A running worker must not move.")));
        let out = CheckpointTool.run(args("ask"), &c).await;
        assert!(!out.is_error, "{}", out.content);

        let dir = tmp.path().join("decisions");
        let file = std::fs::read_dir(&dir).unwrap().next().unwrap().unwrap();
        assert_eq!(file.file_name().to_str().unwrap(), "0001-pin-the-worker-model.md");
        let body = std::fs::read_to_string(file.path()).unwrap();
        assert!(body.contains("# Pin the worker model"));
        assert!(body.contains("pin or retarget?"), "the question is half the record");
        assert!(body.contains("A running worker must not move."));
        // The model is told the answer, so it can act on it.
        assert!(out.content.contains("A running worker must not move."));
    }

    #[tokio::test]
    async fn records_are_numbered_in_the_order_they_were_taken() {
        let tmp = tempfile::tempdir().unwrap();
        let c = ctx(tmp.path(), Arc::new(Answers("yes")));
        for _ in 0..2 {
            CheckpointTool.run(args("ask"), &c).await;
        }
        let mut names: Vec<String> = std::fs::read_dir(tmp.path().join("decisions"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, vec!["0001-pin-the-worker-model.md", "0002-pin-the-worker-model.md"]);
    }

    #[tokio::test]
    async fn nobody_watching_skips_the_checkpoint_rather_than_failing_the_turn() {
        // The whole reason `Asker` is not `Approver`: an eval run has nobody to
        // teach, and must still do the work.
        let tmp = tempfile::tempdir().unwrap();
        let c = ctx(tmp.path(), Arc::new(crate::tools::approval::NoOneToAsk));
        let out = CheckpointTool.run(args("ask"), &c).await;
        assert!(!out.is_error, "a skipped checkpoint is not an error");
        assert!(out.content.contains("Decide it yourself"));
        assert!(!tmp.path().join("decisions").exists(), "nothing was decided, so nothing is filed");
    }

    #[tokio::test]
    async fn the_cap_is_enforced_in_code_not_in_prose() {
        let tmp = tempfile::tempdir().unwrap();
        let c = ctx(tmp.path(), Arc::new(Answers("sure")));
        for i in 0..super::super::CHECKPOINTS_PER_TURN {
            assert!(!CheckpointTool.run(args("note"), &c).await.is_error, "call {i}");
        }
        let out = CheckpointTool.run(args("note"), &c).await;
        assert!(out.is_error, "the budget is spent");
        assert!(out.content.contains("carry on with the work"));
    }

    #[tokio::test]
    async fn every_kind_spends_the_budget() {
        // Three notes in a turn is the same chattiness as three questions.
        let tmp = tempfile::tempdir().unwrap();
        let c = ctx(tmp.path(), Arc::new(Answers("sure")));
        CheckpointTool.run(args("note"), &c).await;
        CheckpointTool.run(args("yours"), &c).await;
        CheckpointTool.run(args("ask"), &c).await;
        assert!(CheckpointTool.run(args("note"), &c).await.is_error);
    }
}
