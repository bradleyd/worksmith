+++
title = "Model configuration"
description = "Connect a local or hosted model, change providers, and troubleshoot setup."
weight = 5
+++

Worksmith reads `~/.worksmith/config.toml`. The [quickstart](@/quickstart.md)
shows a minimal hosted setup. This page covers local servers and additional
provider settings.

## Local models and providers

Point a provider at your server and set `model` to `provider/model-id`.
Enable tool calling on the server so Worksmith can read files, edit code, and
run commands. For example:

```sh
vllm serve Qwen/Qwen3.5-9B --enable-auto-tool-choice \
  --tool-call-parser qwen3_coder --reasoning-parser qwen3 \
  --enable-prefix-caching
```

```toml
model = "vllm/Qwen/Qwen3.5-9B"

[providers.vllm]
type = "openai-compat"
base-url = "http://localhost:8000/v1"
# no api-key-env — a local server needs no key
thinking-param = "chat-template"
reasoning-budget-param = "thinking_token_budget"
```

The parser flags follow the [vLLM Qwen3.5 recipe](https://docs.vllm.ai/projects/recipes/en/latest/Qwen/Qwen3.5.html).
Check that recipe for installation and hardware requirements. Reasoning-budget
fields below are server-specific examples; confirm them against your installed
server's schema before relying on a numeric budget.

## Provider examples

Add the sections you need to `~/.worksmith/config.toml`. Set the top-level
`model` to use the corresponding provider, and avoid duplicating a section
already in your file:

```toml
# OpenRouter: hosted models behind OPENROUTER_API_KEY. Built in for --model,
# but explicit config lets you set routing and timeouts.
[providers.openrouter]
type = "openai-compat"
base-url = "https://openrouter.ai/api/v1"
api-key-env = "OPENROUTER_API_KEY"
# sort = "throughput" # or "latency" / "price"

# OpenAI: hosted models behind OPENAI_API_KEY. Also built in for --model.
[providers.openai]
type = "openai-compat"
base-url = "https://api.openai.com/v1"
api-key-env = "OPENAI_API_KEY"

# vLLM: local or remote OpenAI-compatible server.
[providers.vllm]
type = "openai-compat"
base-url = "http://127.0.0.1:8000/v1"
thinking-param = "chat-template"
reasoning-budget-param = "thinking_token_budget"

# oMLX: macOS/Apple silicon server.
[providers.omlx]
type = "openai-compat"
base-url = "http://127.0.0.1:8000/v1"
thinking-param = "chat-template"
reasoning-budget-param = "thinking_budget"
```

## Check server settings

Reasoning parameter names depend on the server. If your server publishes an
OpenAPI schema, inspect it to see which request fields it supports:

```sh
curl -s http://127.0.0.1:8000/openapi.json \
  | python3 -c "import json,sys; \
    print(*json.load(sys.stdin)['components']['schemas']['ChatCompletionRequest']['properties'])"
```

## Model overrides and separate homes

`--model` overrides the configured model. OpenRouter and OpenAI have built-in
provider defaults, so `--model openrouter/<model-id>` or
`--model openai/<model-id>` works when the matching API key environment
variable is set. Replace `<model-id>` with a model available to your account.
A local or custom provider needs a `base-url` in your config first.

`WORKSMITH_HOME` relocates configuration, sessions, and global memory together.
For a throwaway run, copy a known-good `config.toml` into that directory or edit
the example generated there.

## Troubleshooting

- **The wrong version runs:** `which -a worksmith` shows all installed copies.
  `install.sh` prefers a writable `~/.local/bin` already on PATH, then
  `~/.cargo/bin`, and falls back to `cargo install`. Your shell uses the first
  matching directory on PATH.
- **No model is configured:** copy the generated `config.example.toml` to
  `config.toml` and set `model` plus the provider's `base-url`.
- **Authentication fails:** export the environment variable named by
  `api-key-env` in the shell where you launch Worksmith. Omit `api-key-env`
  for a server that needs no key.
- **Long reasoning with no answer:** try `--fast` at launch or `/fast` in an
  interactive session. Keep `max-tokens` generous: it covers reasoning and
  output, including file contents inside tool calls.

The generated `~/.worksmith/config.example.toml` is the full annotated
reference for model settings, context limits, prices, and agent options.
