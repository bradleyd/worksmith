# Plan: pairing and the session trace

Updated 2026-09-11. **Status: first pairing slice implemented on `codex/pairing-guidance`; offline validation complete.**
This is the next recommended milestone in `NEXT.md`. The original checkpoint
plan is retained below as history, including evidence that prompting alone did
not reliably produce useful checkpoints.

## First slice: implementation decisions

The user authorized starting the first phase on a new branch. This slice adds
mode-aware request guidance and shares bounded question handling between model
and harness checkpoints. Tool calls batched after a blocking checkpoint are
returned as deferred results so the next request can incorporate the answer.
Questions are not filed as decisions. Cancellation and the four-round discussion
limit stop work; skips retain their existing policy. Existing structural events
record questions, answers, and helper accounting without adding an event type.

Resume deliberately retains the current policy: use `[agent].pair` from current
config, with workers/headless execution still off. Persisting live mode changes
is deferred, avoiding a new event/config precedence rule in this first slice.
The request adds guidance once to the existing system message, accounts for it,
and uses one pairing snapshot for tools and context-fit retries. Compaction and
resume rebuild guidance rather than storing repeated reminders in history.

The existing question heuristic remains a trailing question mark. It does not
infer the intent of ambiguous prose. A prompt test proves guidance delivery, not
that a model recognizes consequential decisions. Live 27B dogfooding remains
before claiming that behavioral benefit. Trace rendering and its UX choices are
still proposals; this branch does not implement them.

### First-slice validation

- `cargo test --locked`: 507 passed, 0 failed, 2 live provider probes ignored.
- `cargo clippy --locked --all-targets -- -D warnings`: passed.
- Removing the pairing instruction deliberately caused the request-guard test
  to fail; restored source passes the full suite.
- PTY smoke against a temporary scripted local server passed: pairing on,
  checkpoint, follow-up question, explanation, decision, completion, pairing off.
  Only the decision was filed; the question was retained in structural history.
- `git diff --check`: passed. No live model quality claim; local 27B dogfooding
  and trace UI work remain next.

## 1. Problem and intended result

Before this slice, the model received a task-execution prompt even when `/pair` is on.
Pairing advertised a checkpoint tool and enabled harness interruptions, but it
did not establish a collaboration mode in the main request instructions.
Meanwhile, constantly streaming tool output makes it hard to follow the work,
find a decision, or inspect its evidence without losing one's place.

Make the main session readable as a trace of the work, with the user involved
in consequential decisions. Success means the user can see what is happening,
understand why a choice matters, inspect details, and influence the next action.
It does not mean more interruptions, more model calls, or merely a prettier log.

## 2. Current foundation and limits

- `src/prompt.rs` assembles the base instructions, project instructions, and
  skill catalog; it has a worker preamble but no pairing preamble.
- `src/agent.rs` owns pairing state, tool advertisement, request assembly, and
  mechanical checkpoints. Workers have independent pairing-off state.
- `src/tui.rs` owns `/pair`, pending questions, and event handling.
  `src/tui/transcript.rs` already owns transcript items, selection/search state,
  wrapping caches, and much of rendering. Extraction has started.
- Ctrl+O globally toggles tool collapsing; Ctrl+T toggles thinking visibility.
  The new work adds individual expansion and grouping, not a second transcript.
- The typed event bus and recorded session events provide tool IDs, model
  metrics, validation evidence, and checkpoints. Not every desired relationship
  or duration is present. Inspect live and replay paths before adding fields.
- CLI `--mode json` already emits request metrics. `worksmith stats ID --json`
  aggregates them. Request duration excludes subsequent tools; generic `helper`
  attribution does not identify each helper role.

## 3. Proposed pairing behavior

Reuse `/pair on|off` and the existing config switch. No new global knobs.
At request construction, snapshot the mode with the advertised tool set and add
one short pairing instruction when enabled. Keep the existing first-system-
message shape compatible with local providers. Do not append repeated reminders
into durable user history or change production memory placement.

Candidate wording to evaluate:

> You are pairing with the user on this task. Keep them involved in consequential
> decisions. Before implementing a meaningful design choice, explain the
> tradeoff, recommend an option, and use a checkpoint to get their direction.
> Proceed with routine implementation details. Answer questions before resuming
> work and incorporate the user's decisions into subsequent changes.

This changes guidance, not enforcement. Retain mechanical validation/stuck
checkpoints and existing caps. Do not add a model observer or ask before every
edit. Questions at a checkpoint remain discussion until the user gives direction
or explicitly skips/stops; inspect current question detection and bounded
follow-up handling before choosing any new UI control. Exhausting a discussion
budget must not silently turn an unanswered decision into approval.

