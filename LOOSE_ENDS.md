# Loose ends

Things known to be wrong or unfinished, with enough detail to act on without
re-deriving them. Not a roadmap — `PLAN.md` §10a is the roadmap. This is the
list that otherwise lives only in someone's head or a chat log.

Each entry says what is wrong and how it was found, because "how it was found"
is usually the fastest route back in.

**Closed since this list was written:** `agent.pair` / `decisions-dir` /
provider tables / model tables all merged field-by-field (four separate keys
that parsed, validated, and were then silently dropped); `worksmith config
check` built, which found the fourth itself on its first run; `/model` steps 3
and 4a; compaction no longer trades the whole context for a sentence.

## Bugs

- **A task can fail nine checks in a row and nothing notices it is going
  nowhere.** `pa-reject` on Qwen3.5-4B, three isolated runs: 0/3, taking 327s,
  900s (timed out) and 436s, burning 9,033 / 24,939 / 13,650 generated tokens.
  In every run the model read the file, made valid `edit` calls, ran its check,
  failed, and edited again.

  **It is not a tool-choice problem.** Across those three runs: 32 `edit` calls,
  **zero** `edit` errors, and **zero** `write` calls. The model reaches for the
  right tool and uses it correctly. An earlier note here claimed whole-file
  rewriting, built on one run averaging 1,201 generated tokens per model call;
  the three instrumented runs average 311, 500 and 580, and the theory does not
  survive them. Corrected rather than deleted, because the wrong version is in
  the git history.

  What actually happens is a no-progress loop: valid edits, the same check
  failing the same way, over and over. The harness cannot see it. The supervisor
  keys its repeat detector on `format!("{name}::{arguments}")`
  (`supervisor.rs:159`), so it only catches literally identical calls — which is
  why what finally stopped two of these runs was noticing the same `bash`
  command five times, several minutes in, rather than the nine failed checks.
  And `agent.rs:610` already counts consecutive validation failures, but counts
  *any* failures: three different errors is a model working through a problem,
  three identical ones is not, and it treats them alike.

  **The fix is small and the plumbing exists.** `Event::Validation { ok, detail }`
  (`event.rs:61`) is already emitted on every failed check with the check's
  output as `detail`, and `Supervisor` already has the
  `HashMap<String, u32>` + threshold + nudge-then-escalate shape. What is missing
  is a match arm. Normalise the detail first: our own failures carry a temp path
  and a `line 32` that moves every time the model edits above it, so a raw hash
  changes precisely when the model is editing, which is when you need it not to.

  Not needed, having checked: search/replace (that is `edit`, already anchored
  and unique-match), a `write` nudge (never called), or a port from
  `rustopedia/` (its circuit breaker guards patch-format drift, which worksmith
  structurally cannot have, since tool calls are schema-validated).

- **`config check` accepts `--trust-project` and ignores it.** The flag is on
  the subcommand's `--help`, but `run_config_check` never passes it:
  `Check::run(cwd, probe)` consults only the trust store, so an untrusted
  project config is reported `not trusted` and every key in it is silently
  omitted from the report. Found while adding `[models."omlx/..."]` tables to
  this repo's project config — they did not appear, and the natural reading is
  that the tables are broken rather than that the report is. `main.rs:1479`.

- **`/pair` bare toggles instead of reporting.** Every other state command
  (`/validate`, `/route`, `/mouse`) reports when given no argument. `/pair`
  flips it, so checking whether pairing is on turns it off. Bare should report;
  `/pair on|off` should set.

- **A checkpoint's "Fifty steps, nothing written" subject is hardcoded.** The
  body interpolates `self.max_steps` correctly; the subject does not. With
  `max-steps = 100` it reads "Fifty steps, nothing written / It has used all
  100 steps". `agent.rs`, the `IdleReason::MaxSteps` arm.

- **Enter on an empty composer does nothing while a checkpoint is pending.**
  The handler returns early on empty input *before* it checks `pending_ask`, so
  the prompt says "Enter to send · Esc to skip" and bare Enter does neither. It
  should mean skip, like Esc. Found on the first real checkpoint.

- **`/model`'s mid-turn refusal goes to the footer, not the transcript.**
  `app.status` is overwritten by the next keystroke — the same complaint that
  produced `TurnOutcome::advice()` for the step limit. Probably wants
  `app.push`.

- **`@path` tab completion needs a double `//`.** Unconfirmed which half is
  broken: the trigger or the path resolution. Whether the popup shows nothing
  or shows wrong candidates distinguishes them.

- **`build.rs` breaks its own version stamp after `git gc`.** It watches
  `.git/HEAD` and `.git/refs`, but packing refs deletes the loose files and
  moves updates to `.git/packed-refs`, which is unwatched. The stamp then
  freezes — precisely the stale-binary failure the file exists to prevent.

- **`openai.rs`, four small ones.** `Message::tool_result` sets `name` and
  `message_to_json` never sends it (harmless for OpenAI-compat servers, but
  undocumented, unlike `reasoning`/`finish_reason`/`model` which say they are
  transcript-only). `Delta.content` is noise-stripped and `reasoning` is not.
  `warned_no_budget` is per-client, so a fan-out warns once per client rather
  than once. `usage.reasoning_tokens = r.len() / 4` counts bytes, so CJK
  reasoning undercounts by roughly a third.

