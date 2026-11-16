//! Project knowledge: chunking, indexing, incremental re-index, and search.

use std::fs;

use worksmith::knowledge::KnowledgeStore;

fn write(dir: &std::path::Path, name: &str, body: &str) {
    let path = dir.join(name);
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).unwrap();
    }
    fs::write(path, body).unwrap();
}

#[test]
fn indexes_project_text_and_finds_it_by_content() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("docs"),
        "architecture.md",
        "# Architecture\n\nWorkers are supervised by a foreman that nudges them.\n\n\
         The supervisor pulls the andon cord when a worker will not recover.",
    );
    write(
        dir.path(),
        "style.md",
        "Prefer Result<T, E> over panicking in library code.",
    );
    write(dir.path(), "logo.png", "\u{0}binary-ish");

    let store = KnowledgeStore::open_at(&dir.path().join("k.db"), dir.path()).unwrap();
    let stats = store.index().unwrap();
    assert_eq!(
        stats.files, 2,
        "only indexable text, not the png: {stats:?}"
    );
    assert!(stats.chunks >= 2);

    let hits = store.search("andon cord supervisor", 5).unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0].source, "docs/architecture.md");
    assert!(hits[0].text.contains("andon cord"));

    // A knowledge question about style finds the other document.
    let hits = store.search("panicking library code", 5).unwrap();
    assert_eq!(hits[0].source, "style.md");

    assert!(store.search("kubernetes", 5).unwrap().is_empty());
}

#[test]
fn reindexing_skips_unchanged_files_and_prunes_deleted_ones() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "a.md", "alpha content about widgets");
    write(dir.path(), "b.md", "beta content about gadgets");

    let store = KnowledgeStore::open_at(&dir.path().join("k.db"), dir.path()).unwrap();
    let first = store.index().unwrap();
    assert_eq!(first.files, 2);

    // Nothing changed: a re-index is cheap and rewrites nothing.
    let second = store.index().unwrap();
    assert_eq!(second.files, 0, "unchanged files are not re-chunked");
    assert_eq!(second.skipped_unchanged, 2);

    // A deleted file must stop showing up in search results.
    fs::remove_file(dir.path().join("b.md")).unwrap();
    assert_eq!(store.prune().unwrap(), 1);
    assert!(
        store.search("gadgets", 5).unwrap().is_empty(),
        "pruned source is gone"
    );
    assert!(
        !store.search("widgets", 5).unwrap().is_empty(),
        "the surviving file still hits"
    );
}

#[test]
fn an_edited_file_is_reindexed_not_duplicated() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "notes.md", "the original claim about caching");
    let store = KnowledgeStore::open_at(&dir.path().join("k.db"), dir.path()).unwrap();
    store.index().unwrap();
    let before = store.chunk_count().unwrap();

    // Rewrite with different content; mtime moves, so it re-indexes.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    write(dir.path(), "notes.md", "a revised claim about batching");
    let stats = store.index().unwrap();
    assert_eq!(stats.files, 1);
    assert_eq!(
        store.chunk_count().unwrap(),
        before,
        "chunks replaced, not appended"
    );
    assert!(
        store.search("caching", 5).unwrap().is_empty(),
        "stale text is gone"
    );
    assert!(!store.search("batching", 5).unwrap().is_empty());
}

#[test]
fn search_indexes_itself_so_the_first_query_works() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "notes.md",
        "The parser uses a hand-rolled recursive descent approach.",
    );

    // No index() call: a fresh store must still answer.
    let store = KnowledgeStore::open_at(&dir.path().join("k.db"), dir.path()).unwrap();
    assert_eq!(store.chunk_count().unwrap(), 0);
    let hits = store.search("recursive descent", 5).unwrap();
    assert!(!hits.is_empty(), "search should have indexed on demand");
    assert_eq!(hits[0].source, "notes.md");
}

