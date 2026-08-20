//! Mining past sessions for durable memory.
//!
//! The write side of memory was the half that never worked. Asking the model to
//! volunteer memories mid-turn produced, across 1021 recorded sessions, seven
//! calls — all of them searches. So don't ask mid-turn: read the archive
//! afterwards, when a session is finished and its shape is visible.
//!
//! Everything mined lands as a *proposal* (`worksmith-memory-v1.md` §8), never
//! an active memory. `/memory pending` and `/memory approve` already exist to
//! review them.
//!
//! Mining is **per-project**. A session belongs to the project it ran in, and a
//! lesson learned in one repo is not a global fact — global memory is for things
//! you say are cross-project, not for whatever the last repo happened to teach.
//!
//! Split into three phases because a `MemoryStore` holds SQLite connections and
//! cannot cross a task boundary, while the classifier calls are slow and must
//! not block the UI: [`plan`] and [`record`] touch the database on the caller's
//! thread, and [`classify`] in between is pure model work that can be spawned.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::agent::Agent;
use crate::llm::Role;
use crate::memory::{EXTRACTION_PROMPT, MemoryStore, Scope, parse_candidates};
use crate::session::{Session, session_cwd, sessions_dir};

/// A session this small is a false start, a one-liner, or an abandoned run.
/// Mining them produces noise and costs a model call each.
///
/// Measured against the real archive rather than borrowed: gemini-cli's
/// auto-memory requires 10+ *user* messages, which is right for a chat-shaped
/// agent and wrong for this one. Worksmith sessions are agentic — one
/// instruction, then a long run of tool calls — and across 1021 recorded
/// sessions the median has 1 user message and 4 messages total, with *none*
/// reaching 10 user messages. Total activity is the measure that separates real
/// work from a false start here; 191 of those sessions clear this bar.
const MIN_MESSAGES: usize = 12;

/// How much of a session to show the classifier. The tail is where conclusions
/// live; the head is usually orientation.
const MAX_MESSAGES: usize = 60;

/// Token budget for one classification. The extractor emits one short line per
/// memory and is told to prefer zero, so this is generous.
const CLASSIFY_TOKENS: u32 = 512;

/// What one mining run did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MineReport {
    /// Sessions belonging to this project.
    pub found: usize,
    /// Skipped because they were already mined.
    pub already_mined: usize,
    /// Skipped as too short to be worth a model call.
    pub too_short: usize,
    /// Sessions actually sent to the classifier.
    pub read: usize,
    /// New proposals created.
    pub proposed: usize,
    /// Candidates that duplicated something already stored.
    pub duplicates: usize,
    /// Sessions that could not be read or classified.
    pub failed: Vec<String>,
}

/// One session queued for classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MineItem {
    pub id: String,
    pub transcript: String,
}

/// What [`plan`] decided: the work to do, and the accounting already settled.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MinePlan {
    pub items: Vec<MineItem>,
    pub report: MineReport,
}

/// Sessions recorded for `cwd`, newest first — recent work is likelier to still
/// be true.
pub fn sessions_for_project(cwd: &Path) -> Result<Vec<PathBuf>> {
    let dir = sessions_dir()?;
    let want = cwd.display().to_string();
    let mut out: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();

    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        // Cheap probe: read the meta line rather than replaying the session.
        if session_cwd(&path).as_deref() != Some(want.as_str()) {
            continue;
        }
        let mtime = entry.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
        out.push((mtime, path));
    }
    out.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
    Ok(out.into_iter().map(|(_, p)| p).collect())
}

