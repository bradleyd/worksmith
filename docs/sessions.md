# Session storage and search

New sessions use their creation date in UTC:

```text
~/.worksmith/sessions/YYYY/MM/DD/<session-id>/
  transcript.jsonl
  meta.json
  checks/<attempt-id>.log
  mcp-results/
  knowledge.db
```

Supporting files are created only when used. The transcript stays append-only
JSONL and is the authoritative session history. `meta.json` is a small listing
cache (ID, project, creation time, first user message); a missing or malformed
cache falls back to transcript metadata. There is no SQLite session index.
Memory and knowledge still use SQLite for their existing purposes.

Resuming a session keeps its original directory, even on a later day. IDs remain
UUIDs, and ID lookup scans known directory names rather than transcript content.
Worker sessions have their own directories; parent/worker links use IDs and still
work across dates. Existing flat `<id>.jsonl` files and their supporting paths stay
readable and are not moved or deleted.

```sh
worksmith sessions list
worksmith sessions list --date 2026-09-09
worksmith sessions list --from 2026-09-01 --to 2026-09-09
worksmith sessions search 'chapter 9'
worksmith sessions search 'chapter (8|9)' --from 2026-09-01 --project ~/Projects/book
worksmith --resume <session-id>
```

Dates and ranges filter **creation dates**, inclusively. Listings are ordered by
last transcript modification, newest first. The `--project` filter restricts
results to a project's recorded directory, resolving existing path aliases. A
deleted project's recorded path can still be used. These commands do not load a model or
require provider configuration.

Content search uses `rg` (ripgrep) over JSONL transcripts and prints matching
sessions once each. Patterns are ripgrep regular expressions. Search covers the
serialized history, including tool output, not just the latest compacted model
context. It excludes supporting logs/databases and does not follow symlinks.
Install ripgrep if it is unavailable; there is no model-based search fallback.

Date directories organize storage; they do not impose a disk quota. This change
leaves existing development/test sessions intact. Automatic deletion and explicit
pruning are a separate retention change, which must account for active sessions,
retained worktrees, and their supporting files. Tests use isolated Worksmith homes;
the eval runner also gives each run its own home.
