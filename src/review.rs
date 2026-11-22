//! An independent review of a turn's changes before it is reported done.
//!
//! The reviewer sees the person's request and the diff, never the transcript:
//! an agent grading its own work approves almost all of it, and a reviewer that
//! reads the agent's account of itself inherits the same blind spots. Measured
//! in *Rethinking Verification in Agent Harnesses* (arXiv 2610.34577): an
//! independent critic on requirements plus changes gained 4 to 10 points where
//! a completion-time self-check gained little. See MILESTONES_PLAN.md, part 2.

use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;

/// At most this many findings reach the agent. A long list is a reviewer
/// guessing, and every finding sends the agent somewhere.
pub const MAX_FINDINGS: usize = 5;

pub const REVIEW_PROMPT: &str = "\
You review a change another assistant made for a person. You see the \
person's request and the diff of everything that changed. You do not see \
how the work was done, and you cannot run anything.

Report only problems that matter:
- something the request asked for that the change does not do;
- a bug the change introduces that will break something;
- work left half done: a placeholder, a stub, a TODO in new code, a debug print.
Do not report style, naming, formatting, or things you would merely do differently.
If a part of the diff is cut off, do not guess about what you cannot see.

Answer in exactly this form:
VERDICT: pass
or
VERDICT: revise
- path:line: the problem, in one sentence
(at most five findings, the most important first)";

/// A tree of the working directory as it stands, including new files and
/// honouring .gitignore, written through a throwaway index so the person's
/// own staging area is never touched. `None` outside a git repository.
#[derive(Debug, Clone)]
pub struct Snapshot {
    root: PathBuf,
    tree: String,
}

pub async fn snapshot(cwd: &Path) -> Option<Snapshot> {
    let cancel = CancellationToken::new();
    let git = crate::workspace::git;
    let root = String::from_utf8(
        git(cwd, &["rev-parse", "--show-toplevel"], None, &cancel)
            .await
            .ok()?,
    )
    .ok()?;
    let root = PathBuf::from(root.trim());
    let index = std::env::temp_dir().join(format!("worksmith-review-{}", uuid::Uuid::new_v4()));
    let tree = async {
        // An unborn branch has no HEAD to start from; an empty index is right.
        let _ = git(&root, &["read-tree", "HEAD"], Some(&index), &cancel).await;
        git(&root, &["add", "-A", "--", "."], Some(&index), &cancel).await?;
        git(&root, &["write-tree"], Some(&index), &cancel).await
    }
    .await;
    let _ = std::fs::remove_file(&index);
    let tree = String::from_utf8(tree.ok()?).ok()?.trim().to_string();
    Some(Snapshot { root, tree })
}

/// What changed between two snapshots, with a summary of files first and the
/// patch cut to `max_chars`. Worksmith's own `.worksmith/` bookkeeping is left
/// out: it is not the work. Empty when nothing changed.
pub async fn diff(before: &Snapshot, after: &Snapshot, max_chars: usize) -> String {
    if before.tree == after.tree {
        return String::new();
    }
    let cancel = CancellationToken::new();
    let git = crate::workspace::git;
    let run = |args: Vec<String>| {
        let root = before.root.clone();
        let cancel = cancel.clone();
        async move {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            git(&root, &args, None, &cancel)
                .await
                .map(|o| String::from_utf8_lossy(&o).into_owned())
                .unwrap_or_default()
        }
    };
    let patch = run(vec![
        "diff".into(),
        "--no-color".into(),
        "--no-ext-diff".into(),
        before.tree.clone(),
        after.tree.clone(),
    ])
    .await;
    let patch = without_bookkeeping(&patch);
    if patch.trim().is_empty() {
        return String::new();
    }
    let stat = run(vec![
        "diff".into(),
        "--stat=100".into(),
        before.tree.clone(),
        after.tree.clone(),
    ])
    .await;
    let stat: String = stat
        .lines()
        .filter(|l| !l.trim_start().starts_with(".worksmith/"))
        .map(|l| format!("{l}\n"))
        .collect();
    let mut out = format!("{stat}\n");
    if patch.len() > max_chars {
        // `floor_char_boundary` is newer than the crate's minimum Rust.
        let mut cut = max_chars;
        while !patch.is_char_boundary(cut) {
            cut -= 1;
        }
        out.push_str(&patch[..cut]);
        out.push_str(&format!(
            "\n[diff cut here: {} more characters not shown]\n",
            patch.len() - cut
        ));
    } else {
        out.push_str(&patch);
    }
    out
}

fn without_bookkeeping(patch: &str) -> String {
    let mut out = String::new();
    let mut keep = true;
    for line in patch.split_inclusive('\n') {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            keep = !rest.starts_with("a/.worksmith/");
        }
        if keep {
            out.push_str(line);
        }
    }
    out
}

