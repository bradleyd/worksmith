//! Project knowledge: the repo's own text, chunked and indexed for search.
//!
//! Knowledge is **not** memory (`worksmith-memory-v1.md` §3, §15). Memory is a
//! small set of distilled decisions a human would want kept; knowledge is bulk
//! source material that is always rebuildable from the files. So this database
//! is disposable — delete it and re-index — and nothing here is ever injected
//! into the prompt wholesale; the agent searches it like any other tool.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Files worth indexing. Binary and generated content is skipped — chunking a
/// lockfile just poisons search results.
const INDEXABLE: &[&str] = &[
    "md", "markdown", "txt", "rst", "rs", "toml", "yaml", "yml", "json", "py", "js", "ts", "tsx",
    "jsx", "go", "sh", "sql", "html", "css",
];

/// Roughly a paragraph or two — small enough that a hit is readable in a tool
/// result, large enough to carry context.
const CHUNK_CHARS: usize = 1_200;

/// Never index a single file bigger than this (generated blobs, vendored data).
const MAX_FILE_BYTES: u64 = 512 * 1024;

/// How long an index is trusted before a search re-checks the tree. Long enough
/// that a burst of searches costs one walk, short enough that edits show up.
const FRESH_SECS: i64 = 60;

/// Bumped when the chunks table changes shape. The database is disposable, so
/// an old one is dropped and rebuilt rather than migrated.
const SCHEMA: &str = "2";

/// Files written per transaction. One commit per row made the first build of a
/// large folder take seconds; one commit for everything would hold the write
/// lock for the whole build against another session's search.
const FILES_PER_COMMIT: usize = 200;

/// One indexer per database in this process. The background build at session
/// start and the model's first search race for the same folder; the loser waits
/// and then finds the index fresh instead of building it a second time.
fn index_lock(db: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    locks.entry(db.to_path_buf()).or_default().clone()
}

pub struct KnowledgeStore {
    conn: Connection,
    root: PathBuf,
    db: PathBuf,
}

/// One search hit: the chunk plus where it came from.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub source: String,
    pub ord: i64,
    pub line_start: i64,
    pub line_end: i64,
    pub text: String,
    pub score: f64,
}

impl Chunk {
    /// `path:12-40`, the form `read` and an editor both take. A chunk number
    /// meant nothing to the model, which grepped again to find the lines.
    pub fn location(&self) -> String {
        format!("{}:{}-{}", self.source, self.line_start, self.line_end)
    }
}

