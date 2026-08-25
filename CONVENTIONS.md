# How this codebase is written

You architected worksmith; this is the layer underneath that — how the code
itself is put together. Every rule below is followed by a real `file:line` and a
one-line `grep` so you can **check the claim instead of taking my word for it.**
That is the point of the document: reading 13,000 lines does not stick, and
verifying twenty claims does.

Read top to bottom. It is about an hour, and the shape sections are worth more
than the style sections.

---

## 1. The shape

Everything hangs off one loop and one bus. Nothing else is central.

```
                    ┌──────────────┐
   your keystrokes  │   main.rs    │  picks an OutputMode: Tui│Repl│Print│Json
        ──────────► │  (wiring)    │  builds Config, Session, Agent, tools
                    └──────┬───────┘
                           │ owns
              ┌────────────▼─────────────┐
              │        Agent             │  agent.rs — the loop
              │  run_turn → run_until_   │
              │  idle → call_model       │
              └───┬───────┬──────────┬───┘
                  │       │          │
        asks for  │       │ emits    │ runs
        tokens    │       │ events   │ tools
                  ▼       ▼          ▼
          ┌───────────┐ ┌──────┐ ┌──────────────┐
          │ LlmClient │ │ Event│ │ ToolRegistry │
          │  (trait)  │ │  Bus │ │  (trait Tool)│
          └───────────┘ └──┬───┘ └──────────────┘
                           │  one event, two destinations
                 ┌─────────┴─────────┐
                 ▼                   ▼
          ┌─────────────┐     ┌──────────────┐
          │  Session    │     │ subscribers  │
          │ (JSONL on   │     │ tui.rs       │
          │  disk)      │     │ json renderer│
          └─────────────┘     │ worker views │
                              └──────────────┘
```

**The one thing to take from this diagram:** the event bus is the only place
where "something happened" is expressed. The TUI is a *subscriber*, not a
participant. That is why a worker, a `--print` run, and the TUI all show the
same history — they are three readers of one stream.

```bash
grep -n "pub enum Event" -A 5 src/event.rs      # the vocabulary of the system
grep -rn "bus.subscribe()" src/                  # everyone who listens
```

---

## 2. How one turn runs

```
 user types "add a flag"
        │
        ▼
 run_turn ─────────────────────────────────────────────┐
   │  snapshot the model once (agent.rs:440)           │  outer loop:
   │  reset per-turn budgets                           │  retries after a
   ▼                                                   │  failed validation
 run_until_idle ──────────────────────────────┐        │
   │                                          │        │
   │  ┌─────────────────────────────────┐     │ inner  │
   │  │ 1. compact if over 75% of ctx   │     │ loop:  │
   │  │ 2. build ChatRequest            │     │ one    │
   │  │ 3. call_model  (3 transport     │     │ step   │
   │  │      retries, streams to bus)   │     │ per    │
   │  │ 4. no tool calls? → idle        │     │ pass   │
   │  │ 5. run each tool, append result │     │        │
   │  │ 6. stuck? → nudge               │     │        │
   │  └──────────────┬──────────────────┘     │        │
   │                 └── loop, max_steps ─────┘        │
   ▼                                                   │
 IdleReason: ModelDone │ Stuck │ Blocked │ MaxSteps │ Aborted
   │                                                   │
   ▼                                                   │
 validate (--until) ── fails ──► re-plan, retries_left─┘
   │ passes
   ▼
 TurnComplete
```

The two loops are the whole design. **Inner** = "keep going until the model
stops calling tools." **Outer** = "the model saying done is not evidence it is
done" — that is the harness's differentiator, and it lives at `agent.rs:452`.

```bash
grep -n "enum IdleReason" -A 8 src/agent.rs
grep -n "fn run_turn" -A 30 src/agent.rs
```

---

## 3. Errors: `anyhow` everywhere, no error enums

One rule, applied without exception in 113 `Result` signatures:

- Return `anyhow::Result<T>`.
- Add context at the boundary where a path or a name is known:
  `.context("building HTTP client")`, `.with_context(|| format!(...))`.
- Refuse with `bail!("...")` — 12 of these. Construct with `anyhow!` only twice.
- **No custom error types.** No `enum FooError`, no `impl std::error::Error`.

```bash
grep -rn "enum.*Error" src/          # returns nothing
grep -rn "\.context(\|with_context(" src/ | wc -l    # 34
```

> **Finding:** `thiserror = "2"` is in `Cargo.toml:31` and used nowhere.
> A dead dependency. Safe to drop.

The trade-off you accepted by doing it this way: callers cannot match on *why*
something failed, only that it did. That is fine here because almost every
failure ends the same way — shown to you, or fed back to the model as text. The
one place it bites is `parse_context_error` (`agent.rs:1193`), which has to
**scrape the provider's error string** to recover numbers, because there is no
typed error to inspect.

Error *messages* are written for the person who has to fix it, and name the fix:

```rust
// config.rs:503
"no model configured — set `model` in {path}, or pass --model
 an annotated starter config is at {example}: copy it to config.toml
 and set `model` plus the matching [providers.*] section"
```

---

## 4. Traits are seams, and there are only five

```
  trait            file                  what varies behind it
  ─────────────────────────────────────────────────────────────────────
  LlmClient        llm/mod.rs:366        which provider serves a request
  Tool             tools/mod.rs:163      what the model can do
  Approver         tools/approval.rs:31  who says yes to a risky action
  Asker            tools/approval.rs:152 who answers a pairing question
  Validator        validation.rs:18      what "done" means for a turn
```

All five are `Send + Sync` (they cross task boundaries), all five are
`#[async_trait]` where they do I/O, and each has at least one trivial
implementation for tests and headless runs — `AutoApprove`, `NoOneToAsk`,
`RefuseWhenUnattended`.

**The pattern to notice:** each trait exists because the answer differs by
*context*, not by *type*. `Approver` has three implementations because a TUI can
draw a prompt, a CI job cannot, and the eval harness wants none. That is the
test for whether something here deserves to be a trait.

```bash
grep -rn "^pub trait" src/            # exactly five
grep -rn "impl Approver for" src/     # five implementations
```

### A trait's default direction matters more than its methods

`Approver` and `Asker` look alike and fail **opposite** ways, on purpose:

```
  Approver::ask         nobody there ──► Deny      (guards an action:
                                                    silent yes is the harm)

  Asker::ask_text       nobody there ──► None      (teaches a person:
                                                    refusing to work is the harm)
```

When you add a trait here, decide its no-answer direction first.
See `tools/approval.rs:152` for the reasoning written out.

---

## 5. Settings that change mid-session

Anything you can flip with a slash command is an `Arc<...>` cell on `Agent`,
mutated through `&self`. Not `&mut self` — the TUI holds `&Arc<Agent>` while a
turn borrows the session, so `&mut` is not available.

```
   Agent
     ├── active:   Arc<Mutex<ActiveModel>>   /model   ← swapped as a SET
     ├── route:    Arc<Mutex<Option<String>>> /route
     ├── pairing:  Arc<AtomicBool>            /pair
     └── thinking: ThinkingMode(Arc<Mutex<…>>) /fast, /think
```

Two rules that are easy to get wrong:

1. **Read once per turn, not per use.** `run_turn` takes one snapshot
   (`agent.rs:440`) and threads it down. A setting that changes halfway through
   a turn must not change the model out from under an in-flight request.
2. **Decide per field whether a fork shares it.** `route` is `.clone()`d, so
   workers follow the session. `pairing` and `active` get *fresh* cells, so they
   do not. Look at `fork_with` (`agent.rs:378`) — the difference is deliberate
   and commented at each field.

```bash
grep -n "Arc<Mutex<\|Arc<Atomic" src/agent.rs
grep -n "fn fork_with" -A 40 src/agent.rs
```

---

## 6. Construction: required args positional, optional as `with_*`

