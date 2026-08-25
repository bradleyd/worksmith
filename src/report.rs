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

/// The one-line transcript announcement for a finished worker.
pub fn worker_headline(w: &WorkerSummary) -> String {
    let glyph = match w.status {
        WorkerStatus::Done => "✓",
        WorkerStatus::Failed => "✗",
        _ => "◼",
    };
    let summary = if w.result.trim().is_empty() {
        w.last.clone()
    } else {
        truncate(&w.result, 300)
    };
    let changed = if w.changed.is_empty() {
        String::new()
    } else {
        format!(" · changed {} file(s): {}", w.changed.len(), w.changed.join(", "))
    };
    let stopped = match &w.escalation {
        Some(reason) => format!(" · supervisor stopped it ({reason})"),
        None => String::new(),
    };
    let empty = if w.did_nothing() { " · produced nothing" } else { "" };
    format!(
        "{glyph} agent {} [{}]{}{}{}: {}",
        w.id,
        w.status.label(),
        changed,
        stopped,
        empty,
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