Pairing off removes the instruction and checkpoint advertisement on subsequent
requests; an in-flight request keeps its original snapshot. Switching mode must
not silently answer an already pending question. Keep workers and unattended
runs' existing behavior. Rebuild guidance after compaction; explicitly decide
resume persistence below rather than relying on old prompt text in history.

## 4. Proposed main-session view

Mockups are illustrative, not measured timings or an implemented key contract.
Use terminal text, indentation, and existing theme colors. Status must be legible
without color, at narrow widths, and on light and dark terminals.

Working:

```text
▼ Fix reconnect handling                          working · 24s
  I'll inspect the reconnect path and its tests.
  ▶ ✓ Read connection.rs                                  0.2s
  ▶ ✓ Find reconnect callers                              0.1s
  ▼ Model response                               first 1.3s
    The retry counter currently resets on every attempt…
  ▶ … Run cargo test                                      4.1s

PAIR ON   working                            [existing footer]
> [composer]
```

Waiting for a decision:

```text
▼ Fix reconnect handling                      waiting for you
  ▶ ✓ Read connection.rs
  ▼ ? Retry state ownership
    The counter resets on each reconnect attempt.
    Recommend keeping it on Connection so attempts share a budget.
    Should a successful connection reset that budget?
    ▶ Relevant code / available evidence

PAIR ON   waiting for your answer
> [answer, or ask about the tradeoff]
```

Finished:

```text
▼ Fix reconnect handling                         done · 48s
  ▶ ✓ 2 tool calls before the decision
  ▼ Decision: reset retry budget after success
    You: Yes, reset it after a successful connection.
  ▶ ✓ Edit connection.rs
  ▶ ✓ cargo test passed                                  5.2s
  The retry budget now survives failed reconnects and resets on success.
  ▶ Turn details: model requests, tokens, available cost

PAIR ON   ready
> [composer]
```

The compact multi-call row above is optional polish after individual rows work.
Never generate phase labels or summaries with a model. Root labels derive from
user text; group only relationships supported by events. Do not invent pending
steps, a successful check, or a planned DAG from an unfinished model response.

### Interaction defaults proposed for discussion

- Group by user turn, with tool calls/results joined by ID. Keep assistant prose
  and decisions in chronological order. Model request details are expandable;
  do not put an entire useful answer behind a metrics row.
- Stream assistant prose. Show tool activity in compact rows; successful output
  starts collapsed. Failures and pending decisions start expanded. Honor an
  explicit user expansion choice when the operation completes.
- Keep thinking available through the existing toggle; propose hidden by default
  for the calmer view. Rendering visibility never changes requests or storage.
- In existing transcript navigation mode, propose Enter to toggle the selected
  block. Audit current bindings first. In composer mode, Enter still sends text
  or answers the pending question. Keep Ctrl+O as the bulk tool-output toggle.
- Streaming cannot steal selection or scroll the user away from older content.
  Show that new activity exists while scrolled up; following the tail is explicit.
- Search includes hidden details and expands a matched ancestor. Copy uses the
  selected content consistently with existing behavior, not just visual labels.
- Pending decisions stay visible; answered decisions retain the question, answer,
  and evidence. Approval prompts remain distinct and retain their safety rules.
- The trace is useful with pairing off too. Pairing changes collaboration and
  decision presentation, not whether activity can be inspected.

## 5. Data and module boundaries

Use a small event-to-trace projection with stable identities independent of
wrapped screen rows. Start with turns and tool call/result pairs; retain unmatched
or legacy events as readable entries rather than dropping them. Do not infer
parentage from temporal overlap, especially for helper or worker activity.

Keep rendered expansion/selection separate from durable session evidence and
model context. Build on `src/tui/transcript.rs`; extract grouping/state and
rendering into focused modules only as required. Preserve incremental wrapping
so a streaming token does not rebuild the entire history.

Live tool elapsed time can use local monotonic clocks; replay must use recorded
available evidence. Show unknown durations as unavailable. Model `total_ms` is
request time, not tool time or total turn wall time. Waiting time must be labeled
separately if shown. Never add concurrent durations and label the sum wall time.
If persistent timing or checkpoint correlation needs an additive event change,
justify it explicitly and preserve old-session deserialization and JSON consumers.
A resumed incomplete operation is not automatically still running.

Worker drill-down/branches are later: preserve current inspection and durable
links initially. Do not start workers, introduce tabs, or fabricate a complete
cross-session trace as part of the first renderer slice.

## 6. Implementation sequence after discussion

