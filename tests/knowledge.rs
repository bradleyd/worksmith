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
    write(&dir.path().join("docs"), "architecture.md",
        "# Architecture\n\nWorkers are supervised by a foreman that nudges them.\n\n\
         The supervisor pulls the andon cord when a worker will not recover.");
    write(dir.path(), "style.md", "Prefer Result<T, E> over panicking in library code.");
    write(dir.path(), "logo.png", "\u{0}binary-ish");

    let store = KnowledgeStore::open_at(&dir.path().join("k.db"), dir.path()).unwrap();
    let stats = store.index().unwrap();
    assert_eq!(stats.files, 2, "only indexable text, not the png: {stats:?}");
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
    assert!(store.search("gadgets", 5).unwrap().is_empty(), "pruned source is gone");
    assert!(!store.search("widgets", 5).unwrap().is_empty(), "the surviving file still hits");
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
    assert_eq!(store.chunk_count().unwrap(), before, "chunks replaced, not appended");
    assert!(store.search("caching", 5).unwrap().is_empty(), "stale text is gone");
    assert!(!store.search("batching", 5).unwrap().is_empty());
}

#[test]
fn search_indexes_itself_so_the_first_query_works() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "notes.md", "The parser uses a hand-rolled recursive descent approach.");

    // No index() call: a fresh store must still answer.
    let store = KnowledgeStore::open_at(&dir.path().join("k.db"), dir.path()).unwrap();
    assert_eq!(store.chunk_count().unwrap(), 0);
    let hits = store.search("recursive descent", 5).unwrap();
    assert!(!hits.is_empty(), "search should have indexed on demand");
    assert_eq!(hits[0].source, "notes.md");
}