/// The reviewer's request: what was asked, then the change.
pub fn request(asked: &str, diff: &str) -> String {
    format!("The person asked:\n\n{asked}\n\nThe change:\n\n{diff}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    pub revise: bool,
    pub findings: Vec<String>,
}

/// Read the verdict. Anything unreadable counts as a pass: a reviewer that
/// cannot follow the format must not hold the turn hostage.
pub fn parse(text: &str) -> Review {
    // The last verdict line wins, and only bullets after it are findings: the
    // 27B reasons in prose first, listing the request's requirements as
    // bullets, and those are not problems.
    let lines: Vec<&str> = text.lines().collect();
    let Some((at, verdict)) = lines.iter().enumerate().rev().find_map(|(i, l)| {
        l.trim()
            .trim_start_matches(['*', '#', ' '])
            .to_ascii_lowercase()
            .strip_prefix("verdict:")
            .map(|v| (i, v.trim_matches(['*', ' ']).to_string()))
    }) else {
        return Review {
            revise: false,
            findings: Vec::new(),
        };
    };
    let findings: Vec<String> = lines[at + 1..]
        .iter()
        .map(|l| l.trim())
        .filter_map(|l| l.strip_prefix("- ").or_else(|| l.strip_prefix("* ")))
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .take(MAX_FINDINGS)
        .collect();
    let revise = verdict.starts_with("revise") && !findings.is_empty();
    Review {
        revise,
        findings: if revise { findings } else { Vec::new() },
    }
}

/// What the agent is told when the reviewer asks for changes.
pub fn directive(review: &Review) -> String {
    let list: String = review.findings.iter().map(|f| format!("- {f}\n")).collect();
    format!(
        "An independent reviewer compared your change with the request (it saw \
         only the request and the diff) and raised:\n\n{list}\nCheck each one \
         against the code. Fix the ones that are real. For any you disagree with, \
         say in one line why. Then finish."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_revise_verdict_carries_its_findings() {
        let r = parse(
            "VERDICT: revise\n- src/cart.rs:12: refunds are never written\n\
             - src/cart.rs:40: a debug print was left in\n",
        );
        assert!(r.revise);
        assert_eq!(r.findings.len(), 2);
        assert_eq!(r.findings[0], "src/cart.rs:12: refunds are never written");
    }

    #[test]
    fn pass_and_noise_do_not_send_the_agent_anywhere() {
        assert!(!parse("VERDICT: pass").revise);
        assert!(!parse("**Verdict:** pass\n- looks good").revise);
        assert!(parse("**VERDICT: revise**\n- a.rs:1: broken").revise);
        // Revise with nothing concrete to act on is not a revise.
        assert!(!parse("VERDICT: revise\nIt could be cleaner.").revise);
        // Unreadable is a pass, not a stall.
        assert!(!parse("I think this is mostly fine.").revise);
    }

    #[test]
    fn requirement_bullets_written_before_the_verdict_are_not_findings() {
        // Shape of a real 27B answer: reasoning with bullets, then the verdict.
        let text = "The request asks for:\n   - Returns names of items below threshold\n   \
                    - Sorted by qty then name\n\nProblems:\n1. Sorting is wrong\n\n\
                    VERDICT: revise\n- stock.py:8: sorts by name only\n\
                    - stock.py:7: None qty raises TypeError\n";
        let r = parse(text);
        assert!(r.revise);
        assert_eq!(
            r.findings,
            vec![
                "stock.py:8: sorts by name only".to_string(),
                "stock.py:7: None qty raises TypeError".to_string()
            ]
        );
    }

    #[test]
    fn at_most_five_findings_go_back() {
        let text = format!(
            "VERDICT: revise\n{}",
            (1..=9)
                .map(|i| format!("- f.rs:{i}: problem {i}\n"))
                .collect::<String>()
        );
        assert_eq!(parse(&text).findings.len(), MAX_FINDINGS);
    }

    #[tokio::test]
    async fn the_diff_covers_new_files_and_commits_but_not_bookkeeping() {
        let dir = tempfile::tempdir().unwrap();
        let sh = |cmd: &str| {
            let ok = std::process::Command::new("sh")
                .args(["-c", cmd])
                .current_dir(dir.path())
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "{cmd}");
        };
        sh(
            "git init -q && git config user.email t@t && git config user.name t \
            && echo one > a.txt && git add a.txt && git commit -qm init",
        );
        let before = snapshot(dir.path()).await.unwrap();
        assert!(
            diff(&before, &snapshot(dir.path()).await.unwrap(), 10_000)
                .await
                .is_empty()
        );

        // A new file, an edit committed mid-turn, and Worksmith's own files.
        sh(
            "echo new > b.txt && echo two > a.txt && git commit -qam edit \
            && mkdir -p .worksmith && echo x > .worksmith/memory.db",
        );
        let after = snapshot(dir.path()).await.unwrap();
        let d = diff(&before, &after, 10_000).await;
        assert!(d.contains("+new"), "{d}");
        assert!(d.contains("-one") && d.contains("+two"), "{d}");
        assert!(!d.contains(".worksmith"), "{d}");
        // The person's index was not touched: b.txt is still untracked.
        let status = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&status.stdout).contains("?? b.txt"));

        let cut = diff(&before, &after, 20).await;
        assert!(cut.contains("[diff cut here"), "{cut}");
    }

    #[tokio::test]
    async fn outside_git_there_is_no_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        assert!(snapshot(dir.path()).await.is_none());
    }
}
