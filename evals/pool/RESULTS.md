# Pool sweeps — results

Newest first. Each sweep says what held, what did not, and what it found wrong
with its own design; a result whose caveats are not written next to it gets
quoted later without them.

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
