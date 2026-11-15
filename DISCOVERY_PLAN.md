# Discovery: knowing a folder before working in it

Status: plan, for review. Nothing here is built.

## The problem, measured

Worksmith works in a folder it knows nothing about. Every session the model
rebuilds its picture of the place by searching, and it searches blind: it
cannot look for what it does not know is there.

Interactive sessions since 2026-10-01:

| Tool | Calls |
|---|---|
| bash (mostly `grep -rn`, `find`, `cat`, `sed -n`) | 1,766 |
| read | 664 |
| grep tool | 47 |
| **knowledge** (the existing index) | **3** |

- Repetition is not the cost. Matched exactly, re-reads and repeated searches
  are about 1% of calls. The cost is volume: 1,245 bash searches, and over half
  of the bash file reads (132 of 253) went into `~/.cargo/registry` to learn a
  dependency's API from its source.
- The top failure behaviour in the labelled failures is
  `explore_without_acting`: steps spent reading and searching instead of
  changing anything.
- The index already works when used. In the POS project two `knowledge`
  searches answered in 50 to 70 ms with the right file
  (`src/app/admin_login.rs` for "admin backoffice inventory"). Models almost
  never reach for it.

## What exists today

`knowledge.rs` chunks text files into about 1,200 characters, stores them in
SQLite with an FTS5 index (`.worksmith/knowledge.db`), and re-indexes only files
whose modification time changed. It covers code and plain text (`md`, `rs`,
`py`, `toml`, `json` and so on), skips files over 512 KB, and refreshes when a
search finds the index older than 60 seconds. The first search in a fresh
folder pays for the whole build: 12 seconds in `~/Projects`.

The `doc` tool can already extract text from PDF (poppler), Word, ODT, RTF and
EPUB (pandoc), and convert with LibreOffice.

The `grep` tool is a single-threaded Rust walker. It does not read
`.gitignore` and stops at 10,000 files. ripgrep is installed and already a
Worksmith dependency (session search uses it).

## Why the model ignores the index

1. The first line of the system prompt names the toolkit as `read, write,
   edit, bash, grep, find, ls`. `knowledge` appears later, framed with memory,
   as something to prefer "over guessing which file explains a topic". The
   model is not guessing; it greps. The advice does not meet it where it is.
2. Models are trained on agents that search with `grep` and `rg` through a
   shell. Bash is their universal tool and nothing steers them off it.
3. Full-text search needs good query words. Without knowing what is in the
   folder, the model has no words to ask with. grep only needs a pattern.
4. The first search in a fresh folder is slow because it builds the index.

## Non-goals

- **A new database.** SQLite with FTS5 already answers in milliseconds at
  folder scale, and its write-ahead log gives the append-friendly writes a
  Cassandra-style store would. A folder is thousands of files, not billions of
  rows. The bottleneck is use, not lookup speed.
- **Semantic embeddings.** Possible later. Keyword search plus a map is the
  cheaper first step, and the map supplies the vocabulary embeddings would.
- **Pasting the index into the prompt.** Knowledge stays something the model
  searches. Only the small map is injected.

## Design, in the order to build it

Each phase is useful alone and measured before the next starts.

### Phase 1: make search fast and used (cheap)

- **Back the `grep` tool with ripgrep:** parallel, respects `.gitignore`, no
  10,000-file cap. Keep the tool's output format.
- **Put `knowledge` in the toolkit line of the prompt,** with guidance that
  matches how the model works: search the index when you do not know where
  something is; use grep when you know the exact pattern.
- **Build the index at session start, in the background,** so the first search
  is not the one that pays for it.
- Measure: share of searches through `knowledge` and the `grep` tool against
  bash `grep`. Baseline: 3 and 47 against about 1,800.

### Phase 2: index every kind of file

- Extract PDF, Word, ODT, RTF and EPUB through the `doc` tool's existing
  engines, so chapters and documents are searchable like code. Spreadsheets
  through LibreOffice to CSV.
- **Detect change by content hash, not modification time.** A `git checkout`
  rewrites modification times on files that did not change. Fast path: skip
  hashing when size and modification time both match what was recorded; hash
  only when either moved.
- Record the extractor and its version per file, so upgrading an extractor
  re-extracts what it produced.

### Phase 3: the map (workers)

The step that tells the model what things are, not just what they say.

- **First visit:** "I have not seen this folder before; mapping it, about N
  minutes." Every visit after: map only what changed.
- **Unit of work:** a directory, or a single large document. Each worker reads
  its unit's files (or their chunks) and writes a short summary: what this is
  for, what is inside, and for code, how to build and test.
- **Roll up:** directory summaries combine into one map of 1,000 to 2,000
  tokens at the top.
- **Cache by content:** each summary records the hash of its inputs. A changed
  file marks its directory's summary stale, and the roll-up above it. Only
  stale summaries are regenerated.
- **Runs in the background** on the worker model (`[agents] model`), with a
  token budget, and never blocks the user. Progress shows in the footer.
- **Dependencies on request:** "learn topcoat 0.11" maps and indexes that
  crate's source the same way, stored per dependency version, shared across
  projects.

### Phase 4: use the map

- **The map goes into the session's opening context.** It changes rarely, so it
  costs little prefix-cache churn (see the prompt and cache guide). It states
  its age so a stale map is visible.
- **Compaction notes point at the map** instead of carrying what it says.
- **The `knowledge` result format** leads with where a hit sits in the map.

## Data model

Additions to `.worksmith/knowledge.db`:

```text
files      path, size, mtime, sha256, kind, extractor, extractor_version, indexed_at
chunks     (existing) source, ord, text, plus sha256 of the source file
map_nodes  path, kind (file | dir | root | dependency), inputs_hash,
           summary, model, tokens, created_at, stale
```

The database stays disposable: delete it and everything rebuilds from the
files. Nothing in it is memory.

## Cost and risk

- **Mapping is the expensive part.** Rough guide: a few hundred tokens per
  directory summary. A 500-file repo is tens of thousands of tokens; on the
  local 9B at about 29 tokens/s, tens of minutes on first visit. Budget per
  run, smallest units first, and allow stopping partway.
- **A wrong map misleads.** Summaries are written by a model and can be wrong.
  The map is advice with its age shown, never a source of truth over the
  files.
- **Big folders.** A folder holding many repos already gets a warning; mapping
  should refuse there by default.
- **Privacy.** The map summarises the user's files and is sent to whichever
  model runs the session. Same exposure as reading the files, but in one place.
  `.worksmith/` should stay out of version control.

## How we know it worked

- Phase 1: knowledge and grep-tool share of searches; research steps before
  the first edit; tokens per turn.
- Phase 3 and 4: the same, plus first-visit mapping time and tokens, and how
  often the model's first search lands in the right file.
- Evals: `unfamiliar-library` (dependency learning) and `compaction-refactor`
  (maps surviving compaction), with a harder `unfamiliar-library` variant for
  the 27B. A "where is X handled" task in a larger repo would measure the map
  directly.

## Questions for review

1. Should mapping run automatically on first visit, or only when asked
   (`/discover`)? Automatic is the point, but it spends tokens unasked.
2. Which model maps: the session's model, or the cheaper `[agents] model`?
3. Should dependency maps be shared across projects (stored under
   `~/.worksmith/`) or kept per project?
4. Is a 1,000 to 2,000 token map the right size for your 27B and 9B windows?