1. **Resolve behavior and mockups.** Agree on the defaults and open decisions
   below. Inspect current key routing, checkpoint answer paths, replay, and event
   ordering. Write down any necessary additive event fields before coding.
2. **Pairing instruction slice.** Add bounded mode-aware request guidance, test
   snapshot consistency and context accounting, preserve mechanical checkpoints,
   and settle mode persistence. Correct question/resume behavior where the audit
   finds an actual gap; do not broadly refactor the loop.
3. **Trace state slice.** Project existing events into stable turn/tool entries
   with status and expansion. Cover interleaved and incomplete events with offline
   fixtures before replacing rendering. Preserve chronological conversation.
4. **Trace UI slice.** Render the three states, integrate per-entry expansion,
   search, copying, scroll anchoring, and available timings. Extract only affected
   TUI responsibilities; preserve overlays, steering, approvals, and composer.
5. **Dogfood and document.** Run the same small real coding task with pairing
   off/on on the local 27B. Inspect whether useful decisions happen before edits,
   whether answers influence work, and whether activity is easier to read. Record
   unnecessary interruptions and latency; do not claim general model reliability
   from one run. Update user docs and next-work status after validation.

## 7. Acceptance and validation

For implementation, load the idiomatic Rust skill and use isolated test homes.
Run `cargo test` and `cargo clippy --locked --all-targets -- -D warnings`, plus
PTY checks for affected UI. Deliberately break new guarded behavior to confirm
its tests fail. Record implementation QA separately from the original release baseline.

- Scripted requests prove pairing instruction/tool consistency for on/off,
  changes between requests, compaction, and the chosen resume behavior; workers
  and unattended operation remain compatible. Include any instruction tokens in
  accounting. Test questions, explicit direction, skip, cancel, and mode changes
  while a checkpoint is pending.
- Trace fixtures cover streamed text, repeated/similar tool calls with distinct
  IDs, failures, validation, checkpoint discussion, helper interleaving, and
  missing legacy events. Visible status and metrics never imply unavailable facts.
- Stable selection and expansion survive streaming, resizing, completion, and
  scrolling. Search finds hidden content; explicit review diffs remain readable.
  Long transcripts preserve incremental rendering rather than quadratic churn.
- PTY checks exercise the working/waiting/finished states, narrow terminals,
  theme contrast, keyboard focus, overlays, and interruption behavior.
- CLI JSON still emits the original event stream and metrics; a UI collapse does
  not discard output or alter the session/model history. No model calls are added
  to produce trace labels or summarize tool activity.
- Dogfooding evaluates behavior separately from plumbing. Injection tests prove
  the prompt was sent, not that a 27B asks at the right time.

## 8. Decisions for discussion

1. **Default presentation:** recommend the trace as the main transcript, successful
   tool output collapsed and thinking hidden. Avoid a second permanent view mode
   unless dogfooding shows a need. Retain existing visibility controls.
2. **Checkpoint conversation:** should an explicit Resume action be required, or
   should a clear instruction continue work? Recommend clear direction continues,
   questions stay in discussion, with visible controls for skip and stop. Audit
   ambiguous replies and the current round limit before finalizing.
3. **Resume mode (deferred beyond the first slice):** consider recording pairing-mode changes and restoring the
   last recorded mode on session resume, with legacy sessions using current config
   and unattended runs still disabling interactive pairing. This is a proposal
   requiring explicit event/config precedence design, not existing behavior.
4. **First trace scope:** recommend turn/tool/checkpoint grouping first. Defer
   worker branches, multi-call compression, and detailed per-tool persisted
   timing unless the initial view needs them to be honest and useful.

## 9. Deferred work and rationale

Internal helper routing belongs behind benchmarks with distinct role attribution,
quality and downstream completion checks, cost, and latency. `[agents].model`
already supports cheaper/faster spawned work; adding global helper config has no
proven benefit yet. Do not run live routing experiments as setup for this work.

Workflows remain a separate TOML job artifact replacing shell loops for stages
with dependencies, informed by agent-line. Revisit the old linear-only scope when
that work resumes. A DAG can run serially on one local model; concurrency and
model swapping are optional execution choices. No workflow engine in this slice.

Tabs, general system-prompt editing, a lightweight inference mode, autonomous
spawning, new model observers, and a wholesale TUI rewrite remain deferred.

---

# Historical plan: `checkpoint` — pairing, v0

The following is the original implementation plan. Its file line references and
out-of-scope list are historical; `/pair` has since shipped. Preserve the failure
evidence when changing guidance rather than repeating the same prompt-only bet.


