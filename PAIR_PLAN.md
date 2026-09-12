# Plan: pairing and the session trace

Updated 2026-09-11. **Status: pairing, expandable checkpoints, and individual tool entries implemented on `codex/pairing-guidance`; turn grouping remains next.**
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
that a model recognizes consequential decisions. The later MUD runs below
provide live interaction evidence, without isolating the guidance benefit. The original trace mockups below describe the broader target; the implementation
now includes expandable checkpoints and individual tool activities.

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

## Follow-up from the MUD dogfood run

Session `fe8a4b38-b033-454b-8bb9-3d82638ca7f2` paused before edits and followed the
user's testability requirement. It also exposed duplicated checkpoint text/raw
JSON, awkward wrapping, and a shell pipeline that returned success despite a
missing Python executable. The model recovered, but the shell status was weak
evidence. The run did not exercise a follow-up question or isolate the prompt's
benefit: the user explicitly asked for discussion before editing.

The follow-up slice groups each live checkpoint's question, discussion, and
answer into one expandable entry. It uses a dedicated small checkpoint renderer,
word wrapping, and basic inline emphasis. Enter in transcript navigation toggles
the selected entry; search reveals hidden matches and copying retains full text.
Pending questions start expanded. Raw tool arguments/success results remain in
session history; errors remain visible. This is not the full turn/worker trace.

Bash tool calls and command validators enable `pipefail`. Failure in a pipeline
stage is retained; intentional shell recovery remains possible. This changes
shell defaults, not command approval rules or what counts as validation evidence.

Follow-up validation:

- `cargo test --locked`: 515 passed; 2 opt-in live probes ignored.
- `cargo clippy --locked --all-targets -- -D warnings`: clean.
- Scripted local PTY smoke: one formatted checkpoint, no raw JSON, folding while
  waiting, follow-up discussion, final direction, and completion passed.
- Removing `pipefail` deliberately made the missing-executable regression fail;
  restoring it passed. Validation pipelines also cover failure and recovery.
- `git diff --check`: passed. Live model follow-up-question dogfooding remains.

## Second MUD run and tool activity slice

Session `9f2c22ef-0e0b-4d13-9336-cf336d39ad21` exercised the follow-up question:
Qwen38 explained an injectable clock, waited for explicit direction, then edited.
The user confirmed expansion worked. All 140 game tests passed independently.
The missing `python` pipeline correctly reported exit 127, and the model recovered
with `python3`. The one follow-up model call took 5.5 seconds (290 input / 233
output tokens) on the same local Qwen38 INT4 model. This verifies the interaction,
not the instruction's isolated benefit or another model's quality. The generated
code still chose `time.time` rather than a monotonic clock and recovered from one
malformed edit call.

The next slice joins live tool calls/results by ID in one expandable entry.
Successful results start collapsed, failures open, and explicit user choices
survive completion. Enter uses the existing transcript navigation mode; Ctrl+O
sets all tool entries, search reveals matching output, and copy retains output.
Expanded edit/write results retain diff colors. Unmatched results remain visible.
Grouping lives in `src/tui/activity.rs`; no new event schema, timing estimates,
configuration, model calls, turn trees, or worker hierarchy are added.

Validation:

- Unit coverage checks pairing by ID, failure visibility, explicit expansion,
  search, selection stability, diff colors, and unmatched-result preservation.
- Scripted PTY: tool success/failure, search and folding passed with pairing off.
  Checkpoint folding and follow-up discussion also passed with pairing on.
- `cargo clippy --locked --all-targets -- -D warnings`: clean.
- `cargo test --locked`: 518 passed, 0 failed; 2 opt-in live probes ignored.
- `git diff --check`: passed.

## Tool timing follow-up

Tool results now carry optional `elapsed_ms`, measured with a monotonic clock
around tool dispatch. It includes approval, retry, and checkpoint discussion
waits, so it is wall time rather than CPU time or provider-only latency.
The initiating model request and result rendering are outside this interval;
checkpoint follow-up model calls are inside it. Deferred calls and invalid JSON
omit timing; legacy records deserialize without it. Zero is a
valid measurement below one millisecond. JSON mode serializes the same event
that sessions record, and tool rows show the duration before the command to keep
it visible when a long command is clipped. No timing configuration is added.

Validation:

- Recorded agent-loop regression verifies at least 50ms for successful and failing
  delayed tools and no duration for invalid arguments.
- Legacy JSON round-trip and measured-zero serialization passed.
- `--think off --mode json` with a scripted local server emitted `elapsed_ms`
  for both a successful read and a failing shell command.
- PTY smoke verified visible timing labels, folding, failure expansion, and search.
- Focused UI tests and warning-clean all-target clippy passed.
- `cargo test --locked`: 520 passed, 0 failed; 2 opt-in live probes ignored.
- `git diff --check`: passed.

## Final branch review

Session `21c32176-0ad7-480a-a107-50a29f575f59` exposed empty successful web
fetches and leaked script text. The reported USA Today URL currently returns
HTML containing `<header>`, which the extractor incorrectly treated as `<head>`.
Exact tag boundaries, correct scanning after skipped elements, and ASCII-only
case folding preserve article text and UTF-8 offsets. Empty extracted text now
returns an actionable error instead of successful empty output. This remains a
small static HTML extractor; nonempty navigation alone does not prove an article
was retrieved.

The branch review covered request snapshots, checkpoint cancellation/deferred
calls, tool event compatibility, expansion/selection behavior, and shell status.
The next UI work is turn grouping on a separate branch.

Final checks:

- Captured live HTML replayed through CLI: 2,996 characters of article text;
  an empty HTML fixture returned `ok: false` with recovery guidance.
- A scripted 25-tool-call PTY session retained the earlier expanded row during
  streaming, returned to the tail with G, and showed failures and tool timings.
  This verifies UI mechanics; it is not a new live-model quality benchmark.
- `cargo test --locked`: 522 passed, 0 failed; 2 opt-in live probes ignored.
- All-target clippy with warnings denied and `git diff --check`: passed.
- Integrated into local `main` after final validation; turn grouping is next.

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