```rust
Agent::new(client, registry, bus, model, …)   // 12 required args
    .with_sampling(temp, top_p, top_k)        // optional
    .with_thinking(thinking)
    .with_pairing(true)
```

Twelve `with_*` builders across the codebase. They take `mut self` and return
`Self`, except where they write into an interior-mutable cell
(`with_sampling`, `agent.rs:267`) and only need `self`.

The honest cost: `Agent::new` has twelve positional parameters and carries
`#[allow(clippy::too_many_arguments)]` — one of nine such allows in the tree.
That is a known wart, not a pattern to copy.

```bash
grep -rn "pub fn with_" src/ | wc -l
grep -rn "too_many_arguments" src/
```

---

## 7. Front ends are subscribers, and layers do not reach across

```
   tui.rs / main.rs renderer          ← may draw, may read keys
        ▲ subscribes
   EventBus                           ← the only channel between them
        ▲ emits
   agent.rs                           ← may not draw, may not read keys
        │ calls
   tools/ ──► policy.rs  (whether to ask)
          └► approval.rs (who to ask, over a channel)
```

The agent runs on its own task, so it cannot print or read a key. When a tool
needs a human, it sends a request down a channel and awaits a one-shot reply —
`ChannelApprover` (`tools/approval.rs:116`) and `ChannelAsker`
(`tools/approval.rs:187`). The front end answers from *its* task.

`policy.rs` and `approval.rs` are split for the same reason: *whether* a command
needs asking about is a fixed rule, *who* answers depends entirely on where
worksmith is running.

**The rule to carry:** if you find yourself wanting `println!` inside `agent.rs`
or `tools/`, you want an `Event` or a channel instead. A stray `eprintln!`
during a TUI session paints garbage over the frame — there is a live example at
`llm/mod.rs:393`.

---

## 8. Comments carry *why*, and name the failure

This is the strongest convention in the codebase and the one most worth
preserving. Comments almost never restate the code. They record the thing that
went wrong, usually with numbers:

```rust
// agent.rs:566 — not "clamp the output tokens"
// Never ask for more output than the window can hold. The server adds prompt
// and max_tokens and rejects the sum, so a request can fail by a single token:
// 24577 prompt + 8192 output against a 32768 model.
```

```rust
// tui.rs:2058 — why two commands are not one
// Deliberately not folded into /fast. `sort` changes *which provider* serves
// the request, and OpenRouter endpoints differ in quantization and price. A
// speed button that silently swaps your backend is a surprise, not a feature.
```

Every one of the 20 modules opens with a `//!` header saying what it is *for*
and, often, what it deliberately is *not*. `skill.rs:14` is the best example: it
states which features were left out and why adding them would cost interop.

```bash
for f in src/*.rs; do head -1 $f | grep -q '^//!' || echo "missing: $f"; done
```

---

## 9. Tests: names are sentences about behaviour

```
  a_question_nobody_answers_is_a_no
  unattended_runs_refuse_rather_than_allow
  a_useless_turn_boundary_falls_through_to_the_token_budget
  planner_reasoning_is_never_spawned_as_work
  a_spawned_worker_never_inherits_pairing
```

Not `test_approve` or `fork_works`. The name states the guarantee, so a failure
line tells you what broke without opening the file.

Layout:

```
  src/**/mod tests      unit tests, next to the code, #[cfg(test)]
  tests/*.rs            integration: the loop, workers, safety, trust
  tests/common/mod.rs   shared fixtures (isolate_home)
```

Tests assert the *reason*, not just the value — most carry a message repeating
the rule:

```rust
assert!(!worker.pairing_on(), "a running worker must not start interrupting");
```

A useful one to study: `tui.rs`'s footer-legend test asserts that every glyph
the legend explains actually appears in a fully-populated footer. It fails when
someone adds a footer segment without documenting it. That is the house style —
**tests that enforce conventions, not just correctness.**

---

## 10. Config: two files, field-level merge, and trust

