//! Fan-out: turning one `/spawn` into several workers.
//!
//! Lives outside the TUI because both front-ends spawn workers — the parsing,
//! task assembly, and planner are frontend-agnostic. See PLAN.md §7.

use std::path::Path;
use std::sync::Arc;

use crate::agent::Agent;
use crate::report::truncate;
use crate::worker::{FanOutReport, SpawnOutcome};

/// A fan-out waiting on the planner. Held by the front-end and run off the UI
/// task, because the planner is a model call and must not block rendering.
pub struct PendingFanOut {
    pub task: String,
    /// `Some(n)` = exactly n workers; `None` = let the planner decide.
    pub want: Option<usize>,
    pub system: String,
}

/// How `/spawn` was asked to divide the work.
pub enum FanOut {
    /// Planner decides how many (usually one).
    Auto,
    /// `-n N` — exactly N workers.
    Count(usize),
    /// `--each-files <regex>` — one worker per matching file, no model call.
    Files(String),
}

pub struct SpawnRequest {
    pub fanout: FanOut,
    pub task: String,
}

pub const SPAWN_USAGE: &str =
    "usage: /spawn [-n N | --each-files <regex>] <task>";

/// Parse leading flags off a `/spawn` line; everything after them is the task,
/// verbatim. Flags take a single token, so no quoting rules are needed.
pub fn parse_spawn(args: &str, default_auto: bool) -> Result<SpawnRequest, String> {
    let mut fanout = if default_auto { FanOut::Auto } else { FanOut::Count(1) };
    let mut explicit = false;
    let mut rest = args.trim();

    loop {
        let (flag, after) = match rest.split_once(char::is_whitespace) {
            Some((f, a)) => (f, a.trim_start()),
            None => (rest, ""),
        };
        if !flag.starts_with('-') {
            break;
        }
        // `--` ends the flags; everything after it is the task, dashes and all.
        if flag == "--" {
            rest = after;
            break;
        }
        let (value, after) = match after.split_once(char::is_whitespace) {
            Some((v, a)) => (v, a.trim_start()),
            None => (after, ""),
        };
        match flag {
            "-n" | "--count" => {
                if explicit {
                    return Err("/spawn: use either -n or --each-files, not both".into());
                }
                let n: usize = value
                    .parse()
                    .map_err(|_| format!("/spawn: -n wants a number, got `{value}`"))?;
                if n < 1 {
                    return Err("/spawn: -n must be at least 1".into());
                }
                fanout = FanOut::Count(n);
                explicit = true;
            }
            "--each-files" | "--each-file" => {
                if explicit {
                    return Err("/spawn: use either -n or --each-files, not both".into());
                }
                if value.is_empty() {
                    return Err("/spawn: --each-files wants a regex".into());
                }
                fanout = FanOut::Files(value.to_string());
                explicit = true;
            }
            other => return Err(format!("/spawn: unknown flag `{other}`\n{SPAWN_USAGE}")),
        }
        rest = after;
    }

    if rest.trim().is_empty() {
        return Err(SPAWN_USAGE.to_string());
    }
    Ok(SpawnRequest { fanout, task: rest.trim().to_string() })
}

/// A worker's task: your prose, plus what this particular worker is on.
pub fn assign(task: &str, item: &str) -> String {
    format!("{task}\n\nYour assignment: {item}")
}

/// `-n N` fallback when the planner can't be reached: N takes on one goal.
pub fn variant_task(task: &str, i: usize, n: usize) -> String {
    format!(
        "{task}\n\nThis is draft {i} of {n} — take a distinct angle from the other drafts.",
    )
}

/// Files whose *name* matches `pattern`, same contract as the `find` tool.
pub fn matching_files(cwd: &Path, pattern: &str) -> Result<Vec<String>, String> {
    let re = regex::Regex::new(pattern).map_err(|e| format!("invalid regex: {e}"))?;
    let mut files = Vec::new();
    crate::tools::walk(cwd, &mut files, 10_000);
    Ok(files
        .iter()
        .filter(|f| {
            f.file_name().map(|n| re.is_match(&n.to_string_lossy())).unwrap_or(false)
        })
        .map(|f| crate::tools::display_rel(cwd, f))
        .collect())
}

pub fn spawn_notice(outcome: &SpawnOutcome, task: &str) -> String {
    match outcome {
        SpawnOutcome::Started(id) => format!("spawned {id}: {}", truncate(task, 80)),
        SpawnOutcome::Queued(pos) => {
            format!("queued (#{pos}, all slots busy): {}", truncate(task, 80))
        }
    }
}

pub fn fanout_notice(report: &FanOutReport) -> String {
    let started = if report.started.is_empty() {
        "nothing started".to_string()
    } else {
        format!("spawned {}", report.started.join(" "))
    };
    if report.queued > 0 {
        format!("{started} · {} queued", report.queued)
    } else {
        started
    }
}

