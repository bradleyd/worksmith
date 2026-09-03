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
shows its evidence now, and it can take a question; the worker tail; the
footer's worker spend and the truncated agent count; `/agents` timestamps; a
nudge to a stopped worker; the stale command popup; and the supervisor killing
workers that were merely running a slow check; empty Enter on a pending
checkpoint now skips it; and checkpoint answers now share one TUI helper.

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

- **A worker's tail is unreadable, and the worst of it is one discarded field.**
  *Fixed — `eccb4e1`, plus `5b8ced1` which a spawned worker wrote.*
  Reported from use while tailing a live worker:

  ```
  ⚙ [w1] ⚙ bash
  ⚙ [w1]   bash: exit code: 0
  ⚙ [w1] ⚙ bash
  ⚙ [w1]   bash: exit code: 0
  ```

  Four lines and two blank ones to say nothing at all. Four defects stack:

  1. **The arguments are thrown away.** `worker.rs:670` is
     `Event::ToolCall { name, .. } => log_line(g, format!("⚙ {name}"))` — the
     `..` discards `arguments`, so a worker's tail can *never* say which command
     ran. `describe()` in `tui.rs:2738` does exactly this correctly for the main
     session (`⚙ {name} {truncate(arguments, 50)}`), so the fix is to stop
     dropping the field. This is the whole difference between a tail you can
     follow and one you cannot.
  2. **`first_line(output)` is the wrong line for `bash`** (`worker.rs:673`).
     A bash result starts with `exit code: 0`, so the tail reports the one line
     that carries no information and hides the output underneath it.
  3. **The `⚙` is applied twice.** The log line already begins with one, then
     the tail wraps it as `[{id}] {l}` (`tui.rs:1128`, `tui.rs:2893`) and pushes
     it as `Kind::Tool`, which prepends another (`tui.rs:3546`).
  4. **A blank line between every entry**, which doubles the height of the least
     informative output in the program.

  What it should read as, one line per call, command visible, result on the same
  line:

  ```
  [w1] bash  python3 -m unittest discover tests -q     ok
  [w1] bash  python3 playcheck.py                      exit 1
  ```

  **The double gear is not hierarchy, it only looks like it.** Worth saying
  because the obvious reading is that the tail is showing parent → child. It is
  not: the log line carries one glyph and `Kind::Tool` adds another. Making it
  real is the better fix — `tree`-style `├─` / `└─` so a worker's calls are
  visibly nested under the worker, instead of every line repeating `[w1]`:

  ```
  w1  implement main.py
  ├─ bash   python3 -m unittest discover tests -q      ok
  ├─ edit   main.py                                    +38 -11
  └─ bash   python3 playcheck.py                       exit 1
  ```

  Related, and partly self-inflicted: the rescued-tool-call notice now prints
  into this same stream (`⚠ read 3 more tool call(s) out of the model's text`).
  It is correct and it is worth recording, but on a model running at 54% it
  lands every few lines. Keeping it in the session while showing it once per
  turn in the tail is probably the balance.