#[test]
fn hits_carry_the_line_range_they_came_from() {
    // "chunk 27" meant nothing to the model, which grepped again for the lines.
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "src/refunds.rs",
        "use std::fmt;\n\n\n// Refund approval needs a manager PIN.\nfn approve_refund() {}\n\nfn other() {}\n",
    );
    let store = KnowledgeStore::open_at(&dir.path().join("k.db"), dir.path()).unwrap();
    store.index().unwrap();
    let hit = &store.search("refund approval manager", 5).unwrap()[0];
    assert_eq!((hit.line_start, hit.line_end), (1, 7));
    assert_eq!(hit.location(), "src/refunds.rs:1-7");

    // A file large enough to split: each piece's lines are its own.
    let mut big = String::new();
    for i in 1..=400 {
        big.push_str(&format!("line {i} of the ledger, padded to some length\n"));
        if i % 10 == 0 {
            big.push('\n');
        }
    }
    big.push_str("\nthe zebra paragraph\n");
    write(dir.path(), "big.md", &big);
    store.index().unwrap();
    let hit = &store.search("zebra", 5).unwrap()[0];
    let last = big.lines().count() as i64;
    assert_eq!(hit.line_end, last);
    let first_line = big.lines().nth(hit.line_start as usize - 1).unwrap();
    assert!(
        hit.text.starts_with(first_line.trim_end()),
        "{first_line:?} / {:?}",
        &hit.text[..40]
    );
}

#[test]
fn an_index_from_an_older_version_is_rebuilt_not_misread() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "a.md", "widgets and gadgets");
    let db = dir.path().join("k.db");
    {
        // Version 1's shape: no line columns, no schema marker.
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE chunks (id INTEGER PRIMARY KEY, source TEXT NOT NULL, ord INTEGER NOT NULL,
                                  text TEXT NOT NULL, mtime INTEGER NOT NULL, indexed_at INTEGER NOT NULL);
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta VALUES ('last_index_at', '9999999999');",
        )
        .unwrap();
    }
    let store = KnowledgeStore::open_at(&db, dir.path()).unwrap();
    let hits = store.search("gadgets", 5).unwrap();
    assert_eq!(
        hits.len(),
        1,
        "the stale freshness stamp went with the old table"
    );
    assert_eq!(hits[0].location(), "a.md:1-1");
}

#[test]
fn the_index_honours_gitignore() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), ".gitignore", "build/\n");
    write(dir.path(), "notes.md", "the real notes");
    write(
        dir.path(),
        "build/copy.md",
        "the real notes, copied by a build",
    );
    let store = KnowledgeStore::open_at(&dir.path().join("k.db"), dir.path()).unwrap();
    assert_eq!(store.index().unwrap().files, 1);
}

#[test]
fn the_background_build_leaves_the_index_ready() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "README.md", "Refunds need a manager PIN.");
    worksmith::knowledge::warm_in_background(dir.path())
        .expect("an ordinary folder is indexed")
        .join()
        .unwrap();
    let store = KnowledgeStore::open(dir.path()).unwrap();
    assert_eq!(store.chunk_count().unwrap(), 1);
    assert!(
        std::fs::read_to_string(dir.path().join(".worksmith/.gitignore"))
            .unwrap()
            .contains("knowledge.db*"),
        "an index built unasked must not end up in a commit"
    );
}

#[test]
fn no_background_build_where_an_index_would_be_wrong() {
    assert!(worksmith::knowledge::warm_in_background(std::path::Path::new("/")).is_none());
    let home = dirs::home_dir().unwrap();
    assert!(worksmith::knowledge::warm_in_background(&home).is_none());

    let dir = tempfile::tempdir().unwrap();
    for repo in ["pos", "novel"] {
        fs::create_dir_all(dir.path().join(repo).join(".git")).unwrap();
    }
    assert!(worksmith::knowledge::warm_in_background(dir.path()).is_none());
}
