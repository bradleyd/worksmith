//! Project knowledge: the repo's own text, chunked and indexed for search.
//!
//! Knowledge is **not** memory (`worksmith-memory-v1.md` §3, §15). Memory is a
//! small set of distilled decisions a human would want kept; knowledge is bulk
//! source material that is always rebuildable from the files. So this database
//! is disposable — delete it and re-index — and nothing here is ever injected
//! into the prompt wholesale; the agent searches it like any other tool.

use std::path::{Path, PathBuf};
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

pub struct KnowledgeStore {
    conn: Connection,
    root: PathBuf,
}

/// One search hit: the chunk plus where it came from.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub source: String,
    pub ord: i64,
    pub text: String,
    pub score: f64,
}

/// What an indexing pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexStats {
    pub files: usize,
    pub chunks: usize,
    pub skipped_unchanged: usize,
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
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
            "CREATE TABLE IF NOT EXISTS chunks (
                id         INTEGER PRIMARY KEY,
                source     TEXT NOT NULL,
                ord        INTEGER NOT NULL,
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
            END;
            CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .context("initializing knowledge schema")?;
        Ok(KnowledgeStore { conn, root: root.to_path_buf() })
    }

    /// Index (or re-index) the project. Files whose mtime hasn't moved since
    /// their last pass are left alone, so re-running is cheap.
    pub fn index(&self) -> Result<IndexStats> {
        let mut files = Vec::new();
        crate::tools::walk(&self.root, &mut files, 20_000);

        let mut stats = IndexStats::default();
        for path in files {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !INDEXABLE.contains(&ext) {
                continue;
            }
            let Ok(meta) = std::fs::metadata(&path) else { continue };
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

            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            self.conn.execute("DELETE FROM chunks WHERE source = ?1", [&source])?;
            for (ord, chunk) in chunk_text(&text).into_iter().enumerate() {
                self.conn.execute(
                    "INSERT INTO chunks (source, ord, text, mtime, indexed_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![source, ord as i64, chunk, mtime, now()],
                )?;
                stats.chunks += 1;
            }
            stats.files += 1;
        }
        Ok(stats)
    }

    /// Drop chunks whose file no longer exists, so search can't cite a deleted
    /// path. Returns how many sources were pruned.
    pub fn prune(&self) -> Result<usize> {
        let mut stmt = self.conn.prepare("SELECT DISTINCT source FROM chunks")?;
        let sources: Vec<String> =
            stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut pruned = 0;
        for s in sources {
            if !self.root.join(&s).exists() {
                self.conn.execute("DELETE FROM chunks WHERE source = ?1", [&s])?;
                pruned += 1;
            }
        }
        Ok(pruned)
    }

    pub fn chunk_count(&self) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?)
    }

    /// Index if the index is empty or stale, so a search never has to answer
    /// "run the indexer first" — a tool whose first reply is a setup step
    /// doesn't get called twice. Incremental, so the steady-state cost is a
    /// directory walk and some `stat`s.
    pub fn ensure_fresh(&self, max_age_secs: i64) -> Result<()> {
        let last: Option<i64> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'last_index_at'", [], |r| {
                r.get::<_, String>(0)
            })
            .ok()
            .and_then(|v| v.parse().ok());
        let stale = match last {
            Some(t) => now() - t > max_age_secs,
            None => true,
        };
        if !stale && self.chunk_count()? > 0 {
            return Ok(());
        }
        self.index()?;
        self.prune()?;
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES ('last_index_at', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [now().to_string()],
        )?;
        Ok(())
    }

    /// BM25 search over indexed chunks. Auto-indexes first (see [`Self::ensure_fresh`]).
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Chunk>> {
        self.ensure_fresh(FRESH_SECS)?;
        let q = fts_query(query);
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT c.source, c.ord, c.text, bm25(chunks_fts) AS rank
             FROM chunks_fts JOIN chunks c ON c.id = chunks_fts.id
             WHERE chunks_fts MATCH ?1
             ORDER BY rank LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![q, limit as i64], |r| {
            let rank: f64 = r.get(3)?;
            Ok(Chunk {
                source: r.get(0)?,
                ord: r.get(1)?,
                text: r.get(2)?,
                score: ((-rank) / 5.0).clamp(0.0, 1.0),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

/// Split on blank lines, packing paragraphs up to the chunk size. Splitting on
/// prose boundaries keeps a hit readable; a fixed byte window would cut
/// sentences (and code blocks) in half.
fn chunk_text(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for para in text.split("\n\n") {
        let para = para.trim_end();
        if para.trim().is_empty() {
            continue;
        }
        if !cur.is_empty() && cur.chars().count() + para.chars().count() > CHUNK_CHARS {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push_str("\n\n");
        }
        // A single oversized paragraph still has to be broken up.
        if para.chars().count() > CHUNK_CHARS * 2 {
            for piece in hard_split(para, CHUNK_CHARS) {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                out.push(piece);
            }
            continue;
        }
        cur.push_str(para);
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// Break a long paragraph on line boundaries.
fn hard_split(s: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in s.lines() {
        if !cur.is_empty() && cur.chars().count() + line.chars().count() > max {
            out.push(std::mem::take(&mut cur));
        }
        cur.push_str(line);
        cur.push('\n');
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
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
        assert!(chunks.iter().all(|c| !c.trim().is_empty()), "no empty chunks");
        assert!(chunks[0].contains("para one"));
    }

    #[test]
    fn fts_query_is_punctuation_safe() {
        // Bare punctuation would otherwise be FTS5 syntax and error the query.
        assert_eq!(fts_query("a (b) c-d"), "\"a\" OR \"b\" OR \"c-d\"");
        assert_eq!(fts_query("   "), "");
    }
}
