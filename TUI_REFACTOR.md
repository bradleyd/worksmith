# `src/tui.rs` — Review & Refactor Plan

Scope: originally a read-only review of `src/tui.rs` (then 5065 lines, the
largest file in the crate) for **performance issues**, **bugs/correctness**,
and a **refactor plan**. This is now a living note tracking the incremental TUI
split. Older line numbers refer to the original reviewed file unless a section
has a follow-up note.

The code is sound and unusually well-commented — the comments record real
incidents (a 15 ms/token re-wrap, a 79-minute "working…" freeze, a transcript
silently dropped). The problems below are *concentration* and *hot-path*
problems, not design failures.

---

## 1. Performance issues (ranked by impact)

### P1. ~~Idle redraw at 8 Hz — the ticker forces a full redraw forever~~ ✅ DONE

> **Follow-up, 2026-08-30.** The guard shipped as `if app.running`, which is
> true only while a *main-session* turn runs. That also silenced the loop's
> heartbeat while background workers were alive, and the top of that loop is
> where `take_newly_finished` surfaces a completion and `pump` starts a queued
> worker. A worker finishing while the session sat idle was invisible until the
> user pressed a key. Condition is now
> `app.running || app.agents_running > 0 || app.agents_queued > 0`.

`run_loop`'s `select!` (line 1143) has a `_ = ticker.tick()` branch with **no
`if` guard** (line 1452). `tokio::time::interval(120ms)` completes every 120 ms,
so that branch is always ready. The loop body runs, and the top of the next
iteration calls `terminal.draw(|f| ui(f, &app))` (line 1141).

Consequence: the UI redraws **at least 8×/second even when completely idle** —
no turn running, no keys, no events. Every idle draw still pays:

- `terminal.size()` (1139)
- a full `render_transcript` (which deep-clones every visible `Line`, P3)
- `wrap_input` over the whole composer
- `footer_string`

This is the single biggest source of wasted CPU, and it is the multiplier that
makes P2/P3/P5 hurt. **Fix:** gate the ticker branch with `if app.running` so it
only forces redraws while the spinner is actually animating. When nothing is
running, redraw only on real input/events.

### P2. ~~`search_hits()` re-scans the whole transcript on every frame in normal mode~~ ✅ DONE

> **Follow-up.** Search hits now live on `Transcript` and are invalidated when
> the pattern or wrapped rows change. `render_transcript` reads the cached row
> indexes instead of re-scanning every row per frame.

`render_transcript` calls `app.search_hits()` whenever `mode == Normal`
(line 3436). `search_hits` (line 508) allocates a lowercased needle, then for
**every cached row** builds `row_text(l)` (a fresh `String`, line 97) plus a
`.to_ascii_lowercase()` `String`, and runs `contains`.

That is O(total_rows × row_len) allocations per frame — and combined with P1,
that is 8×/second for the life of the session while in normal mode. This is the
worst-case hot path.

The hits depend only on the pattern and the rows, so they should be computed
**once when the pattern changes (or the transcript dirties)** and cached, not
per-frame.

### P3. `render_transcript` deep-clones every visible `Line` per frame

Lines 3437–3471. The common case (insert mode, no search, no cursor row) hits
`line.clone()` (3468) for all ~40 visible rows. `Line` owns a `Vec<Span>`, each
`Span` owns a `String`, so this copies all visible text every frame.

The row cache (`ensure_rows`) correctly avoids re-*wrapping*, but the render
step throws that away by cloning. The clone is forced because the cursor-row
(reverse video) and search-hit (yellow) restyling need owned `Line`s, and
ratatui's `Paragraph` takes one `Lines` collection.

The realistic fix is not to avoid the clone (ratatui API) but to stop drawing
8×/sec when idle (P1) and to cache search hits (P2) so the restyled path is
rare.

### P4. ~~`hits.contains(&row)` is O(n) per visible row~~ ✅ DONE

