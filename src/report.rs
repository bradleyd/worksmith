//! Reporting a finished worker back to the parent.
//!
//! Formatting lives here rather than in a front-end because both the TUI and
//! the plain REPL have to answer the same question — what did the worker do,
//! and how much of it can the parent afford to read?

use crate::worker::{WorkerStatus, WorkerSummary};

/// A fan-out group collecting its members' results before reporting as a unit.
pub struct GroupAcc {
    pub group: u64,
    pub request: String,
    pub total: usize,
    pub done: Vec<WorkerSummary>,
}

/// Split a fan-out's tasks into the wording they share and what is left.
///
/// Tasks must be self-contained — workers cannot talk to each other — so every
/// one of them repeats the same setup ("Read X and Y, then draft…"). Listed
/// verbatim and truncated to fit, they come out identical on screen and a
/// perfectly good split reads as a planner that lost its mind. Say the shared
/// part once and let the tails be the tails.
///
/// Returns `None` when there is nothing worth sharing — one task, or too little
/// in common to be worth a line of its own.
pub fn common_opening(tasks: &[String]) -> Option<(String, Vec<String>)> {
    /// Below this a shared opening is not what is crowding out the difference,
    /// and hoisting it costs a line to save a few characters.
    const WORTH_HOISTING: usize = 24;

    if tasks.len() < 2 {
        return None;
    }
    let first: Vec<char> = tasks[0].chars().collect();
    let mut n = first.len();
    for t in &tasks[1..] {
        n = n.min(
            t.chars().zip(first.iter()).take_while(|(a, b)| a == *b).count(),
        );
        if n == 0 {
            return None;
        }
    }
    // Cut on a word boundary: half a word shared and half repeated reads worse
    // than not hoisting at all.
    while n > 0 && !first[n - 1].is_whitespace() {
        n -= 1;
    }
    let shared: String = first[..n].iter().collect();
    let shared = shared.trim_end().to_string();
    if shared.chars().count() < WORTH_HOISTING {
        return None;
    }
    let tails = tasks
        .iter()
        .map(|t| t.chars().skip(n).collect::<String>().trim_start().to_string())
        .collect();
    Some((shared, tails))
}

/// Record a finished worker against its group, and hand back the whole group
/// once every member has reported.
///
/// `total` is re-read from the manager on every call rather than trusted from
/// when the group was created. `/agents drop-queued` lowers a group's expected
/// count *after* it has begun reporting, and a snapshot taken at first-finish
/// left the group waiting on members that no longer existed — no report, no
/// synthesis, no error, just silence. `WorkerManager::drop_queued` decrements
/// precisely to prevent that, and the decrement never reached the accumulator.
pub fn record_in_group(
    groups: &mut Vec<GroupAcc>,
    group: u64,
    request: &str,
    total: usize,
    worker: WorkerSummary,
) -> Option<GroupAcc> {
    let idx = match groups.iter().position(|a| a.group == group) {
        Some(i) => i,
        None => {
            groups.push(GroupAcc {
                group,
                request: request.to_string(),
                total,
                done: Vec::new(),
            });
            groups.len() - 1
        }
    };
    let acc = &mut groups[idx];
    acc.total = total;
    acc.done.push(worker);
    // `>=`, not `==`: a count that drops below what already reported must still
    // complete rather than sail past the equality and hang.
    if acc.done.len() >= acc.total {
        Some(groups.swap_remove(idx))
    } else {
        None
    }
}

