# Plan: the work pool — small tasks, fed by the harness

Branch `pool-experiment`, and it is meant to be throwaway. Section 8 says what
result kills it.

The question: **does cutting work small enough make a 14B consistent?** The
thesis (PLAN.md §0) says the harness carries what a weak model cannot. So far
that means the validation loop, and the eval bounded it — +34 points on a 9B,
nothing on a 27B (`worksmith-differentiator-eval-finding`). Task size is the
next candidate lever, and it is untested.

The bet: a 14B fails at *holding a job in its head*, not at doing the work. If
that is right, then a job cut into pieces small enough that none of them needs
holding will run at close to the model's per-step accuracy rather than its
per-job accuracy. If it is wrong, cutting smaller buys nothing but overhead and
this branch dies.

## 1. This is a third object, not a bigger fan-out

Two nearby things exist and neither is this.

**`/spawn` fan-out is anti-dependency by construction.** The planner prompt
(`fanout.rs:270`) tells the model the tasks run at the same time, in one
directory, cannot talk, and must not write the same file — "if the work is a
sequence of phases, do not describe the phases." That rule was earned: Kimi K3
split a request into read → write → review, a correct decomposition and a
useless one, because all three ran at once and the reviewer found nothing to
review. Fan-out is N takes on one goal. It is not a pipeline and must not
become one.

**Workflows (PLAN.md §8a) are a hand-written linear chain.** `[[step]]`,
`after`, `workers = 3`. Static shape, decided before anything runs.

The pool is neither: an ordered backlog of small tasks with dependencies, a
concurrency cap, and a dispatcher that hands out the next *ready* task as slots
free. Half of it is already in the tree — `Manager` holds `queued:
VecDeque<PendingTask>`, enforces the cap, and `pump()` starts queued work as
slots free (`worker.rs:391`). That is the mailbox. Missing: dependency gating,
result handoff, per-task acceptance.

## 2. The dispatcher is code, no model in the loop

Ready means every id in `needs` has finished. Ready tasks go out to the pool up
to the cap. Nothing about that needs judgment, so nothing about it gets a model
call.

This is the same argument `PAIR_PLAN.md` already lost once and rewrote: a model
asked to notice the right moment will not. It applies twice as hard here,
because the thing being asked is a *scheduler*, and a scheduler that is
sometimes wrong is worse than no scheduler.

The supervisor stays what it is — a per-worker state machine watching one event
stream (`supervisor.rs`). It is not promoted to foreman of the pool. Dispatch
is the manager's job and it is a loop over a graph.

## 3. Two risks, two experiments, and they must not be run together

- **A** — can a 14B execute a well-specified one-or-two-action task reliably?
- **B** — can a planner *emit* tasks that small and that well-specified?

B is the likelier failure. Run them together and a bad number is unattributable,
so:

**Phase 1 writes the backlog by hand.** No planner anywhere. That measures the
ceiling: the best a 14B does when decomposition is perfect. A bad ceiling kills
the idea before a planner is ever written, which is the whole point of doing it
in this order.

Phase 2 adds a planner, judged against Phase 1's hand-written backlogs as an
answer key — coverage, granularity, no elisions. `usable_subtask()`
(`fanout.rs:170`) is the seed of that check.

## 4. The experiment: one goal, three granularities

Synthetic first, to get the shape. A real task grounds the result but makes the
per-task checks fiddly, and fiddly checks are how an experiment ends up
measuring its own harness.

One goal — a small Python library, in the register of `evals/tasks/05` — cut
three ways:

| backlog | tasks | roughly |
|---|---|---|
| coarse | 3 | the decomposition a person would type |
| medium | 8 | one function each |
| fine | ~20 | one function, or one edge case of one function |

**The end-to-end check is byte-identical across all three.** That is what makes
the comparison fair, and it is the same discipline `evals/run.py` already
enforces between raw and guided. The finer backlogs additionally carry per-task
checks; the coarse one mostly cannot, which is itself part of what is being
measured.

Per run, recorded:

- end-to-end pass (the shared check)
- per-task pass rate
- **confidently-wrong count** — task reported done, its own check fails
- gen tokens total, and **per solved task**
- retries consumed, wall clock

The third one is the one that matters. The eval's sharpest finding was that all
ten raw failures on the 9B had outcome `done` — the model declared victory and
was wrong. The hypothesis here is that small tasks plus a per-task check convert
confidently-wrong into caught-and-retried. **If confidently-wrong does not fall
as tasks shrink, granularity is not the lever and the rest of this is
decoration.**

## 5. Disposable workers, not warm ones

