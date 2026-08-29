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
- **Workers** (coming soon) — one `/spawn` into N workers, the supervisor that
  is the same nudge/escalate mechanism applied to many, and the worker
  validator that closes the same hole in the background.
- **Memory and knowledge** (coming soon) — why a distilled decision is memory
  and a chunk of the repo's own text is knowledge, and why the prompt never
  gets the latter wholesale.
- **Trust** (coming soon) — why a project's `.worksmith/config.toml` is not
  applied until you say it is, and why the decision is keyed by content, so an
  edit re-asks.
- **Thinking cost** (coming soon) — what `↻` in the footer means, why
  `max-tokens` must cover reasoning *and* output, and when `/fast` is the
  right call.

The reference — CLI flags, config keys, tool descriptions, the event enum — is
generated from the binary and the source, not hand-written here. When the two
disagree, the generated reference is right.