/// What a finished worker leaves in the transcript.
///
/// Reported three times from use, most bluntly as: *"I feel like as a user I
/// might not know what to do after this."* The old version led with the
/// model's own prose — up to 300 characters of it — and never printed the one
/// fact that decides what happens next: **whether the check passed**. That was
/// visible only to whoever happened to be tailing the worker.
///
/// So: the facts first, in the order a reader needs them — did it pass, what
/// did it touch, how long did it take — then a line saying what to do about it,
/// and the model's summary last and shorter. The harness already knows all of
/// this; none of it needed a new subsystem, only a decision about what matters.
pub fn worker_headline(w: &WorkerSummary) -> String {
    let glyph = match (w.check_passed, w.status) {
        (Some(true), _) => "✓",
        (Some(false), _) => "✗",
        (None, WorkerStatus::Done) => "✓",
        (None, WorkerStatus::Failed) => "✗",
        (None, _) => "◼",
    };

    // The check outranks the status label: "stopped" with a passing check and
    // "done" with a failing one are both real, and the check is the one that
    // says whether the work is usable.
    let verdict = match w.check_passed {
        Some(true) => " · check passed".to_string(),
        Some(false) => " · CHECK FAILED".to_string(),
        None => String::new(),
    };

    let took = match (w.finished, w.started) {
        (Some(end), start) => match end.duration_since(start) {
            Ok(d) if d.as_secs() < 60 => format!(" in {}s", d.as_secs()),
            Ok(d) => format!(" in {}m{:02}s", d.as_secs() / 60, d.as_secs() % 60),
            Err(_) => String::new(),
        },
        _ => String::new(),
    };

    let changed = if w.changed.is_empty() {
        " · changed nothing".to_string()
    } else {
        format!(" · changed {}", w.changed.join(", "))
    };
    let stopped = match &w.escalation {
        Some(reason) => format!(" · supervisor stopped it ({reason})"),
        None => String::new(),
    };
    let empty = if w.did_nothing() { " · produced nothing" } else { "" };

    // What to actually do about it. A finished worker is a decision point, and
    // the two things a reader wants are the diff and the full result.
    let mut next: Vec<String> = Vec::new();
    if !w.changed.is_empty() {
        next.push(format!("git diff {}", w.changed.join(" ")));
    }
    next.push(format!("/agents show {}", w.id));
    if w.check_passed == Some(false) || w.escalation.is_some() {
        next.push(format!("/agents tail {}", w.id));
    }

    let summary = if w.result.trim().is_empty() {
        w.last.clone()
    } else {
        truncate(&w.result, 160)
    };

    format!(
        "{glyph} agent {} [{}]{took}{verdict}{changed}{stopped}{empty}\n  → {}\n  {}",
        w.id,
        w.status.label(),
        next.join("  ·  "),
        summary
    )
}

/// What a worker actually produced, as the parent model should see it. Results
/// are capped: several verbose workers must not blow the parent's context.
pub fn worker_block(w: &WorkerSummary) -> String {
    let mut out = format!("[{}] {} — task: {}", w.id, w.status.label(), w.task);
    if !w.changed.is_empty() {
        out.push_str(&format!("\nfiles changed: {}", w.changed.join(", ")));
    }
    if let Some(reason) = &w.escalation {
        out.push_str(&format!("\nstopped by supervisor: {reason}"));
    }
    // Say it plainly rather than leaving the parent to infer it from an absent
    // file list. It judges what came back; this is part of what came back.
    if w.did_nothing() {
        out.push_str(
            "\nWARNING: reported done but changed no files and returned almost no text. \
             Treat this result as unverified — the work may not have happened.",
        );
    }
    let body = if w.result.trim().is_empty() { &w.last } else { &w.result };
    out.push_str(&format!("\n{}", truncate_chars(body, WORKER_REPORT_LIMIT)));
    out
}

/// Per-worker cap on what gets injected into the parent's history.
pub const WORKER_REPORT_LIMIT: usize = 4_000;

pub fn single_report(w: &WorkerSummary) -> String {
    format!("A background worker you spawned finished.\n\n{}", worker_block(w))
}

pub fn group_report(acc: &GroupAcc) -> String {
    let mut out = format!(
        "The {} background workers for this request finished: {}\n",
        acc.done.len(),
        acc.request
    );
    for w in &acc.done {
        out.push_str(&format!("\n{}\n", worker_block(w)));
    }
    out
}


