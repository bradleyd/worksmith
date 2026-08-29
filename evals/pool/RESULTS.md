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

## Harness bugs this sweep found

Two of the three runs were invalid on first attempt, both for harness reasons,
neither for capability:

- **`qwen3.5-9b` always reasons.** Probed directly it answered "reply with
  exactly: hello" with 46 reasoning tokens, no content, `finish_reason=length`.
  The whole sweep scored `stuck: the model returned an empty response`.
  `run.py` had `--fast`; `run_pool.py` did not. Fixed.
- **Two timeouts set to the same number.** Worksmith bounds the gap between
  stream chunks at `DEFAULT_STREAM_IDLE_SECS` = 600s and reports a stall as a
  retryable error. This harness also killed the process at 600s, so a provider
  that accepted a request and went quiet was killed at the exact moment
  worksmith was about to handle it — surfacing as `outcome=None gen_tok=0`,
  which reads as the model failing. Harness default is now 900s.

  The 9B's `0/3` on coarse is still that artifact, twice, and should not be read
  as capability. Its completed tasks were fast and cheap (221–531 tokens).

## Next

The remaining variance is the provider, not the model: OpenRouter routing gives
different endpoints run to run, and a stall costs 10 minutes. The local `omlx`
provider already has a capability ladder configured (`gemma-3-270m` →
`Qwen3.5-2B` → `SmolLM3-3B` → `Strand-Rust-Coder-14B` → `Qwen3.8-27B-4bit`) and
is free, deterministic in routing, and stall-free. That is the better place to
ask where the capability floor is.