The problem: worksmith writes code its user does not know. Reading the diff
afterwards does not fix it — retention comes from *deciding*, not from reading.
Plans already get this treatment (`MODEL_SWITCH_PLAN.md` §7 is an ADR with the
question pre-written); implementation does not.

v0 is the smallest thing that tests the only real risk: **do checkpoints land on
the decisions that matter?** So it is nearly all plumbing and no judgment — the
judgment stays in the plan doc, where it already lives.

## Three kinds

| kind | when | blocks | output |
|---|---|---|---|
| `ask` | before writing | yes | an ADR file |
| `note` | after writing | no | one line of *why* in the transcript |
| `yours` | instead of writing | no | `todo!()` + contract comment |

`ask` blocks because a question answered three turns later is worthless — the
code is already written. Nothing else blocks.

`yours` needs no queue and no UI: `cargo check` will not let the user forget,
and `--until` already knows how to say whether what they wrote is right. That is
the harness's own differentiator, pointed at the user's code instead of ours.

## Selection is not the model's job

An 8B cannot reliably judge "was that load-bearing" — it will checkpoint on
every match arm or on none, which is the load the eval says belongs in the
harness (`worksmith-differentiator-eval-finding`). So:

- **v0 trigger: a marker in the plan doc.** *Tested, does not work.* A 27B with
  the tool available and the plan in context made twenty edits across fifty
  steps and never called it once. A marker was not worth a separate run:
  `MODEL_SWITCH_PLAN.md` §7 is titled "settle before coding" and ends "Confirm
  before implementing", the model read the plan twice, and it still never asked.
- **v0.1 triggers, mechanical, no judgment — built 2026-08-26.** A `--until`
  check that has failed twice, and a turn about to end as stuck. Both are points
  where the harness already knows something is wrong and needs no judgment to
  see it, so the model cannot decline, forget, or be too busy. The answer is fed
  back as a directive: a checkpoint that changes nothing is not a checkpoint.

  A harness-raised checkpoint does **not** file a decision record, unlike the
  `ask` tool — "try X instead" is a course correction, not an architectural
  decision, and filing those into `docs/decisions/` would devalue the ones that
  are. It is recorded as an `Event::Checkpoint` and reaches `/history`.

  It also does not spend the per-turn cap. That cap stops a model being chatty;
  this fires at most once per validation retry and once per stuck turn, and
  being crowded out by three notes would be exactly backwards.

A hard per-turn cap lives in code, not prose: a model can ignore a paragraph and
cannot ignore a tool that refuses the fourth call.

## Where decisions go

`.worksmith/decisions/NNNN-slug.md`, overridable with a top-level
`decisions-dir` config key (this repo will point at `docs/decisions/`).

`.worksmith/` is already the per-project namespace and already holds the
committed half — `config.toml` travels by `git pull`, which is the entire reason
`trust.rs` exists. Git is the durable form; the knowledge DB indexes `.md` for
free (`knowledge.rs:19`) and is disposable by design, so nothing new is stored.

**Hazard:** plenty of projects gitignore `.worksmith/` wholesale (it also holds
`knowledge.db` and `sessions/`). Check on first write and say so, rather than
filing decisions into a hole.

## A checkpoint nobody answers is a skip, not a failure

The opposite default from approval. `RefuseWhenUnattended` exists because a
headless agent that pushes unasked is a harm; a checkpoint is pedagogy, and
refusing to work because no human was there to be taught would break every eval
and `--print` run. So: no asker → skip and continue.

## Work

1. `Asker` trait + `ChannelAsker` + `TextRequest` in `tools/approval.rs`
   (free text, unlike `Approval`'s yes/no).
2. `ToolContext`: `asker`, the per-turn cap counter, `decisions_dir`.
3. `tools/checkpoint.rs` — the tool. Its own description carries the essentials,
   the way `doc`'s does, so no skill catalog tax on a 32k window.
4. `Event::Checkpoint` → session JSONL → `/history`. Three exhaustive matches
   will fail to compile until updated (`tui.rs:859`, `tui.rs:2476`, `main.rs`).
5. TUI: `pending_ask` routes the composer's Enter to the oneshot instead of
   starting a turn. Simpler than `pending_approval`, which has to seize the
   keyboard.
6. ADR writer + the gitignore check.

Out of v0: the skill, workers, ADR lifecycle (superseding/status), `/pair`.

## Test

Implement `/model` with it on. `MODEL_SWITCH_PLAN.md` is the answer key: §1's
atomic swap, §10a's pin-vs-retarget, the copy-vs-share fork. Land near those and
selection works; land on the `Event::ModelChanged` match arms and it does not.