/// Ask the model to divide a request into independent worker tasks. Falls back
/// to something sensible rather than failing the spawn: a fan-out that can't be
/// planned is still work the user asked for.
pub async fn plan_fanout(
    agent: Arc<Agent>,
    task: String,
    want: Option<usize>,
    max: usize,
) -> Vec<String> {
    let system = match want {
        Some(n) => format!(
            "You split a work request into exactly {n} tasks for {n} independent \
             background workers. If the request contains {n} distinct pieces of work, \
             write one per line. If it does not divide that way, write {n} variations \
             on the same goal, each taking a different angle. Output exactly {n} lines, \
             one self-contained instruction per line, no numbering, no preamble, no \
             blank lines."
        ),
        None => format!(
            "You decide whether a work request should be split across independent \
             background workers. Output ONE line — the request restated — unless it \
             obviously contains separate pieces of work that can run in parallel \
             without editing the same files; only then output one self-contained \
             instruction per piece, at most {max} lines. Prefer one line when unsure. \
             No numbering, no preamble, no blank lines."
        ),
    };

    let want_n = want.unwrap_or(max);
    match agent.ask(&system, &task, 1024).await {
        Ok(text) => match parse_subtasks(&text, want, max) {
            Ok(tasks) => tasks,
            Err(_) => fallback_tasks(&task, want_n, want.is_some()),
        },
        Err(_) => fallback_tasks(&task, want_n, want.is_some()),
    }
}

/// No planner (call failed or unusable output): honour an explicit `-n N` with
/// variants, otherwise just run the task once, unchanged.
pub fn fallback_tasks(task: &str, n: usize, explicit: bool) -> Vec<String> {
    if explicit && n > 1 {
        (1..=n).map(|i| variant_task(task, i, n)).collect()
    } else {
        vec![task.to_string()]
    }
}

/// One instruction per line: strip list markers, drop blanks, take what we
/// asked for (or clamp to the cap in auto mode).
pub fn parse_subtasks(text: &str, want: Option<usize>, max: usize) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let cleaned = line
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .trim_start_matches(['.', ')', '-', '*', ':'])
            .trim();
        let cleaned = if cleaned.is_empty() { line } else { cleaned };
        out.push(cleaned.to_string());
    }
    if out.is_empty() {
        return Err("planner returned nothing usable".into());
    }
    match want {
        // Too few lines for an explicit -n: not usable, let the caller fall back.
        Some(n) if out.len() < n => Err(format!("planner returned {} of {n} tasks", out.len())),
        Some(n) => {
            out.truncate(n);
            Ok(out)
        }
        None => {
            out.truncate(max.max(1));
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_parses_flags_then_takes_the_rest_verbatim() {
        

        let r = parse_spawn("write 3 articles about sqlite", true).unwrap();
        assert!(matches!(r.fanout, FanOut::Auto));
        assert_eq!(r.task, "write 3 articles about sqlite");

        // fanout = "off" makes a bare /spawn a single worker.
        let r = parse_spawn("refactor the parser", false).unwrap();
        assert!(matches!(r.fanout, FanOut::Count(1)));

        let r = parse_spawn("-n 3 draft an intro", true).unwrap();
        assert!(matches!(r.fanout, FanOut::Count(3)));
        assert_eq!(r.task, "draft an intro");

        let r = parse_spawn("--each-files .*\\.md proofread this file", true).unwrap();
        match r.fanout {
            FanOut::Files(p) => assert_eq!(p, ".*\\.md"),
            _ => panic!("expected a file fan-out"),
        }
        assert_eq!(r.task, "proofread this file");

        // `--` ends the flags, so a task may start with a dash.
        let r = parse_spawn("-n 2 -- --write the changelog", true).unwrap();
        assert!(matches!(r.fanout, FanOut::Count(2)));
        assert_eq!(r.task, "--write the changelog");
    }

    #[test]
    fn spawn_rejects_bad_input() {
        
        assert!(parse_spawn("", true).is_err(), "no task");
        assert!(parse_spawn("-n 3", true).is_err(), "flags but no task");
        assert!(parse_spawn("-n zero do it", true).is_err(), "non-numeric count");
        assert!(parse_spawn("-n 0 do it", true).is_err(), "count below 1");
        assert!(parse_spawn("--wat do it", true).is_err(), "unknown flag");
        assert!(
            parse_spawn("-n 2 --each-files x do it", true).is_err(),
            "-n and --each-files conflict"
        );
    }

    #[test]
    fn assignment_is_appended_not_interpolated() {
        
        let t = assign("proofread this file", "docs/indexing.md");
        assert!(t.starts_with("proofread this file"), "original prose is preserved");
        assert!(t.ends_with("Your assignment: docs/indexing.md"));
        assert!(variant_task("draft an intro", 2, 3).contains("draft 2 of 3"));
    }

    #[test]
    fn subtasks_parse_out_of_list_formatting() {
        
        let text = "1. write about WAL\n2) write about FTS5\n- write about JSON1\n\n";
        let got = parse_subtasks(text, Some(3), 4).unwrap();
        assert_eq!(got, vec!["write about WAL", "write about FTS5", "write about JSON1"]);

        // Auto mode clamps to the worker cap.
        let many = (1..=9).map(|i| format!("task {i}")).collect::<Vec<_>>().join("\n");
        assert_eq!(parse_subtasks(&many, None, 4).unwrap().len(), 4);

        // One line back from auto mode is a normal single spawn.
        assert_eq!(parse_subtasks("just do it", None, 4).unwrap().len(), 1);

        // Explicit -n that the planner under-delivered on is unusable.
        assert!(parse_subtasks("only one", Some(3), 4).is_err());
        assert!(parse_subtasks("   \n\n", None, 4).is_err(), "nothing usable");
    }

    #[test]
    fn fallback_honours_an_explicit_count() {
        
        // Planner unreachable + `-n 3` → three variants, not a dropped request.
        let tasks = fallback_tasks("draft an intro", 3, true);
        assert_eq!(tasks.len(), 3);
        assert!(tasks[0].contains("draft 1 of 3"));
        // Auto mode falls back to a single worker with the original text.
        assert_eq!(fallback_tasks("refactor the parser", 4, false), vec!["refactor the parser"]);
    }
}