A thread pool keeps its threads. This should not.

Each task gets a **fresh agent**: new context, the task, and one line per
finished dependency. Context accumulation is where a 14B rots — by task six a
warm worker is carrying five tasks of transcript and starts answering the wrong
one. Small tasks and a long context are contradictory, and the context is the
half that is cheap to throw away.

The pool is a concurrency cap over disposable workers, not a set of persistent
minds. Prefill is paid per task, and it is the cost that buys the consistency —
so Phase 1b runs one backlog both ways and prices it rather than assuming.

**Dependencies pass paths, not payloads** (§8a settled this and it holds double
here): "task `parse` wrote `parser.py`" — read it if you need it. Passing the
text keeps context growing with the backlog, which is the thing being avoided.

## 6. Backlog format

```toml
name = "expenses"
goal = "..."                  # for the context line only, never the instruction
validate = "python3 check.py" # the shared end-to-end check

[[task]]
id = "money"
prompt = "Create money.py with format_cents(n) returning ..."
validate = "python3 -c '...'"

[[task]]
id = "parse"
needs = ["money"]
prompt = "..."
validate = "..."
```

Same TOML register as `evals/tasks/*.toml`, and `[files]` fixtures work the same
way, because the runner should be a mode of `run.py` rather than a second
harness with its own bugs.

A task that exhausts its retries is **failed, and its dependents are blocked,
not attempted**. Running a dependent on a broken dependency produces a second
failure that looks independent and is not — which is how one bad number becomes
five.

## 7. Pairing falls out of the same object

Separate ask, same machinery, and this is the part with the best odds.

`PAIR_PLAN.md` recorded a tested negative: a marker in the plan doc does not
work. A 27B with the tool available and the plan read twice made twenty edits
across fifty steps and never called `checkpoint` once. The lesson was that the
model cannot be the trigger. What replaced it — a check that failed twice, a
turn ending stuck — only fires when something is already *wrong*, which is why
pairing reads as reactive rather than conversational.

A structured backlog is a harness-side trigger that fires when things are going
fine. Let a task carry a kind:

```toml
[[task]]
kind = "ask"
subject = "Hash or Vec for the pending set?"
detail = "hash: O(1) lookup, ordering lost. vec: insertion order kept, O(n)."

[[task]]
kind = "yours"
needs = ["parse"]
prompt = "Write the failing test in tests/parse.rs first. Signature roughly: ..."
```

The dispatcher stops on `ask` and skips `yours` to a `todo!()` — a scheduling
decision, requiring no judgment and no cooperation. Both of these are shapes
`tools/checkpoint.rs` already knows how to render; what is new is that something
other than the model decides when they happen.

Unattended still means skip and continue (`PAIR_PLAN.md`, "a checkpoint nobody
answers is a skip") — otherwise every eval run blocks forever on an empty room.

## 8. What kills this branch

Stated up front so the answer is not negotiated after the fact.

- **Fine does not beat coarse on end-to-end pass, at the 14B.** The bet is
  wrong; delete the branch.
- **Confidently-wrong does not fall as tasks shrink.** Any end-to-end gain came
  from retries, which `--until` already buys without a pool.
- **Cost per solved task blows up.** The eval's bar is +3% on the 9B. A pool
  that needs 5× to win is a benchmark result, not a feature.
- **Fine wins but only on hand-written backlogs no planner could produce.** Not
  a kill on its own — it moves the whole problem to Phase 2 and says so.

## 9. Work

1. Backlog format + loader, as a mode of `evals/run.py`.
2. Dependency gating and result lines in `Manager` — `pump()` grows a
   readiness test; `PendingTask` grows `id`, `needs`, `validate`.
3. Per-task acceptance. Workers already take a `CommandValidator`
   (`worker.rs:472`), so this is plumbing per task rather than per fan-out.
4. Write the three backlogs by hand. This is the experiment; it deserves more
   care than the code.
5. Run the sweep on the 14B, and on the 27B as a control — if the 27B gains too,
   the result is about task size generally and not about weak models.
6. Only then: `kind = "ask" | "yours"`, and the Phase 2 planner.

## 10. Deliberately not

- **Not a general DAG.** `needs` on a list of ids, gated by a readiness test.
  §8a declined a DAG for workflows and the reason holds: the graph is not the
  hard part, the decomposition is.
- **Not a new binary or a new harness.** A mode of `run.py`, reusing its
  scratch dirs, fixtures, and event parsing.
- **Not warm workers.** §5.
- **Not a model in the dispatch loop.** §2.
- **No planner in Phase 1.** §3, and it is the whole design.