Line 3455. `hits` is a `Vec<usize>`; for each of the ~40 visible rows it
linearly scans the whole hits list. With many matches this is
40 × len(hits) comparisons/frame. Make it a `HashSet<usize>` (or a small set
built only for the visible `start..end` window).

### P5. ~~`Overlay::matches()` is recomputed 3–4× per frame and allocates per item~~ ✅ DONE

> **Follow-up.** `Overlay` now stores `matched` indexes and rebuilds them when
> the filter changes. `matches()` still materializes a small vector of
> `(index, item)` references for callers, but it no longer lowercases and scans
> every item on every render.

`Overlay::matches` (line 184) is called from `render_hint` (3297),
`render_overlay` (3355), and transitively from `move_by`/`chosen`/`sel_index`
(198, 217, 211). Each call does `to_ascii_lowercase()` on every item's label
*and* description.

Fine for the 18-command list, but a large overlay (a long `/memory` list) pays
O(n) allocations several times a frame. Cache the filtered index when
`filter`/`items` change.

### P6. Composer edits are O(n) per keystroke on a large buffer

Lines 587–666. `byte_at` (587) is `char_indices().nth(i)` — O(n).
`move_home`/`move_end`/`delete_word` (626, 651, 660) each `chars().collect()`
into a `Vec<char>` — O(n) allocation. After a multi-KB paste, every Ctrl+A/E/W
and each `insert_char` is O(buffer).

Pastes go through one `insert_str` (fine), but *editing* a big pasted buffer is
O(n²) overall. Low priority (the composer is usually short) but real for the
`/spawn` paste case the tests specifically cover.

### P7. Unbounded transcript memory

`items` (233) and `cached_rows` (276) only grow (cleared on `/new`). With tool
results capped ~24 KB each, a long session holds megabytes in both, and
`search_hits` (P2) scans all of it. No trimming cap exists. A scaling concern,
not a crash.

---

## 2. Bugs / correctness

- **`scroll_up` is `u16`** (247) — saturates at 65535 rows. A transcript longer
  than that can't scroll past the cap. Latent; use `usize`.

- **Normal-mode window isn't centered** (3428–3434). Near the top the cursor
  sits a few rows down from the top edge; near the bottom it's pinned to the
  last row. No centering band. UX quirk, not a crash — but worth a deliberate
  decision.

- **`escape_pair` calls `char_len()` (O(n)) on every character keypress** (463)
  just to test `cursor == char_len()`. Cheap to replace with a cached char
  count, or compute `input.chars().count()` only when `c == first`.

- **`deliver_to_parent`** (2636): the
  `if !app.running && let Ok(mut s) = session.try_lock()` guard means a report
  arriving while a turn *just* finished but the lock is momentarily held
  silently routes to `steering` instead of the session. Acceptable, but the two
  paths produce different model-visible results (steering is drained at the
  next turn; a session append is immediate). Worth a comment, not a fix.

- **The `async { (&mut x.as_mut().unwrap()).await }` `select!` idiom** (1260,
  1323, 1340, 1408) is correct but fragile — the `unwrap` is guarded by the
  `if x.is_some()` branch guard, so it's safe, but a future edit that drops the
  guard panics. A small helper (`poll_opt(&mut opt)`) would make the invariant
  explicit.

- **`return` from the middle of a `match` arm** (847): `apply_event` for
  `Checkpoint { kind: "ask" | "answered" }` does `return`, skipping the rest of
  `apply_event`. Fine today (nothing follows in that arm), but a footgun if an
  arm is ever added after it.

---

## 3. Refactor plan

The file is one 50-field `App` god-object plus three ~300–500-line functions.
The logic is sound; the problem is concentration.

### R1. Split `App` into focused structs — IN PROGRESS

Each independently testable:

- **`Transcript`** — ✅ moved to `src/tui/transcript.rs`: `items`, `cached_rows`, `item_starts`, `dirty`,
  `dirty_from`, `cache_width`, `scroll_up`, `follow`, `collapse_tools`,
  `show_thinking`, `cursor_row`, `mode`, `search`. Owns `ensure_rows` /
  `touch` / `search_hits`.