/// Cap a block of text on a char boundary, keeping its line structure (unlike
/// [`truncate`], which flattens to one line for status rows).
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max).collect();
    format!("{kept}\n[…truncated]")
}

pub fn truncate(s: &str, max: usize) -> String {
    let one = s.replace('\n', " ");
    if one.chars().count() <= max {
        one
    } else {
        let cut: String = one.chars().take(max).collect();
        format!("{cut}…")
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    // ---- fan-out task display ----

    #[test]
    fn a_shared_opening_is_said_once_instead_of_truncating_every_task() {
        // The real case: two genuinely different tasks that looked identical on
        // screen because the first 55 characters matched and the line was cut
        // at 100. The split was fine; the display made it look broken.
        let tasks = vec![
            "Read CONVENTIONS.md and DOCS_PLAN.md, then draft an outline for a \"Getting Started\" section".to_string(),
            "Read CONVENTIONS.md and DOCS_PLAN.md, then draft an outline for a \"Why This Harness\" section".to_string(),
        ];
        let (shared, tails) = common_opening(&tasks).expect("a long shared opening");
        assert_eq!(shared, "Read CONVENTIONS.md and DOCS_PLAN.md, then draft an outline for a");
        assert_eq!(tails[0], "\"Getting Started\" section");
        assert_eq!(tails[1], "\"Why This Harness\" section");
    }

    #[test]
    fn nothing_is_hoisted_when_there_is_little_or_nothing_in_common() {
        let distinct = vec!["Update the README".to_string(), "Refactor the parser".to_string()];
        assert!(common_opening(&distinct).is_none(), "no shared opening");

        // Shared but too short to be what is crowding the line out.
        let short = vec!["Fix the parser bug".to_string(), "Fix the render bug".to_string()];
        assert!(common_opening(&short).is_none());

        let one = vec!["Only one task here, nothing to compare it against".to_string()];
        assert!(common_opening(&one).is_none());
    }

    #[test]
    fn the_shared_part_is_cut_on_a_word_boundary() {
        // "Draft the intro" / "Draft the index" share "Draft the in" — hoisting
        // mid-word would leave "tro" and "dex".
        let tasks = vec![
            "Read the plan and then draft the introduction section".to_string(),
            "Read the plan and then draft the index section".to_string(),
        ];
        let (shared, tails) = common_opening(&tasks).unwrap();
        assert!(shared.ends_with("draft the"), "cut mid-word: {shared}");
        assert_eq!(tails[0], "introduction section");
        assert_eq!(tails[1], "index section");
    }

    #[test]
    fn multibyte_openings_do_not_split_a_character() {
        let tasks = vec![
            "文档を読んでから、序章の概要を作成する".to_string(),
            "文档を読んでから、目次の概要を作成する".to_string(),
        ];
        // No panic, and whatever comes back reassembles.
        if let Some((shared, tails)) = common_opening(&tasks) {
            assert!(tasks[0].starts_with(&shared) || shared.is_empty());
            assert!(!tails[0].is_empty());
        }
    }

    // ---- group accumulation ----

    #[test]
    fn a_group_whose_queued_members_were_dropped_still_completes() {
        // The hang: fan out 5 with a cap of 3, the first worker finishes (so
        // the group is created expecting 5), then `/agents drop-queued`
        // removes the two still waiting. The remaining two finish, and the
        // group sat at 3-of-5 forever — no report, no synthesis, no error.
        let mut groups: Vec<GroupAcc> = Vec::new();

        assert!(record_in_group(&mut groups, 1, "req", 5, summary("w1", "t", "r")).is_none());
        assert!(record_in_group(&mut groups, 1, "req", 3, summary("w2", "t", "r")).is_none());

        let done = record_in_group(&mut groups, 1, "req", 3, summary("w3", "t", "r"))
            .expect("the lowered count must complete the group");
        assert_eq!(done.done.len(), 3);
        assert!(groups.is_empty(), "a completed group is removed");
    }

    #[test]
    fn a_count_that_drops_below_what_already_reported_still_completes() {
        // `>=` rather than `==`: if the count falls under the number already
        // in hand, an equality check would sail straight past it.
        let mut groups: Vec<GroupAcc> = Vec::new();
        record_in_group(&mut groups, 1, "req", 4, summary("w1", "t", "r"));
        record_in_group(&mut groups, 1, "req", 4, summary("w2", "t", "r"));
        assert!(
            record_in_group(&mut groups, 1, "req", 2, summary("w3", "t", "r")).is_some(),
            "3 reported against a lowered total of 2 must still finish"
        );
    }

    #[test]
    fn groups_accumulate_independently() {
        let mut groups: Vec<GroupAcc> = Vec::new();
        assert!(record_in_group(&mut groups, 1, "a", 2, summary("w1", "t", "r")).is_none());
        assert!(record_in_group(&mut groups, 2, "b", 1, summary("w2", "t", "r")).is_some());
        assert_eq!(groups.len(), 1, "group 1 is untouched by group 2 completing");
        assert!(record_in_group(&mut groups, 1, "a", 2, summary("w3", "t", "r")).is_some());
    }

    // ---- worker results reaching the parent ----

    fn summary(id: &str, task: &str, result: &str) -> WorkerSummary {
        WorkerSummary {
            id: id.into(),
            task: task.into(),
            status: crate::worker::WorkerStatus::Done,
            last: "done".into(),
            tool_calls: 3,
            changed: vec!["notes.md".into()],
            result: result.into(),
            session_id: "s".into(),
            tokens: 0,
            nudges: 0,
            escalation: None,
            group: Some(1),
            model: None,
            started: std::time::SystemTime::now(),
            finished: None,
            prompt_tokens: 0,
            check_passed: None,
        }
    }

    #[test]
    fn a_group_report_carries_every_worker_to_the_parent() {
        
        let acc = GroupAcc {
            group: 1,
            request: "3 articles on sqlite".into(),
            total: 3,
            done: vec![
                summary("w1", "write about WAL", "WAL article done"),
                summary("w2", "write about FTS5", "FTS5 article done"),
                summary("w3", "write about JSON1", "JSON1 article done"),
            ],
        };
        let report = group_report(&acc);
        assert!(report.contains("3 articles on sqlite"), "the original ask is restated");
        for needle in ["w1", "w2", "w3", "WAL article done", "FTS5 article done", "JSON1 article done"] {
            assert!(report.contains(needle), "missing {needle} in:\n{report}");
        }
        assert!(report.contains("notes.md"), "changed files are reported");
    }

    #[test]
    fn a_verbose_worker_cannot_blow_the_parent_context() {
        
        let huge = "x".repeat(WORKER_REPORT_LIMIT * 3);
        let block = worker_block(&summary("w1", "dump everything", &huge));
        assert!(block.chars().count() < WORKER_REPORT_LIMIT + 200, "result must be capped");
        assert!(block.contains("truncated"));
    }

    #[test]
    fn an_empty_result_is_flagged_but_honest_work_is_not() {
        // Only the combination counts. Nothing here may assume what the work
        // was — a research worker that changes no files is doing its job.
        let mut w = summary("w1", "write draft-1.md", "");
        w.changed.clear();
        w.tool_calls = 1;
        assert!(w.did_nothing(), "no files, no text");

        // The case the first version missed: plenty of reading, nothing to
        // show for it. Effort is not output.
        let mut busy = summary("w5", "write draft-2.md", "");
        busy.changed.clear();
        busy.tool_calls = 6;
        assert!(busy.did_nothing(), "read six files, wrote nothing, said nothing");
        assert!(single_report(&w).contains("WARNING"), "the parent must be told");
        assert!(worker_headline(&w).contains("produced nothing"));

        // Answering a question changes no files and is perfectly good work.
        let mut research = summary("w2", "where is retry configured?", &"x".repeat(400));
        research.changed.clear();
        research.tool_calls = 6;
        assert!(!research.did_nothing(), "investigation is work");
        assert!(!single_report(&research).contains("WARNING"));

        // A one-line answer after real searching is still work. Only
        // near-silence counts as nothing.
        let mut terse =
        summary("w3", "does the parser handle CRLF?", "Yes — parser.rs:88 handles CRLF explicitly.");
        terse.changed.clear();
        terse.tool_calls = 5;
        assert!(!terse.did_nothing(), "few words, but it looked");

        // And a worker that wrote a file is never flagged, however quiet.
        let mut wrote = summary("w4", "create the changelog", "");
        wrote.tool_calls = 1;
        assert!(!wrote.did_nothing(), "it changed a file");
    }

    #[test]
    fn a_stopped_worker_reports_why() {
        
        let mut w = summary("w1", "spin", "");
        w.status = crate::worker::WorkerStatus::Stopped;
        w.escalation = Some("token budget exceeded".into());
        let report = single_report(&w);
        assert!(report.contains("stopped by supervisor: token budget exceeded"));
        // With no result text, the last-known line stands in.
        assert!(report.contains("done"));
    }
}

#[cfg(test)]
mod headline_tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn worker() -> WorkerSummary {
        WorkerSummary {
            id: "w1".into(),
            task: "fix /pair".into(),
            status: WorkerStatus::Stopped,
            last: "✓ validated".into(),
            tool_calls: 7,
            changed: vec!["src/tui.rs".into()],
            result: "Modified the /pair command handler.".into(),
            session_id: "s".into(),
            tokens: 100,
            prompt_tokens: 900,
            nudges: 2,
            escalation: Some("still off track after 2 nudges".into()),
            group: None,
            model: None,
            check_passed: Some(true),
            started: SystemTime::now() - Duration::from_secs(492),
            finished: Some(SystemTime::now()),
        }
    }

    /// The exact line that prompted this: a worker that passed its check,
    /// announced as "stopped · supervisor stopped it", with the pass nowhere on
    /// screen and nothing saying what to do next.
    #[test]
    fn a_finished_worker_leads_with_whether_its_check_passed() {
        let h = worker_headline(&worker());
        assert!(h.contains("check passed"), "the deciding fact is present: {h}");
        assert!(h.starts_with('✓'), "and the glyph follows the check, not the status: {h}");
        assert!(h.contains("8m12s"), "how long ago it ran: {h}");
        assert!(h.contains("git diff src/tui.rs"), "what to do about it: {h}");
        assert!(h.contains("/agents show w1"), "where the full result is: {h}");
        // The supervisor's own account survives — it is still true, just no
        // longer the headline.
        assert!(h.contains("still off track"), "{h}");
    }

    #[test]
    fn a_failing_check_says_so_loudly_and_offers_the_tail() {
        let mut w = worker();
        w.check_passed = Some(false);
        w.escalation = None;
        let h = worker_headline(&w);
        assert!(h.contains("CHECK FAILED"), "{h}");
        assert!(h.starts_with('✗'), "{h}");
        assert!(h.contains("/agents tail w1"), "a failure wants the transcript: {h}");
    }

    /// No `--until` means no verdict to report, and the line must not invent
    /// one — "done" without a check is a much weaker claim.
    #[test]
    fn a_worker_with_no_check_claims_nothing_about_one() {
        let mut w = worker();
        w.check_passed = None;
        w.status = WorkerStatus::Done;
        w.escalation = None;
        let h = worker_headline(&w);
        assert!(!h.contains("check"), "{h}");
        assert!(h.starts_with('✓'), "{h}");
    }
}
