# Pool sweeps — results

Newest first. Each sweep says what held, what did not, and what it found wrong
with its own design; a result whose caveats are not written next to it gets
quoted later without them.

# Sweep 6 — the comparison the branch was for, 2026-08-30

Three attempts per task, every arm, same 22 tasks from the same snapshots.

| arm | passed | always | flaky | never | cost | wall |
|---|---|---|---|---|---|---|
| Claude Sonnet 5, one shot | 66/66, 100% | 22 | 0 | 0 | $0.2604 | mins |
| **Qwen3.5-4B + worksmith** | **64/66, 97%** | **20** | **2** | **0** | free | 111m |
| Qwen3.5-9B, one shot, local 4-bit | 52/65, 80% | 13 | 8 | 1 | free | - |
| Qwen3.5-9B, one shot, hosted | 51/65, 78% | 15 | 5 | 2 | $0.0088 | - |
| Qwen3.5-4B, one shot | 35/62, 56% | 9 | 7 | 6 | free | - |

**The harness is worth more than doubling the model.** A 4B in the loop beats a
bare 9B by 17 points, and beats itself by 41.

**Consistency moved further than accuracy.** Same 4B, one-shot to harnessed:
tasks that always pass 9 → 20, tasks that never pass 6 → 0. Nothing in the suite
is beyond the loop, which is a different claim from a good average and the more
useful one.

**Where it stops.** Sonnet is 22/0/0; the 4B harnessed is 20/2/0. Three points
per task, but across a 22-step chain two coin flips compound to roughly 44%
against 100%. The harness closes most of the distance, not all of it.

