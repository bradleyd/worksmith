# Command output and skill browser — completed

Status: implementation and manual validation complete on
`codex/command-output-fix`; changes remain uncommitted. M9 metrics was already
committed as `c4268f7` and merged into `main` before this branch.

## Original problem

Typing `/stats` or `/skill` during a running turn appended command output to the
transcript while thinking/assistant deltas continued updating an earlier item.
The command output stayed at the bottom, separated from the live response.
Separately, `/skill <name>` printed instructions without activating them, despite
sharing its name with the model's skill-loading tool.

The implemented approach uses overlays and status messages for these commands,
so they remain usable during a running turn without adding transcript entries.
Other commands that append notices have not all been migrated; this is not a
blanket fix for every command's output.

## Implemented behavior

### Accounting reports

- `/stats [session-id]` and `/metrics [session-id]` are intentional aliases for
  the same recorded accounting report. Both frontends share the report data;
  the TUI opens an overlay and the plain REPL prints text.
- The report reads the append-only session file without waiting for the running
  turn's session lock. Command errors appear in the TUI status area.
- Arrows, `j/k`, `gg/G`, page keys, Ctrl+U/D, and the mouse wheel navigate the
  overlay rather than the transcript beneath it.
- `/` starts a substring filter. Enter or Esc leaves filter editing, retaining
  the query and results. Esc or `q` while navigating closes the overlay.

### Skill discovery and activation

- Skills are discovered automatically from the existing global and project
  directories, including `~/.worksmith/skills/<name>/SKILL.md`. Names and
  descriptions enter the system-prompt catalog subject to its size cap.
- `[catalog]` means discovered, with full instructions not yet pinned.
  `[active]` means the full body and reference map are pinned for subsequent
  requests. Fresh processes start with the catalog, not active full bodies.
- The model is instructed to call `skill(name)` when relevant. Manual activation
  is optional: `/skill <name>` and the browser's Enter action use the same loader
  as the model's tool. Reference sections remain available on demand through
  `skill(name, section)`.
- `/skill unload <name>` and the browser's `u` action remove pinned instructions
  from future requests. They do not change an in-flight request, erase historical
  tool results, or prevent the model from activating the skill again.
- Duplicate activation does not duplicate instructions. Active previews show the
  pinned snapshot; deactivate/reactivate to pick up file edits. Active state is
  in-process and is not restored from session JSONL after restart.
- There is no separate 12 KB or skill-count limit. All active instructions remain
  in insertion order without silent eviction. They still consume model context;
  excessive input remains subject to provider context-limit errors and the
  existing retry/compaction behavior.
- Workers inherit independent snapshots of active skills. Subsequent parent or
  worker activation/deactivation does not rewrite another agent's instructions.

### Browser and terminal layout

- Bare `/skill` opens a dedicated screen with a searchable list and instruction
  preview. Browsing does not activate skills or make model requests. The composer
  is preserved and reappears when the browser closes.
- Tab switches pane focus. The focused heading has an underline and `>` marker,
  in addition to bold, so focus does not depend solely on terminal theme colors.
- `/` always focuses and filters the skill list by name/description, even from
  the preview. It does not search inside preview text. The query has a dedicated
  wrapping line above both panes, instead of sharing the narrow list title.
- First Esc while editing a filter returns to navigation and preserves results;
  a second Esc closes. To clear a filter, press `/` to start an empty query, then
  Esc. An empty query restores the full list and removes the filter line.
- Enter activates the selected skill; `u` deactivates it. The browser stays open
  and refreshes its state. Arrows, `j/k`, page keys, `gg/G`, and the wheel scroll
  the focused pane. Narrow terminals stack the panes vertically.
- Preview text wraps at words. Browser help and status reserve their wrapped
  height. The normal footer keeps short statuses beside metrics; longer statuses
  wrap below metrics with space reserved to keep the composer visible.

## Prompt/cache validation

Request comparisons cover ordinary follow-ups, preview and failed-load no-ops,
activation/deactivation, worker snapshots, changing turn memory, and compaction.
The OpenAI-compatible wire tests verify that transcript metadata does not alter
replayed messages and that tool-call IDs and argument strings remain intact.

Production turn-memory placement is unchanged: it remains a separate message
immediately after the system message, before conversation history. Changing it
can therefore shorten a shared prompt prefix early in the next turn.

An opt-in synthetic probe exercises the real OpenAI-compatible adapter. The LAN
run returned usage but no cached-token counts. The user subsequently confirmed
concurrent workload on that server, so its latency comparisons are inconclusive.
They are not evidence of a measured cache hit rate. The probe is ignored by the
normal test suite. Use OpenRouter for any further live checks while the local B70
is busy; validate task quality before changing memory placement.

See [prompt and cache behavior](docs/content/guide/prompt-cache.md) and
[metrics documentation](docs/content/guide/metrics.md) for details and reproduction
instructions.

## Validation completed

- 445 Rust tests passed; the live cache probe is excluded from normal runs.
- `cargo clippy --all-targets -- -D warnings` and `cargo build` passed.
- Three Python metrics tests, the Zola docs build, and `git diff --check` passed.
- PTY smoke checks with a local mock provider covered reports, navigation,
  filtering, first-Esc return to navigation, mid-turn activation/deactivation,
  duplicate activation, streaming completion, and clean exit.
- Plain-REPL smoke checks verified activation/deactivation in subsequent requests.
- Rendering regressions cover narrow and wide layouts, long queries/statuses,
  pane focus cues, Unicode display width, and composer isolation.
- User manual testing drove the layout, focus, filter, and Escape corrections.

## Wrap-up and follow-ups

1. Commit this completed slice, then merge the feature branch into `main`.
2. Keep `/stats` and `/metrics` as aliases for this commit. A concise `/stats`
   summary and detailed `/metrics` view are a possible later product decision.
3. Evaluate memory placement and task quality separately, using repeatable live
   measurements before making cache-related production changes.
4. Broader tool/thinking presentation and migration of other command notices
   remain separate UI work.

The earlier alternatives and speculative tab migration are superseded by the
implemented behavior above. Unrelated local scripts and eval output remain
outside this change.
