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
    /// Model the resulting workers run on, already resolved.
    pub model: Option<crate::llm::ModelOverride>,
    /// `Some(n)` = exactly n workers; `None` = let the planner decide.
    pub want: Option<usize>,
    pub system: String,
    /// The per-worker check, carried through the planner so a planned fan-out
    /// is validated the same as an explicit one.
    pub validate: Option<String>,
}

/// Take one flag value off the front of `rest`: a quoted string, or a single
/// whitespace-delimited token.
///
/// `--until` is a *shell command* and every real one is multi-word — `cargo
/// test`, `zola check`, `npm run lint`. Taking a single token meant
/// `--until "cd docs && zola check"` set the check to the literal `"cd` and
/// silently swallowed the rest into the task text. The failure surfaced fifteen
/// steps later inside a worker as `bash: unexpected EOF while looking for
/// matching "`, which reads as the task failing rather than the harness never
/// having run a check at all.
///
/// Deliberately not a shell lexer: no escapes, no nesting. It takes a quoted
/// run verbatim, which is what a person typing a command into a prompt means.
fn take_value(rest: &str) -> Result<(String, &str), String> {
    let rest = rest.trim_start();
    let Some(quote) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') else {
        return Ok(match rest.split_once(char::is_whitespace) {
            Some((v, a)) => (v.to_string(), a.trim_start()),
            None => (rest.to_string(), ""),
        });
    };
    let body = &rest[quote.len_utf8()..];
    match body.find(quote) {
        Some(end) => Ok((body[..end].to_string(), body[end + quote.len_utf8()..].trim_start())),
        // Loudly, and now: the alternative is a broken command discovered by a
        // worker minutes later, wearing the costume of a failing task.
        None => Err(format!("has an unterminated {quote} quote")),
    }
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
    /// `--model provider/model` — run these workers on something else.
    pub model: Option<String>,
    /// A success check this worker must pass — the same contract as the
    /// session's `--until`, applied per worker.
    pub validate: Option<String>,
}

pub const SPAWN_USAGE: &str =
    "usage: /spawn [-n N | --each-files <regex>] [--model <provider/model>] \
     [--until <check>] <task>\n\
     Quote a multi-word check: --until \"cargo test\". A fan-out's check runs in \
     every worker at once, in one directory, so it must be read-only — \
     `zola check`, not `zola build`.";

/// Parse leading flags off a `/spawn` line; everything after them is the task,
/// verbatim. Flags take a single token, so no quoting rules are needed.
pub fn parse_spawn(args: &str, default_auto: bool) -> Result<SpawnRequest, String> {
    let mut fanout = if default_auto { FanOut::Auto } else { FanOut::Count(1) };
    let mut explicit = false;
    let mut model: Option<String> = None;
    let mut validate: Option<String> = None;
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
        let (value, after) = take_value(after).map_err(|e| format!("/spawn: {flag} {e}"))?;
        let value = value.as_str();
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
            "--until" | "-u" => {
                if value.is_empty() {
                    return Err("/spawn: --until wants a shell command".into());
                }
                // The same regexes `bash` already refuses, applied at the one
                // other place worksmith runs a shell. Not a new gate: a
                // validation command runs unattended after every turn *and*
                // every retry, so a mistyped one runs on a loop, and a
                // validator cannot stop to ask. Refusing here is the only
                // moment it can be reported to the person who typed it.
                if let Some(reason) = crate::tools::dangerous_command(value) {
                    return Err(format!("/spawn: --until refused — {reason}"));
                }
                validate = Some(value.to_string());
            }
            "--model" | "-m" => {
                if value.is_empty() {
                    return Err("/spawn: --model wants a `provider/model` spec".into());
                }
                model = Some(value.to_string());
            }
            other => return Err(format!("/spawn: unknown flag `{other}`\n{SPAWN_USAGE}")),
        }
        rest = after;
    }

    if rest.trim().is_empty() {
        return Err(SPAWN_USAGE.to_string());
    }
    Ok(SpawnRequest { fanout, task: rest.trim().to_string(), model, validate })
}