**The residue has a name.** Both flaky tasks are `pa-reject` and `cli-print`,
and both are whole-file rewriters: `pa-reject` averages 1,201 generated tokens
per model call against a suite median of 140, over 23 consecutive calls. So the
next piece of work (anchored search/replace patches, `rustopedia/`'s §3) is
aimed at the measured gap rather than a guess.

**Cost, stated plainly.** Sonnet did the whole suite for 26 cents, about four
tenths of a cent per solved task. The argument for local is free, private, no
rate limit, works offline. It is not that Sonnet is expensive.

# Sweep 5 — the bare arm, and what the branch actually found, 2026-08-30

**Task shape does not help the model. The loop does.**

Every task run with no harness at all — no tools, no retries, no supervisor, no
timeout — one shot each, three attempts, seeded from a per-task snapshot so each
is measured independently rather than as a link in a chain. Qwen3.5-4B-4bit.

| backlog | tasks | phrasing | bare pass rate |
|---|---|---|---|
| medium | 8 | "per SPEC.md section 1" | 13/22 = **59%** |
| medium-inline | 8 | criteria inlined | 12/20 = **60%** |
| fine | 22 | criteria inlined | 35/62 = **56%** |

- **Granularity: no effect.** 8 tasks vs 22, same phrasing: 60% vs 56%.
- **Self-containment: no effect.** Same 8 tasks, same seeds, phrasing the only
  variable: 59% vs 60%. Sweep 3's claim is dead, and its 7/8-vs-2/8 was noise.

**What that leaves, and it is the useful part.** Bare, this model solves ~57% of
these tasks. Inside worksmith's loop it solved 18-22 of 22 of the same tasks.
The lift is the harness, and it is roughly the +34 points the original
differentiator eval measured on a 9B — now reproduced per-task, on a different
model, against a control that removes every harness policy rather than just
turning off `--until`.

So the branch's premise was wrong in an instructive way. Cutting work small does
not make a weak model more accurate at each piece; per-attempt accuracy is flat
at ~57% whatever the size or phrasing. What it changes is **blast radius and
checkpoint density**. At 57% per attempt a 3-task backlog dies almost at once —
which is the 0/3 coarse result at every model size, on every provider. A 22-task
backlog fails just as often per task, but each failure is one small piece the
validation loop can catch and retry rather than a whole module going wrong at
once.

That claim is consistent with every number collected here, including the two
that had to be retracted, and it does not require a planner that can decompose
to 22 pieces — which POOL_PLAN §3 named as the likelier failure and never
tested.

**Why the earlier sweeps said otherwise.** Of the first 67 harness-run tasks, 8
failed and **7 of those 8 were ended by a harness policy** — the supervisor's
repeat-abort (threshold 3) or the subprocess timeout — not by a wrong answer.
Both are numbers somebody picked. The chained sweeps were largely measuring when
worksmith gives up, with task shape as a minor input, and no amount of repeating
them would have fixed that.

**Cost.** The bare arm is 114 trials in minutes. A single chained sweep is one to
two hours and yields one coin flip. Everything above should have been measured
this way first.

# Sweep 4 — sweep 3 does not replicate, 2026-08-30

**Retraction.** Sweep 3's conclusion was drawn from one run of each backlog. Run
again on the same model with the same settings:

| backlog | run A (sweep 3) | run B |
|---|---|---|
| medium-inline | 7/8 | **2/8** |
| fine | 18–22/22 | **7/22** |

Run B is not clean — `kern.sleeptime` puts a system sleep at 00:19:42, mid-sweep
— and one task reported 3,155s elapsed against an 1,800s timeout that had
correctly not fired, because the harness measured wall clock while
`subprocess.run` enforces a monotonic one. A sweep that straddles a sleep has
dropped connections and stale server state in it, so its failures cannot be read
as the model's.

But that cuts both ways: **run A is one sample and run B is unusable, so
nothing here supports the sweep 3 claim.** The honest position is that
run-to-run variance on this model is large enough to swamp every effect reported
above, and the single-run numbers throughout this document — sweeps 1, 2 and 3
alike — are not evidence at the precision they were written with.

What has survived every run without exception: **the 4B never completes a coarse
task (0/3), and reaches somewhere between 6 and 22 of the 22 fine ones.** That
ordering is real. Every finer-grained claim built on top of it is not yet
measured.

**Fixed here.** `elapsed` is now monotonic, matching the clock the timeout
enforces, and the gap between the two clocks is reported as `slept_secs` with a
loud warning — a run that straddles a sleep now says so instead of producing a
number that quietly implies the harness is broken.

**What it would take to answer the question properly:**

- `--repeat 3` minimum on every arm; the effect sizes being chased are smaller
  than the observed spread.
- `caffeinate -i` around any sweep, or the machine will keep doing this.
- A budget that fits the work: `pa-reject` and `parse-amount` have now consumed
  1,672s / 48,532 tokens and 1,800s / 21,782 tokens without finishing. Tasks
  that routinely exceed the timeout make the timeout the independent variable.

Estimated cost of doing it right: 4 backlogs x 3 repeats x ~1-2h. That is a
day of machine time, which is the actual price of the claim.

# Sweep 3 — RETRACTED, see sweep 4 — 2026-08-29

**RETRACTED — did not replicate.** See sweep 4. Kept in full because the
reasoning was right and the sample size was not, and a retraction that deletes
its own evidence teaches nothing.

~~Self-containment is the lever. Task size is not.~~ One run of each backlog.

Qwen3.5-4B-4bit, granularity held fixed at 8 tasks with the same dependency
graph, the same checks and the same grader. The only variable is how the task is
phrased:

| backlog | tasks | phrasing | solved | per-task |
|---|---|---|---|---|
| coarse | 3 | "per SPEC.md §1–2" | 0/3 | 0% |
| medium | 8 | "per SPEC.md section 1" | 2/8 | 25% |
| **medium-inline** | **8** | **criteria inlined** | **7/8** | **87.5%** |
| fine | 22 | criteria inlined | 18–22/22 | 82–100% |

Two readings, and the data separates them cleanly:

- **Hold size, change phrasing** (medium → medium-inline, both 8 tasks):
  25% → 87.5%. Nearly all of the effect.
- **Hold phrasing, change size** (medium-inline → fine, both self-contained,
  8 → 22 tasks): 87.5% → 82–100%. Nothing beyond noise.

So the earlier sweeps measured self-containment and attributed it to
granularity, because every fine task happened to be both. `medium.toml` says
"implement parse_amount per SPEC.md section 1"; `medium-inline.toml` says
`parse_amount("$1,234.56") == 123456` and lists the cases that must raise. The
model then stops reading a four-module specification and implementing all of it
— which is what the `wrote=` column showed weak models doing all along.

**What this changes.** "Cut the work into 22 pieces" was expensive advice: it
needs a planner that can decompose to that grain, which POOL_PLAN §3 called the
likelier failure and never tested. "Write tasks that carry their own acceptance
criteria" is cheaper, is already what `--until` does at the session level, and
generalises past this fixture.

**Caveats.** One run of `medium-inline`; the effect is large (2/8 → 7/8) but
unreplicated. Both spec-referencing backlogs are also the ones whose prompts are
shortest, so "inlined" and "longer prompt" are not yet separated — though a
longer prompt making a weak model *better* is itself the interesting direction.
`fine` did not run in this sweep: the disk filled and the results write failed
with ENOSPC after the first backlog.

**Server metrics, first sweep with them wired in** (per-backlog deltas, not
lifetime): cache 82.5%, prompt 4,168,827 tokens against completion 82,257 —
**50.7:1** — and prefill 38.1% of compute. The ratio is the number to remember:
a fresh process per task re-sends the system prompt on every turn, so this
workload is dominated by prompt tokens by a factor of fifty, and only the cache
keeps prefill from swamping generation entirely.

# Sweep 2 — local MLX, Qwen3.5-4B-4bit, 2026-08-29

**The result the branch was built to get.**

| backlog | solved | end-to-end | tok/solved | wall |
|---|---|---|---|---|
| coarse | 0/3 | fail | — | 900s (timed out on task 1) |
| medium | 2/8 | fail | 15,228 | 1,340s |
| fine | **22/22** | **pass** | **3,288** | 2,784s |

One model, one specification, one grader. Cut into three tasks it cannot finish
the first. Cut into twenty-two it finishes nearly all of them.

**Replication, run 2 (after an oMLX settings change): 18/22, end-to-end fail.**
The headline above is one run and should not have been written as settled.

| | run 1 | run 2 |
|---|---|---|
| solved | 22/22 | 18/22 |
| end-to-end | pass | fail |
| tok/solved | 3,288 | 2,970 |
| wall | 2,784s | 2,545s |

The entire difference is one task on the timeout boundary. `fr-rows` hit the
900s cap in both runs — 23,135 tokens, then 25,246 — and in run 1 its check
happened to pass on work already written, in run 2 it did not. `pa-negative`
also hit the cap in run 2 and passed the same way. So **the 900s budget, not
the model, decides whether this reads 22/22 or 18/22**: the same class of
artifact as the `gen_tok=0` bug, and harder to catch because either number
looks plausible.

What survives both runs: **the 4B solves 18–22 of 22 fine tasks and 0 of 3
coarse ones.** That ordering is the result. The clean "22/22 with a working
program" is a demonstration still owed a run with a budget that fits the work.

Nothing was spliced in; `--keep-going` was not used in either run.

Cost per solved task fell 4.6x from medium to fine, so the granularity that
made it possible also made it cheaper. Wall clock tripled, which is the trade
this project takes.

**Three of the 22 passed while the model was failing.** `pf-read` ended
`stuck: repeated bash 5 times`, `fr-rows` and one other hit the 900s clock — and
their checks passed anyway, because the work was already correct and only the
model's stopping was broken. The check is the arbiter, not the model's account
of itself. That is the whole thesis in three rows.

**Floor check first, and it is what makes this readable.** Bare, no harness, no
tools, one shot at the smallest task: 2B **0/5**, 9B **1/3**, 4B **5/5**
(`evals/pool/floor.py`). The 2B is below the floor, so granularity has nothing
to work with. The 27B is above it (33/33 everywhere), so there is no headroom.
The 4B sits between, which is why it is the rung that shows the effect.

## The caveat this sweep found in its own design

Weak models given a medium task **implement the whole specification**. Both the
9B and the 4B, asked only for `parse_amount`, wrote `cli.py`, `money.py`,
`records.py` and `report.py` — then failed, debugging four files against a check
that tests one function.

That is the shared-`SPEC.md` decision biting back. Making it complete and
identical removed the information confound; it also hands a weak model the whole
job on every task, so a "small task" is not small in practice.

And it introduces a confound not seen when the backlogs were written: **fine
tasks are not only smaller, they are more self-contained.**

- medium: "Implement parse_amount in money.py, per SPEC.md section 1." → must
  read the whole spec
- fine: "Extend format_cents to insert thousands separators:
  format_cents(123456) == '$1,234.56'." → expected values inline, never needs
  the spec

So the gain above may be self-containment rather than size. Separable with one
more backlog: `medium-inline.toml`, the same 8 tasks at the same granularity
with expected values inlined. Performing like fine means the lever is
self-containment; performing like medium means it is size. **Until that runs,
"granularity helps" is the honest claim and "task size is the lever" is not.**

# Sweep 1 — OpenRouter, 2026-08-29

Three backlogs (3 / 8 / 22 tasks), one shared spec, one shared end-to-end check.
Numbers are tasks solved **before the first wall**, not pass rates: the backlogs
are chains, so a failure blocks everything downstream (see "What these numbers
are not").

| model | coarse | medium | fine | tok/solved (fine) |
|---|---|---|---|---|
| `qwen3.8-27b` | 3/3 | 8/8 | **22/22** | 1,235 |
| `ministral-14b-2512` | 0/3 | 2/8 | 7/22 | 1,127 |
| `qwen3.5-9b` (thinking off) | 0/3 | 2/8 | 6/22 | 670 |

## What held

**Granularity helps every model that can fail.** coarse < medium < fine on both
weak models, same spec, same checks, same shared grader. Nothing but the cutting
changed.

**Cost per solved task falls as tasks shrink.** On the 27B, which has no
accuracy headroom and so isolates cost cleanly: 3,029 → 2,187 → 1,235, a 2.5×
drop. The kill criterion in §8 moved the right way.

**Wall clock rose as predicted** — 156s → 438s → 699s on the 27B for the same
job. Accepted (§4).

## What did not hold: the mechanism

**Confidently-wrong was 0 in every run.** §8 named it the metric that decides
the experiment: the bet was that small tasks plus a per-task check would convert
"declared done, was wrong" into caught-and-retried. That is not what happened,
because on these models the failure never wore that shape. Every weak-model
failure was `stuck: repeated bash 5 times with identical arguments` — thrashing,
caught by the supervisor's repeat detector, not by a check catching a false
claim.

So the gain is real and arrives by a different route than predicted:
**containment.** A failure costs one small task instead of the whole job. The
27B needed 189s and 9,835 tokens to get `parse-amount` right; the same capability
gap took down all three of coarse's tasks on the 14B.

§8 is revised accordingly: confidently-wrong stays measured but is no longer the
sole decider, because a metric that reads zero on every arm cannot discriminate
between them.

**The wall is the same task at every granularity and on every weak model** —
`parse_amount`'s rejection logic (coarse `money-and-records`, medium
`parse-amount`, fine `pa-reject`). The failure mode is over-correction: told to
reject bad input, the model broke valid input, and the per-task check caught the
regression because it re-asserts the earlier cases.

## What these numbers are not

**They are time-to-first-wall, not pass rates.** `fine.toml` is a 22-deep chain,
so one failure blocks the remaining tasks and they are never attempted. "7/22"
means "failed at task 8", not "failed 15 tasks".

`--keep-going` does not fix this and was not meant to: it splices in the
reference, which implements the *whole* backlog, so every later check would pass
for free. Everything after a repair is marked tainted and unscored — without
that rule a stub agent writing nothing scores 21 of 22, which the dispatcher
test asserts.

Turning these into real per-task pass rates needs a reference snapshot per task
(the workspace as it should look *after* task N, not the finished solution), so
each task can be run independently from a known-good state. ~22 snapshots for
`fine.toml`, self-verifying: each must pass its own task's check and fail the
next one's. Not built — the ordering above is unambiguous without it.

## A confound found afterwards: sampling is not uniform across the ladder

The global config sets `temperature = 0.7` for everything, then overrides
`omlx/Qwen3.5-9B-OptiQ-4bit` alone to `temperature = 0.6, top-k = 20`. So the
local 9B arm was sampled differently from the 2B and 27B arms, and the claim
that "only the cutting changed" holds *within* a model but not *between* the
local ones.

It costs little in practice — the local 9B produced only a timeout, so no result
rests on it — but it is recorded rather than quietly levelled, because changing
it now would break comparison with the hosted numbers already taken at 0.7.

**Settings a result depends on must be written down here.** `.gitignore`
excludes `.worksmith/` wholesale, so this repo's own project config is not
tracked: a context window or a temperature set there travels no better than a
value typed into a model server's UI. The local runs above used
`context = 32768` for the 2B and 9B, global `temperature = 0.7`, and the 9B
override noted above.

## Harness bugs this sweep found

Two of the three runs were invalid on first attempt, both for harness reasons,
neither for capability:

- **`qwen3.5-9b` always reasons.** Probed directly it answered "reply with
  exactly: hello" with 46 reasoning tokens, no content, `finish_reason=length`.
  The whole sweep scored `stuck: the model returned an empty response`.
  `run.py` had `--fast`; `run_pool.py` did not. Fixed.
- **A timeout threw away the evidence of what it interrupted.**
  `subprocess.TimeoutExpired` carries everything the process printed before the
  kill, and this harness discarded it — so a task killed by the clock reported
  `gen_tok=0, outcome=None`, byte-identical to a model that never produced a
  token.

  **This produced a wrong diagnosis, recorded here because the wrong one is
  still in the git history.** Three coarse runs were called provider stalls on
  that evidence. They were not. Running the same task by hand against the local
  9B shows it reading `SPEC.md` and then generating 1,044 tokens of
  implementation — working normally, just slower than the clock allowed. The
  hosted 9B's two 600s failures cannot now be classified either way; their
  output is gone.

  Fixed: partial output is parsed on timeout, the row is flagged `timed_out`,
  and the error names how far it got. A timeout is also no longer eligible to
  count as confidently-wrong, since the model never claimed anything.

  **So every `0/3` on coarse in the table above is "did not finish in the time
  allowed", not "could not do it".** Coarse is three module-sized tasks; on a
  local 4-bit 9B that is a genuinely long job, and the number to fix is the
  budget, not the model.

## Next

Re-run coarse with a budget that fits the work, now that a timeout reports what
it interrupted. The local `omlx` provider has a capability ladder configured (`gemma-3-270m` →
`Qwen3.5-2B` → `SmolLM3-3B` → `Strand-Rust-Coder-14B` → `Qwen3.8-27B-4bit`) and
is free, deterministic in routing, and stall-free. That is the better place to
ask where the capability floor is.
