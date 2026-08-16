//! Durable memory: distilled decisions/constraints/preferences/facts/lessons in
//! SQLite. Two databases, same schema: global (`~/.worksmith/memory.db`) and
//! project (`<repo>/.worksmith/memory.db`). See `worksmith-memory-v1.md`.
//!
//! M1 scope: create/read/supersede/forget/list + exact-subject lookup, and a
//! small `<MEMORY>` section for prompt injection. FTS5, extraction, dedup, and
//! retrieval-ranking are deferred to M5.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use uuid::Uuid;

use crate::config;

/// Which database a memory lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Global,
    Project,
}

impl Scope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::Global => "global",
            Scope::Project => "project",
        }
    }
    pub fn parse(s: &str) -> Option<Scope> {
        match s {
            "global" => Some(Scope::Global),
            "project" => Some(Scope::Project),
            _ => None,
        }
    }
}

/// The five durable memory kinds. Everything else is not durable memory.
pub const KINDS: &[&str] = &["decision", "constraint", "preference", "fact", "lesson"];

/// A stored memory row.
#[derive(Debug, Clone)]
pub struct MemoryRow {
    pub id: String,
    pub scope: String,
    pub kind: String,
    pub subject: String,
    pub content: String,
    pub importance: i64,
    pub confidence: f64,
    pub created_at: i64,
    pub updated_at: i64,
    pub supersedes_id: Option<String>,
    pub status: String,
}

/// Handle to the global (and optional project) memory databases.
pub struct MemoryStore {
    global: Connection,
    project: Option<Connection>,
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn open_db(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let conn = Connection::open(path)
        .with_context(|| format!("opening memory db {}", path.display()))?;
    // WAL + a shared busy timeout so concurrent workers (later) don't deadlock.
    conn.pragma_update(None, "journal_mode", "WAL").ok();
    conn.pragma_update(None, "busy_timeout", 5000).ok();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS memories (
            id            TEXT PRIMARY KEY,
            scope         TEXT NOT NULL,
            kind          TEXT NOT NULL,
            subject       TEXT NOT NULL,
            content       TEXT NOT NULL,
            importance    INTEGER NOT NULL DEFAULT 50,
            confidence    REAL NOT NULL DEFAULT 1.0,
            created_at    INTEGER NOT NULL,
            updated_at    INTEGER NOT NULL,
            supersedes_id TEXT,
            status        TEXT NOT NULL DEFAULT 'active'
        );
        CREATE INDEX IF NOT EXISTS idx_memories_subject ON memories(subject);
        CREATE INDEX IF NOT EXISTS idx_memories_status  ON memories(status);",
    )
    .context("initializing memory schema")?;
    Ok(conn)
}

impl MemoryStore {
    /// Open the global db and, if `project_dir` is given, the project db.
    pub fn open(project_dir: Option<&Path>) -> Result<MemoryStore> {
        let global_path = config::global_dir()
            .context("cannot locate home directory")?
            .join("memory.db");
        let project_path = project_dir.map(|dir| dir.join(".worksmith").join("memory.db"));
        Self::open_paths(&global_path, project_path.as_deref())
    }

    /// Open at explicit paths (used by tests to avoid touching real home).
    pub fn open_paths(global_path: &Path, project_path: Option<&Path>) -> Result<MemoryStore> {
        let global = open_db(global_path)?;
        let project = match project_path {
            Some(p) => Some(open_db(p)?),
            None => None,
        };
        Ok(MemoryStore { global, project })
    }

    fn conn(&self, scope: Scope) -> Result<&Connection> {
        match scope {
            Scope::Global => Ok(&self.global),
            Scope::Project => self
                .project
                .as_ref()
                .context("no project memory database open (not in a project directory)"),
        }
    }