```
   ~/.worksmith/config.toml        global — yours
            │  merge, field by field
            ▼  project wins per field, not per file
   <project>/.worksmith/config.toml
            │
            ▼  but first: is it trusted?
   trust.toml  ── asks once per project, remembered by CONTENT hash
```

A project config is *code*: it can set `agent.validate` (a shell command run
unattended after every turn) and add a provider whose `base-url` points
anywhere. So it is asked about once per project, and the answer is keyed to the
file's **hash** — trusting a file must not bless whatever it becomes after the
next `git pull`. `trust.rs:1-15` states this.

Every knob follows the same three-layer precedence:

```
   config default   →   CLI flag   →   runtime slash command
   [agent] pair          --pair          /pair
   thinking              --fast          /fast
```

Accessors on `Config` return concrete values with defaults baked in
(`config.rs:392` `context_limit()` → `unwrap_or(128_000)`), so callers never
handle `Option` for a setting that has a sensible default.

---

## 11. Everything that can flood the context has a cap that explains itself

```
   MAX_TOOL_RESULT_BYTES   8_000   tools/mod.rs:69
   MAX_CATALOG_CHARS       4_000   skill.rs:31
   CHECKPOINTS_PER_TURN        3   tools/mod.rs:61
   CONTEXT_RESERVE           512   agent.rs:150
   MIN_OUTPUT_TOKENS         256   agent.rs:160
```

Each constant carries a comment with the arithmetic that produced it. And when
a cap truncates, it **says so in the output** — `cap()` (`tools/mod.rs:94`)
appends a notice plus an outline of the headings that were cut, because
"truncating silently is worse than the size: the model reasons about a file as
if it had seen the end of it."

Copy that instinct: a limit the model cannot see is a limit it will violate.

---

## 12. Where things live

```
  main.rs        argument parsing, mode dispatch, wiring. No logic.
  agent.rs       the loop. Turns, steps, compaction, stuck detection.
  llm/           provider clients. mod.rs = traits + resolution,
                 openai.rs = the one implementation (openai-compat).
  tools/         one file per tool + the registry.
                 policy.rs = whether to ask, approval.rs = who to ask.
  session.rs     the JSONL transcript. Append-only.
  event.rs       the vocabulary. ~20 variants.
  config.rs      TOML load + merge + accessors.
  trust.rs       "should this project's config apply?"
  tui.rs         the terminal UI. 4,400 lines — the biggest file by far.
  worker.rs      spawned background agents + concurrency cap.
  fanout.rs      turning one /spawn into several workers.
  supervisor.rs  watching workers for idle/stuck/budget.
  memory.rs      distilled facts (small, human-wanted, SQLite).
  knowledge.rs   bulk repo text, indexed for search. Disposable.
  skill.rs       markdown instruction packs, loaded on demand.
  validation.rs  the check a turn must pass.
```

**memory vs knowledge** is the distinction most worth internalising, and it is
stated at `knowledge.rs:3`: memory is a small set of distilled decisions a human
would want kept; knowledge is bulk source material always rebuildable from the
files. One is precious, one is a cache.

---

## 13. Two documented exceptions

Both are places the rules are deliberately broken, with reasons. Knowing them
tells you the rules are enforced rather than aspirational.

1. **`agent.rs:661`** writes to the session *without* emitting to the bus. The
   rule is that both destinations move together (`Agent::emit`, `agent.rs:290`),
   but a repeated "request would not fit" warning would spam the UI, so on
   repeat it goes to the transcript only. `/history` keeps the full picture.

2. **`tools/mod.rs`'s `cap()`** rewrites a tool's own output. Layers normally
   do not touch each other's results — but a tool cannot know the context
   budget, and the registry can.

---

## What to do with this

Spend twenty minutes running the `grep` lines. Where the output surprises you,
that is a place your mental model and the code disagree, and it is worth a
question. Where it matches, you have actually learned it rather than read it.

The soundness-and-idiom review is the separate piece — this document says how
the code is *meant* to be written, not whether it lives up to it.
