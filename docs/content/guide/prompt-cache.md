+++
title = "Prompt and cache behavior"
description = "What changes model requests, how skill loading works, and how to measure prefix reuse."
weight = 35
+++

## What stays stable

Worksmith assembles the base instructions, project instructions, and sorted skill
catalog into the system message. Loaded skills follow in insertion order.
Tool definitions use registration order. Ordinary steps append conversation
messages; opening overlays, filtering, previewing, and scrolling do not change
these inputs. Duplicate loads, failed loads, and unloading an absent skill do not
change the system message. Regression tests compare the actual requests.

Reasoning traces, served-model labels, and finish reasons are session metadata;
the OpenAI-compatible adapter does not send them back. Tool-call IDs and argument
strings are preserved. Identical JSON is a useful stability check, but actual
cache reuse depends on the provider's tokenizer, chat template, routing, cache
capacity, and configuration.

## What intentionally changes

- Loading or unloading a skill changes the system suffix on the next request.
- Turn memory is a separate user message immediately after the system message.
  It is frozen within a turn. Different relevant memory on the next turn changes
  the prefix **before all conversation history**.
- Compaction replaces old history with a summary. Loaded skills survive it.
- Project/catalog edits and explicit tool or model settings can change requests.

For providers using prefix caching, a changed early token limits reuse of the
suffix. See [vLLM's automatic prefix caching documentation](https://docs.vllm.ai/en/latest/features/automatic_prefix_caching.html).
Moving memory closer to the latest question is a possible follow-up, but it also
changes instruction placement. The probe below compares that hypothetical layout;
production memory placement is unchanged pending task-quality validation.

## Skill controls and limits

`/skill` opens a searchable list and instruction preview. Tab changes the focused
pane; its heading has a `>` marker and underline so focus remains visible across
terminal themes. Arrows, `j/k`, page keys, and the mouse wheel scroll that pane. `gg` and `G`
go to the top and bottom. `/` always focuses and filters the skill list by name
or description, including when the preview was focused. The query appears on a
wrapping line above both panes; it does not search the preview text. Enter loads the selection, `u`
unloads it. While editing a filter, Esc returns to navigation and preserves the
query/results; pressing Esc again closes the browser. Narrow terminals stack the panes vertically.

`[catalog]` means automatically discovered but not pinned; `[active]` means its instructions
are included in subsequent requests. Loaded previews show the pinned text even if
the file changes. Unload and reload to refresh it. Browsing makes no model call.
Both frontends also support `/skill <name>` and `/skill unload <name>`.

Loading pins the body plus reference map. There is no separate skill byte/count
limit and no silent eviction. Active instructions consume the model context window;
loading many large skills can still cause a provider context-limit error, handled
by the existing request retry/compaction path. Workers inherit a snapshot and then
manage their own loaded state independently.

The browser occupies its own screen while open, preserving the composer beneath
it. Preview text wraps at words; help and status reserve as many rows as their
wrapped text requires. Narrow terminals stack the two panes.

Unload removes standing instructions from future requests. It does not cancel an
in-flight request, erase historical tool results, or prevent a later model tool
call from loading the skill again. Loaded state is in-process; it is not restored
from session JSONL on restart.

## Repeatable provider measurements

The two ignored Rust probes use entirely synthetic instructions, conversations,
and schemas through the real OpenAI-compatible adapter. They send no repository
or user instructions. Ordinary `cargo test` runs only the two offline layout and
scoring checks; live probes require an explicit endpoint and model.

```sh
export WORKSMITH_CACHE_BASE_URL=https://openrouter.ai/api/v1
export WORKSMITH_CACHE_MODEL=qwen/qwen3.8-27b
export WORKSMITH_CACHE_API_KEY_ENV=OPENROUTER_API_KEY
export WORKSMITH_CACHE_SESSION_ID=worksmith-cache-check-unique-run-id
cargo test --test prompt_cache -- --ignored --nocapture --test-threads=1
```

Set the API key separately in the named environment variable. For a local server,
change the endpoint/model and unset the optional key-variable and session settings.
Use a fresh session ID for each run. OpenRouter's explicit session header avoids
changing its inferred conversation key when opening messages change; affinity is
best effort, not a provider pin. See [OpenRouter prompt caching](https://openrouter.ai/docs/guides/best-practices/prompt-caching).

`compare_prefix_reuse` sends 21 requests across three rounds: first requests,
identical repeats, changed memory in both layouts, and a changed skill suffix.
`compare_memory_quality` sends 24 requests across six direct-answer cases, two
layouts, and two rounds. It checks memory conventions, superseded history,
explicit overrides, irrelevant or missing memory, and a prior tool result.
Current tools are omitted from the quality probe to separate answer quality from
tool selection. Returned tool calls, truncation, malformed JSON, extra fields,
and incorrect values fail its exact-JSON rubric. It prints scores without a
pass-rate assertion: successful execution does not mean every answer passed.

Rows report input/output tokens, optional cached tokens, usage availability,
finish reason, tool calls, total elapsed milliseconds, and milliseconds to the
first nonempty text/reasoning delta. Tool-only replies can have no first-delta
measurement. Missing cached tokens mean unavailable telemetry, not zero.
Neither probe flushes caches or asserts latency thresholds. Even a first request
can share a cached prefix. These are placement experiments, not completed coding
agent evaluations.

See the [OpenRouter comparison results](@/guide/prompt-cache-results.md) for the
2026-09-08 measurements and the decision to preserve production memory placement.

### Local observation, 2026-09-08

One run against the configured LAN vLLM server (`qwen38`) produced:

| Case | Input tokens | First output (ms) |
| --- | ---: | ---: |
| Early memory, first request | 6,213 | 3,495 |
| Identical repeat | 6,213 | 1,662 |
| Early memory changed | 6,213 | 3,255 |
| Tail memory, first request | 6,213 | 3,234 |
| Identical repeat | 6,213 | 1,684 |
| Tail memory changed | 6,213 | 1,626 |
| Loaded skill suffix changed | 6,233 | 3,227 |

The server reported input/output usage but no cached-token counts. The user subsequently confirmed that the server was busy with another workload.
These latency comparisons are therefore inconclusive and need an isolated rerun. They are one synthetic run, not a cache hit-rate measurement
or a controlled benchmark: queueing, server load, and template behavior can affect
them. Validate task outcomes on the target model before moving memory in production.
