+++
title = "Prompt placement experiment"
description = "OpenRouter cache and memory-following measurements from September 8, 2026."
weight = 36
+++

## Decision

Keep production memory immediately after the system message. Moving it before the
current turn showed potential for cache reuse, but these measurements establish
neither a consistent latency improvement nor sufficient task-quality evidence to
change production behavior. This branch changes probes and documentation only.

## Method

Two runs used OpenRouter `qwen/qwen3.8-27b`, temperature zero, thinking off, and an
explicit session header. Each made 21 cache requests and 24 quality requests.
Cache cases ran three times with alternating layout order; quality cases ran twice
per layout. The candidate places memory after completed history, before the whole
current turn, including its subsequent tool exchanges.

The exploratory run used Worksmith's base prompt and builtin schemas. Its quality
rubric required direct JSON but did not execute returned tool calls. The final
repeatable probe uses wholly synthetic instructions and schemas; its quality
requests advertise no tools. These are different experiments, not comparable
latency samples. Raw rows, scores, and synthetic answers are recorded in
`evals/results/prompt-cache-20260908.json`. The current probe reproduces the
synthetic protocol, not the earlier exploratory prompt.

OpenRouter can infer a conversation key from opening messages. An explicit
`x-session-id` avoids changing that inferred key during the experiment, but
routing affinity remains best effort. Actual providers were not recorded or
pinned. See [OpenRouter's cache documentation](https://openrouter.ai/docs/guides/best-practices/prompt-caching).

## Cache observations

Each cell lists cached tokens for the three rounds. Timing is median milliseconds
to the first nonempty text/reasoning delta. No cache flush was performed; “first”
is a case label, not a guaranteed cold cache.

### Exploratory prompt and builtin tools

Normal requests contained 6,228 input tokens; skill changes contained 6,248.

| Layout | Case | Cached tokens, three rounds | Median first output (ms) |
| --- | --- | --- | ---: |
| Prefix | First | 0, 0, 0 | 2,286 |
| Prefix | Repeat | 4,800, 4,800, 4,800 | 1,421 |
| Prefix | Memory changed | 0, 0, 0 | 1,849 |
| Turn start | First | 0, 0, 0 | 2,100 |
| Turn start | Repeat | 4,800, 4,800, 4,800 | 1,382 |
| Turn start | Memory changed | 4,800, 4,800, 4,800 | 1,843 |
| Turn start | Skill changed | 0, 0, 0 | 2,295 |

The candidate retained reported cache reads when memory changed. The corresponding
median first-output times were essentially equal (1,849 versus 1,843 ms), so this
sample does not demonstrate a practical speedup for that case.

### Synthetic prompt and schema

Normal requests contained 3,656 input tokens; skill changes contained 3,676.

| Layout | Case | Cached tokens, three rounds | Median first output (ms) |
| --- | --- | --- | ---: |
| Prefix | First | 0, 0, 320 | 760 |
| Prefix | Repeat | 0, 320, 320 | 946 |
| Prefix | Memory changed | 0, 0, 320 | 760 |
| Turn start | First | 0, 0, 320 | 953 |
| Turn start | Repeat | 0, 0, 320 | 622 |
| Turn start | Memory changed | 320, 320, 320 | 961 |
| Turn start | Skill changed | 320, 320, 0 | 749 |

Cache coverage was small and variable. Different prompt/schema lengths and
unobserved provider routing prevent attributing the difference between runs to
memory placement alone. These rows do not support a general latency claim.

## Memory-following observations

Scores require exactly the requested JSON, with no tool calls or truncation.
Each layout has twelve responses: six cases repeated twice.

| Run | Prefix | Turn start |
| --- | ---: | ---: |
| Exploratory, tools advertised | 7/12 | 5/12 |
| Synthetic, no current tools | 10/12 | 10/12 |

Every exploratory failure returned tool calls instead of direct JSON. Because the
probe did not execute those calls, these scores measure compliance with the answer
rubric, not completed-agent success or whether those tool actions were appropriate.

In the synthetic run, prefix placement failed both prior-tool-result cases by
emitting literal function markup instead of JSON. Turn-start placement failed both
irrelevant-memory cases with `{"key":42}` instead of `{"value":42}`. The arithmetic
was correct; the field name failed the rubric. All other cases passed in that run.
Equal totals therefore hide different failures. Two repetitions at temperature
zero are not independent statistical evidence of reliability.

## Next production gate

Use completed tool-loop tasks that edit a fixture and run an objective check, with
an observed or fixed provider and explicitly supported reasoning settings. Include
conflicting historical preferences, user overrides, and irrelevant memory. Compare
solved tasks as well as reported cache reads and latency.

Offline coverage currently checks the experimental layout's append-only behavior
within a turn and preservation of completed history when current memory changes.
It does not prove cross-turn append-only requests: ephemeral memory can disappear
or move on the next turn. Any production change must cover that transition,
compaction, and resumed sessions before rollout.

Repeat commands and metric definitions are in
[Prompt and cache behavior](@/guide/prompt-cache.md).
