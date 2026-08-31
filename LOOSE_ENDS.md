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
and 4a; compaction no longer trades the whole context for a sentence; the
forgiving tool-call parser (`llm/rescue.rs`); both checkpoint complaints — it
shows its evidence now, and it can take a question.

## Bugs

- **A worker's terminal status can print before its last events.** Seen live:
  the manager printed `agent w1 [stopped] ... stuck: the model returned an empty
  response`, and an `edit` on `rooms.py` appeared after it, which reads as a
  stopped worker still writing to your files.

  **It is not.** The worker's session file settles it:

  ```
  1788150324  tool_call      edit
  1788150333  nudge          Your last response was empty…
  1788150349  tool_call      edit          <- the one shown after "stopped"
  1788150356  turn_complete  stuck: the model returned an empty response
  ```

  Both edits precede `turn_complete` by seven seconds. The tail reads the
  worker's bounded log while the status line comes from the manager's runtime
  state, and nothing orders the two, so the terminal line can print before the
  tail has drained. Cosmetic, but alarming in exactly the moment a user is
  deciding whether a runaway worker actually stopped. The status line should
  come last.

  Ruled out along the way: a worker-level retry (nothing in `worker.rs` retries
  or respawns, one `run_turn` each), and a leaked write past cancellation, which
  the timestamps above disprove.

- **A small model drops out of structured tool calls and the turn is scored
  empty.** *Fixed — `src/llm/rescue.rs`.* Seen live, hosted qwen3.5-9b: the
  model emitted `<tool_call><function=bash><parameter=command>python -m pytest…`
  as ordinary text instead of through the API's `tool_calls` field. Worksmith
  saw no calls, scored the reply empty, nudged, and got the same thing back —
  "Stopped going in circles: the model returned an empty response."

  The reply is now read rather than re-requested: when `tool_calls` comes back
  empty, `content` and then `reasoning` are scanned for a call, and a call to an
  *advertised* tool with arguments that parse is promoted to a real one. The
  block is taken out of the field it came from so the same call is not both
  spoken and executed, and the prose around it stays. Announced once per session
  on the warning channel, because a model doing this is drifting and that is
  worth knowing when choosing one.

  **Two things the plan had wrong, both found in the code:**

  1. The `<tool_call>` wrapper is *already stripped from `content`* by
     `strip_toolcall_noise` (`openai.rs:526`), which has to be there because
     providers leak fragments of it into ordinary text. A parser anchored on
     that wrapper would have found nothing in the channel it was written for.
     It anchors on `<function=` and on the JSON object instead. The same
     stripping is why a third shape is accepted: the classic Hermes
     `<tool_call>{"name":…}</tool_call>` arrives here as a bare object, and it
     is taken only when it is the entire field.
  2. `Accumulator::into_completion` cannot emit the warning — the sink is not
     in scope. The rescue runs one line later in `stream()`, where `req.tools`
     supplies the advertised-name check without `ToolRegistry` being involved
     at all.

  Not measured against a model yet. The check is the count of `stuck: the model
  returned an empty response` outcomes across a hosted-9B run, which should fall
  to near zero; the regression to watch is the harness arm's 162/164 on
  HumanEval, scored 2026-08-30 with the parser absent.

- **A checkpoint asks for help without showing the evidence.** *Fixed —
  `recent_evidence` in `agent.rs`.* The same incident: the "Going in circles"
  checkpoint offered only `the model returned an empty response`. It did not
  show the `<tool_call>` XML that *was* the problem, so the user had to read it
  out of the transcript to answer usefully.

  Every checkpoint now carries the last six messages rendered for a human —
  what the model said, what it called, what came back — appended to the question
  under "What just happened". `reasoning` is included when `content` is empty,
  which is the whole point: on this exact failure the words are in the reasoning
  and nowhere else. The retry directives in `rustopedia/retry_loop.rs` were the
  model for it, handing back the real slice rather than a summary.

- **The checkpoint wants a directive and cannot take a question.** *Fixed —
  `harness_checkpoint`.* The answer was appended as `"You were repeating
  yourself (…). The user says:\n\n{a}\n\nFollow that."`, so asking "what
  seems to be the issue?" was passed to the model as an instruction to follow,
  and it spent the one intervention the turn allows, since `offered_a_way_in`
  blocks a second.

  An answer ending in `?` is now answered instead of obeyed: a side call
  (`Agent::ask`, given the same evidence) replies, the reply is shown, and the
  checkpoint asks again. Nothing is appended to the conversation and
  `offered_a_way_in` is untouched, because nothing was decided. Bounded at
  `MAX_CHECKPOINT_ROUNDS` = 4 — a conversation that never reaches an
  instruction is still a turn that never ends — and running out is a normal
  ending, not an error.

  The question test is `ends_with('?')` and deliberately nothing cleverer.
  Anything smarter occasionally reads a directive as a question and refuses to
  act on it, which is the worse of the two mistakes.

- **Pair mode has no visual identity.** When the harness stops to ask, it looks
  like any other prompt. It should read as a different mode — that the loop is
  waiting on *you*, what triggered it, and what a useful answer looks like.

