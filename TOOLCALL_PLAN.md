# Plan: a forgiving tool-call parser

**Built, 2026-08-31.** `src/llm/rescue.rs`, promoted from `stream()` in
`openai.rs`. 13 unit tests plus two end-to-end tests through the real client and
the mock SSE server (`tests/streaming.rs`). Everything below is kept as written
— it is the reasoning the thing was built from — with a section at the end
recording where the code had to depart from it. Unmeasured against a live
model; **the MUD run below is the first real exercise.**

The rest of this file, from "After the parser", is still ahead.

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


## What the plan got wrong

Two, both found in the code rather than at runtime, and both worth keeping
because the same mistakes are available to whoever touches this next.

**The `<tool_call>` wrapper never reaches the parser in `content`.**
`strip_toolcall_noise` (`openai.rs:526`) removes `<tool_call>` and its variants
from every content delta before they are accumulated, and it has to: providers
leak fragments of that wrapper into ordinary text, which is the bug it was
written for. A parser anchored on `<tool_call>` would therefore have fired on
`reasoning`, which is not stripped, and never once on `content` — passing its
own tests and half-working in production. The shapes anchor on `<function=` and
on the JSON object, both of which survive. The stripping is also split across
deltas, so a wrapper can arrive as `<tool_` + `call>` and be only half removed;
the end-to-end test sends it that way deliberately.

**A third shape had to be accepted.** The classic Hermes format really is
`<tool_call>{"name": "bash", "arguments": {…}}</tool_call>` — JSON, not the
`<function=` XML — and after stripping it arrives as a bare object with no
fence. So: XML, fenced JSON, and a bare JSON object *that is the entire field*.
The last one is the narrow case on purpose: an object sitting inside prose is
far more likely to be the model showing its work than asking for a tool, and
the object must carry no keys beyond `name` / `arguments` / `parameters`.

**And one thing the plan did not think of.** XML parameters carry no types, so
`<parameter=limit>40</parameter>` would arrive as the string `"40"` and fail
`read`'s schema on the way in — a promoted call that fails at the tool boundary
costs the same turn as an unread one. The advertised `ToolDef.parameters` is
already in hand for the name check, so the value is coerced by what the schema
says the key should be, and left a string whenever the schema does not say.

Placement moved one line. `into_completion` has no `sink`, so it cannot say the
rescue happened; the call sits immediately after it in `stream()`, where
`req.tools` also supplies the advertised-name check without `ToolRegistry`
being involved. Same moment, same three fields known together.

The `ScriptedClient` test the plan suggested was dropped for a better one: the
mock SSE server in `tests/streaming.rs` drives the real client, so the
noise-stripper and the delta accumulation are in the loop instead of stubbed
past. A scripted `Completion` would have sailed straight over the wrapper
problem above.

---

# After the parser: the MUD fan-out test

`~/Projects/mud-test` is committed at `701b479` and is the fixture for this.
It is not the scaffold it was meant to be, and the reasons are the point.

## What is actually in there

**Corrected 2026-08-31 by measuring it, and the earlier account was wrong in
both directions.** What follows replaces it; the old version is in the git
history, and it is worth knowing how it went wrong because the mistake is easy
to make again.

It said: *four modules, all fully implemented, zero `NotImplementedError`
anywhere, the scaffold session over-produced after being told not to,
confidently wrong in the one turn a human was watching.*

The truth, from an AST walk over the four files at `701b479`:

| module | stubbed | implemented |
|---|---|---|
| `rooms.py` | 1 | 14 |
| `items.py` | 22 | 0 |
| `combat.py` | 28 | 0 |
| `parser.py` | 19 | 0 |

**The scaffold session did what it was told.** It stubbed all three modules it
wrote. `rooms.py` is the *only* implemented one, and it is implemented because
it is the only one that was ever given to a worker — so the worker did its job
too. The evening's account had both agents failing at the one thing each of
them actually got right.

**How the reading went wrong: line count was mistaken for implementation.** The
files are 165 to 278 lines, which looks like working code and is almost entirely
docstrings. `items.py` is 278 lines and 22 empty bodies.

