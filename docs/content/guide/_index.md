+++
title = "Guide"
description = "Configure your model, understand validation, and inspect usage and results."
+++

Start with the [quickstart](@/quickstart.md) to install Worksmith and try the
TUI or CLI. These guides cover configuration, validation, and how to understand
a run's results.

## The pages

- [**Model configuration**](@/guide/configuration.md): connect local and hosted
  providers, set how hard the model thinks, and fix setup problems.
- [**Configuration recipes**](@/guide/recipes.md): complete, tested configs for
  OpenRouter, Qwen 3.8 on vLLM, oMLX on a Mac, cheap workers, project checks, a
  project skill, and a remote MCP server.
- [**The validation loop**](@/guide/validation-loop.md): why the loop exists,
  what "done" means, how a failure becomes a re-plan, how long turns are
  bounded, and the two evals that bound the claim. It is decisive on a weak
  model and dead weight on a capable one.
- [**Measuring the harness**](@/guide/measuring.md): the 22 task comparison.
  56% for a 4B alone, 97% for the same model in the loop, 100% for Sonnet at
  26 cents. Includes the four results we retracted and the measurement bugs
  that produced them.
- [**Metrics and cost accounting**](@/guide/metrics.md): what every dashboard,
  footer, and JSON value means, the cache and cost formulas, provider-neutral
  accounting, and how to verify recorded accounting.
- [**Prompt and cache behavior**](@/guide/prompt-cache.md): where skills live,
  how loading them works, stable request prefixes, and a repeatable local cache
  probe.

Some topics have no page of their own yet:

- **Workers:** one `/spawn` into N workers, the supervisor that applies the same
  nudge and escalate mechanism to many, and the worker validator that closes the
  same hole in the background. The README is the reference for now.
- **Memory and knowledge:** memory is a distilled decision, preference, fact, or
  lesson. Knowledge is the repo's own rebuildable text index. Memory can be
  mined from past sessions and reviewed with `/memory pending`; knowledge is
  searched on demand with `/knowledge search`. Both belong to the directory you
  launch in, so start Worksmith in the project's own folder. Launched in a
  folder that holds several repos, it warns at startup and the model's
  knowledge search refuses to run.
- **Trust:** a project's `.worksmith/config.toml` is not applied until you say
  it is. The decision is keyed by content, so an edit asks again. `/trust`
  shows the current decision and `/trust revoke` reopens it.
- **Thinking cost:** `↻` in the footer is reasoning spend, and `max-tokens` has
  to cover reasoning *and* output. `/fast`, `/think low`, and `/think <tokens>`
  are the controls when a local model thinks itself into silence. See
  [reasoning effort and budgets](@/guide/configuration.md#reasoning-effort-and-budgets).

The reference lives in the README, `config.example.toml`, and the in-app
`/help`. When they disagree, `/help` and the shipped `config.example.toml` are
closest to the code.