- **A big source file can eat a small context, and the only defence is advice
  the model already follows.** Reported from real use: `max-steps = 100` hit
  repeatedly on qwen3.8 via OpenRouter while working on documents, because
  reading large Rust files exhausts a 32,768-token window.

  `cap()` (`tools/mod.rs:94`) trims a result at `MAX_TOOL_RESULT_BYTES` and
  appends a notice telling the model to grep for what it needs rather than page
  through. The comment above it records this failing anyway: *fifty steps, 46
  reads, seventeen of them the same file, and not one edit* — while the model
  used `offset`/`limit` for 41 of them. It obeyed and still lost the turn, so
  the fix is not better wording.

  **What the cap does for Markdown and not for code** is the shape of the fix.
  On truncation it pulls headings from the whole file first, so the notice can
  name what was cut. A `.md` file gets an outline; a 5,000-line `.rs` file gets
  "about 24 further reads, do not". `rustopedia/` already has the missing half:
  `File Skeletons` and `Current Code Facts` list every struct, enum, field and
  fn in the touched files, which turns `src/tui.rs` from 5,070 lines of text
  into roughly 100 lines of signatures with line numbers, followed by one
  targeted read.

  Not measured here. Every task in `evals/pool/` works on files of 20 to 80
  lines, so nothing in the suite exercises this and the evidence is one report
  plus one code comment. A fixture with a genuinely large file would settle both
  the size of the problem and whether a skeleton fixes it.

- **The two tool-output caps are fixed constants and share a name.**
  `tools/mod.rs:88` is 8,000 bytes, `agent.rs:29` is 24,000, both called
  `MAX_TOOL_RESULT_BYTES`, and neither is derived from the model's context
  window. At 8,000 bytes a read costs roughly 2,000 tokens: 6% of a 32k window
  each, so a dozen reads is most of it and compaction then discards the earliest
  ones. The same constant on a 262k model is trivially small. `ResolvedModel`
  already carries `settings.context`, which is what the footer and compaction
  use, so scaling from it is small. Two constants with one name is worth fixing
  regardless: it reads as one value until you grep.

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

- **A worker's approvals queue behind the composer; its checkpoints were
  deliberately spared that and its approvals were not.** `fork_with`
  (`agent.rs:417`) replaces the asker with `NoOneToAsk` and argues for it: a
  blocking question stalls a background task against a user who does not know
  it was asked, and a fan-out of five would queue five questions behind one
  composer. The approver is inherited unchanged by `tool_ctx.clone()` and has
  exactly that hazard, without the same consideration.

  The unattended case is already right, contrary to an earlier note here:
  `--print` and `--mode json` get `RefuseWhenUnattended`, so a background worker
  refuses `git push` on its own. Only `--approve-all` bypasses it, which is the
  eval's deliberate choice and how it overwrote its own answer key.

  So the open question is the TUI with several workers, where refusing would be
  wrong (a worker may legitimately need to push) and queueing five surprise
  prompts is what the asker comment already rejects for checkpoints. Unmeasured:
  every worker run recorded here was a single worker on a single task.

- **An unattended worker can approve its own outward-facing actions.**
  `approve_write_outside_cwd` (`tools/mod.rs:270`) does gate writes that leave
  the project, so the earlier note here blaming a missing confinement was wrong:
  the eval overwrote its own answer key because it passes `--approve-all`, which
  approves the gate on the model's behalf. That is the eval's choice and fine
  for the eval. The open question is what a *spawned worker* inherits. Nobody is
  watching a background worker, so it answering its own approval prompt is the
  same hazard `PAIR_PLAN.md` names for checkpoints and solves with
  `RefuseWhenUnattended`. Worth checking which approver `worker.rs` hands out
  before the supervisor grows any policy role, since enforcement lives in the
  tool layer and the supervisor only observes.

- **`config check` accepts `--trust-project` and ignores it.** The flag is on
  the subcommand's `--help`, but `run_config_check` never passes it:
  `Check::run(cwd, probe)` consults only the trust store, so an untrusted
  project config is reported `not trusted` and every key in it is silently
  omitted from the report. Found while adding `[models."omlx/..."]` tables to
  this repo's project config — they did not appear, and the natural reading is
  that the tables are broken rather than that the report is. `main.rs:1479`.

- **1,620 sessions in one flat directory, 89% of them junk, and the TUI never
  shows you which one you are in.** Reported from use: finding a particular
  session is hard. Measured, and the naming is only half of it.

  `~/.worksmith/sessions/<uuid>.jsonl`, one directory, no nesting. The filename
  carries no project, no date, and no indication whether it was a main session
  or a spawned worker — every worker gets its own file, so a fan-out of three
  adds four.

  **The larger half: the store is 89% throwaway.** Of 1,620 files, **1,447**
  have a `cwd` under `/var/folders` or `/tmp` — `cargo test` runs and eval-pool
  fixtures, written into the user's real session store. Only **173** are actual
  project work, and 124 of those are this repo. So it is not merely that the
  names are opaque; the signal is one file in nine. Tests and evals should be
  writing somewhere else, which is a smaller fix than a directory scheme and
  probably wants doing first.

  **It is also a performance bug on the path that matters.**
  `most_recent_for_cwd` (`session.rs:169`) opens and parses the first line of
  *every* file in the directory to find one whose `cwd` matches. That is the
  `--resume` path, and it gets slower with every test run.

  **And the id is never visible.** `Event::SessionStarted { id }` is emitted and
  the TUI's handler explicitly drops it (`tui.rs:890`,
  `SessionStarted { .. } | TurnComplete { .. } => {}`). It is formatted at
  `tui.rs:2765` for the one-line event summary, so the string exists and simply
  never reaches the screen. There is no `/session` command and nothing in the
  footer, so the id you would need in order to go find the file is the one thing
  you cannot read off the session. `DOCS_PLAN.md` Phase 0.5 already wants it
  printed on exit; showing it live is the cheaper half.

  The meta line already carries `cwd` and `ts`, so a scheme like
  `sessions/<project-slug>/<date>-<short-id>.jsonl` needs no new data — only a
  migration for the existing files, and a decision about what a worker's file is
  named relative to its parent's.

- **`/pair` bare toggles instead of reporting.** Every other state command
  (`/validate`, `/route`, `/mouse`) reports when given no argument. `/pair`
  flips it, so checking whether pairing is on turns it off. Bare should report;
  `/pair on|off` should set.

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
