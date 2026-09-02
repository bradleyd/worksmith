# What to do next

Written at the end of 2026-08-31, then updated after the 2026-09-02 TUI
refactor checkpoint. `LOOSE_ENDS.md` is the full list of what is wrong. This is
the short list of what to *do*, in order, with enough context to start without
re-deriving it.

## Current stopping point

The worktree was clean after `f199b91 Deduplicate checkpoint answers`.

The TUI refactor has been moving one small behavior at a time out of the old
monolithic input path. The latest run extracted `answer_pending_ask`, so typed
checkpoint answers, bare Enter skips, and Esc skips all go through one helper.
Checks for that slice were:

- `cargo test checkpoint --lib`
- `cargo test tui::tests --lib`
- `git diff --check`
- `cargo check`
- `cargo clippy --all-targets` (exits 0, still prints the pre-existing
  `src/agent.rs:2093` type-complexity warning)
- `cargo test`

Manual testing is not needed for that exact helper extraction; it was covered
by focused checkpoint/TUI tests and full suite. Do a manual test after the next
modal-state move, because that changes where the keyboard ownership state lives.

## 1. Keep the TUI refactor going: modal state next.

`TUI_REFACTOR.md` §3 is still the active path. The next small step should be
**one modal-state move**, not a broad rewrite:

- introduce a focused modal/checkpoint state holder, probably `Modals`, for
  `pending_approval` and `pending_ask`;
- move `answer_pending_ask` onto that state or next to it;
- route `handle_approval_key`, `handle_enter_key`, and the Esc checkpoint path
  through the same owner;
- keep `handle_key`'s precedence exactly the same: approval, overlay, normal,
  hint, insert.

The risky part is not the struct move; it is accidentally changing key
ownership. Pin these by review and tests:

- approval prompt owns all keys until answered;
- `n` denies approval and does not abort the turn;
- Esc in approval mode denies approval;
- Esc while a checkpoint is pending skips the checkpoint, not the running turn;
- Esc while running and no checkpoint is pending aborts the turn;
- Enter with an empty composer skips a checkpoint but otherwise does nothing.

Suggested manual test after this slice: run with pairing on and a failing
validator, trigger an approval prompt with `git push --dry-run`, deny it with
`n`, then use Esc to abort the continuing turn. Confirm the transcript/session
distinguishes denial from abort.

## 2. Then break command handling, one command family at a time.

After modal state, move to `TUI_REFACTOR.md` R4. Do not extract all of
`handle_command` at once. Start by grouping the command dependencies into a
command context, then move one low-risk command family such as `/pair` or
`/validate`.

This is where the refactor gets riskier: slash commands touch session state,
worker state, config, validation, hints, footer status, and transcript output.
The checks can pass while user-visible command text or mid-turn behavior
regresses, so review diffs closely and manually test each moved command.

## 3. Then revisit run-loop event dispatch.

Once input and command handling are less tangled, extract `run_loop`'s select
branches into named handlers. This is another risk point because ordering is
observable: worker completions, parent synthesis prompts, approvals,
checkpoints, mining results, and turn completion all race through the same loop.

Do this only after command handling has smaller boundaries, and keep each
handler extraction behavior-preserving.

## 4. Supervisor escalation follow-up, if it reappears.

A worker was stopped with `still off track after 2 nudges` during a run where
the only long gap was a 60s bash call, at `stuck-timeout = 20`. That is exactly
three ticks, so the tool-in-flight guard should have held.

**It does hold.** `tests/supervisor.rs::a_worker_inside_a_slow_tool_call_is_not_nudged`
drives a 2s bash call at a 200ms timeout through the real worker loop and the
worker finishes with zero nudges. Delete the guard and that test fails with
`got 2 and escalation Some("still off track after 2 nudges")`, byte for byte
what the live run said. So the mechanism is right and something else produced
those nudges.

`f091842` now logs every supervisor decision where it is *made* rather than
where it lands, and the directive text names the rule: idle opens "No progress
for", the repeat detector "You have called", the blocked detector "You said you
are blocked". Run any spawn, wait for an escalation, read `/agents tail`.

Do this first because it is cheap and it has been guessed at twice, wrongly.

## 5. Why TUI is still the right path.

`TUI_REFACTOR.md` §3 has the steps. R1 splits `App` into focused structs, R4
breaks up `handle_command`. Each is independently checkable with `cargo test`.

**Why this one.** It is the gate on three separate ideas already filed: the
dashboard, the tail-as-trace port, and the fan-out roster. All three want to
land in a file that has been broken up first, and adding several hundred lines
to a 5,153-line file before that makes the project's own thesis harder to
demonstrate in its own codebase.

**And the evidence now supports attempting it.** `LOOSE_ENDS` calls that file
the wound on the strength of a 27B making zero edits across 404 steps. On
2026-08-31 a 9B made two correct changes in it (`5b8ced1`, `d6c400b`) using grep
and offset reads rather than paging. So the open question is no longer whether a
small model can work in a 218KB file. It is whether it can do so repeatedly
across a sequence of related changes, which nothing has measured.

Use `--until "cargo test"`. The suite takes about seven minutes after an edit;
tell the worker so in the prompt, because a model that does not know sets its own
short `timeout_secs`, watches it expire, and concludes the build is stuck.

**Read every diff.** Four times on 2026-08-31 a worker passed its check with a
change that was wrong in a way the check could not see.

## 6. The fan-out shares one check.

The larger job, and the last place the differentiator degrades into something
meaningless. `/spawn -n 3 --until "..."` copies one command to every worker, so
worker 1 cannot pass until 2 and 3 have also finished. Needs both halves:
worktrees per worker for coherent state (PLAN.md M11), and a per-task check
emitted by the planner rather than one string copied to all. `TOOLCALL_PLAN.md`
has the full argument at the end.

## 4. Small things worth clearing while in the area.

- The finished-worker line prints an absolute path in `changed` where it should
  be repo-relative. The model passed an absolute path to `edit` and it is stored
  verbatim.
- The footer glyphs have now been wrong three times for three different reasons.
  `LOOSE_ENDS` has the position: fewer glyphs, not better ones.
- `README.md` claims `--until "vale docs/"` works for prose. Nobody has run it.

## Not yet, and why

**Prompt triage** and **post-run security checks** are both good and both new.
They will be easier to design once the refactor gives them somewhere to live.
Both are written up in `LOOSE_ENDS.md`.

## The thing to carry over

Nearly every fix on 2026-08-31 came from *using* worksmith rather than reading
it, and the recurring failure was never the model. It was a check that could not
fail, five separate times:

1. the MUD scaffold check, which passed just as happily on implemented code;
2. `worksmith spawn --until`, which accepted the flag and ran no check at all;
3. `playcheck.py` v1, three of whose five assertions could not fail, certifying
   a game with a locked door and no key;
4. `playcheck.py` v2, which then failed a *working* game and sent a worker that
   had already succeeded off to break things;
5. the guard test in `tui.rs`, which passed with the guard deleted because it
   matched a `return` belonging to a different branch.

Two of those five were checks written while explicitly warning about this
failure mode. **Test the test**: delete the thing it guards and watch it fail.
That habit is the single highest-value practice to keep.
