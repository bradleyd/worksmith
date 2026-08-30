+++
title = "Measuring the harness"
description = "A weak model plus the loop scored 95% where the same model alone scored 56%. Getting to that number took four wrong answers first, and they are written down here."
weight = 20
+++

The claim worksmith makes is that the harness carries a weak model. That is
easy to say and annoying to measure, because almost everything you can vary
moves the number for reasons that have nothing to do with the harness.

This page is the measurement, the arms it took to isolate it, and the four
results we published to ourselves and then had to retract. If you want to poke
holes in the claim, the holes we already found are here.

## The number

One fixture, 22 small coding tasks that build a working command line tool.
Every task carries a check. Each arm runs the same tasks from the same starting
state, so the only thing that changes is the model and whether the harness is in
the loop.

| arm | passed | always | flaky | never | cost |
|---|---|---|---|---|---|
| Claude Sonnet 5, one shot | 66/66, 100% | 22 | 0 | 0 | $0.26 |
| Qwen3.5-4B + worksmith | 64/66, 97% | 20 | 2 | 0 | free |
| Qwen3.5-9B, one shot, local 4-bit | 52/65, 80% | 13 | 8 | 1 | free |
| Qwen3.5-9B, one shot, hosted | 51/65, 78% | 15 | 5 | 2 | $0.0088 |
| Qwen3.5-4B, one shot | 35/62, 56% | 9 | 7 | 6 | free |

Three attempts per task, every arm, same tasks from the same starting states.

The 4B goes from 56% alone to 97% inside the loop. That is worth more than
doubling the model: a bare 9B, more than twice the parameters, manages 80%.

The consistency columns say it better than the pass rate does. The same 4B goes
from 9 tasks that always pass to 20, and from 6 tasks it never passes to none.
There is no task in the suite the harness cannot eventually get right.

Cost is not the interesting part, and we should say so plainly. Sonnet did the
whole suite for 26 cents, about four tenths of a cent per solved task. Nobody is
going broke on that. The argument for a local model is that it is free, private,
has no rate limit, and works on a plane. The argument is not that Sonnet is
expensive.

Wall clock is the real price. Sonnet finished in a few minutes. The 4B with the
harness took 35.

## Always, flaky, never

The pass rate is the wrong column to read first. Look at the flaky one.

A task that passes every attempt can go in a pipeline. A task that passes two
of three is a coin flip wearing a percentage, and an average hides which one you
have. The bare 9B gets a respectable 80%, and 8 of its 22 tasks are coin flips.
Sonnet has none.

This matters more than it looks, because chained work compounds. At 95% per
task, a 22 step chain finishes 32% of the time. At 99% it finishes 80% of the
time. Consistency is not a nice property here, it is the whole thing.

Which is where the claim stops. Sonnet is 22 always-pass and no coin flips. The
4B with the harness is 20 and 2. Per task that gap is three points. Across a 22
step chain those two coin flips compound to about 44% against Sonnet's 100%, so
the harness closes most of the distance and not all of it.

The two flaky tasks are `pa-reject` and `cli-print`, and both are the same
failure: the model rewrites the whole file instead of editing it. `pa-reject`
averages 1,201 generated tokens per model call against a suite median of 140,
across 23 consecutive calls. So the remaining gap has a name and a known fix,
which is anchored search and replace patches rather than whole file writes.

## The four wrong answers

Every one of these was written down as a result before it fell over. They are
kept because the reasoning was usually right and the measurement was not.

**Task size is the lever.** The first sweeps showed coarse backlogs failing 0/3
while fine ones reached 22/22, on four different models. It looked decisive. It
was not measuring task size. Every fine task also carried its own check and its
own inline acceptance criteria, so three variables moved together.

**Self containment is the lever.** So we built a backlog at the same
granularity with the criteria inlined, and it scored 7/8 against the original's
2/8. That looked decisive too. It did not replicate: the same backlog scored 2/8
on the next run. Run to run spread on a 4B at temperature 0.7 is large enough to
produce either number, and a single run reports either as fact.

