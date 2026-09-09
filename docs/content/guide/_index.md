+++
title = "Guide"
description = "The concepts behind worksmith, one page each. Start with the validation loop: why the harness exists, what 'done' means, and the measurements that bound the claim."
+++

One binary, one bet. The other terminal agents — Codex, Gemini CLI, pi — are
thin wrappers around a frontier model that mostly stays on task. Worksmith bets
the other way: **the harness does the work of keeping a weaker model honest.**
These pages explain the machinery that bet relies on.

The single most important page is the [validation loop](validation-loop.md).
Everything else hangs off it. In one paragraph: a task carries a check you
named — `--until "cargo test"` — and the turn is not done when the model says
it is done, but when that check exits 0. When the model spins, the harness
notices and sends it back with the failure output. That is the whole product,
and it is measurable: on a small model (qwen3.5-9b) it was worth +34 points —
52% to 86% — at flat cost per solved task, because all ten of the unguided
failures had outcome `done`. The model declared itself finished and was wrong.
On a capable 27B the same loop changed nothing — 21/21 either way, for about
18% more tokens — which is why guidance is earned, not assumed, and why the
docs say so out loud.

## The pages

- [**The validation loop**](validation-loop.md) — why the loop exists, what
  "done" means, how a failure becomes a re-plan, and the two evals that bound
  the claim: decisive on a weak model, dead weight on a capable one.
- [**Measuring the harness**](measuring.md) — the 22 task comparison: 56% for a
  4B alone, 95% for the same model in the loop, 100% for Sonnet at 26 cents.
  Includes the four results we retracted and the measurement bugs that produced
  them.
- [**Metrics and cost accounting**](@/guide/metrics.md) — what every dashboard, footer,
  and JSON value means; cache and cost formulas; provider-neutral accounting;
  and a dogfood checklist for the feature branch.
- **Workers** — one `/spawn` into N workers, the supervisor that is the same
  nudge/escalate mechanism applied to many, and the worker validator that
  closes the same hole in the background. The current reference is in the
  README until this gets its own guide page.
- **Memory and knowledge** — memory is a distilled decision, preference, fact,
  or lesson; knowledge is the repo's own rebuildable text index. Memory can be
  mined from past sessions and reviewed with `/memory pending`; knowledge is
  searched on demand with `/knowledge search`.
- **Trust** — a project's `.worksmith/config.toml` is not applied until you say
  it is. The decision is keyed by content, so an edit re-asks. `/trust` shows
  the current decision and `/trust revoke` reopens it.
- **Thinking cost** — `↻` in the footer is reasoning spend, `max-tokens` must
  cover reasoning *and* output, and `/fast` or `/think <budget>` are the main
  controls when a local model thinks itself into silence.

The reference lives in the README, `config.example.toml`, and the in-app
`/help`. When they disagree, `/help` and the shipped `config.example.toml` are
closest to the code.