/// What an indexing pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexStats {
    pub files: usize,
    pub chunks: usize,
    pub skipped_unchanged: usize,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl KnowledgeStore {
    /// Open (creating if needed) `<root>/.worksmith/knowledge.db`.
    pub fn open(root: &Path) -> Result<KnowledgeStore> {
        Self::open_at(&root.join(".worksmith").join("knowledge.db"), root)
    }

    pub fn open_at(path: &Path, root: &Path) -> Result<KnowledgeStore> {
        if let Some(parent) = path.parent() {
            crate::config::ensure_project_dir(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening knowledge db {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "busy_timeout", 5000).ok();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .context("initializing knowledge schema")?;
        let schema: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| {
                r.get(0)
            })
            .ok();
        if schema.as_deref() != Some(SCHEMA) {
            conn.execute_batch(
                "DROP TRIGGER IF EXISTS chunks_ai;
                 DROP TRIGGER IF EXISTS chunks_ad;
                 DROP TABLE IF EXISTS chunks_fts;
                 DROP TABLE IF EXISTS chunks;
                 DELETE FROM meta;",
            )
            .context("resetting an old knowledge index")?;
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS chunks (
                id         INTEGER PRIMARY KEY,
                source     TEXT NOT NULL,
                ord        INTEGER NOT NULL,
                line_start INTEGER NOT NULL,
                line_end   INTEGER NOT NULL,
                text       TEXT NOT NULL,
                mtime      INTEGER NOT NULL,
                indexed_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_chunks_source ON chunks(source);
            CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
                text, source, id UNINDEXED, tokenize = 'porter unicode61'
            );
            CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN
                INSERT INTO chunks_fts(text, source, id) VALUES (new.text, new.source, new.id);
            END;
            CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN
                DELETE FROM chunks_fts WHERE id = old.id;
            END;",
        )
        .context("initializing knowledge schema")?;
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('schema', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [SCHEMA],
        )?;
        Ok(KnowledgeStore {
            conn,
            root: root.to_path_buf(),
            db: path.to_path_buf(),
        })
    }

    /// Index (or re-index) the project. Files whose mtime hasn't moved since
    /// their last pass are left alone, so re-running is cheap.
    pub fn index(&self) -> Result<IndexStats> {
        let lock = index_lock(&self.db);
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        self.index_locked()
    }

    fn index_locked(&self) -> Result<IndexStats> {
        let mut files = Vec::new();
        crate::tools::walk(&self.root, &mut files, 20_000);

        let mut stats = IndexStats::default();
        let mut tx = self.conn.unchecked_transaction()?;
        let mut written = 0usize;
        for path in files {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !INDEXABLE.contains(&ext) {
                continue;
            }
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if meta.len() > MAX_FILE_BYTES {
                continue;
            }
            let mtime = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let source = crate::tools::display_rel(&self.root, &path);

            let indexed: Option<i64> = self
                .conn
                .query_row(
                    "SELECT mtime FROM chunks WHERE source = ?1 LIMIT 1",
                    [&source],
                    |r| r.get(0),
                )
                .ok();
            if indexed == Some(mtime) {
                stats.skipped_unchanged += 1;
                continue;
            }

            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            tx.execute("DELETE FROM chunks WHERE source = ?1", [&source])?;
            for (ord, piece) in chunk_text(&text).into_iter().enumerate() {
                tx.execute(
                    "INSERT INTO chunks (source, ord, line_start, line_end, text, mtime, indexed_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![
                        source,
                        ord as i64,
                        piece.start as i64,
                        piece.end as i64,
                        piece.text,
                        mtime,
                        now()
                    ],
                )?;
                stats.chunks += 1;
            }
            stats.files += 1;
            written += 1;
            if written.is_multiple_of(FILES_PER_COMMIT) {
                tx.commit()?;
                tx = self.conn.unchecked_transaction()?;
            }
        }
        tx.commit()?;
        Ok(stats)
    }

    /// Drop chunks whose file no longer exists, so search can't cite a deleted
    /// path. Returns how many sources were pruned.
    pub fn prune(&self) -> Result<usize> {
        let mut stmt = self.conn.prepare("SELECT DISTINCT source FROM chunks")?;
        let sources: Vec<String> = stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut pruned = 0;
        for s in sources {
            if !self.root.join(&s).exists() {
                self.conn
                    .execute("DELETE FROM chunks WHERE source = ?1", [&s])?;
                pruned += 1;
            }
        }
        Ok(pruned)
    }

    pub fn chunk_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?)
    }

    /// Index if the index is empty or stale, so a search never has to answer
    /// "run the indexer first" — a tool whose first reply is a setup step
    /// doesn't get called twice. Incremental, so the steady-state cost is a
    /// directory walk and some `stat`s.
    pub fn ensure_fresh(&self, max_age_secs: i64) -> Result<()> {
        if !self.is_stale(max_age_secs)? {
            return Ok(());
        }
        let lock = index_lock(&self.db);
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        // Whoever held the lock may have just built it.
        if !self.is_stale(max_age_secs)? {
            return Ok(());
        }
        self.index_locked()?;
        self.prune()?;
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES ('last_index_at', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [now().to_string()],
        )?;
        Ok(())
    }

    fn is_stale(&self, max_age_secs: i64) -> Result<bool> {
        let last: Option<i64> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'last_index_at'",
                [],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .and_then(|v| v.parse().ok());
        Ok(match last {
            Some(t) => now() - t > max_age_secs || self.chunk_count()? == 0,
            None => true,
        })
    }

    /// BM25 search over indexed chunks. Auto-indexes first (see [`Self::ensure_fresh`]).
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Chunk>> {
        self.ensure_fresh(FRESH_SECS)?;
        let q = fts_query(query);
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT c.source, c.ord, c.line_start, c.line_end, c.text, bm25(chunks_fts) AS rank
             FROM chunks_fts JOIN chunks c ON c.id = chunks_fts.id
             WHERE chunks_fts MATCH ?1
             ORDER BY rank LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![q, limit as i64], |r| {
            let rank: f64 = r.get(5)?;
            Ok(Chunk {
                source: r.get(0)?,
                ord: r.get(1)?,
                line_start: r.get(2)?,
                line_end: r.get(3)?,
                text: r.get(4)?,
                score: ((-rank) / 5.0).clamp(0.0, 1.0),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

/// A chunk of a file and the lines it spans, 1-based and inclusive.
struct Piece {
    start: usize,
    end: usize,
    text: String,
}

/// Split on blank lines, packing paragraphs up to the chunk size. Splitting on
/// prose boundaries keeps a hit readable; a fixed byte window would cut
/// sentences (and code blocks) in half. Each piece keeps its line span so a hit
/// can say where it is.
fn chunk_text(text: &str) -> Vec<Piece> {
    // Paragraphs: runs of non-blank lines, with their line numbers.
    let mut paras: Vec<Vec<(usize, &str)>> = Vec::new();
    let mut cur_para: Vec<(usize, &str)> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            if !cur_para.is_empty() {
                paras.push(std::mem::take(&mut cur_para));
            }
        } else {
            cur_para.push((i + 1, line.trim_end()));
        }
    }
    if !cur_para.is_empty() {
        paras.push(cur_para);
    }

    let mut out = Vec::new();
    let mut cur: Option<Piece> = None;
    for para in paras {
        let body = para.iter().map(|(_, l)| *l).collect::<Vec<_>>().join("\n");
        let (first, last) = (para[0].0, para[para.len() - 1].0);
        // A single oversized paragraph still has to be broken up.
        if body.chars().count() > CHUNK_CHARS * 2 {
            out.extend(cur.take());
            out.extend(hard_split(&para, CHUNK_CHARS));
            continue;
        }
        match &mut cur {
            Some(c) if c.text.chars().count() + body.chars().count() <= CHUNK_CHARS => {
                c.text.push_str("\n\n");
                c.text.push_str(&body);
                c.end = last;
            }
            _ => {
                out.extend(cur.take());
                cur = Some(Piece {
                    start: first,
                    end: last,
                    text: body,
                });
            }
        }
    }
    out.extend(cur);
    out
}

/// Break a long paragraph on line boundaries.
fn hard_split(lines: &[(usize, &str)], max: usize) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut cur: Option<Piece> = None;
    for &(n, line) in lines {
        if let Some(c) = &cur
            && c.text.chars().count() + line.chars().count() > max
        {
            out.extend(cur.take());
        }
        let c = cur.get_or_insert_with(|| Piece {
            start: n,
            end: n,
            text: String::new(),
        });
        c.text.push_str(line);
        c.text.push('\n');
        c.end = n;
    }
    out.extend(cur);
    out
}

/// Start indexing `root` on a background thread at session start, so the first
/// search finds the index built instead of paying for it: the first search in
/// a fresh folder took 12 seconds, and a tool that slow the first time is a
/// tool the model learns to route around. Returns `None` where an index would
/// be wrong: a folder of projects, the home directory, or the filesystem root.
pub fn warm_in_background(root: &Path) -> Option<std::thread::JoinHandle<()>> {
    let home = dirs::home_dir();
    if root.parent().is_none()
        || home.as_deref() == Some(root)
        || crate::config::folder_of_projects(root).is_some()
    {
        return None;
    }
    let root = root.to_path_buf();
    std::thread::Builder::new()
        .name("knowledge-index".into())
        .spawn(move || {
            // Best effort: a search that finds no index builds one itself.
            if let Ok(store) = KnowledgeStore::open(&root) {
                let _ = store.ensure_fresh(FRESH_SECS);
            }
        })
        .ok()
}

/// Quote each term so arbitrary text is a safe FTS5 query.
fn fts_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{w}\""))
        .collect::<Vec<_>>()
        .join(" OR ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunking_splits_on_paragraphs_and_caps_size() {
        let text = "para one\n\npara two\n\n".to_string() + &"x".repeat(CHUNK_CHARS * 3);
        let chunks = chunk_text(&text);
        assert!(chunks.len() > 1, "a long document should chunk");
        assert!(
            chunks.iter().all(|c| !c.text.trim().is_empty()),
            "no empty chunks"
        );
        assert!(chunks[0].text.contains("para one"));
    }

    #[test]
    fn fts_query_is_punctuation_safe() {
        // Bare punctuation would otherwise be FTS5 syntax and error the query.
        assert_eq!(fts_query("a (b) c-d"), "\"a\" OR \"b\" OR \"c-d\"");
        assert_eq!(fts_query("   "), "");
    }
}
