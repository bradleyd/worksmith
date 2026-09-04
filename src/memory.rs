//! Durable memory: distilled decisions/constraints/preferences/facts/lessons in
//! SQLite. Two databases, same schema: global (`~/.worksmith/memory.db`) and
//! project (`<repo>/.worksmith/memory.db`). See `worksmith-memory-v1.md`.
//!
//! M5 scope: create/read/supersede/forget/list, exact-subject lookup, FTS5
//! search with hybrid ranking (§14), write-time dedup, worker *proposals* that
//! need approval (§8), and a compact `<MEMORY>` section for prompt injection.
//! Semantic/vector retrieval is deliberately deferred (§30 stage 4).

use std::collections::HashSet;
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
    project_terms: Vec<String>,
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn open_db(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        crate::config::ensure_project_dir(parent)
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
        CREATE INDEX IF NOT EXISTS idx_memories_status  ON memories(status);
        -- Which past sessions the miner has already read. Without this every
        -- run re-reads the whole archive and re-proposes what you rejected.
        CREATE TABLE IF NOT EXISTS mined_sessions (
            session_id TEXT PRIMARY KEY,
            mined_at   INTEGER NOT NULL,
            candidates INTEGER NOT NULL DEFAULT 0
        );",
    )
    .context("initializing memory schema")?;

    // FTS5 index over the searchable text, kept in sync by triggers. The rows
    // stay the source of truth; this index is rebuildable (§15).
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
            subject, content, kind, id UNINDEXED, tokenize = 'porter unicode61'
        );
        CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
            INSERT INTO memories_fts(subject, content, kind, id)
            VALUES (new.subject, new.content, new.kind, new.id);
        END;
        CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
            DELETE FROM memories_fts WHERE id = old.id;
        END;
        CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
            DELETE FROM memories_fts WHERE id = old.id;
            INSERT INTO memories_fts(subject, content, kind, id)
            VALUES (new.subject, new.content, new.kind, new.id);
        END;",
    )
    .context("initializing memory search index")?;

    // Backfill anything written before the index existed.
    conn.execute_batch(
        "INSERT INTO memories_fts(subject, content, kind, id)
         SELECT subject, content, kind, id FROM memories
         WHERE id NOT IN (SELECT id FROM memories_fts);",
    )
    .ok();
    Ok(conn)
}

/// A search hit: the memory plus why it ranked where it did.
#[derive(Debug, Clone)]
pub struct Hit {
    pub row: MemoryRow,
    pub score: f64,
}

/// Hard-bounded memory budget for one model request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryCaps {
    pub max_items: usize,
    pub max_chars: usize,
    pub max_item_chars: usize,
}

impl MemoryCaps {
    pub fn for_context(context_window: usize) -> Self {
        Self {
            max_items: (context_window / 16_000).clamp(2, 8),
            max_chars: (context_window / 32).clamp(600, 2_400),
            max_item_chars: 320,
        }
    }
}

const TURN_MEMORY_MIN_SCORE: f64 = 0.20;

/// Dynamic memory injected for one turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryContext {
    pub text: String,
    pub ids: Vec<String>,
}

/// A proposed memory plus active rows it may replace.
#[derive(Debug, Clone)]
pub struct ProposalReview {
    pub proposal: MemoryRow,
    pub existing: Vec<MemoryRow>,
}

