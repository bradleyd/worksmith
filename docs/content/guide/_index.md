+++
title = "Guide"
description = "Configure your model, understand validation, and inspect usage and results."
+++

Start with the [quickstart](@/quickstart.md) to install Worksmith and try the
TUI or CLI. These guides cover configuration, validation, and how to understand
a run's results.

## The pages

- [**Model configuration**](@/guide/configuration.md) — connect local and hosted providers and troubleshoot setup.

- [**The validation loop**](@/guide/validation-loop.md) — why the loop exists, what
  "done" means, how a failure becomes a re-plan, and the two evals that bound
  the claim: decisive on a weak model, dead weight on a capable one.
- [**Measuring the harness**](@/guide/measuring.md) — the 22 task comparison: 56% for a
  4B alone, 97% for the same model in the loop, 100% for Sonnet at 26 cents.
  Includes the four results we retracted and the measurement bugs that produced
  them.
- [**Metrics and cost accounting**](@/guide/metrics.md) — what every dashboard, footer,
  and JSON value means; cache and cost formulas; provider-neutral accounting;
  and steps to verify recorded accounting.
- [**Prompt and cache behavior**](@/guide/prompt-cache.md) — skill lifecycle,
  stable request prefixes, and a repeatable local cache probe.
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
