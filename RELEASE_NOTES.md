# Worksmith release notes

This release adds parent MCP support and isolated worker worktrees, and makes
worker results easier to inspect across the TUI, plain REPL, and CLI.

Since v0.5.0, it also adds session/worker metrics, interactive skill management,
and command-overlay fixes. Prompt-cache reuse and memory placement now have
expanded regression checks and documented measurements.

Workers keep their edits in separate Git worktrees until you explicitly apply
them. Review diffs are colored, and worker reports distinguish the model's summary
from the actual validation command, outcome, output, and saved log.

New sessions live under `sessions/YYYY/MM/DD/<session-id>/`, with their transcript
and supporting files together. Find prior work without starting a model:

```sh
worksmith sessions list --date 2026-09-09
worksmith sessions search 'chapter 9' --from 2026-09-01 --to 2026-09-09
worksmith --resume <session-id>
```

Search uses ripgrep (`rg`). No session database index is required. Existing flat
sessions continue to work without migration. This release does not automatically
prune sessions or remove your previous development runs.

**Use MCP at your own risk. MCP servers are not sandboxed.** Approved servers run
with your account's permissions, and their internal operations bypass Worksmith's
command checks. Worker worktrees provide edit isolation, not OS containment.

See [MCP setup](https://github.com/bradleyd/worksmith/blob/main/docs/mcp.md), [worker worktrees](https://github.com/bradleyd/worksmith/blob/main/docs/worktrees.md), and
[session storage](https://github.com/bradleyd/worksmith/blob/main/docs/sessions.md) for commands and limitations.