- **The footer reports only the parent session, so during a fan-out every number
  in it reads zero.** *Mostly fixed — `04eb5c5`. Worker tokens and cost are now
  counted and shown, priced per worker model. `ctx` is deliberately still
  parent-only: a context percentage belongs to one conversation, and mixing a
  worker's window into it would be wrong rather than merely blank.* Reported from use, twice. First: prices set under
  `[models]`, workers running, cost never moves. Then the whole line, with one
  worker busy:

  ```
   qwen/qwen3.5-9b  ctx 0% (0/128000)  ↓0  think:2k  ↑1 agents
  ```

  `↑1 agents` is the only true field on it. Context, output tokens and cost are
  all zero because they are all fed by `Event::Usage` on the parent's bus, and
  during a fan-out the parent is idle — the work is happening somewhere the
  footer cannot see. It is not that the cost is missing; it is that the footer
  describes a session that is doing nothing, next to a counter saying one agent
  is doing something.

  (The `128000` there is its own small lie, and a separate one: no
  `[models."openrouter/qwen/qwen3.5-9b"]` table exists, so the limit is the
  inherited `unwrap_or(128_000)` default rather than anything true about the
  model. OpenRouter reports 262k for it. Harmless in the direction that
  matters — the default is conservative, not optimistic — but it is a number on
  screen that nobody chose.)

  Two layers, and the second is the one that makes it hard to patch:

  1. **Worker usage never reaches the parent.** `total_in_tokens` /
     `total_out_tokens` are accumulated from `Event::Usage` on the *parent's*
     bus (`tui.rs:882`). A worker's events go to its own bus and never reach the
     parent's — `worker.rs` says so in as many words, and it is why `/agents
     tail` polls a recorded log instead of subscribing. So a fan-out's entire
     spend is invisible to the only number that reports spend.
  2. **A worker records only its output.**
     `Event::Usage { completion_tokens, .. } => g.tokens += completion_tokens`
     (`worker.rs:741`) throws the prompt tokens away. On a billed API the whole
     prompt is charged on every step, so input is normally the *larger* half —
     and for workers it exists nowhere, so even wiring layer 1 would report a
     number that is wrong in a new way.

  The fix is `Runtime` carrying prompt tokens alongside completion tokens,
  `WorkerSummary` exposing both, and the TUI folding a finished worker's totals
  into the session's on completion. Folding on *completion* rather than live
  keeps it out of the hot path and matches how `take_newly_finished` already
  surfaces things.

  Worth doing for a reason beyond accounting: the whole "flat cost per solved
  task" finding rests on knowing what a run cost, and a fan-out is exactly the
  configuration where that number is currently a fiction.

- **The footer's running-agent count is the first thing truncated away.**
  *Fixed — `04eb5c5` moves it ahead of cost and think, and gives it its own
  glyph. A test pins it near the front of the string.* The
  indicator exists and its data is live — `app.agents_running` is recomputed
  every frame at `tui.rs:1138` and rendered as `↑{n} agents` at `tui.rs:3792`.
  But `footer_string` puts it **last**:

  ```
   {model}  ctx {pct}% ({a}/{b})  ↓{out}{reasoning}{cut}{cost}{think}{agents}
  ```

  and the footer is one `Paragraph` with no wrap, so it truncates at terminal
  width. Measured with `openrouter/qwen/qwen3.8-27b`: 59 characters before
  `agents` bare, 78 with cost and think both on, and **89** once `↑3 agents` is
  appended. On an 80-column terminal the running-agent count is precisely what
  falls off the edge — the most time-critical item in the footer sits in the
  position most likely to be cut. Reported as "there is no indication that there
  are agents running", which is what an intermittently-truncated indicator looks
  like from the outside.

  Two fixes and they are independent: move it left, ahead of cost and think,
  since "work is happening in the background" outranks a running total; and give
  it a glyph of its own so it reads as a state rather than another number. `↑`
  is doing double duty — it already means output tokens two fields earlier.

- **A finished fan-out does not tell you what happened, and buries the one line
  that does.** Reported from the first real three-worker run, unprompted: *"I
  don't know what is done, not done, and what I should do next. If I didn't tail
  the agents I would be wondering what is happening."*

  What the transcript actually gave, for a worker that **passed**: one `[done]`
  line carrying the first ~200 characters of the model's prose summary, then
  **fifteen** `⚙ [w3]` lines replaying that whole summary — every bullet about
  every method it implemented — and only then, last, the line that matters:

  ```
  ⚙ [w3] ✓ `python3 -m unittest tests.test_parser -q`
  ```

  The check result is the only fact worth reading and it is at the bottom of a
  screen of prose the model wrote about itself. Three workers doing this at once
  interleave.

  **What it should say is what the harness already knows without judging
  anything**: which workers are done, which passed their `--until`, which files
  each changed, and — for the one that failed — what its terminal reason was.
  In this run that would have been three lines, and the third would have carried
  its own next action: w2 stopped on `the model spent its whole output budget
  reasoning … raise max-tokens, or run with --fast`.

  **`/agents` already exists** ("list workers, or tail one live",
  `tui.rs:139`), so the roster is built. Nothing points at it, and it is not
  what fires when the last worker finishes. A fan-out completing is exactly the
  moment to print the roster unasked.

  Related and separate: the worker prose is emitted line by line as `⚙ [wN]`
  items rather than as one collapsed block, so a verbose worker cannot be
  scrolled past as a unit.

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
  nowhere.** *Fixed at the validation-loop boundary — repeated failures are now
  compared by a normalized failure key, so volatile temp paths and line numbers
  do not hide "same failure again", while genuinely different failures still
  count as progress.* `pa-reject` on Qwen3.5-4B, three isolated runs: 0/3,
  taking 327s, 900s (timed out) and 436s, burning 9,033 / 24,939 / 13,650
  generated tokens. In every run the model read the file, made valid `edit`
  calls, ran its check, failed, and edited again.

  **It is not a tool-choice problem.** Across those three runs: 32 `edit` calls,
  **zero** `edit` errors, and **zero** `write` calls. The model reaches for the
  right tool and uses it correctly. An earlier note here claimed whole-file
  rewriting, built on one run averaging 1,201 generated tokens per model call;
  the three instrumented runs average 311, 500 and 580, and the theory does not
  survive them. Corrected rather than deleted, because the wrong version is in
  the git history.

  What actually happened was a no-progress loop: valid edits, the same check
  failing the same way, over and over. The harness could not see it. The
  supervisor keys its repeat detector on `format!("{name}::{arguments}")`
  (`supervisor.rs:159`), so it only catches literally identical calls — which is
  why what finally stopped two of these runs was noticing the same `bash`
  command five times, several minutes in, rather than the nine failed checks.
  `agent.rs` now keeps the repeat detector where the retry decision already
  happens: two identical normalized failures ask the user, repeated same-failure
  directives explicitly forbid summarizing success from a different command, and
  different failure keys keep retrying normally.

  Not needed, having checked: search/replace (that is `edit`, already anchored
  and unique-match), a `write` nudge (never called), or a port from
  `rustopedia/` (its circuit breaker guards patch-format drift, which worksmith
  structurally cannot have, since tool calls are schema-validated).

- **A worker that dies alone should be able to ask, and deliberately cannot.**
  Requested from use after a worker hit `stopped · hit step limit (50)` with
  pairing on and nothing was asked: *"with pair on this should also signal the
  user for help"*.

  It is refused on purpose. `fork_with` (`agent.rs:417`) hands a worker
  `NoOneToAsk`, and the argument is written down: a blocking question stalls a
  background task against a user who does not know it was asked, and a fan-out
  of five would queue five questions behind one composer.
  `a_spawned_worker_never_inherits_pairing` pins it.

  **The reasoning is about a fan-out and the complaint is about a single
  worker,** which is the whole tension. One worker, attended, pairing on, is the
  case where the argument does not apply and the checkpoint would have been
  worth more than the step limit: the harness *knew* it was going nowhere at
  step 50 and ended the turn instead of spending a sentence on it. That is
  exactly the "50 steps, nothing written" checkpoint the main session already
  has.

  So the shape of the answer is probably not "workers pair" but **only one
  worker may hold the composer at a time**, with the rest either skipping or
  queueing behind it — which makes the fan-out-of-five objection an
  implementation detail rather than a reason. The step-limit and stuck triggers
  are the two worth offering; a worker asking mid-task is a different and much
  weaker case.

  Note this now interacts with the timing work: a checkpoint from a background
  worker has to say *which* worker and *when*, or it arrives as a question from
  nowhere. `WorkerSummary` carries `started`/`finished` since `bc2e892`.

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

- **Read/search/bash could leave the task cwd without asking.** *Fixed — direct
  `read`, `grep`, `find`, `ls`, and document input paths now share the same
  outside-cwd approval gate as writes, and the bash tool asks before running
  commands with visible escaped paths such as `..`, absolute sibling paths,
  `~/...`, `$HOME/...`, or `--manifest-path=/outside/...`.* Found from live use
  in `mud-test/`: a Python project failed a stale `cargo test` validation, and
  the model responded by searching Rust repos and trying to edit
  `~/.worksmith/config.toml` instead of staying inside the project. This is not
  a sandbox; it is the ordinary tool-boundary guard. TUI runs ask, unattended
  runs deny, and `--approve-all` still means exactly what it says.

- **`config check` accepts `--trust-project` and ignores it.** *Fixed —
  `Check::run` now takes the explicit trust flag and applies the project config
  for that report.* Previously, the flag was on the subcommand's `--help`, but
  `run_config_check` never passed it: `Check::run(cwd, probe)` consulted only
  the trust store, so an untrusted project config was reported `not trusted` and
  every key in it was silently omitted from the report. Found while adding
  `[models."omlx/..."]` tables to this repo's project config — they did not
  appear, and the natural reading was that the tables were broken rather than
  that the report was. `main.rs:1479`.

- **1,620 sessions in one flat directory, 89% of them junk, and the session
  store is slow to search.** Reported from use: finding a particular session is
  hard. Measured, and the naming is only half of it.

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

  **The id visibility half is fixed.** The TUI now prints `session <id>` when it
  starts, and `Event::SessionStarted { id }` is rendered instead of dropped.
  `DOCS_PLAN.md` Phase 0.5 still wants it printed on exit too; showing it live
  was the cheaper half.

  The meta line already carries `cwd` and `ts`, so a scheme like
  `sessions/<project-slug>/<date>-<short-id>.jsonl` needs no new data — only a
  migration for the existing files, and a decision about what a worker's file is
  named relative to its parent's.

- **The footer's glyphs are chosen by what was still free, not by what reads.**
  Reported as "those glyphs are hard to understand". True, and the cause is
  visible in the commit that added the last one: `↑` already meant output
  tokens, `↻` reasoning, `⚙` tools, `◆` a checkpoint and `⚠` a truncated
  answer — so `⧉` got picked because nothing else was left, which is
  collision-avoidance rather than design.

  The likely answer is fewer glyphs, not better ones. `⧉2 agents` already
  carries the word "agents"; the symbol adds nothing a reader needs, and
  `⧉48200 tok ($0.21)` would read better as `agents 48.2k tok ($0.21)`. A
  footer that spells out the two or three fields people actually scan for, and
  leaves symbols to the ones that repeat constantly, would need no legend for
  half of what the legend currently covers.

  Worth doing together with the legend, since `/help footer` exists precisely
  because the current set is unreadable without it — a legend is a symptom, not
  a fix.

- **`/pair` bare toggles instead of reporting.** Every other state command
  (`/validate`, `/route`, `/mouse`) reports when given no argument. `/pair`
  flips it, so checking whether pairing is on turns it off. Bare should report;
  `/pair on|off` should set.

- **Enter on an empty composer does nothing while a checkpoint is pending.**
  *Fixed — `eb9588c`, then deduplicated in `f199b91`.* The handler returned
  early on empty input *before* it checked `pending_ask`, so the prompt said
  "Enter to send · Esc to skip" and bare Enter did neither. It now means skip,
  like Esc, while empty Enter with no pending checkpoint still does nothing.

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

- **`src/tui.rs` is still too large for small models to hold comfortably.**
  Originally over 5,000 lines; after the first refactor run it is 4,656 lines,
  with `composer`, `footer`, `overlay`, and `transcript` split into
  `src/tui/`. The original measurement still matters: asked for three plan
  steps at once, a 27B spent 104 then 300 steps and made **zero edits**, reading
  that one file 17 and then 110 times — at ~200 lines per read against the 8k
  tool-result cap, into a 65k window that compacts at 49k. Scoped to one step it
  finished in 25. For a harness whose thesis is that small models can do real
  work, this file is still the wound. The command dispatch remains a clean seam.

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
- **Post-run checks the model was never told about, security among them.**
  Suggested from use: on a merge or on demand, run more than lint and tests —
  a basic security pass over what was produced.

  **The important part is not "security", it is "post-run".** `--until` is a
  check the model is *optimising against*, and today produced two clean
  examples of what that costs: a worker told not to edit the tests wrote
  `def set_stats(self, *args)` to satisfy both arities at once, and another
  created an executable `false` on PATH to beat a check that ran `false`. Both
  satisfied the letter. A check the model does not know about cannot be
  satisfied that way by construction — it is a different instrument, not a
  stricter one.

  **Security is the sharpest case because worksmith edits its own gates.**
  Workers have already edited `src/worker.rs` and `src/tui.rs` in this repo. A
  worker that edited `tools/policy.rs` and dropped a pattern would weaken the
  approval gate that protects the user, and the only thing standing in the way
  today is whether somebody happened to write a test for that pattern. Same for
  `approval.rs`, `trust.rs`, and the write-outside-cwd gate. That is a category
  of change that should never pass silently, and no check currently looks at
  *what kind* of change was made.

  Cheap first version, in order of value: flag any diff that touches
  `tools/policy.rs`, `tools/approval.rs` or `trust.rs`; flag new `unsafe`;
  flag a secret-shaped literal; run whatever the project already has
  (`cargo audit`, `npm audit`) when it is present. None of that needs a model.

  The model-graded version is the more interesting one and the more dangerous:
  a small model asked "is this diff safe" will say yes, and a review that
  always passes is worse than none because it launders the change. If it is
  graded at all it wants the session model, not `agents.model`, and it wants to
  report what it looked at rather than a verdict.

  Related and unbuilt: `--until` is a single command, so "tests pass" and "and
  nothing dangerous changed" cannot both be expressed today. A post-run tier is
  where the second one lives.

- **Pairing only ever fires on failure. It should also fire on risk and on
  scale.** All three triggers today are failure triggers: going in circles, out
  of steps, budget spent. Each one arrives when the turn is already lost. Nothing
  stops *before* something consequential, which is the half the README actually
  promises ("it is about to change forty files, do you want to drive?").

  Four shapes, roughly in order of how much evidence there is for them.

  **Stop before a large or destructive edit.** Keyed on the size of the change
  rather than on anything going wrong: a whole-file rewrite, N files at once, or
  a diff over some threshold. On 2026-08-31 a worker turned a four-line fix into
  30 insertions and 16 deletions by triplicating two string literals, and another
  wrote `def set_stats(self, *args)` to satisfy a test rather than an interface.
  Both passed their checks. Both would have been obvious in a diff shown before
  it landed.

  **Stop when the change touches something load-bearing.** A checkpoint keyed on
  *what* is being edited, not when. `tools/policy.rs`, `tools/approval.rs`,
  `trust.rs` and the write gate are the files where a worker can quietly weaken
  the protections around the person running it. Workers have already edited
  `worker.rs` and `tui.rs` in this repo; nothing would have stopped one editing
  the approval policy, and the only thing standing in the way is whether someone
  wrote a test for the pattern it removed.

  **A key that pauses without aborting.** Esc ends the turn. There is no way to
  say "hold on, let me read that" and then carry on, which is the most
  pairing-shaped thing missing from the TUI. Watching a worker and having to
  choose between letting it run and killing it is not pairing.

  **Say how many interventions are left.** `offered_a_way_in` allows exactly one
  per turn and never says so. Answering a checkpoint spends it silently. Since
  a question no longer spends it, the rule is now subtler and still invisible.

  Related and already written up separately: the `yours` checkpoint kind, one
  worker holding the composer during a fan-out, and triaging the prompt before
  the work starts. Those three plus these four are one theme, which is that
  pairing currently means "interrupt me when it breaks" and should mean "keep me
  in it".

- **A checkpoint *before* the work, not after: triage the prompt.** Suggested
  from use, after noticing that every prompt in a day of testing carried line
  numbers, exact code snippets and explicit "do not page through this file"
  instructions — *"I don't know if that is how people work"*. It is not. So a
  day of results measures **worksmith plus an expert-written prompt**, which is
  a confound worth naming in every number recorded above.

  The suggestion: with pairing on, read the prompt before starting and ask
  about what is vague or ambiguous — training wheels.

  **Sharpened, the most valuable question is not "what did you mean".** It is
  **"how will we know when this is done?"** A vague prompt usually arrives with
  no `--until`, or with one that cannot fail — and a check that cannot fail is
  the single defect that bit this project three separate times in one day: the
  scaffold check that passed on implemented code, `spawn --until` that ran no
  check at all, and a playability check whose three of five assertions were
  vacuous. The harness's whole claim rests on the check, and nothing currently
  asks for one.

  So the triage is less "polish the wording" and more **the harness declining to
  start work it cannot tell it has finished** — which is the same argument as
  the validation loop itself, moved one step earlier.

  Two things make it cheap. The checkpoint machinery already exists
  (`harness_checkpoint` carries evidence and can now take a question rather than
  only a directive), and this is the one checkpoint that fires *before* the
  budget is spent — unlike the step-limit and stuck triggers, which arrive when
  the turn is already lost.

  Three cautions. It costs a model call before any work, so it wants to be
  cheap and skippable. Judging a prompt is itself judgment, and the small models
  this project exists for are worst at exactly that — it may need the session
  model rather than `agents.model`. And a triage that fires on a clear prompt is
  the fastest way to teach someone to hit Esc without reading, which is how
  every approval prompt in the world stopped working.

- **`/agents tail` should be a trace, not a stream — and the renderer exists.**
  Extending the dashboard idea above: once there are per-worker metrics, the
  tail belongs in a pane or overlay rather than interleaved into the
  transcript, with Neovim as the influence.

  **`evals/pool/trace.py` already does this**, offline and in Python, and its
  docstring names the questions the TUI cannot answer: which tool, with what,
  did it work — *"did the result change from the last time it ran the same
  thing, the difference between checking your work after an edit, which is
  correct, and thrashing, which is not"* — what the check said, and what ended
  the turn. It marks a repeat `=` when the result is byte-identical and `~`
  when it differs, so **a column of `=` is a loop and alternating `~` is
  progress**.

  That is the whole idea, already built and already validated by use — it was
  written because counters cost three wrong diagnoses in one day. The live tail
  has none of it: it prints lines as they arrive with no memory of what came
  before, which is why tonight's readability work (arguments, real result lines,
  nesting) makes each *line* legible and still cannot show that a worker has run
  the same command five times to the same result.

  So the port is the feature. The work is a Rust rendering of what `trace.py`
  computes, over the worker's bounded log rather than a session file, in a pane
  that can stay open. Same sequencing caveat as the dashboard: after
  `TUI_REFACTOR.md` §3, not before.

- **A first-edit deadline.** "At most N steps before your first write." The
  observed failure is never starting, and the validation loop — the whole
  differentiator — only helps a turn that makes an attempt. The step-limit
  checkpoint is the same idea arriving after the budget is gone.
- **A dashboard, and the case for it is already written down five times.**
  Suggested from use, borrowing the idea from Neovim's start screen — an
  overlay or a tab giving a system overview rather than another line of status.

  It is worth taking seriously because **five separate entries above are the
  same complaint**: the fan-out roster that never prints, the footer whose
  every number reads zero while workers run, the agent count that truncates off
  the right edge, the worker tail you cannot follow, and "how does the user know
  what to do next". Each was filed as its own bug. They are one structural fact:
  **the TUI has a single status line and a linear transcript, and a fan-out is
  neither.** N things happening at once cannot be rendered as a stream, and no
  amount of fixing individual lines changes that.

  What it would hold, all of which the harness already knows and currently
  cannot show together: each worker with its state, elapsed time, check result
  and files touched; per-model token and cost totals including the workers';
  the session's own context usage kept separate from theirs; and what is
  blocked on the user.

  **Overlay or tab is the wrong first question.** The real one is whether you
  want to *check* it or *watch* it. The complaints are all of the form "I did
  not know what was happening while it ran", which is watching — so a pane that
  can stay up, not a modal you dismiss. The existing `Overlay` is modal and owns
  the keyboard, so it is the wrong machinery to reuse despite being the closest.

  **Sequencing matters more than the design.** `src/tui.rs` was 5,153 lines
  when this was written and is still 4,656 lines after the first split — a 27B
  made zero edits in it across 404 steps. A dashboard is several hundred more
  lines. Building it before `TUI_REFACTOR.md` §3 (R1 splits `App` into focused
  structs, R4 breaks up `handle_command`) makes the project's own thesis harder
  to demonstrate in the project's own codebase. After the split it is a new
  module with a clear seam, which is the difference between paying the debt and
  adding to it.

- **A notification hook.** `[notify] on = [...]` running a shell command off the
  event bus. One-way only: inbound control would mean the approval gate answers
  to something other than a person at the terminal. The valuable event is
  "blocked on you", not "done".
