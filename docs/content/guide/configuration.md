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
# no api-key-env: a local server needs no key
thinking-param = "chat-template"
reasoning-budget-param = "thinking_token_budget"
```

The parser flags follow the [vLLM Qwen3.5 recipe](https://docs.vllm.ai/projects/recipes/en/latest/Qwen/Qwen3.5.html).
Check that recipe for installation and hardware requirements. The
reasoning-budget field names below are per server; [check yours](#check-server-settings)
before relying on a budget.

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

## Reasoning effort and budgets

Thinking models reason before they answer, and the reasoning counts against
`max-tokens`. A small model with no limit will reason until it hits the cap and
return nothing. There are two ways to rein it in.

A **budget** is a hard ceiling in tokens. With `thinking = 2000` in `[agent]`,
`--think 2000`, or `/think 2000`, the server cuts the reasoning off at 2,000
tokens and the model goes straight to its answer.

An **effort level** (`/think low`, `thinking = "medium"`) is advice. On its own
it adds a line to the prompt asking the model to keep its thinking brief, and
on a hard problem the model ignores it. Qwen 3.8 27B on vLLM, told `low`,
reasoned to a 6,000-token cap with no answer. So when the provider has a
`reasoning-budget-param`, Worksmith sends a budget with every level:

| Level | Budget sent |
| --- | --- |
| `minimal` | 512 |
| `low` | 1,024 |
| `medium` | 4,096 |
| `high` | 8,192 |
| `xhigh` | 16,384 |
| `max` | none |

A level's budget never goes above three quarters of `max-tokens`, so there is
always room left for the answer. A number you set yourself is sent as is. With the budget, the same Qwen question at `low`
stopped reasoning at 1,023 tokens and answered.

Servers disagree on which levels exist. Qwen 3.8 accepts only `low`, `medium`,
and `xhigh`, and rejects anything else with a message listing those three.
Worksmith passes the word through, so you see the server's own error. With no
level set, Qwen 3.8 uses `xhigh`.

`/fast` (or `--fast`) turns thinking off entirely. It is the cheapest setting,
and the validation loop is what catches the difference.

## Check server settings

Reasoning parameter names depend on the server: vLLM takes
`thinking_token_budget`, oMLX takes `thinking_budget`. If your server publishes
an OpenAPI schema, list the request fields it knows:

```sh
curl -s http://127.0.0.1:8000/openapi.json \
  | python3 -c "import json,sys; \
    print(*json.load(sys.stdin)['components']['schemas']['ChatCompletionRequest']['properties'])"
```

A field in that list is not proof it does anything. Servers accept fields they
ignore, so a request with a misspelled budget succeeds and the budget silently
does nothing. To be sure, send a hard question with a small budget and check
that the reasoning stops near it:

```sh
curl -s http://127.0.0.1:8000/v1/chat/completions -H 'content-type: application/json' -d '{
  "model": "<served-model-name>", "max_tokens": 4000,
  "chat_template_kwargs": {"enable_thinking": true}, "thinking_token_budget": 300,
  "messages": [{"role": "user", "content": "How many primes are there between 1000 and 1100?"}]
}' | python3 -c "import json,sys; m=json.load(sys.stdin)['choices'][0]['message']; \
  print(len(m.get('reasoning_content') or m.get('reasoning') or ''), 'reasoning chars')"
```

A few hundred tokens of reasoning (roughly 700 to 1,000 characters) means the
budget works. Thousands means the server ignored it.

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
- **Long reasoning with no answer:** the footer shows `⚠cut` and the model
  replies with nothing. Set a budget or a level (`/think 2000`, `/think low`),
  or turn thinking off with `/fast`. Keep `max-tokens` generous, at least 4096:
  it covers reasoning and output, including file contents inside tool calls.
- **A reply dies with "Connection reset by peer":** usually the laptop slept or
  the network dropped. Worksmith retries on its own, waiting 2, 4, 8, and 16
  seconds, which covers wifi coming back after a wake. If it stays down longer
  than that, the turn ends with the error. Nothing is lost; the cut-off reply
  was never saved, so `continue` picks up from the last finished step.
- **A warning that the folder holds several projects:** Worksmith was started in
  a directory like `~/Projects`. Memory, knowledge, and decisions belong to the
  launch directory, so every project started there would share them. Quit and
  start again in the project's own folder.

The generated `~/.worksmith/config.example.toml` is the full annotated
reference for model settings, context limits, prices, and agent options.