/// Render a session for the classifier. Tool *output* is dropped: it is exactly
/// what must never become durable memory — re-derivable, and bulky.
fn render_for_mining(session: &Session) -> String {
    let msgs = session.messages();
    let start = msgs.len().saturating_sub(MAX_MESSAGES);
    let mut out = String::new();
    for m in &msgs[start..] {
        let role = match m.role {
            Role::System | Role::Tool => continue,
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        if let Some(c) = &m.content
            && !c.trim().is_empty()
        {
            let body: String = c.chars().take(2_000).collect();
            out.push_str(&format!("[{role}] {body}\n"));
        }
        for tc in &m.tool_calls {
            out.push_str(&format!("[{role} called {}]\n", tc.name));
        }
    }
    out
}

/// Choose what to mine: this project's sessions, newest first, skipping the ones
/// already read and the ones too slight to be worth a model call. `limit` caps
/// how many are queued, so a first run over a large archive can be taken in
/// bites.
///
/// Touches the database (to check and record what has been seen), so it runs on
/// the caller's thread.
pub fn plan(mem: &MemoryStore, cwd: &Path, limit: usize) -> Result<MinePlan> {
    let mut plan = MinePlan::default();
    let paths = sessions_for_project(cwd)?;
    plan.report.found = paths.len();

    for path in paths {
        if plan.items.len() >= limit {
            break;
        }
        let id = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        if mem.was_mined(&id).unwrap_or(false) {
            plan.report.already_mined += 1;
            continue;
        }
        let session = match Session::open(&path) {
            Ok(s) => s,
            Err(e) => {
                plan.report.failed.push(format!("{id}: {e}"));
                continue;
            }
        };

        let users = session.messages().iter().filter(|m| m.role == Role::User).count();
        let transcript = render_for_mining(&session);
        if users == 0 || session.messages().len() < MIN_MESSAGES || transcript.trim().is_empty() {
            plan.report.too_short += 1;
            // Mark it anyway: a finished short session will not grow, and
            // re-reading it on every run is the same wasted work forever.
            let _ = mem.mark_mined(&id, 0);
            continue;
        }

        plan.items.push(MineItem { id, transcript });
    }

    plan.report.read = plan.items.len();
    Ok(plan)
}

/// Classify each queued session. Pure model work — no database — so this is the
/// half that can be spawned off the UI task.
pub async fn classify(
    agent: &Agent,
    items: &[MineItem],
    mut progress: impl FnMut(usize, usize),
) -> Vec<(String, std::result::Result<String, String>)> {
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        progress(i + 1, items.len());
        let res = agent
            .ask(EXTRACTION_PROMPT, &item.transcript, CLASSIFY_TOKENS)
            .await
            .map_err(|e| e.to_string());
        out.push((item.id.clone(), res));
    }
    out
}

/// File what the classifier produced as proposals, and record which sessions
/// were read. Database work, so back on the caller's thread.
pub fn record(
    mem: &MemoryStore,
    results: Vec<(String, std::result::Result<String, String>)>,
    mut report: MineReport,
) -> MineReport {
    for (id, res) in results {
        let text = match res {
            Ok(t) => t,
            Err(e) => {
                report.failed.push(format!("{id}: {e}"));
                continue;
            }
        };
        let mut kept = 0;
        for c in parse_candidates(&text) {
            // Scope is forced. A session ran in this project, so what it taught
            // is this project's; the classifier's guess is not authoritative
            // about that, and a wrong guess pollutes every other repo.
            match mem.propose(Scope::Project, &c.kind, &c.subject, &c.content, c.importance) {
                Ok((_, true)) => {
                    report.proposed += 1;
                    kept += 1;
                }
                Ok((_, false)) => report.duplicates += 1,
                Err(e) => report.failed.push(format!("{id}: {e}")),
            }
        }
        let _ = mem.mark_mined(&id, kept);
    }
    report
}

impl MineReport {
    /// One line for the TUI / CLI.
    pub fn summary(&self) -> String {
        if self.found == 0 {
            return "no past sessions recorded for this project".to_string();
        }
        let mut s =
            format!("mined {} of {} sessions — {} proposals", self.read, self.found, self.proposed);
        if self.duplicates > 0 {
            s.push_str(&format!(", {} already known", self.duplicates));
        }
        if self.already_mined > 0 {
            s.push_str(&format!(", {} previously mined", self.already_mined));
        }
        if self.too_short > 0 {
            s.push_str(&format!(", {} too short", self.too_short));
        }
        if !self.failed.is_empty() {
            s.push_str(&format!(", {} failed", self.failed.len()));
        }
        if self.proposed > 0 {
            s.push_str(" — review with /memory pending");
        }
        s
    }
}
