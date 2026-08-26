# Plan: `checkpoint` — pairing, v0

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
