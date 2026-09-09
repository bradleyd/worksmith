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

## Repeatable local measurement

The ignored Rust test sends seven small requests using synthetic conversation
text and the real OpenAI-compatible adapter. It requires an explicit endpoint and
model; ordinary `cargo test` runs never invoke it. Use a local server with an
appropriate chat template and thinking dialect.

```sh
WORKSMITH_CACHE_BASE_URL=http://localhost:8000/v1 \
WORKSMITH_CACHE_MODEL=your-model \
cargo test --test prompt_cache compare_prefix_reuse -- --ignored --nocapture
```

Each JSON row reports `prompt_tokens`, optional `cached_tokens`, whether usage was
reported, milliseconds to the first nonempty text/reasoning delta, and total
elapsed milliseconds. A missing cache count is unavailable telemetry, not zero.
The probe compares identical repeats, changed early memory, changed tail memory,
and a changed skill suffix. It does not assert latency thresholds or flush the
server cache. Shared tool prefixes may already be cached even on the first case.

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
