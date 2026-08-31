# Plan: a forgiving tool-call parser

**Start here.** This is the next thing to build. It is written to be picked up
cold, so it carries its own evidence.

## The failure

A small model drops out of structured tool calling under load and writes the
call as *text* instead. Seen live on 2026-08-30, hosted `qwen/qwen3.5-9b`, in
both a main session and a spawned worker:

```
<tool_call>
<function=bash>
<parameter=command>
cd /Users/bradleydsmith/Projects/mud-test && python -m pytest tests/ -v 2>&1 | head -100
</parameter>
</function>
</tool_call>
```

That is a well-formed intention in the wrong channel.

**And usually the wrong channel is `reasoning`, not `content`.** A second sighting
the same evening, rendered by the TUI with its `thinking` prefix:

```
thinking <tool_call>
  <function=bash>
  <parameter=command>
  pip3 install pytest -q 2>&1 | tail -5
  </parameter>
  </function>
  </tool_call>The background worker task ran but I need to verify …
```

That is the whole explanation for "empty response": `content` genuinely is
empty, the call went into the provider's `reasoning` field, and `reasoning` is
display-only and is never sent back to the model (`StreamEvent::ReasoningDelta`).
So the model issues a tool call and worksmith throws it away.

**A parser that reads only `content` will therefore never fire.** It has to
consider `reasoning` as well. Note also that the reasoning holds prose *after*
the block, so extraction must take the block and leave the rest rather than
assuming the whole field is a call.

With `tool_calls` empty and `content` empty, worksmith concludes the model said
nothing.
It nudges (`Your last response was empty. Make a tool call or give your answer.`),
the model does the same thing again, and the turn ends
`stuck: the model returned an empty response`.

**It is not rare.** In one evening it ended a main-session turn and a worker
turn, and the worker's session file shows two of its replies unusable out of
roughly a dozen. It also alternates: a structured `edit` lands *between* two
text-format failures, so the model is not stuck in one mode, it slips in
and out of the format.

**It may also explain results we blamed on something else.** The hosted 9B
HumanEval and sweep runs produced `stuck: the model returned an empty response`
repeatedly and it was attributed entirely to thinking being on. Same signature,
two possible causes. Traces are kept now (`evals/pool/run_pool.py --trace`), so
this is checkable rather than arguable.

## Where it goes

`OpenAiClient`'s stream accumulator assembles the final reply in
`into_completion` (`src/llm/openai.rs:617`). That is the one place where
`content`, `reasoning` and `tool_calls` are known together, and the only place
this belongs.

The rule: **if `tool_calls` is empty and a tool call can be parsed out of
`content` or `reasoning`, promote it.** Never when structured calls are present , a model that produced both is
already being understood, and reinterpreting its prose would invent calls it did
not make.

## Formats to accept

Two, both seen in the wild from Qwen-family models. Keep the list short and
literal; a loose parser here fabricates tool calls out of prose, which is worse
than the failure it fixes.

1. **Hermes/Qwen XML** , the one observed above:
   `<tool_call><function=NAME><parameter=KEY>VALUE</parameter>…</function></tool_call>`
2. **A fenced JSON object** naming a tool:
   ` ```json {"name": "bash", "arguments": {...}} ``` `

Anything else is left alone. When a block is promoted, strip it from whichever
field it came from so the text is not both spoken and executed, and keep the
surrounding prose: the sighting above has a sentence after the block.

## Guard rails

- **Only when `tool_calls` is empty.** Stated twice because it is the one rule
  that makes this safe.
- **Only if the name matches an advertised tool.** `ToolRegistry` knows them.
  A promoted call to something that does not exist is a hallucination given
  hands.
- **Arguments must parse as JSON**, or become JSON from the XML parameters.
  A malformed block stays as text: a tool call with garbage arguments fails at
  the tool boundary and costs another turn either way.
- **Say it happened.** Emit a warning (`Event::Warning`) the first time per
  session. A silent rescue hides a model that is drifting, which is worth
  knowing when choosing one.

## Testing

`ScriptedClient` (`src/agent.rs`, `mod scripted`) already returns arbitrary
`Completion`s, so the whole thing tests without a model: hand it a completion
with the XML in `content` and empty `tool_calls`, assert the loop executes the
tool. Unit-test the parser directly for both formats, for a name that is not a
real tool, for malformed arguments, and for the case where structured calls are
present and content merely *mentions* a tool call.

The end-to-end check is the eval that already exists:

```sh
python3 evals/pool/humaneval.py --model openrouter/qwen/qwen3.5-9b --limit 60 \
    --arms harness --json before.json     # baseline, before the change
```

Harness arm scored 162/164 (99%) on 2026-08-30 with the parser absent, so a
regression there is the thing to watch. The clearer signal is the count of
`stuck: the model returned an empty response` outcomes across a run, which
should fall to near zero.

## Prior art

`rustopedia/SMALLCODE_ADOPTION.md` §5, "Forgiving Tool Call Parser", is the same
idea from the same reasoning. Worth reading before starting; it is a design
sketch rather than an implementation, so there is nothing to port directly.

## Deliberately not

- **Not a general "extract intent from prose" layer.** Two literal formats, an
  advertised-name check, and JSON that parses. Everything else stays text.
- **Not a retry.** The reply is understood, not re-requested. Asking again is
  what currently costs the turn.
- **Not in the agent loop.** `into_completion` is where a reply becomes a
  `Completion`; putting it in `agent.rs` would mean every future client
  reimplements it.


---

# After the parser: the MUD fan-out test

`~/Projects/mud-test` is committed at `701b479` and is the fixture for this.
It is not the scaffold it was meant to be, and the reasons are the point.

## What is actually in there

Four modules, **all fully implemented**, 165 to 278 lines each, and **zero**
`NotImplementedError` anywhere. Only `rooms.py` was ever given to a worker, so
the scaffold session wrote the other three itself, having been told "Do NOT
implement any module body" and having reported back that "all stub methods raise
NotImplementedError as required". Confidently wrong, in the one turn a human was
watching.

Plus 130 tests across four files: 17 rooms, 26 items, 35 combat, 52 parser.

## Three things to fix before running it

1. **There is no test runner.** `pytest` is not installed, so
   `--until "python3 -m pytest ..."` could never pass, whatever the worker
   wrote. Every check failed on an import error rather than a test result, and
   the worker spent its turn trying `pip install pytest` and hitting
   `pip: command not found`. Either install pytest or convert the tests to
   `unittest` from the standard library. Stdlib is safer: the goal is testing
   the harness, not the runner.

2. **The scaffold check could not fail for the reason it existed.**
   `python3 -c 'import rooms…' && pytest --collect-only` passes just as happily
   on implemented code as on stubs, which is why the over-production went
   unnoticed. A check that cannot fail for the thing you care about is not a
   check. The stronger form asserts the tests error with `NotImplementedError`.

3. **Decide which experiment this is.** Two options, and the second is probably
   better:
   - **Greenfield:** re-stub the modules and fan out `-n 3` to implement them.
     Tests the planner and parallel workers, which is what the fan-out has no
     evidence for.
   - **Brownfield:** leave it implemented and give it the real task, which is
     "make the tests runnable and passing" on existing code with 130 tests and
     no runner. Closer to how anyone actually uses this, and it exercises
     reading a codebase the model did not write.

## What the fixture already bought

The evening produced four filed findings and two shipped fixes without the test
ever completing, which is worth remembering when the next run also fails:

- the stray tool call landing in `reasoning` (this plan)
- the TUI loop sleeping through worker completions, fixed
- the checkpoint asking for help without showing the evidence
- the checkpoint accepting only a directive, never a question