**The harness numbers were mostly the harness giving up.** Of the first 67 task
runs, 8 failed, and 7 of those 8 ended because a supervisor threshold tripped or
a subprocess timeout fired. Both are numbers somebody picked. The sweeps were
measuring worksmith's patience, with task shape as a minor input.

**Small tasks are not small.** The fixture asked every task to rewrite a whole
60 line file with one more behaviour added. The description got shorter as the
backlog got finer; the output never did. So the experiment varied the wording
and not the work, which is why granularity showed no effect at all once the
harness was removed.

What survived all of that is narrower than where we started. Cutting work into
smaller pieces does not make a weak model more accurate per piece. It shrinks
the blast radius of a failure and gives the loop more places to catch one. When
we ran the same 14 tasks with the per task checks removed, every task reported
done and the end to end check failed. Same granularity, checks removed, broken
result. The checkpoints are doing the work.

## Bugs the measurement found in itself

Three of these produced published numbers that were wrong.

A task killed by the clock reported `gen_tok=0` and no outcome, byte identical
to a model that produced nothing. Python hands you the partial output on a
timeout and we threw it away. Three runs got diagnosed as provider stalls on
that evidence. Running the same task by hand showed the model reading the spec
and writing a thousand tokens of implementation before the axe fell.

The harness timeout was set to 600 seconds and so is worksmith's own stream idle
timeout. A stalled provider got killed at the exact moment worksmith was about
to report it properly.

The bare arm sent Qwen's spelling of "do not think" and OpenRouter ignores it.
Measured on one trivial prompt: 1 completion token locally with the flag, 387
without it, and 186 tokens on OpenRouter with the flag of which 159 were
reasoning. So the quantisation control was comparing thinking against no
thinking. Worksmith itself was never wrong here, it picks the right spelling per
provider.

An agent also overwrote the reference solution the whole eval grades against.
The first diagnosis was that nothing confines a write to the working directory,
and that was wrong: `approve_write_outside_cwd` already gates exactly this and
tells the model not to retry. What happened is that every eval run passes
`--approve-all`, so the gate fired and approved itself. The eval turned off the
protection and then blamed the harness for not having one. The corrupted answer
key surfaced as a test failure, which was the lucky version.

## What we still do not know

Three attempts per task is weak resolution for this. A task with a true 90% rate
still shows 3/3 about three quarters of the time, and 90% per task is a disaster
across 22 of them.

Every number here is one fixture of Python coding tasks and one model family. A
second fixture built around file and directory work, closer to how people
actually delegate, turned out too easy to separate anything: the 4B passed both
the 3 task and the 14 task versions.

The Sonnet arm runs on OpenRouter and the 4B runs on a Mac, so the two differ in
more than the model: different serving stack, different sampling defaults,
different everything below the API. That is unavoidable if the claim is about
local models, but it means this compares one local setup against the hosted
frontier rather than model against model. The 111 minutes is partly the 4B and
partly a 4-bit MLX model on a Pro-tier chip at about 45 tok/s.

Quantisation is the one thing we can rule out. Local 4-bit scored 80% and the
same model hosted scored 78%, which is inside the noise.

## Reproducing it

```sh
python3 evals/pool/verify.py                        # the answer key is correct
python3 evals/pool/check_snapshots.py               # each snapshot stops where it should
python3 evals/pool/bare.py <model> -n 3             # one shot, no harness
python3 evals/pool/run_pool.py evals/pool/expenses/fine.toml \
    --independent --model <model> --fast            # same tasks, harness in the loop
python3 evals/pool/compare.py results/*.json        # every arm in one table
```

`bare.py` is the control that matters. It sends the task straight at the
provider with no tools, no retries, no supervisor and no timeout, and grades the
answer with the same check the harness uses. Anything the harness scores above
that number is the harness.

The raw results, including the retracted ones, are in `evals/pool/RESULTS.md`.
