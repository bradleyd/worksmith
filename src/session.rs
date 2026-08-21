//! Sessions: append-only JSONL files under `~/.worksmith/sessions/`. Each entry
//! is `{id, parent_id, type, ts, data}`, forming a tree (linear in M1; in-place
//! branching comes later). The full transcript stays on disk; the in-memory
//! `messages` vector is the replayable conversation.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config;
use crate::llm::Message;

/// One line in the session JSONL file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    pub ts: u64,
    pub data: serde_json::Value,
}

/// An open session with an append handle and the replayed message history.
pub struct Session {
    pub id: String,
    path: PathBuf,
    file: File,
    last_id: Option<String>,
    messages: Vec<Message>,
    /// Entry id of each replayable message, positionally parallel to
    /// `messages`. Compaction records *which* message it kept from, and that
    /// pointer has to survive a reload — an index into a vector does not.
    message_ids: Vec<String>,
    cwd: String,
}

/// How a persisted compaction summary re-enters the conversation. Written in
/// one place so a reload reconstructs exactly what the live session had.
pub fn summary_message(summary: &str) -> Message {
    Message::user(format!("[Summary of earlier conversation]\n{summary}"))
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// `~/.worksmith/sessions`, created if missing.
pub fn sessions_dir() -> Result<PathBuf> {
    let dir = config::global_dir()
        .context("cannot locate home directory")?
        .join("sessions");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

impl Session {
    /// Start a fresh session for `cwd` under the global sessions directory.
    pub fn create(cwd: &Path) -> Result<Session> {
        let id = Uuid::new_v4().to_string();
        let path = sessions_dir()?.join(format!("{id}.jsonl"));
        Self::create_at(&path, cwd)
    }

    /// Start a fresh session at an explicit file path (used by tests).
    pub fn create_at(path: &Path, cwd: &Path) -> Result<Session> {
        let id = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let path = path.to_path_buf();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("creating session file {}", path.display()))?;
        let cwd = cwd.display().to_string();
        let mut s = Session {
            id: id.clone(),
            path,
            file,
            last_id: None,
            messages: vec![],
            message_ids: vec![],
            cwd: cwd.clone(),
        };
        s.write_entry("meta", serde_json::json!({ "cwd": cwd, "id": id }))?;
        Ok(s)
    }

    /// Open an existing session file, replaying its messages.
    pub fn open(path: &Path) -> Result<Session> {
        let f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let reader = BufReader::new(f);

        let mut messages: Vec<Message> = Vec::new();
        let mut message_ids: Vec<String> = Vec::new();
        let mut last_id = None;
        let mut id = String::new();
        let mut cwd = String::new();

        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let entry: SessionEntry = match serde_json::from_str(&line) {
                Ok(e) => e,
                Err(_) => continue,
            };
            last_id = Some(entry.id.clone());
            match entry.kind.as_str() {
                "meta" => {
                    if let Some(c) = entry.data.get("cwd").and_then(|v| v.as_str()) {
                        cwd = c.to_string();
                    }
                    if let Some(i) = entry.data.get("id").and_then(|v| v.as_str()) {
                        id = i.to_string();
                    }
                }
                "message" => {
                    if let Ok(m) = serde_json::from_value::<Message>(entry.data.clone()) {
                        messages.push(m);
                        message_ids.push(entry.id.clone());
                    }
                }
                // Replay the compaction, don't ignore it. Skipping this entry
                // rebuilds the full pre-compaction history and drops the summary
                // entirely, so a resumed session silently undoes its own
                // context management and comes back over the limit.
                "compaction" => {
                    let Some(summary) = entry.data.get("summary").and_then(|v| v.as_str()) else {
                        continue; // legacy marker: count only, nothing to replay
                    };
                    let start = entry
                        .data
                        .get("kept_from")
                        .and_then(|v| v.as_str())
                        .and_then(|k| message_ids.iter().position(|i| i == k))
                        .unwrap_or(messages.len());
                    let kept = messages.split_off(start);
                    let kept_ids = message_ids.split_off(start);
                    messages = std::iter::once(summary_message(summary)).chain(kept).collect();
                    message_ids =
                        std::iter::once(entry.id.clone()).chain(kept_ids).collect();
                }
                _ => {}
            }
        }

        if id.is_empty() {
            id = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        }

        let file = OpenOptions::new().append(true).open(path)
            .with_context(|| format!("reopening {} for append", path.display()))?;

        Ok(Session { id, path: path.to_path_buf(), file, last_id, messages, message_ids, cwd })
    }

    /// Find the most recent session whose meta `cwd` matches `cwd`.
    pub fn most_recent_for_cwd(cwd: &Path) -> Result<Option<PathBuf>> {
        let dir = sessions_dir()?;
        let want = cwd.display().to_string();
        let mut best: Option<(std::time::SystemTime, PathBuf)> = None;

        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if session_cwd(&path).as_deref() != Some(want.as_str()) {
                continue;
            }
            let mtime = entry.metadata().and_then(|m| m.modified()).unwrap_or(UNIX_EPOCH);
            match &best {
                Some((t, _)) if *t >= mtime => {}
                _ => best = Some((mtime, path)),
            }
        }
        Ok(best.map(|(_, p)| p))
    }

    /// Resolve a session id to its file path.
    pub fn path_for_id(id: &str) -> Result<PathBuf> {
        Ok(sessions_dir()?.join(format!("{id}.jsonl")))
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Replace the in-memory message history (used by compaction). The JSONL
    /// file keeps the full transcript — only the working context shrinks. A
    /// `compaction` marker is logged for traceability.
    /// Collapse everything before `split` into `summary`, keeping the rest
    /// verbatim. The original entries stay on disk untouched — this appends a
    /// record of the decision (the summary text plus the id of the first kept
    /// message) so the compacted view is reproducible on reload.
    pub fn compact(&mut self, summary: &str, split: usize) -> Result<()> {
        let split = split.min(self.messages.len());
        let kept_from = self.message_ids.get(split).cloned();
        self.write_entry(
            "compaction",
            serde_json::json!({
                "summary": summary,
                "kept_from": kept_from,
                "kept_messages": self.messages.len() - split,
            }),
        )?;
        let entry_id = self.last_id.clone().unwrap_or_default();

        let kept = self.messages.split_off(split);
        let kept_ids = self.message_ids.split_off(split);
        self.messages = std::iter::once(summary_message(summary)).chain(kept).collect();
        self.message_ids = std::iter::once(entry_id).chain(kept_ids).collect();
        Ok(())
    }

    /// Record a structural event in the session file, so the loop's history
    /// outlives the process.
    ///
    /// Messages say what was *said*; they cannot say when a model call started,
    /// why the supervisor nudged, or that a stream ended without a finish
    /// reason. Answering those meant reading the provider's own logs and
    /// correlating timestamps by hand, three times in one afternoon.
    ///
    /// Per-token deltas are skipped: they would multiply the file by the length
    /// of every answer and say nothing the assistant message does not.
    pub fn append_event(&mut self, ev: &crate::event::Event) -> Result<()> {
        use crate::event::Event;
        if matches!(ev, Event::MessageDelta { .. } | Event::Thinking { .. }) {
            return Ok(());
        }
        let data = serde_json::to_value(ev).context("serializing event")?;
        self.write_entry("event", data)
    }

    /// Append a message to both the in-memory history and the JSONL file.
    pub fn append_message(&mut self, msg: Message) -> Result<()> {
        let data = serde_json::to_value(&msg).context("serializing message")?;
        self.write_entry("message", data)?;
        self.message_ids.push(self.last_id.clone().unwrap_or_default());
        self.messages.push(msg);
        Ok(())
    }

    fn write_entry(&mut self, kind: &str, data: serde_json::Value) -> Result<()> {
        let id = Uuid::new_v4().to_string();
        let entry = SessionEntry {
            id: id.clone(),
            parent_id: self.last_id.clone(),
            kind: kind.to_string(),
            ts: now_secs(),
            data,
        };
        let line = serde_json::to_string(&entry).context("serializing session entry")?;
        writeln!(self.file, "{line}").context("writing session entry")?;
        self.file.flush().ok();
        self.last_id = Some(id);
        Ok(())
    }
}

/// Read just the meta `cwd` from a session file (first `meta` line).
pub fn session_cwd(path: &Path) -> Option<String> {
    let f = File::open(path).ok()?;
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<SessionEntry>(&line)
            && entry.kind == "meta" {
                return entry.data.get("cwd").and_then(|v| v.as_str()).map(String::from);
            }
    }
    None
}

/// One recorded event, with the time it happened.
#[derive(Debug, Clone)]
pub struct TimedEvent {
    pub ts: u64,
    pub event: crate::event::Event,
}

/// The structural history of a session: tool calls, nudges, validations,
/// escalations, model-call boundaries, and when each happened.
///
/// Read from the file rather than the live bus, because the question is always
/// asked afterwards, about a run that has already ended.
pub fn events(path: &Path) -> Result<Vec<TimedEvent>> {
    let f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut out = Vec::new();
    for line in BufReader::new(f).lines() {
        let line = line?;
        let Ok(entry) = serde_json::from_str::<SessionEntry>(&line) else {
            continue;
        };
        if entry.kind != "event" {
            continue;
        }
        if let Ok(event) = serde_json::from_value(entry.data) {
            out.push(TimedEvent { ts: entry.ts, event });
        }
    }
    Ok(out)
}