/// Collapse whitespace/case so near-identical writes compare equal (§20).
fn normalize(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// FTS5 treats bare punctuation as syntax; quote each term so arbitrary user
/// text is a safe query.
fn fts_query(query: &str) -> String {
    fts_query_with_filter(query, |_| false)
}

fn turn_fts_query(query: &str, project_terms: &[String]) -> String {
    fts_query_with_filter(query, |word| is_turn_query_noise(word, project_terms))
}

fn fts_query_with_filter(query: &str, is_noise: impl Fn(&str) -> bool) -> String {
    query_terms_with_filter(query, is_noise)
        .into_iter()
        .map(|w| format!("\"{w}\""))
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn query_terms_with_filter(query: &str, is_noise: impl Fn(&str) -> bool) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .map(str::trim)
        .filter(|w| !w.is_empty() && !is_noise(w))
        .map(|w| w.to_ascii_lowercase())
        .collect()
}

fn turn_query_terms(query: &str, project_terms: &[String]) -> Vec<String> {
    query_terms_with_filter(query, |word| is_turn_query_noise(word, project_terms))
}

fn row_contains_any_exact_term(row: &MemoryRow, terms: &[String]) -> bool {
    row_exact_term_count(row, terms) > 0
}

fn signal_overlap(row: &MemoryRow, terms: &[String]) -> f64 {
    if terms.is_empty() {
        return 0.0;
    }
    row_exact_term_count(row, terms) as f64 / terms.len() as f64
}

fn row_exact_term_count(row: &MemoryRow, terms: &[String]) -> usize {
    let text = format!("{} {} {}", row.kind, row.subject, row.content);
    let row_terms = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(|word| word.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    terms.iter().filter(|term| row_terms.contains(*term)).count()
}

fn is_turn_query_noise(word: &str, project_terms: &[String]) -> bool {
    let word = word.to_ascii_lowercase();
    project_terms.iter().any(|term| term == &word) || is_query_noise(&word)
}

fn is_query_noise(word: &str) -> bool {
    matches!(
        word,
        "a" | "an"
            | "and"
            | "are"
            | "as"
            | "be"
            | "but"
            | "change"
            | "check"
            | "do"
            | "does"
            | "file"
            | "for"
            | "i"
            | "in"
            | "is"
            | "it"
            | "make"
            | "me"
            | "memory"
            | "rs"
            | "my"
            | "of"
            | "on"
            | "or"
            | "run"
            | "small"
            | "src"
            | "test"
            | "testing"
            | "that"
            | "the"
            | "this"
            | "to"
            | "we"
            | "with"
    )
}

fn project_dir_from_memory_path(path: &Path) -> Option<&Path> {
    let worksmith_dir = path.parent()?;
    if worksmith_dir.file_name()? != ".worksmith" {
        return None;
    }
    worksmith_dir.parent()
}

fn project_terms_from_dir(dir: &Path) -> Option<Vec<String>> {
    let name = dir.file_name()?.to_string_lossy();
    let terms = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(|term| term.to_ascii_lowercase())
        .collect::<Vec<_>>();
    (!terms.is_empty()).then_some(terms)
}

impl MemoryStore {
    /// Open the global db and, if `project_dir` is given, the project db.
    pub fn open(project_dir: Option<&Path>) -> Result<MemoryStore> {
        let global_path = config::global_dir()
            .context("cannot locate home directory")?
            .join("memory.db");
        let project_path = project_dir.map(|dir| dir.join(".worksmith").join("memory.db"));
        Self::open_paths_with_project_name(
            &global_path,
            project_path.as_deref(),
            project_dir.and_then(project_terms_from_dir),
        )
    }

    /// Open at explicit paths (used by tests to avoid touching real home).
    pub fn open_paths(global_path: &Path, project_path: Option<&Path>) -> Result<MemoryStore> {
        Self::open_paths_with_project_name(
            global_path,
            project_path,
            project_path
                .and_then(project_dir_from_memory_path)
                .and_then(project_terms_from_dir),
        )
    }

    pub fn open_paths_with_project_name(
        global_path: &Path,
        project_path: Option<&Path>,
        project_terms: Option<Vec<String>>,
    ) -> Result<MemoryStore> {
        let global = open_db(global_path)?;
        let project = match project_path {
            Some(p) => Some(open_db(p)?),
            None => None,
        };
        Ok(MemoryStore {
            global,
            project,
            project_terms: project_terms.unwrap_or_default(),
        })
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

    /// Write a memory unless one just like it is already there (§20). Returns
    /// the existing row when it's a duplicate, so callers can say so instead of
    /// growing the store with restatements.
    pub fn remember_deduped(
        &self,
        scope: Scope,
        kind: &str,
        subject: &str,
        content: &str,
        importance: i64,
    ) -> Result<(MemoryRow, bool)> {
        if let Some(existing) = self.find_duplicate(scope, kind, subject, content)? {
            return Ok((existing, false));
        }
        Ok((self.remember(scope, kind, subject, content, importance)?, true))
    }

    /// An active memory in the same scope with the same subject+kind and the
    /// same content modulo whitespace/case.
    fn find_duplicate(
        &self,
        scope: Scope,
        kind: &str,
        subject: &str,
        content: &str,
    ) -> Result<Option<MemoryRow>> {
        let conn = self.conn(scope)?;
        let sql = format!(
            "SELECT {COLS} FROM memories \
             WHERE subject = ?1 AND kind = ?2 AND status IN ('active', 'proposed')"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params![subject, kind], row_from)?;
        let want = normalize(content);
        for r in rows {
            let r = r?;
            if normalize(&r.content) == want {
                return Ok(Some(r));
            }
        }
        Ok(None)
    }

    /// A worker's candidate memory: stored but *not* active until a human (or
    /// the parent) approves it (§8 — workers propose, they don't persist).
    pub fn propose(
        &self,
        scope: Scope,
        kind: &str,
        subject: &str,
        content: &str,
        importance: i64,
    ) -> Result<(MemoryRow, bool)> {
        if let Some(existing) = self.find_duplicate(scope, kind, subject, content)? {
            return Ok((existing, false));
        }
        let row = self.remember(scope, kind, subject, content, importance)?;
        let conn = self.conn(scope)?;
        conn.execute(
            "UPDATE memories SET status = 'proposed' WHERE id = ?1",
            [&row.id],
        )?;
        Ok((MemoryRow { status: "proposed".into(), ..row }, true))
    }

    /// Resolve a possibly-abbreviated id. Memories are keyed by UUID, and a
    /// user cannot be expected to retype 36 characters to approve one — so any
    /// unambiguous prefix works, the way git resolves short hashes.
    pub fn resolve_id(&self, prefix: &str) -> Result<IdMatch> {
        let prefix = prefix.trim();
        if prefix.is_empty() {
            return Ok(IdMatch::None);
        }
        let mut hits: Vec<String> = Vec::new();
        for conn in self.conns() {
            let mut stmt = conn.prepare("SELECT id FROM memories WHERE id LIKE ?1 || '%'")?;
            let rows = stmt.query_map([prefix], |r| r.get::<_, String>(0))?;
            for id in rows {
                hits.push(id?);
            }
        }
        hits.sort();
        hits.dedup();
        match hits.len() {
            0 => Ok(IdMatch::None),
            1 => Ok(IdMatch::Unique(hits.remove(0))),
            _ => Ok(IdMatch::Ambiguous(hits)),
        }
    }

    /// Ids of proposals awaiting review, for tab completion.
    pub fn pending_ids(&self) -> Result<Vec<String>> {
        Ok(self.pending()?.into_iter().map(|r| r.id).collect())
    }

    /// Has the miner already read this session? Recorded in the project store
    /// when there is one — mining is per-project, and so is its bookkeeping.
    pub fn was_mined(&self, session_id: &str) -> Result<bool> {
        let conn = self.project.as_ref().unwrap_or(&self.global);
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM mined_sessions WHERE session_id = ?1",
            [session_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Record that a session was mined, so a later run skips it.
    pub fn mark_mined(&self, session_id: &str, candidates: usize) -> Result<()> {
        let conn = self.project.as_ref().unwrap_or(&self.global);
        conn.execute(
            "INSERT OR REPLACE INTO mined_sessions (session_id, mined_at, candidates) \
             VALUES (?1, ?2, ?3)",
            rusqlite::params![session_id, now(), candidates as i64],
        )?;
        Ok(())
    }

    /// Proposals awaiting a decision, both scopes.
    pub fn pending(&self) -> Result<Vec<MemoryRow>> {
        let mut rows = query_status(&self.global, "proposed")?;
        if let Some(p) = &self.project {
            rows.extend(query_status(p, "proposed")?);
        }
        Ok(rows)
    }

    /// Proposals with exact same-scope/kind/subject active rows called out.
    pub fn pending_review(&self) -> Result<Vec<ProposalReview>> {
        self.pending()?
            .into_iter()
            .map(|proposal| {
                let scope = Scope::parse(&proposal.scope)
                    .with_context(|| format!("invalid memory scope `{}`", proposal.scope))?;
                let conn = self.conn(scope)?;
                let existing =
                    query_same_subject_kind(conn, &proposal.subject, &proposal.kind, "active")?;
                Ok(ProposalReview { proposal, existing })
            })
            .collect()
    }

    /// Approve (`active`) or reject (delete) a proposal. False if no such id.
    pub fn approve(&self, id: &str) -> Result<bool> {
        for conn in self.conns() {
            let n = conn.execute(
                "UPDATE memories SET status = 'active', updated_at = ?1 \
                 WHERE id = ?2 AND status = 'proposed'",
                rusqlite::params![now(), id],
            )?;
            if n > 0 {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Approve a proposal and mark the active memory it corrects as superseded.
    pub fn approve_superseding(&self, proposal_id: &str, old_id: &str) -> Result<bool> {
        for conn in self.conns() {
            let Some(proposal) = query_one(conn, proposal_id)? else {
                continue;
            };
            if proposal.status != "proposed" {
                bail!(
                    "{} is {}, not proposed",
                    short_id(proposal_id),
                    proposal.status
                );
            }
            let old = query_one(conn, old_id)?.with_context(|| {
                format!("no active memory {} in the same scope", short_id(old_id))
            })?;
            if old.status != "active" {
                bail!("{} is {}, not active", short_id(old_id), old.status);
            }
            if old.scope != proposal.scope
                || old.kind != proposal.kind
                || old.subject != proposal.subject
            {
                bail!(
                    "supersede requires the same scope, kind, and subject; proposal is [{}/{}] {}, existing is [{}/{}] {}",
                    proposal.scope,
                    proposal.kind,
                    proposal.subject,
                    old.scope,
                    old.kind,
                    old.subject
                );
            }

            let ts = now();
            conn.execute("BEGIN IMMEDIATE", [])?;
            let result = (|| -> Result<()> {
                conn.execute(
                    "UPDATE memories SET status = 'superseded', updated_at = ?1 WHERE id = ?2",
                    rusqlite::params![ts, old_id],
                )?;
                conn.execute(
                    "UPDATE memories SET status = 'active', supersedes_id = ?1, updated_at = ?2 WHERE id = ?3",
                    rusqlite::params![old_id, ts, proposal_id],
                )?;
                Ok(())
            })();
            if let Err(e) = result {
                conn.execute("ROLLBACK", []).ok();
                return Err(e);
            }
            conn.execute("COMMIT", [])?;
            return Ok(true);
        }
        Ok(false)
    }

    fn conns(&self) -> Vec<&Connection> {
        let mut v = vec![&self.global];
        if let Some(p) = &self.project {
            v.push(p);
        }
        v
    }

    /// Hybrid search over both scopes (§14): exact-subject matches first, then
    /// FTS/BM25, weighted by importance and recency, with a boost for project
    /// memories since they're the more specific context.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        self.search_with_options(query, limit, false)
    }

    fn search_with_options(
        &self,
        query: &str,
        limit: usize,
        for_turn_context: bool,
    ) -> Result<Vec<Hit>> {
        let mut hits: Vec<Hit> = Vec::new();
        let now_ts = now();
        let signal_terms = turn_query_terms(query, &self.project_terms);

        for (conn, is_project) in self.conns().into_iter().zip([false, true]) {
            for (row, text_score) in fts_rows(
                conn,
                query,
                for_turn_context,
                &self.project_terms,
            )? {
                let exact = if normalize(&row.subject) == normalize(query) {
                    1.0
                } else {
                    0.0
                };
                // Age in days, decayed so a year-old memory keeps ~half weight.
                let age_days = ((now_ts - row.updated_at).max(0) as f64) / 86_400.0;
                let recency = 1.0 / (1.0 + age_days / 180.0);
                let importance = (row.importance.clamp(0, 100) as f64) / 100.0;
                let signal = signal_overlap(&row, &signal_terms);
                let score = 0.25 * text_score
                    + 0.25 * exact
                    + 0.35 * signal
                    + 0.10 * importance
                    + 0.05 * recency
                    + if is_project { 0.10 } else { 0.0 };
                hits.push(Hit { row, score });
            }
        }

        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(limit);
        Ok(hits)
    }

    /// Build a small, relevant memory block for this turn. It is intentionally
    /// separate from the stable system prompt so provider prefix caching can
    /// still reuse the base prompt, skills catalog, and project instructions.
    pub fn turn_context(
        &self,
        query: &str,
        context_window: usize,
    ) -> Result<Option<MemoryContext>> {
        self.turn_context_with_caps(query, MemoryCaps::for_context(context_window))
    }

    pub fn turn_context_with_caps(
        &self,
        query: &str,
        caps: MemoryCaps,
    ) -> Result<Option<MemoryContext>> {
        let exact_terms = turn_query_terms(query, &self.project_terms);
        let mut hits = self.search_with_options(
            query,
            caps.max_items.saturating_mul(3).max(caps.max_items),
            true,
        )?;
        hits.retain(|hit| {
            hit.score >= TURN_MEMORY_MIN_SCORE
                && row_contains_any_exact_term(&hit.row, &exact_terms)
        });
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| (b.row.scope == "project").cmp(&(a.row.scope == "project")))
                .then_with(|| b.row.importance.cmp(&a.row.importance))
                .then_with(|| b.row.updated_at.cmp(&a.row.updated_at))
                .then_with(|| a.row.id.cmp(&b.row.id))
        });

        let mut ids = Vec::new();
        let mut lines = Vec::new();
        let mut used = "Relevant memory for this turn:\n".len();
        for hit in hits.into_iter().take(caps.max_items) {
            let row = hit.row;
            let body = compact_text(&row.content, caps.max_item_chars);
            let line = format!(
                "- [{}/{}/{}] {}: {}",
                row.scope,
                row.kind,
                short_id(&row.id),
                compact_text(&row.subject, 80),
                body
            );
            let line_len = line.len() + 1;
            if used + line_len > caps.max_chars {
                break;
            }
            used += line_len;
            ids.push(row.id);
            lines.push(line);
        }

        if lines.is_empty() {
            return Ok(None);
        }

        let mut text = String::from("Relevant memory for this turn:\n");
        text.push_str(&lines.join("\n"));
        Ok(Some(MemoryContext { text, ids }))
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

/// The classifier prompt for end-of-task extraction (§9). Deliberately biased
/// toward saving nothing: a store where every row feels intentional is worth
/// more than a complete one.
pub const EXTRACTION_PROMPT: &str = "\
You are evaluating whether anything in a completed coding task should be stored \
as durable agent memory.

Durable memory is information that will materially improve future work.

SAVE when it records: a durable decision; a persistent user preference; a \
requirement or constraint; a non-obvious lesson that prevents repeated work; or \
a durable fact not obvious from the project's own source.

DO NOT SAVE: intermediate reasoning, temporary hypotheses, tool output, file \
contents, line numbers, routine implementation details, completed one-time \
actions, generic programming knowledge, duplicates, or anything unlikely to \
matter again.

Prefer ZERO memories. A normal task produces 0-3. Each memory holds exactly one \
durable idea, stated in one or two sentences.

Output one line per memory, and nothing else:
scope|kind|subject|content|importance

scope: global (true everywhere) or project (this repo only)
kind: decision, constraint, preference, fact, or lesson
subject: a short topic key, 1-4 words
importance: 0-100

If nothing is worth keeping, output exactly: NONE";

/// The result of resolving a short id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdMatch {
    Unique(String),
    /// Several memories share the prefix — the caller should show them rather
    /// than guess.
    Ambiguous(Vec<String>),
    None,
}

/// How much of a UUID to show. Eight hex characters is what git settled on and
/// it stays unambiguous well past any plausible number of memories.
pub const SHORT_ID: usize = 8;

/// The displayable short form of an id.
pub fn short_id(id: &str) -> &str {
    &id[..id.len().min(SHORT_ID)]
}

fn compact_text(text: &str, max_chars: usize) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        return compact;
    }
    let mut out: String = compact.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// A memory the extractor proposed, before it's written anywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub scope: Scope,
    pub kind: String,
    pub subject: String,
    pub content: String,
    pub importance: i64,
}

/// Parse the extractor's `scope|kind|subject|content|importance` lines. Junk
/// lines are dropped rather than failing the batch — a malformed suggestion
/// from a weak model shouldn't cost the well-formed ones.
pub fn parse_candidates(text: &str) -> Vec<Candidate> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.eq_ignore_ascii_case("none") {
            continue;
        }
        let parts: Vec<&str> = line.splitn(5, '|').map(str::trim).collect();
        if parts.len() < 4 {
            continue;
        }
        let Some(scope) = Scope::parse(&parts[0].to_lowercase()) else {
            continue;
        };
        let kind = parts[1].to_lowercase();
        if !KINDS.contains(&kind.as_str()) {
            continue;
        }
        if parts[2].is_empty() || parts[3].is_empty() {
            continue;
        }
        let importance = parts
            .get(4)
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(60)
            .clamp(0, 100);
        out.push(Candidate {
            scope,
            kind,
            subject: parts[2].to_string(),
            content: parts[3].to_string(),
            importance,
        });
    }
    out
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

fn query_same_subject_kind(
    conn: &Connection,
    subject: &str,
    kind: &str,
    status: &str,
) -> Result<Vec<MemoryRow>> {
    let sql = format!(
        "SELECT {COLS} FROM memories \
         WHERE subject = ?1 AND kind = ?2 AND status = ?3 \
         ORDER BY importance DESC, updated_at DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![subject, kind, status], row_from)?;
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

fn query_status(conn: &Connection, status: &str) -> Result<Vec<MemoryRow>> {
    let sql = format!(
        "SELECT {COLS} FROM memories WHERE status = ?1 ORDER BY updated_at DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([status], row_from)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Active rows matching `query`, each with a 0..1 text score from BM25 (lower
/// bm25 is better, so it's inverted).
fn fts_rows(
    conn: &Connection,
    query: &str,
    for_turn_context: bool,
    project_terms: &[String],
) -> Result<Vec<(MemoryRow, f64)>> {
    let q = if for_turn_context {
        turn_fts_query(query, project_terms)
    } else {
        fts_query(query)
    };
    if q.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT {} , bm25(memories_fts) AS rank
         FROM memories_fts
         JOIN memories m ON m.id = memories_fts.id
         WHERE memories_fts MATCH ?1 AND m.status = 'active'
         ORDER BY rank LIMIT 50",
        COLS.split(", ").map(|c| format!("m.{c}")).collect::<Vec<_>>().join(", ")
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([q], |r| {
        let row = row_from(r)?;
        let rank: f64 = r.get(11)?;
        Ok((row, rank))
    })?;
    let rows: Vec<(MemoryRow, f64)> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .map(|(row, rank)| {
            // SQLite's bm25() is negative, more-negative = better. Typical hits
            // land around -0.5..-5, so divide by 5 for a 0..1 text score.
            let score = ((-rank) / 5.0).clamp(0.0, 1.0);
            (row, score)
        })
        .collect())
}
