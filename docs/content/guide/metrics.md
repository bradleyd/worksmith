+++
title = "Metrics and cost accounting"
description = "How to read each metric, compare sessions, and test the accounting without confusing context size, token spend, or provider billing."
weight = 30
+++

Use `/metrics` or `/stats` for a report overlay in the TUI, or a text report in
the plain REPL. Both read the
session's recorded data. A turn can make several model calls; each call can
resend the conversation. That is why input-token spend can grow much faster
than the context window.

The footer keeps short status messages beside the metrics. Longer messages move
to wrapped rows below them, with space reserved so the composer stays visible.

## Open a report

| Command | Result |
|---|---|
| `/metrics [session-id]` in the TUI | Dashboard overlay for the current session or the given session. |
| `/stats [session-id]` | Report overlay in the TUI; text report in the plain REPL. In plain mode, `/metrics` is also an alias. |
| `worksmith stats <session-id>` | Offline text report. No model configuration or provider connection needed. |
| `worksmith stats <session-id> --json` | Structured accounting for scripts and evals. |

Use the full session ID printed at startup. In reference overlays, **j/k**,
**↑/↓**, or **Ctrl+P/Ctrl+N** move one row; **PageUp/PageDown** and
**Ctrl+U/Ctrl+D** move ten rows. **gg/G** jump to the top/bottom. The mouse wheel
also scrolls the overlay, leaving the transcript in place. Press **/** to type a
substring filter, **Enter** or **Esc** to return to navigation with the filter
preserved, and **Esc** while navigating to close.
**q** also closes while navigating; in a filter it is ordinary text.
Opening `/metrics` or `/stats` does not add transcript rows. It is a snapshot: reopen it to
refresh the numbers while work continues.

## Which work is counted?

The **Summary**, **Latest Call**, **Averages**, **Context Breakdown**, and
**Recent Calls** sections describe the selected session's conversation model
calls. They exclude helper requests and workers. **Session Totals** and
**Cost by Model** include that session's helper requests, such as compaction,
fan-out planning, and memory extraction. Helpers contribute spend without
changing the reported conversation context.

**Workers** lists separately recorded child sessions linked to this parent.
**Parent + Available Workers** adds their totals to the parent once each.
Worker context does not get added to the parent's context percentage. A missing
worker file produces a warning and is excluded from combined totals. Historical
sessions without recorded worker links cannot recover that association.

Counts cover successfully returned requests for which Worksmith wrote a
`model_metrics` event. Failed or interrupted requests that never return usage
may still incur provider charges that this accounting cannot see. A transport
attempt is not a completed model call. The separate `usage` event drives live
UI counters; it is not added again to the metrics totals.

## Request timing and context

| Display value | Meaning |
|---|---|
| Summary `calls` | Number of recorded conversation model calls in the selected session. Helpers are excluded here, so this can be lower than Session Totals `calls`. |
| `compactions` | Recorded completed history-compaction operations. A skipped compaction does not increment it, although its helper call may have spent tokens. |
| `peak ctx` | Largest input-token count of a conversation request. It is not cumulative input spend. |
| Latest Call `ctx` | Input tokens for the latest conversation request, including cached input. |
| `output` | All generated tokens for that request, including reasoning and tool-call output. |
| `reasoning` | The portion of output spent reasoning. Do not add it to `output` again. The current adapter estimates it from reasoning text when the provider reports zero or omits the breakdown; zero can also mean unavailable. |
| `first` | Time from starting the client call until Worksmith receives its first text, reasoning, or tool-call output event. This can precede a visible answer. `n/a` means no such event was observed. |
| `total` | Elapsed client-call time, including streaming completion. It includes network/client overhead, provider queueing, and prompt processing, but not subsequent tool execution. |
| `prompt/s` | Input tokens divided by seconds to first output. This is a client-observed estimate, not server prefill throughput: queueing and cache hits affect it. It is zero when first-output timing is unavailable. |
| `decode/s` | Output tokens divided by the time from first output to request completion. If first-output timing is unavailable, the denominator is the full request time. The denominator is at least 1 ms. |
| Averages `first` | Arithmetic mean of available first-output timings only. If none exist, the current display shows `0ms`; that is not evidence of instantaneous output. |
| Averages `total`, `prompt/s`, `decode/s` | Arithmetic means of the per-call values, not token-weighted throughput. Missing first-output timing contributes a zero to the `prompt/s` average. |
| Recent Calls `#` | Conversation-call sequence number, distinct from the user-turn number. The table shows at most the last eight calls. |
| Recent Calls `age` | Seconds from the first recorded event to this call's metrics event. Despite the label, this is a position on the session timeline, not “seconds ago.” |

Timings in JSON use milliseconds. Display values use `ms` or seconds; `k`
means thousands of tokens rounded to one decimal place. Display rounding can
hide small changes that remain visible in JSON.

### Context Breakdown

These are local estimates of prompt components, not provider billing categories.
They primarily estimate text bytes divided by four, counting tool-call names
and arguments and serialized tool schemas where applicable. They do not
reproduce the provider's tokenizer or chat-template framing, so the sum can
differ from the provider count.

| Value | What it attributes |
|---|---|
| `system` | The base system prompt, before separately attributed loaded skills. |
| `tools` | The advertised tool schemas. In this section, this is a token estimate, not a tool-call count. |
| `skills` | Loaded skill instructions added to the system prompt. |
| `memory` | The dynamic memory context injected for the turn. |
| `history` | Conversation messages other than the most recent user-role message, including tool results and retained compaction summaries. |
| `latest user` / `user` | The most recent user-role message in the assembled conversation. This can be an injected instruction, not necessarily the user's original typed prompt. |
| `est sum` | Sum of the six local attribution estimates. |
| `provider` | Actual normalized input-token count returned for the request. Compare it with `est sum`; do not expect exact equality. |
| Compaction `latest ~before -> ~after` | Estimated conversation tokens before and after the most recent completed compaction. These exclude some request overhead and are not provider billing totals. |

A rising `history` value often explains growing context. A large `system`,
`tools`, or `skills` value indicates standing prompt overhead that compaction
may not remove. Breakdown data can be absent in older sessions.

## Session totals, turns, and cache coverage

The JSON field names below apply to a `totals` object. Per-turn rows use the
same token, call, time, and cost definitions within that turn.

| Display / JSON field | Meaning |
|---|---|
| `turns` / length of `turns` | Reconstructed user turns, including a started turn with no recorded completion yet. A turn can contain several model and tool calls. |
| `calls` | Recorded model requests, including helpers within the selected scope. |
| `tools` / `tool_calls` | Tool-call events, including calls whose results failed. |
| `in` / `prompt_tokens` | Sum of all request input tokens, including cache reads and writes. Resending the same history counts again. |
| `out` / `completion_tokens` | Sum of generated tokens, including reasoning. |
| `reasoning` / `reasoning_tokens` | Sum of the reported or estimated reasoning subset of output. |
| `model` / `model_ms` | Sum of elapsed model-request times. It is not session wall-clock duration. Combined worker time can exceed wall time because workers run concurrently. |
| `compactions` | Completed compaction events in this scope. |
| `nudges` | Recorded `nudge` events. This is not a count of all retries or all interventions. |
| `cache` numerator / `cached_tokens` | Input tokens served from cache, summed only where reported. Each request's count is capped at its input-token count for aggregation. |
| `cache` denominator / `cache_prompt_tokens` | Input tokens from calls that reported cache-read telemetry. Calls with unavailable telemetry are excluded from this denominator. |
| `cache` percentage | `100 × cached_tokens / cache_prompt_tokens`, or zero for a zero denominator. It is a token-weighted hit rate over reporting calls. |
| Cache `X/Y calls reported` / `cache_reported_calls` | `X` requests reported cache reads, out of `Y = calls`. An explicit zero counts as a report; unavailable data does not. |
| `cache writes` / `cache_write_tokens` | Input tokens used to create cache entries, summed where reported and capped per request at input tokens. These are not cache hits or additional input tokens. |
| Cache-write coverage / `cache_write_reported_calls` | Calls reporting cache writes, including explicit zero. The text line is omitted when no calls report it. |
| `cost` / `known_cost_usd` | Sum of available request-time cost estimates in USD. It can be only a partial total. |
| `unpriced_calls` | Calls with no usable cost estimate, including missing or partial provider usage. The text report prints the known cost plus this count rather than treating them as free. |

For example, if one request reports 800 cached tokens out of 1,000 input tokens
and a second request reports no cache information, the rate is **80% over 1/2
reporting calls**. It does not mean 80% of all session input was cached. Cache
writes are already in input totals and are never added to the hit numerator.

### Per-Turn History and Highest Known Turn Costs

`#1`, `#2`, and so on identify reconstructed turns. Each row includes its usage,
last and peak conversation context, and the latest available context breakdown.
`done` is the loop's outcome; **`(validated)`** means the last recorded validation
result passed. A bare `done` is not proof that a check ran. `(validation failed)`
means the last recorded check failed. `in progress / incomplete` means no
`turn_complete` event was recorded; the report cannot tell a live turn from an
interrupted process solely from that absence.

Helper calls between turns contribute to session totals without creating a new
turn or being charged to the next one. Helpers recorded during an active turn
contribute to that turn. Attribution follows event order, not causal tracing of
background requests. Per-turn totals therefore need not sum to session totals.

**Highest Known Turn Costs** shows up to five turns ranked by known cost. Unknown
costs do not participate in the numeric ranking, so a partly unpriced turn could
actually be the most expensive. Worker costs are separate; this ranking does
not allocate a worker's spend to its originating parent turn.

## Cost estimates and the footer

Prices in `[models."provider/model"]` are USD per million tokens. For each
request, Worksmith records:

```text
cost_usd = (input_tokens × input_price + output_tokens × output_price) / 1,000,000
```

Both prices must be known. For loopback endpoints, unset prices default to zero;
for remote endpoints, unset prices remain unknown. Configured local prices take
precedence. The model key and estimate are captured with the request, so changing
`/model` or editing prices later does not reprice previous events.

These are estimates at standard rates, not invoices. Provider-specific cache
read discounts, cache-write premiums, special token-category prices, and
charges without returned usage are not calculated. Zero means known free only
when the call was priced; a missing price is different from zero. Small known
amounts can also round to `$0.000` in the footer.

| Footer value | Meaning |
|---|---|
| Model name | The currently selected conversation model. The cost may include earlier models. |
| `ctx N% (used/limit)` | Most recent conversation input count divided by the configured context limit. It excludes workers and helper context. |
| `↓N` | Conversation output tokens observed in this UI session, excluding helper and worker output. It is a live counter, not the authoritative persisted total. |
| `↻N` | Current/last reasoning spend: a live text-size estimate while streaming, then the completion's reported or estimated reasoning count. |
| `⚠cut` | The last completion ended with finish reason `length`, indicating truncation. |
| `$N` | Accumulated recorded parent-session cost, including helpers and previous models. Restored when reopening a session. |
| `$N+?` | Known parent-session cost plus calls whose prices are unknown. Use `/stats` for the unpriced-call count. |
| `🤖 … running / queued` | Current worker-manager activity. |
| Worker `🪙 N ($cost)` | Output tokens and known spend for workers tracked by the current manager. The compact footer does not expose unpriced coverage; use `/stats` for persisted worker accounting and warnings. |
| `think:…` | Configured reasoning mode/budget, not measured token spend. |
| Spinner and elapsed seconds | Wall time for the current turn or manual compaction, including waiting and tool work. This differs from summed model time. |

On resume, the cost is restored from recorded metrics, but the live conversation
context/output counters start fresh. `/stats` is the source for historical
counts. A reopened session's worker links also remain available to `/stats`,
even though the new UI's worker manager is not tracking those old processes.

## JSON and future providers

The offline JSON report has four top-level fields:

| Field | Contents |
|---|---|
| `parent` | Selected session's `totals`, `turns`, and `models`. |
| `workers` | Linked worker records with `id`, `session_id`, and their own `accounting`. |
| `combined` | Parent plus readable, uniquely linked worker totals. No combined context percentage. |
| `warnings` | Missing or invalid worker-link diagnostics; combined totals may be incomplete. |

`models` maps the recorded model key to request-only totals. Tool calls,
compactions, and nudges are counted at session/turn level, not assigned to a
model in this map. Old events without model identity use `unknown (legacy)`.
Each turn includes `number`, `outcome`, `validation`, `totals`, `peak_context`,
`last_context`, and `last_context_breakdown`. `validation` is `true`, `false`,
or `null` for no recorded check.

Raw JSONL `model_metrics` events additionally carry `session_id` (absent in legacy events), `model`, `purpose`
(`agent` or `helper`; empty for legacy events), `cost_usd`, normalized token
counts, optional cache data and `context_breakdown`, `total_ms`,
`first_output_ms`, `prompt_tokens_per_second`, and
`completion_tokens_per_second`. Timing trends are available in those events
and the text report; the offline JSON accounting report does not repeat every
request's timings. Read a missing optional measurement as unavailable. A legacy
session's missing prices, cache details, or worker links cannot be reconstructed
by changing today's config.

Background helper jobs capture their originating session before launch. Their
metrics stay in that session even if `/new` runs before the job finishes; late
events from the old session do not update the new session's footer.

All provider adapters return the shared `Usage` contract: input includes cache
reads and writes, output includes reasoning, and optional cache details are
subsets. Adapters with inclusive totals populate it directly; adapters with
disjoint counts can use `Usage::from_parts`. `Usage.reported` is true only when
both input and output counts are available, including explicitly reported zeros.
The default is false. Direct adapters must set it; `from_parts` sets it to true
and must only be used with complete reported categories. Missing or partial
usage has no cost estimate, even with configured prices; its numeric zero
placeholders do not establish that the request was free. Wire field names and
streaming usage accumulation belong in the adapter. New measurements extend the shared
types with optional, serde-defaulted fields, keeping older sessions readable.

The eval harness in `evals/run.py` reads the offline JSON report's `combined`
totals for headline accounting, including workers and helpers. Stats collection
failures are recorded as `metrics_error` on the evaluation row; validation still
runs and determines pass/fail. Failed stats collection leaves accounting
incomplete. Its dollars per solved task is **total recorded cost across attempts / externally validated
passing attempts**, including spend on failures in the numerator. It reports
that ratio only when all rows have metrics, no unpriced calls, no worker warnings,
and at least one pass. This differs from a session turn merely saying `done`.
Eval `elapsed` is wall time around execution, stats collection, and validation;
`model_ms` is summed request time.

## Verify the accounting

Run these commands from the repository so `cargo run` uses the current checkout
rather than an older installed binary. Replace placeholders with your configured
model and the full session ID printed at startup.

```sh
cargo run -- --model <your-provider/model>
```

1. Ask it to explain a source file, then trace a related function. Open
   `/metrics` and check that calls, tool usage, tokens, and per-turn rows grow.
   Scroll the dashboard with page keys or the wheel; the transcript should stay
   in place. Try `/` filtering, Enter, and `gg/G`, then close with Esc.
2. Run `/stats` while thinking is streaming; closing it should reveal uninterrupted
   thinking without command output in the transcript. Compare its totals with a freshly opened
   `/metrics`. Cache data should show reported coverage or unavailable; cache
   hits depend on the provider and are not guaranteed by repeating a prompt.
3. Use `/model` to choose another configured model and send another prompt.
   Check the cost breakdown has both model keys and earlier costs stay fixed.
   A remote model without prices should show unpriced calls. A loopback model
   without configured prices should show zero cost.
4. Run `/spawn Summarize src/validation.rs without changing files.` Wait for it
   and any synthesis to finish. Check `/stats` for separate worker totals and
   the combined total; parent context should not become worker context.
5. After enough turns to have old history, run `/compact`. If it actually makes
   a helper request, its spend should appear in session totals. A completed
   compaction should increment the compaction count. There may be nothing old
   enough to compact in a short session.
6. Copy the session ID, quit, and run the offline commands below. Counts and
   prices should survive reopening. Editing configured prices afterwards must
   not change this session's recorded costs.

```sh
cargo run -- stats <session-id>
cargo run -- stats <session-id> --json
cargo run -- --plain --resume <session-id>
```

In the plain REPL, run `/stats` and `/quit`. The offline report can also be read
when the model server is stopped.

These automated tests exercise inclusive and disjoint provider accounting
without adding or contacting another provider:

```sh
cargo test disjoint_provider_usage_uses_the_same_events_costs_and_reports
cargo test inclusive_wire_counts_are_not_added_to_their_details
cargo test --test metrics
python3 -m unittest discover -s evals -p test_metrics.py
```

Before committing code changes, run the full `cargo test` suite and
`cargo clippy --all-targets -- -D warnings` as well.