**What was really wrong was one word.** The session reported that "all stub
methods raise NotImplementedError as required" and had written `pass` — 66 of
them. That is a small error with a large consequence, and it is the actual
finding: `pass` returns `None` silently, so a test gets an `AssertionError`
about a value rather than being told the body was never written, and any check
looking for `NotImplementedError` sees nothing. Fixed in `7e057e2`; the
`Protocol` bodies in `combat.py` stay `...`, which is a declaration and not a
stub.

**So "brownfield" as described below is not available.** There is no
substantial existing implementation to work on — the option was invented from
the same misreading. The two real choices are greenfield, or greenfield with
`rooms.py` already done as a worked example.

Plus 131 tests across four files: 22 rooms, 30 items, 34 combat, 45 parser.
Every one of them runs now, and every failure is a `NotImplementedError` except
one, which is discussed below.

## Three things to fix before running it — two are done

1. **There is no test runner.** *Done, `b09c039`.* `pytest` is not installed,
   so `--until "python3 -m pytest ..."` could never pass, whatever the worker
   wrote. Every check failed on an import error rather than a test result, and
   the worker spent its turn trying `pip install pytest` and hitting
   `pip: command not found`.

   The conversion turned out to be trivial and was done here rather than handed
   to a worker: `import pytest` was a **dead import**, with zero fixtures, zero
   `pytest.raises`, zero `parametrize` and zero marks across all four files.
   Bare `assert` works inside a `TestCase`, so it was the import, the base
   class, and a `__main__` block. Handing that to a worker would have meant
   every `--until "python3 -m unittest tests.test_X"` failing on a
   `ModuleNotFoundError` before it could say anything about module X — a check
   that fails for a reason other than the thing you care about, which is
   exactly the defect in item 2.

   `tests/__init__.py` exists, so `python3 -m unittest tests.test_items`
   resolves. Without it the check would have been broken a third way.

2. **The scaffold check could not fail for the reason it existed.**
   `python3 -c 'import rooms…' && pytest --collect-only` passes just as happily
   on implemented code as on stubs. A check that cannot fail for the thing you
   care about is not a check.

   It is now expressible, because the stubs raise. The strong form is that the
   tests **error with `NotImplementedError`**:

   ```sh
   python3 -m unittest discover tests 2>&1 | grep -q NotImplementedError
   ```

   Note what that would have caught and what it would not: it fails on
   implemented code, which is what it is for, and it would *not* have caught
   the real error that evening, since `pass` and `raise NotImplementedError`
   are both unimplemented. Detecting the `pass` version needs the AST walk that
   found this, not a test run.

3. **Decide which experiment this is.** Only one option survives the correction
   above, in two shapes:
   - **Greenfield, three modules.** Leave `rooms.py` implemented and fan out
     `items` / `combat` / `parser`. Three modules maps onto `-n 3`, and the
     finished module is a worked example of the house style sitting in the same
     directory.
   - **Greenfield, four modules.** Re-stub `rooms.py` as well. Discards the one
     piece of evidence the evening produced — that a worker given one module
     implements it correctly — for no obvious gain.

   The first is better on both counts.

   **But note what greenfield collides with.** Its stated value is exercising
   the planner and parallel workers, and that is precisely the path with the
   known shared-check defect described at the end of this file: `-n 3` copies
   one `--until` to all three, so no worker can pass until all three finish.
   A greenfield fan-out today mostly re-measures that defect. Either run it
   deliberately as a test *of* the shared check — cheap, and the fixture is
   well shaped to expose it — or fix the per-task check first and have the run
   mean something.

4. **One test is wrong, and it is left wrong on purpose.**
   `test_get_all_rooms_returns_all_rooms_in_order_added` asserts `rooms[3] is
   room3` after adding three rooms, having already asserted `len(rooms) == 3`.
   `rooms.py` is correct and the test has an off-by-one.

   It is the ambiguous case the spawn prompts below have a rule for, arrived at
   by accident, and it is the sharpest thing in the fixture: editing the test
   until it passes is the cheapest way to satisfy any check, and a worker that
   does it has told you the check is not measuring what you wanted. Do not fix
   it.

## What the fixture already bought

The evening produced four filed findings without the test ever completing, and
**all four are now fixed** — which is the argument for running it again even
though it will probably fail again:

- the stray tool call landing in `reasoning` — fixed, `llm/rescue.rs`
- the TUI loop sleeping through worker completions — fixed
- the checkpoint asking for help without showing the evidence — fixed
- the checkpoint accepting only a directive, never a question — fixed

A fifth arrived from re-reading the fixture rather than running it: the account
of what the scaffold session produced was wrong, and had been carried forward
into a whole experimental option that could not exist. Measuring the thing took
one AST walk.


## The commands to run

Three separate spawns, not `-n 3`. Reason below.

```
/spawn --until "python3 -m unittest tests.test_items -q" Implement items.py so
tests/test_items.py passes. Every method body currently raises
NotImplementedError; the docstring above each one says what it should do. Do not
edit the tests. Edit items.py only.

/spawn --until "python3 -m unittest tests.test_combat -q" Implement combat.py so
tests/test_combat.py passes. Every method body currently raises
NotImplementedError; the docstring above each one says what it should do. Do not
edit the tests. Edit combat.py only.

/spawn --until "python3 -m unittest tests.test_parser -q" Implement parser.py so
tests/test_parser.py passes. Every method body currently raises
NotImplementedError; the docstring above each one says what it should do. Do not
edit the tests. Edit parser.py only.
```

Four things are deliberate. A check each worker can pass on its own. A file
fence, since workers share one directory and there is no worktree isolation
(PLAN.md M11). Nothing about pytest, because the runner question is settled and
a worker asked to convert its tests spends its turn on that instead of the job.
And **"do not edit the tests"**, which is stronger than the previous "fix the
test only if it contradicts the docstring": the modules are empty, so a test
that fails is telling the truth by construction, and the only reason to touch
one is to make a check pass without doing the work. `rooms.py` is left as the
worked example.

If a worker edits its test anyway, that is the result — not a spoiled run.

Reset between attempts with `git checkout .` in `~/Projects/mud-test`, and read
what each worker did with `git diff` — which is also the only way to catch two
workers writing the same file. The fixture is ready at `b09c039`.

**What to watch, in order.** Whether `stuck: the model returned an empty
response` still appears at all, which is the parser's first real exercise.
Whether a checkpoint fires and whether its evidence block was enough to answer
from. Whether any worker touches a test file. And whether three workers writing
three different files in one directory interfere anyway, which nothing has
measured yet.

# Open problem: a fan-out shares one check

`/spawn -n 3 --until "..."` gives **every** worker the same check. Its own usage
string says so: *"A fan-out's check runs in every worker at once, in one
directory."* `PendingFanOut.validate` is a single value carried through the
planner to all of them.

So on this fixture, worker 1 cannot pass until workers 2 and 3 have also
finished. Each spends its retries waiting on the others, and the thing worth
enforcing — *this* worker's module passes *its* tests — cannot be expressed at
all. The check is the entire differentiator, and the fan-out path is where it
degrades to a shared pass/fail on the whole directory.

Three separate spawns avoid it, at the cost of never exercising the planner,
which is the one part of the fan-out with no evidence behind it.

Not solved here, and **M11's worktree-per-worker is half the answer, not all of
it.** The two halves are worth keeping apart, because "worktrees fix it" will
read as settled later and it is not.

**What a worktree fixes, and it is the more urgent half.** Today three workers
mutate one directory while a shared check runs inside it, so the check is not
merely shared, it is *nondeterministic*: worker 1's `--until` can pass or fail
depending on what worker 2 wrote a second earlier. Every fan-out result taken so
far carries that, which is one more reason there is no trustworthy multi-worker
data yet. Isolated trees make each check evaluate a coherent state.

**What a worktree does not fix.** The check is still one command string copied
to every worker. `--until "python3 -m unittest discover"` in an isolated tree
still runs *all* the tests, so worker 1 remains gated on modules 2 and 3 it was
never given. A directory becoming per-worker does not make a command
per-worker.

So both are needed: worktrees for coherent state, and a per-task check emitted
by the planner alongside each task rather than one check copied to all. That
second half is the same shape as PLAN.md §8a's `out` key giving each worker its
own output path, and both exist for the same underlying reason — workers share
one directory and the design pretends they do not.
