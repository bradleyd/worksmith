+++
title = "Configuration recipes"
description = "Complete, copyable configs for common setups: hosted, local vLLM, oMLX on a Mac, cheap workers, project checks, skills, and remote MCP."
weight = 6
+++

Each recipe is a whole file, not a fragment. Copy it, change the parts the
comments point at, and it runs. Every TOML block on this site is parsed by
Worksmith's real config loader in `cargo test`, so a renamed setting breaks the
build instead of your copy.

Global settings go in `~/.worksmith/config.toml`. Project settings go in
`<project>/.worksmith/config.toml` and override the global file field by field.
The [model configuration](@/guide/configuration.md) page explains the settings
these recipes use.

## Hosted model on OpenRouter

The quickest start. One key, many models, nothing to run locally.

```toml
model = "openrouter/qwen/qwen3.8-27b"
max-tokens = 8192

[providers.openrouter]
type = "openai-compat"
base-url = "https://openrouter.ai/api/v1"
api-key-env = "OPENROUTER_API_KEY"
sort = "throughput"   # or "latency", or "price" (the default)

[agent]
# A reasoning cap, sent as OpenRouter's `reasoning.max_tokens`, so the model
# can't spend all of max-tokens thinking.
thinking = 2000

# Prices make the footer show dollars instead of `$0.000+?`. Copy them from the
# model's OpenRouter page, in USD per million tokens.
# [models."openrouter/qwen/qwen3.8-27b"]
# input = 0.0
# output = 0.0
```

```sh
export OPENROUTER_API_KEY="your-api-key"
```

## Qwen 3.8 27B on your own vLLM server

The setup most of Worksmith is tuned on. Start the server with tool calling and
the reasoning parser on. Without `--reasoning-parser qwen3`, the model's
thinking lands in the answer.

```sh
vllm serve Qwen/Qwen3.8-27B-FP8 \
  --served-model-name qwen38 \
  --max-model-len 131072 \
  --reasoning-parser qwen3 \
  --enable-auto-tool-choice --tool-call-parser qwen3_coder \
  --enable-prefix-caching
```

```toml
model = "vllm/qwen38"   # provider name / --served-model-name
max-tokens = 8192

[providers.vllm]
type = "openai-compat"
base-url = "http://127.0.0.1:8000/v1"   # the server's address
thinking-param = "chat-template"
reasoning-budget-param = "thinking_token_budget"

[models."vllm/qwen38"]
context = 131072        # match --max-model-len, or compaction fires late
temperature = 1.0       # Qwen's recommended sampling for thinking mode
top-p = 0.95
top-k = 20

[agent]
# "low" asks for brief thinking and sends a 1,024-token budget the server
# enforces. Qwen 3.8 knows low, medium, and xhigh.
thinking = "low"
```

Check that the budget is honored before you rely on it. The
[check server settings](@/guide/configuration.md#check-server-settings) section
has a one-line test.

## A small model on a Mac with oMLX

Runs on Apple Silicon with no GPU server. oMLX spells the budget field
differently from vLLM. `curl http://127.0.0.1:8000/v1/models` lists the model
names it will answer to.

```toml
model = "omlx/Qwen3.5-9B-OptiQ-4bit"
max-tokens = 8192

[providers.omlx]
type = "openai-compat"
base-url = "http://127.0.0.1:8000/v1"
thinking-param = "chat-template"
reasoning-budget-param = "thinking_budget"

[models."omlx/Qwen3.5-9B-OptiQ-4bit"]
context = 32768         # the context you loaded the model with
temperature = 0.6
top-k = 20

[agent]
thinking = "low"
```

A 9B is where the [validation loop](@/guide/validation-loop.md) earns its
keep. Give it a check with `--until` or `/validate` and it catches the "I'm
done" that isn't.

## Main model local, workers on something cheaper

`/spawn` runs a task in background workers. Workers can use a different model
from the main session, so the judgment stays on the strong model and the
legwork goes somewhere cheap.

```toml
model = "vllm/qwen38"
max-tokens = 8192

[providers.vllm]
type = "openai-compat"
base-url = "http://127.0.0.1:8000/v1"
thinking-param = "chat-template"
reasoning-budget-param = "thinking_token_budget"

[providers.openrouter]
type = "openai-compat"
base-url = "https://openrouter.ai/api/v1"
api-key-env = "OPENROUTER_API_KEY"

[agents]
model = "openrouter/qwen/qwen3.5-9b"
max = 4                     # workers running at once
validate = "cargo check"    # each worker's result has to pass this
token-budget = 100000       # a worker that spends this much is escalated
```

## Project checks and pairing

A project file that makes every turn prove itself with the project's own
tests, and stops at decisions worth your say. It lives in the repository, so
everyone working on it gets the same check.

```toml
# <project>/.worksmith/config.toml

# Where pairing checkpoints file decision records. Keep it in a tracked
# directory, or the record isn't in the history it was written for.
decisions-dir = "docs/decisions"

[agent]
validate = "cargo test"   # a turn is not done until this passes
max-retries = 3           # re-plans after a failed check before giving up
pair = true               # offer checkpoints at decisions
```

A project config can run shell commands, so Worksmith ignores it until you
trust it. It asks the first time, and again whenever the file changes.
`--trust-project` trusts it without asking.

## A skill for one project

Skills are instructions the model can load when a task calls for them. Put a
skill in the project's `.worksmith/skills/` and it is only offered when you
launch in that project. The folder name and the `name` field have to match.

```markdown
---
name: run-migrations
description: Create and apply SQLite schema migrations for this app. Use when a change adds or alters a table.
---

# Running migrations

1. Add a new file under `migrations/` named `NNNN_short_description.sql`, one
   number higher than the last.
2. Apply it with `cargo run -- migrate`.
3. Run `cargo test` before finishing. Never edit a migration that has already
   been applied; add a new one.
```

Save it as `<project>/.worksmith/skills/run-migrations/SKILL.md`. Run `/skill`
from the project to confirm it loaded. The
[prompt and cache](@/guide/prompt-cache.md#where-skills-live) page has the full
search order.

## A remote MCP server through a bridge

Worksmith talks to local stdio MCP servers. A server you reach by URL, with an
OAuth login, needs a bridge such as `mcp-remote` in between. Install it once
(`npm install -g mcp-remote`) and log in from a normal terminal first:
`mcp-remote https://mcp.example.com/mcp`.

```toml
[mcp.example]
enabled = true
command = "/opt/homebrew/bin/mcp-remote"   # wherever `which mcp-remote` points
args = ["https://mcp.example.com/mcp"]

[mcp.example.env]
HOME = "HOME"   # the bridge finds its saved login under ~/.mcp-auth
```

Worksmith passes almost nothing from your environment to an MCP server, which
is why `HOME` has to be named. The bridge is someone else's code holding your
login, so turn on only the tools you need. The
[MCP notes](https://github.com/bradleyd/worksmith/blob/main/docs/mcp.md) cover
approval, limits, and the risks.
