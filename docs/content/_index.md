+++
title = "Worksmith"
description = "A terminal coding agent for small, cheap, or local models."
+++

Worksmith reads your repository, edits code, and runs tools. Use the
full-screen terminal interface for interactive work, or run a single task
from the command line. It connects to local and hosted models through an
OpenAI-compatible API.

**[Start with the quickstart](@/quickstart.md)** — install, connect a model,
and run your first task.

## Two ways to work

**Interactive TUI:** launch in your repository, type a task, and follow up
as you work.

```sh
worksmith
```

**One-shot CLI:** run a task, print the answer, and exit.

```sh
worksmith --print "explain how this project is organized"
```

Both modes share the same tools, model configuration, and validation loop.
Use `worksmith --plain` for a line-based interactive session or `--mode json`
for structured output.

## Give the task a check

Name the command that must pass before Worksmith can accept the task as done:

```sh
worksmith --print --until "cargo test" "make the failing test pass"
```

When the model finishes, Worksmith runs the check. If it fails, the output
goes back to the model for another attempt, up to a configured retry limit.
In the TUI, set the same check with `/validate cargo test`.

The check is as useful as its coverage. Worksmith can stop with a failure,
and a passing test is still a reason to review the diff.

## Find what you need

- [Quickstart](@/quickstart.md) — installation, setup, and both ways to run a task.
- [Model configuration](@/guide/configuration.md) — local and hosted providers, plus setup fixes.
- [Validation loop](@/guide/validation-loop.md) — how Worksmith checks completion and retries.
- [Metrics and cost](@/guide/metrics.md) — token usage, latency, and the TUI footer.
- [Measuring the harness](@/guide/measuring.md) — evaluation results and their limitations.

For commands while you work, type `/help` in an interactive session.
