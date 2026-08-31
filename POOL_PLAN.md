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

One goal — `evals/pool/expenses/`, a small Python library plus a CLI in the
register of `evals/tasks/05` — cut three ways:

| backlog | tasks | roughly |
|---|---|---|
| coarse | 3 | one module group each |
| medium | 8 | one function each |
| fine | 22 | one behaviour each, extended in place |

**Two things are held identical so that granularity is the only variable.**

*The specification.* `SPEC.md` carries every module, signature, edge case and
the exact report layout, and every backlog points at its sections rather than
adding requirements. Without this the experiment is unreadable: the natural way
to write a fine-grained backlog is to spell out every function and edge case,
and a win would then be the extra *information*, not the smaller tasks. The
eval had to guard exactly this once already — guided beat raw on the 9B and the
finding only counted because guided leaked nothing the goal did not carry.

*The end-to-end check.* `files/check.py` drives `cli.py` as a subprocess and
compares stdout byte for byte. It knows nothing about which modules exist, so it
cannot tell the three granularities apart, which is the only way it grades the
same thing three times. It writes CSVs of its own as well as reading the
fixture, so it is not scoring a file the model was handed.

Every backlog also gets per-task checks at its own granularity. Giving the fine
one checks and the coarse one none would move enforcement and size together,
which is two variables again.

The backlogs are graded before any model sees them: `evals/pool/verify.py` runs
all 33 per-task checks and all 3 end-to-end checks against a reference
implementation, and refuses a cycle or a dangling `needs`. An answer key nobody
graded is how a day gets spent discovering the model was right and the check was
wrong. All three pass.

**Recorded in advance, and accepted:** `fine.toml` is not mostly a chain, it is
*entirely* one — twenty-two tasks, twenty-two deep, never more than one runnable
at a time, because each extends the file the last one wrote. Its wall clock will
be worse than coarse, and pool concurrency does nothing for it at all.

That is a trade this project takes without argument. The user is running a local
model on a Mac; they are already slow, so seconds are not the scarce resource
and a design that spends them on correctness costs them nothing they had. Work,
then correct, then fast — and phase one is not the place to spend effort on
phase three.

**Seconds and tokens are not the same currency, though.** "Slower is fine" does
not license "more expensive is fine": generation cost still buys nothing back,
and cost per solved task stays a kill criterion in §8 while wall clock is
explicitly not one. Both get recorded; only one can end the branch.

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
name = "expenses-medium"
granularity = "medium"
spec = "SPEC.md"                 # the only source of requirements, shared
validate = "python3 check.py"    # the shared end-to-end check
files = ["expenses.csv", "check.py", "SPEC.md"]

[[task]]
id = "parse-amount"
prompt = "Implement parse_amount in money.py, per SPEC.md section 1."
validate = """..."""

[[task]]
id = "parse-line"
needs = ["parse-amount"]
prompt = "Implement parse_line in records.py, per SPEC.md section 2."
validate = """..."""
```

Same TOML register as `evals/tasks/*.toml`, so the runner is a mode of `run.py`
rather than a second harness with its own bugs.

**A `validate` is a heredoc, not `python3 -c "..."`.** Every amount in this
domain starts with `$`, and inside bash double quotes `$12` expands to a
positional parameter — silently, producing a check that tests nothing. Two of
the twelve short checks were written that way and `verify.py` caught both. Any
new check goes through a `python3 - <<'PY'` block.

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

**Sweep 1 (2026-08-29) fired none of these.** Coarse < medium < fine on every
model that can fail, and cost per solved task fell 2.5x as tasks shrank. Results
and caveats in `evals/pool/RESULTS.md`; the branch lives.

- **Fine does not beat coarse on end-to-end pass, at the 14B.** The bet is
  wrong; delete the branch. *(Sweep 1: did not fire — 0/3 coarse, 7/22 fine.)*
- ~~**Confidently-wrong does not fall as tasks shrink.**~~ **Retired after sweep
  1.** It read 0 on every arm of every model, so it cannot discriminate between
  them. The bet was that small tasks plus a per-task check would convert
  "declared done, was wrong" into caught-and-retried; on these models failure
  never wore that shape — every weak-model failure was thrashing, caught by the
  supervisor's repeat detector rather than by a check catching a false claim.
  The gain is real and arrives by another route: **containment**, a failure
  costing one small task instead of the whole job. Still measured, no longer
  decisive.
- **Cost per solved task blows up.** The eval's bar is +3% on the 9B. A pool
  that needs 5× to win is a benchmark result, not a feature. This is about
  *tokens*, not time — see below.
- **Fine wins but only on hand-written backlogs no planner could produce.** Not
  a kill on its own — it moves the whole problem to Phase 2 and says so.

**Explicitly not a kill: wall clock.** However much slower fine-grained turns
out to be, that number is reported and the branch lives. It is the one cost this
project is happy to pay, and a kill list that quietly included it would be
optimising the third thing before the first two are settled.

## 9. Work

1. **Write the three backlogs by hand — done.** Deliberately first: this *is*
   the experiment, it deserves more care than the code, and writing it first
   settled the format by use instead of by guess. `evals/pool/`, graded by
   `verify.py`.
2. Backlog loader, as a mode of `evals/run.py`.
3. Dependency gating and result lines in `Manager` — `pump()` grows a
   readiness test; `PendingTask` grows `id`, `needs`, `validate`.
4. Per-task acceptance. Workers already take a `CommandValidator`
   (`worker.rs:472`), so this is plumbing per task rather than per fan-out.
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