/// Is this a self-contained instruction, or the wreckage of a model that
/// didn't write one?
///
/// The checks are deliberately about *form*, not subject matter — a worker's
/// task can be about anything, so nothing here may assume what the work is.
/// Two signals survive that constraint:
///
/// - **Elision.** A planner that writes `Read ... and write draft-1.md ...`
///   has abbreviated instead of instructing. The worker that received exactly
///   that ran one `ls` and reported success.
/// - **Length.** Below a dozen characters there isn't an instruction there.
///
/// Duplicates are dropped by the caller for the same reason: N copies of one
/// line is a planner that failed to divide anything.
fn usable_subtask(task: &str) -> bool {
    const MIN_CHARS: usize = 12;
    if task.chars().count() < MIN_CHARS {
        return false;
    }
    if task.contains("...") || task.contains('…') {
        return false;
    }
    true
}

/// `TASK: do the thing` → `do the thing`, case-insensitively. `None` when the
/// line isn't marked as a task at all.
fn strip_task_marker(line: &str) -> Option<&str> {
    let lower = line.to_ascii_lowercase();
    if lower.starts_with("task:") {
        return Some(&line["task:".len()..]);
    }
    // Bold markdown is the most common way a model "follows" the format.
    for prefix in ["**task:**", "**task**:", "`task:`"] {
        if lower.starts_with(prefix) {
            return Some(&line[prefix.len()..]);
        }
    }
    None
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

/// What a planning attempt produced, and how.
///
/// The `note` exists because three fan-outs in a row went wrong and there was
/// no way to tell whether the planner had failed or the guard had rejected good
/// output — `Agent::ask` emits no events and writes to no session, so the raw
/// response was invisible and every diagnosis was an inference from downstream
/// symptoms. A surprising fan-out should be explainable without a rebuild.
pub struct FanOutPlan {
    pub tasks: Vec<String>,
    pub note: String,
}

/// Ask the model to divide a request into independent worker tasks. Falls back
/// to something sensible rather than failing the spawn: a fan-out that can't be
/// planned is still work the user asked for.
pub async fn plan_fanout(
    agent: Arc<Agent>,
    task: String,
    want: Option<usize>,
    max: usize,
) -> FanOutPlan {
    // Every line must carry a marker. "No preamble" is a request a weak model
    // ignores — a local 27B once answered with its own deliberation and we
    // spawned three workers whose tasks were fragments of its thinking. A
    // required prefix makes the shape checkable instead of hoped for.
    // The concurrency constraint has to be stated. Left implicit, a *better*
    // model gives a worse fan-out: Kimi K3 split a request into read → write →
    // review, which is a correct decomposition and useless here, because all
    // three would run at once and the reviewer would find nothing to review.
    let rules = "The tasks all run AT THE SAME TIME, in the same directory, and cannot talk \
                 to each other. So: no task may depend on another task's output, no task may \
                 be a step that only makes sense after another finishes, and no two tasks may \
                 write the same file. If the work is a sequence of phases rather than parallel \
                 pieces, do not describe the phases — split the largest parallel part instead. \
                 Begin each task with what makes it DIFFERENT from the others; shared setup \
                 (which files to read, context to gather) goes at the end. Tasks that open with \
                 the same words are indistinguishable in a list, however different their ends.";
    let system = match want {
        Some(n) => format!(
            "You split a work request into exactly {n} tasks for {n} independent \
             background workers. If the request contains {n} distinct pieces of work, \
             write one per line. If it does not divide that way, write {n} variations \
             on the same goal, each taking a different angle.\n\n{rules}\n\n\
             Output exactly {n} lines. Every line MUST begin with `TASK: ` followed by \
             one self-contained instruction. Write nothing else — no reasoning, no \
             numbering, no preamble, no blank lines."
        ),
        None => format!(
            "You decide whether a work request should be split across independent \
             background workers. Output ONE line — the request restated — unless it \
             obviously contains separate pieces of work that can run in parallel \
             without editing the same files; only then output one line per piece, at \
             most {max}. Prefer one line when unsure.\n\n\
             Every line MUST begin with `TASK: ` followed by one self-contained \
             instruction. Write nothing else — no reasoning, no numbering, no preamble."
        ),
    };

    let want_n = want.unwrap_or(max);
    // Generous, because a thinking model reasons inside this budget and an
    // exhausted one returns nothing at all.
    match agent.ask(&system, &task, 2048).await {
        Ok(text) => match parse_subtasks(&text, want, max) {
            Ok(tasks) => {
                // Say whose idea the count was. "planner split the work into 2"
                // reads as a decision the planner made, and with `-n` it is not:
                // the count is fixed before the model sees the request, and the
                // prompt tells it to invent variations when the work does not
                // divide that way. Reporting both cases identically invites the
                // user to blame the planner for obeying them.
                let note = match want {
                    Some(n) => format!("split into {n} task(s), as you asked"),
                    None => format!("planner chose to split this into {} task(s)", tasks.len()),
                };
                FanOutPlan { tasks, note }
            }
            Err(why) => {
                let tasks = fallback_tasks(&task, want_n, want.is_some());
                FanOutPlan {
                    note: format!(
                        "planner output unusable ({why}); running {} variant(s) of the original \
                         request instead. Planner said: {}",
                        tasks.len(),
                        first_chars(&text, 400)
                    ),
                    tasks,
                }
            }
        },
        Err(e) => {
            let tasks = fallback_tasks(&task, want_n, want.is_some());
            FanOutPlan {
                note: format!(
                    "planner call failed ({e}); running {} variant(s) of the original request",
                    tasks.len()
                ),
                tasks,
            }
        }
    }
}

/// A one-line peek at model output for a diagnostic message.
fn first_chars(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    format!("{}…", flat.chars().take(max).collect::<String>())
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

/// Take only the lines the planner marked as tasks.
///
/// Anything without the `TASK:` marker is discarded, which is the whole point:
/// a model that prefaces its answer with reasoning would otherwise have that
/// reasoning spawned as work. If nothing is marked, we say so and the caller
/// falls back rather than inventing tasks out of prose.
pub fn parse_subtasks(text: &str, want: Option<usize>, max: usize) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        // Tolerate a leading list marker before TASK:, since models love them.
        let line = line
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .trim_start_matches(['.', ')', '-', '*', ' '])
            .trim();
        let Some(rest) = strip_task_marker(line) else {
            continue;
        };
        // The pre-trim above can leave a stray `**` from bold markers.
        let rest = rest.trim_start_matches(['*', '`', ':', ' ']).trim();
        if usable_subtask(rest) && !out.iter().any(|t| t == rest) {
            out.push(rest.to_string());
        }
    }
    if out.is_empty() {
        return Err("planner returned no `TASK:` lines".into());
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
    fn until_takes_a_whole_shell_command() {
        // The bug: a single-token value made the check the literal `"cd` and
        // swallowed the rest of the command into the task, so the fan-out ran
        // unchecked and said nothing about it.
        let r = parse_spawn(
            r#"-n 3 --until "cd docs && zola check --skip-external-links" Write the docs"#,
            true,
        )
        .unwrap();
        assert_eq!(r.validate.as_deref(), Some("cd docs && zola check --skip-external-links"));
        assert_eq!(r.task, "Write the docs", "the task is not eaten by the flag");
        assert!(matches!(r.fanout, FanOut::Count(3)));
    }

    #[test]
    fn quoting_is_optional_and_both_quotes_work() {
        let bare = parse_spawn("--until cargo do the thing", false).unwrap();
        assert_eq!(bare.validate.as_deref(), Some("cargo"));
        assert_eq!(bare.task, "do the thing");

        let single = parse_spawn("--until 'cargo test --all' do the thing", false).unwrap();
        assert_eq!(single.validate.as_deref(), Some("cargo test --all"));

        // Both flags quoted, in either order, with the task intact after.
        let both = parse_spawn(
            r#"--model "openrouter/qwen/qwen3.8-27b" --until "cargo test" fix it"#,
            false,
        )
        .unwrap();
        assert_eq!(both.model.as_deref(), Some("openrouter/qwen/qwen3.8-27b"));
        assert_eq!(both.validate.as_deref(), Some("cargo test"));
        assert_eq!(both.task, "fix it");
    }

    #[test]
    fn an_unterminated_quote_is_a_parse_error_not_a_broken_command() {
        // Loudly and now, rather than as `bash: unexpected EOF` fifteen steps
        // deep in a worker, wearing the costume of a failing task.
        let Err(e) = parse_spawn(r#"--until "cargo test do the thing"#, false) else {
            panic!("an unterminated quote must not parse")
        };
        assert!(e.contains("unterminated"), "{e}");
        assert!(e.contains("--until"), "says which flag: {e}");
    }

    #[test]
    fn a_check_that_bash_would_refuse_is_refused_here_too() {
        // A validation command runs unattended after every turn and every
        // retry, and cannot stop to ask — so parse time is the only moment a
        // refusal reaches the person who typed it.
        let Err(e) = parse_spawn(r#"--until "rm -rf /" do the thing"#, false) else {
            panic!("a refused command must not parse")
        };
        assert!(e.contains("refused"), "{e}");

        // And an ordinary check is untouched.
        assert!(parse_spawn(r#"--until "cargo test" go"#, false).is_ok());
    }

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
    fn elided_and_degenerate_subtasks_are_refused() {
        // A planner answered with placeholders and three workers dutifully ran
        // them. The checks are about form only — nothing here may assume the
        // task is about newsletters, code, or anything else.
        let elided = "TASK: Read ... and write draft-1.md ...\n\
                      TASK: Read ... and write draft-2.md ...\n\
                      TASK: Read ... and write draft-3.md ...";
        assert!(
            parse_subtasks(elided, Some(3), 4).is_err(),
            "elided tasks must fall back, not spawn"
        );

        // Too short to be an instruction.
        assert!(parse_subtasks("TASK: do it\nTASK: go\nTASK: x", Some(3), 4).is_err());

        // N copies of one line is a planner that divided nothing.
        let dupes = "TASK: Summarize the quarterly figures\n\
                     TASK: Summarize the quarterly figures\n\
                     TASK: Summarize the quarterly figures";
        assert!(parse_subtasks(dupes, Some(3), 4).is_err(), "duplicates aren't a fan-out");

        // A mix keeps the good ones but can't satisfy an explicit -n 3.
        let mixed = "TASK: Audit the retry policy in the payments client\n\
                     TASK: ...\n\
                     TASK: Document the queue's backpressure behaviour";
        assert!(parse_subtasks(mixed, Some(3), 4).is_err());
        let kept = parse_subtasks(mixed, None, 4).unwrap();
        assert_eq!(kept.len(), 2, "auto mode uses what survived: {kept:?}");

        // And nothing above rejects an ordinary terse task.
        let fine = "TASK: Fix the failing test in parser.rs";
        assert_eq!(parse_subtasks(fine, Some(1), 4).unwrap().len(), 1);
    }

    #[test]
    fn planner_reasoning_is_never_spawned_as_work() {
        // A local 27B answered the planner with its own deliberation, and every
        // line of it became a worker's task. Unmarked prose must be discarded.
        let text = "We need decide how to split. The request has: read the skill, write three \
                    drafts, review and pick one.\n\
                    Based on your description, here's the likely 3-task split:\n\
                    TASK: Write a newsletter draft about logging\n\
                    TASK: Write a newsletter draft about CI pipelines\n\
                    TASK: Write a newsletter draft about caching\n\
                    Let me know if you'd like me to adjust these.";
        let got = parse_subtasks(text, Some(3), 4).unwrap();
        assert_eq!(got.len(), 3, "only the marked lines: {got:?}");
        assert!(got[0].starts_with("Write a newsletter draft about logging"));
        assert!(
            !got.iter().any(|t| t.contains("We need decide")),
            "reasoning must not become a task: {got:?}"
        );

        // Nothing marked at all is a failure, not an excuse to invent tasks.
        let err = parse_subtasks("Here are three ideas:\n1. logging\n2. CI\n3. caching", Some(3), 4);
        assert!(err.is_err(), "unmarked prose is unusable, got {err:?}");
    }

    #[test]
    fn task_markers_survive_the_formatting_models_add() {
        let want = "run the whole test suite";
        for line in [
            "TASK: run the whole test suite",
            "task: run the whole test suite",
            "- TASK: run the whole test suite",
            "2. TASK: run the whole test suite",
            "**TASK:** run the whole test suite",
        ] {
            let got = parse_subtasks(line, Some(1), 4).unwrap();
            assert_eq!(got, vec![want.to_string()], "failed on {line:?}");
        }
    }

    #[test]
    fn subtasks_parse_out_of_list_formatting() {
        
        let text = "1. TASK: write about WAL\n2) TASK: write about FTS5\n- TASK: write about JSON1\n\n";
        let got = parse_subtasks(text, Some(3), 4).unwrap();
        assert_eq!(got, vec!["write about WAL", "write about FTS5", "write about JSON1"]);

        // Auto mode clamps to the worker cap.
        let many =
            (1..=9).map(|i| format!("TASK: investigate subsystem {i}")).collect::<Vec<_>>().join("\n");
        assert_eq!(parse_subtasks(&many, None, 4).unwrap().len(), 4);

        // One line back from auto mode is a normal single spawn.
        assert_eq!(parse_subtasks("TASK: just run the linter", None, 4).unwrap().len(), 1);

        // Explicit -n that the planner under-delivered on is unusable.
        assert!(parse_subtasks("TASK: only one instruction here", Some(3), 4).is_err());
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