- **`Composer`** — ✅ moved to `src/tui/composer.rs`: `input`, `cursor`, `history`, `history_idx`, `draft`,
  `completion`, `hint`. Owns all the editing methods.
- **`Footer` / `Status`** — ✅ footer rendering moved to `src/tui/footer.rs`: model, context/token counters, `prices`, `spinner`,
  `turn_start`, `think_label`, `agents_*`, `status`, `route`,
  `last_finish_reason`. The state still lives on `App`; only the rendering and
  legend helpers have moved.
- **`Overlay`** — ✅ moved to `src/tui/overlay.rs`.
- **`Modals`** — still pending: `pending_approval`, `pending_ask`.

This turns the 50-line `App::new` (349) into small constructors and lets
`reset_for_new_session` (420) reset the right sub-structs by construction
instead of a hand-maintained field list.

### R2. Break `run_loop`'s 8-branch `select!` into `fn`s

Extract `handle_worker_pump`, `handle_bus_event`, `handle_extract_done`,
`handle_approval`, `handle_ask`, `handle_mine_done`, `handle_fanout_done`,
`handle_turn_done`. The `select!` then reads as a dispatch table, and each
handler is unit-testable.

### R3. Break `handle_key` (1492) into per-mode methods returning `Flow`

`handle_approval_key`, `handle_overlay_key`, `handle_normal_key`,
`handle_hint_key`, `handle_insert_key`. The current nesting (approval →
overlay → normal → hint → insert → readline) is five levels deep.

### R4. Break `handle_command` (1834)

Several arms already delegate (`memory_command`, `agents_command`, …); lift the
remaining inline arms (`help`, `new`, `compact`, `spawn`, `history`, `fast`,
`think`, `trust`, `pair`, `route`, `model`, `mouse`, `validate`) into one
function each. Consider a `Command` enum parsed once instead of repeated `&str`
matching.

### R5. Replace the `PlannedFanOut` 5-tuple (54) with a named struct

The per-element comments are a smell that the names are missing.

### R6. Precompute `Kind` styling

The `match item.kind` in `item_rows` (3520) rebuilds `Style`/`label` per item
per wrap. Hoist to a `fn kind_style(kind) -> (Style, &'static str)` (or a
`const` table) so the hot wrap loop does no allocation for styling.

### R7. Cache the search index (pairs with P2)

Store `search_hits` on `Transcript`, invalidated when the pattern or the rows
change. `jump_match` and `render_transcript` then read the cache.

### R8. Make the `select!` poll idiom explicit (pairs with the bug list)

A `poll_opt` helper removes four copies of the fragile
`async { (&mut x.as_mut().unwrap()).await }` pattern.

---

## 4. Suggested order of work

Ordered by (impact × safety), each independently shippable and testable:

1. **P1** — gate the ticker with `if app.running`. One-line change, removes the
   idle 8 Hz redraw. Biggest win, lowest risk. ✅ DONE
2. **P4** — `HashSet` for search hits. Local, low risk. ✅ DONE
3. **P2 + R7** — cache `search_hits`. Removes the worst hot path. ✅ DONE
4. **P5** — cache `Overlay::matches`. ✅ DONE
5. **R1** — split `App`. Mechanical, large, but each sub-struct is covered by
   the existing `#[cfg(test)]` module.
6. **R2 / R3 / R4** — break the three big functions. Pure extraction; the
   existing tests (footer, wrap, completion, approval/checkpoint rendering)
   keep the refactor honest.
7. **R5 / R6 / R8** — small cleanups.
8. **P6 / P7** — composer O(n) edits and transcript cap. Defer until a real
   long-session complaint appears.

Verification for each step: `cargo test` + `cargo clippy --all-targets` clean
(the repo standard is zero clippy warnings). The TUI is smoke-tested under a
PTY (`script -q /dev/null …`).