    /// Insert a new active memory.
    pub fn remember(
        &self,
        scope: Scope,
        kind: &str,
        subject: &str,
        content: &str,
        importance: i64,
    ) -> Result<MemoryRow> {
        if !KINDS.contains(&kind) {
            bail!("invalid kind `{kind}`; expected one of {KINDS:?}");
        }
        let conn = self.conn(scope)?;
        let ts = now();
        let row = MemoryRow {
            id: Uuid::new_v4().to_string(),
            scope: scope.as_str().to_string(),
            kind: kind.to_string(),
            subject: subject.to_string(),
            content: content.to_string(),
            importance,
            confidence: 1.0,
            created_at: ts,
            updated_at: ts,
            supersedes_id: None,
            status: "active".to_string(),
        };
        conn.execute(
            "INSERT INTO memories
                (id, scope, kind, subject, content, importance, confidence, created_at, updated_at, supersedes_id, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![
                row.id, row.scope, row.kind, row.subject, row.content, row.importance,
                row.confidence, row.created_at, row.updated_at, row.supersedes_id, row.status,
            ],
        )
        .context("inserting memory")?;
        Ok(row)
    }

    /// Insert a new memory that supersedes `old_id`, marking the old one
    /// `superseded` (never silently overwritten).
    pub fn supersede(
        &self,
        scope: Scope,
        old_id: &str,
        kind: &str,
        subject: &str,
        content: &str,
        importance: i64,
    ) -> Result<MemoryRow> {
        let mut row = self.remember(scope, kind, subject, content, importance)?;
        let conn = self.conn(scope)?;
        conn.execute(
            "UPDATE memories SET supersedes_id = ?1, updated_at = ?2 WHERE id = ?3",
            rusqlite::params![old_id, now(), row.id],
        )?;
        conn.execute(
            "UPDATE memories SET status = 'superseded', updated_at = ?1 WHERE id = ?2",
            rusqlite::params![now(), old_id],
        )?;
        row.supersedes_id = Some(old_id.to_string());
        Ok(row)
    }

    /// Delete a memory by id from whichever scope holds it. Returns true if a
    /// row was removed.
    pub fn forget(&self, id: &str) -> Result<bool> {
        let mut removed = self.global.execute("DELETE FROM memories WHERE id = ?1", [id])?;
        if removed == 0
            && let Some(p) = &self.project {
                removed = p.execute("DELETE FROM memories WHERE id = ?1", [id])?;
            }
        Ok(removed > 0)
    }

    /// Fetch a single memory by id from either scope.
    pub fn get(&self, id: &str) -> Result<Option<MemoryRow>> {
        if let Some(r) = query_one(&self.global, id)? {
            return Ok(Some(r));
        }
        if let Some(p) = &self.project {
            return query_one(p, id);
        }
        Ok(None)
    }

    /// Exact-subject lookup across both scopes (active rows only).
    pub fn get_by_subject(&self, subject: &str) -> Result<Vec<MemoryRow>> {
        let mut rows = query_by_subject(&self.global, subject)?;
        if let Some(p) = &self.project {
            rows.extend(query_by_subject(p, subject)?);
        }
        Ok(rows)
    }

    /// List active memories; `scope` = None lists both.
    pub fn list(&self, scope: Option<Scope>) -> Result<Vec<MemoryRow>> {
        let mut rows = Vec::new();
        if scope != Some(Scope::Project) {
            rows.extend(query_active(&self.global)?);
        }
        if scope != Some(Scope::Global)
            && let Some(p) = &self.project {
                rows.extend(query_active(p)?);
            }
        Ok(rows)
    }

    /// Build a compact `<MEMORY>` block for the system prompt: active memories,
    /// importance-first, capped. Empty string if there's nothing to inject.
    pub fn memory_section(&self, limit: usize) -> Result<String> {
        let mut rows = self.list(None)?;
        rows.sort_by(|a, b| b.importance.cmp(&a.importance).then(b.updated_at.cmp(&a.updated_at)));
        rows.truncate(limit);
        if rows.is_empty() {
            return Ok(String::new());
        }

        let (global, project): (Vec<_>, Vec<_>) =
            rows.into_iter().partition(|r| r.scope == "global");

        let mut out = String::from("<MEMORY>\n");
        if !global.is_empty() {
            out.push_str("Relevant global memories:\n");
            for r in &global {
                out.push_str(&format!("- [{}] {}: {}\n", r.kind, r.subject, r.content));
            }
        }
        if !project.is_empty() {
            out.push_str("Relevant project memories:\n");
            for r in &project {
                out.push_str(&format!("- [{}] {}: {}\n", r.kind, r.subject, r.content));
            }
        }
        out.push_str("</MEMORY>\n");
        Ok(out)
    }
}

fn row_from(r: &rusqlite::Row) -> rusqlite::Result<MemoryRow> {
    Ok(MemoryRow {
        id: r.get(0)?,
        scope: r.get(1)?,
        kind: r.get(2)?,
        subject: r.get(3)?,
        content: r.get(4)?,
        importance: r.get(5)?,
        confidence: r.get(6)?,
        created_at: r.get(7)?,
        updated_at: r.get(8)?,
        supersedes_id: r.get(9)?,
        status: r.get(10)?,
    })
}

const COLS: &str =
    "id, scope, kind, subject, content, importance, confidence, created_at, updated_at, supersedes_id, status";

fn query_one(conn: &Connection, id: &str) -> Result<Option<MemoryRow>> {
    let sql = format!("SELECT {COLS} FROM memories WHERE id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map([id], row_from)?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

fn query_by_subject(conn: &Connection, subject: &str) -> Result<Vec<MemoryRow>> {
    let sql = format!(
        "SELECT {COLS} FROM memories WHERE subject = ?1 AND status = 'active' ORDER BY importance DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([subject], row_from)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn query_active(conn: &Connection) -> Result<Vec<MemoryRow>> {
    let sql = format!(
        "SELECT {COLS} FROM memories WHERE status = 'active' ORDER BY importance DESC, updated_at DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], row_from)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}