- **A worker whose stream dies mid-reasoning is never retried.** `call_model`
  treats "already streamed" as final, and `ReasoningDelta` sets that flag. Right
  for the TUI, where re-streaming would double the visible text; much weaker for
  a worker nobody is watching.

- **`config check` exits non-zero for unexported keys of providers you do not
  use.** Correct by the letter, but it means the check is red on a healthy
  machine, which is how checks come to be ignored. An unexported key for the
  *session's* provider is a problem; for an idle one it is a note. Its result
  also varies by shell, since it reads the live environment — true and
  occasionally confusing.

## Gaps in what shipped

- **A blocking checkpoint has no timeout.** It waits on `pending_ask`
  indefinitely. `Asker` returns `None` when nobody is there, but in the TUI
  somebody always *structurally* is — the channel exists — so it cannot tell
  "away from the desk" from "thinking". Observed: a run sat at a checkpoint for
  an unknown length of time. A timeout that treats silence as a skip needs no
  new subsystem; a notification hook is the larger answer.

- **Compaction notes may now be squeezed, and may ossify.** After the prompt
  fix they run ~3,100 characters against a 1,024-token budget — about 75%, where
  they used to be 3%. Raise it if Locations starts getting truncated. Separately,
  each compaction summarizes the previous notes plus new messages, so knowledge
  carries forward — but nothing forces it to *absorb* new findings rather than
  re-emit the old ones.

- **Thinking cannot be both capped and steered.** `Thinking` is an enum, so
  `Budget(n)` and `Effort(level)` are exclusive: a budget sends
  `thinking_token_budget` (a hard server-side cap) and an effort sends
  `reasoning_effort` (a hint the model may ignore). "Think hard, but never more
  than 2k" is inexpressible, though vLLM would accept both fields. Measured: a
  2000 budget lands between 2,175 and 2,356 in practice — the stop is a
  boundary, not a line.

- **A fan-out runs its `--until` in every worker at once, in one directory.**
  `zola check` is safe, `zola build` deletes its output directory first, and
  nothing in a command string tells them apart. Documented in `/help`; M11's
  tree-per-worker is the real answer.

## Structural

- **`src/tui.rs` is over 5,000 lines and no small model can hold it.** Measured:
  asked for three plan steps at once, a 27B spent 104 then 300 steps and made
  **zero edits**, reading that one file 17 and then 110 times — at ~200 lines per
  read against the 8k tool-result cap, into a 65k window that compacts at 49k.
  Scoped to one step it finished in 25. For a harness whose thesis is that small
  models can do real work, this file is the wound. The command dispatch is a
  clean seam.

- **`CONVENTIONS.md` §12 does not mention `build.rs`**, so the document written
  to answer "where does everything live" does not cover a file in the root.

- **Unreviewed:** most of `tui.rs`, plus `memory.rs`, `knowledge.rs`,
  `mining.rs`, `skill.rs`, `supervisor.rs` — roughly half the codebase. The
  reviewed half yielded a silent write-gate bypass, a fan-out that hung forever,
  a half-swapped model, an allocation sized by the network peer, and compaction
  quietly destroying context on every fire.

## Unfinished features

- **`MODEL_SWITCH_PLAN.md` §5** — cost segmentation. Prices change at a switch
  and the footer multiplies lifetime totals by one price, so the number is wrong
  after the first `/model`.
- **`MODEL_SWITCH_PLAN.md` step 5** — the same command in the plain REPL.
- **`worksmith config check`** — print the effective config with provenance
  (global / project / default) and run the validations. Provenance is the point:
  a key sourced from `default` while a loaded file sets it is a merge bug, which
  is exactly how `agent.pair` was inert for two days. Sibling of
  `config schema --json` from `DOCS_PLAN.md` §0.
- **Docs Phase 0.5** (`DOCS_PLAN.md` §7) — `config schema --json`,
  `tools list --json`, worker lifecycle events on the parent bus, session id
  printed on exit, shell completions.

## Worth knowing, not broken

- **Bigger context is right locally and wrong on a billed API.** On the A40 with
  KV headroom, tokens are free and a larger window means fewer compactions, each
  of which otherwise costs a full re-prefill (visible in vLLM's log as prompt
  throughput spiking after every rewrite). On OpenRouter the whole prompt is
  billed on every step, so a larger window costs money and compaction *saves* it.
  Same mechanism, opposite conclusion. `[models."…"].context` should be a
  deliberate number per model, not an inherited default.

- **The wall clock is the card, not the harness.** The A40 generates ~31 tok/s
  on this model at 100% utilization and 299 of 300W. A turn producing 52k output
  tokens takes half an hour and no prompt will change that. Everything here
  reduces *wasted* tokens; only MTP / speculative decoding attacks the rate
  itself.

## Ideas, not commitments

- **The `yours` checkpoint kind.** Stub the load-bearing function with `todo!()`
  and a contract comment, wire around it, leave it for the user. `cargo check`
  is the queue and `--until` is the check. The one kind that serves "I want to
  write code" rather than "tell me what you wrote".
- **A first-edit deadline.** "At most N steps before your first write." The
  observed failure is never starting, and the validation loop — the whole
  differentiator — only helps a turn that makes an attempt. The step-limit
  checkpoint is the same idea arriving after the budget is gone.
- **A notification hook.** `[notify] on = [...]` running a shell command off the
  event bus. One-way only: inbound control would mean the approval gate answers
  to something other than a person at the terminal. The valuable event is
  "blocked on you", not "done".
