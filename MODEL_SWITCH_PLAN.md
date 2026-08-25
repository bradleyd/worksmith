# Plan: `/model` — switch models mid-session

Let the user change which model the session runs on without restarting. The
`[models."provider/model"]` table was reserved for this: its doc says it is
where "which models `/model` offers" lives (`config.rs:31`). This plan makes
that promise real.

## 0. The core problem

`Agent` holds `client`, `model`, and the four numbers that travel with a model
as **immutable fields** (`agent.rs:173-183`). Every request builds from them:

- `self.client.stream(...)` — `agent.rs:798` (turn steps), `agent.rs:859` (`ask`).
- `model: self.model.clone()` — `agent.rs:502`, `613`, `846`.
- `self.context_limit` — the output clamp (`agent.rs:480`), the compaction
  trigger (`agent.rs:762`), the keep budget (`agent.rs:895`).

So a switch is not one field. It is a **six-value set that must move together**:
client, model name, temperature, top_p, top_k, context_limit. Half-swapping it —
new model, old context window — is worse than not switching at all.

`/route` is the precedent to copy: `route: Arc<Mutex<Option<String>>>` with
`set_route(&self, …)` (`agent.rs:265`), read per request at `agent.rs:510`.
Same shape, more fields.

## 1. `ActiveModel` — the unit that swaps

```rust
/// What one model *is*, as far as a request is concerned. Swapped as a set:
/// a new model with the old context window is a request that gets rejected.
#[derive(Clone)]
pub struct ActiveModel {
    pub client: Arc<dyn LlmClient>,
    pub model: String,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub top_k: Option<u32>,
    pub context_limit: usize,
}
```

On `Agent`, the six fields collapse into `active: Arc<Mutex<ActiveModel>>`, plus:

- `set_model(&self, ActiveModel)` — swap the cell. `&self` + interior
  mutability, so it works from the TUI (`&Arc<Agent>`) and the REPL (owned
  `Agent`) alike, exactly like `set_route`.
- `current(&self) -> ActiveModel` — read the cell (cheap clone; the client is
  an `Arc`).

`with_sampling` (`agent.rs:241`) becomes a write into the cell, keeping its
current precedence: `temperature` only overwrites when `Some`, `top_p`/`top_k`
overwrite unconditionally. That precedence is the answer to the old plan's open
question — see §7.

`thinking` is **deliberately not** in this set. It is a separate axis (`/fast`).
A model change must not silently re-enable reasoning.

### Read sites

- `run_turn` calls `current()` **once at the top** and threads the snapshot
  through the step loop, so a switch landing mid-turn cannot change the model
  out from under an in-flight step, and cannot hand the clamp a context limit
  the current prompt was not built for. `agent.rs:502-507` reads the snapshot;
  so do `480`, `613`, `762`, `798`.
- `ask` (`agent.rs:846`) and `compact` (`agent.rs:895`) call `current()`
  themselves — both are entered from outside a turn.
- `fork_with`'s `None` branch (`agent.rs:303-306`) becomes `self.current()`.
  Note it **copies the value, not the Arc** — unlike `route`, which forks share
  (`agent.rs:327`). A worker that started on model A finishes on model A;
  `/model` mid-run must not retarget a request already in flight in a worker.
  `agents.model` workers are untouched either way.

### The two things that must be reset with it

Both are missing from the first draft, and both are hard failures, not polish.

1. **`last_prompt_tokens` (`agent.rs:192`) must be zeroed.** It is the
   *provider's* count for the previous model — a different tokenizer, over a
   different system prompt. Carry a 200k-model count into a 32k model and
   `room` (`agent.rs:479`) saturates to zero, so every request asks for the
   `MIN_OUTPUT_TOKENS` floor of 256 output tokens, and the compaction trigger
   fires on the first step, forever. Reset to 0 and let `estimate_tokens` carry
   it until the new model reports its own number.

2. **A downshift can leave the session already over the new window.** Switching
   from 256k to 32k with 60k of history is legal and will fail on the next
   request. `set_model` itself must not go compact — that is a model call the
   user did not ask for. The **front-end** compares `working_tokens` against the
   new `compaction_trigger()` and, when it is over, says so and points at
   `/compact`. (Expose a small `Agent::over_context(&Session) -> bool`, or
   return the fact from `set_model`.)

## 2. `ModelOverride` must carry its settings

The draft said "reuse `ModelOverride::resolve`, do not duplicate it." Correct —
but as written `resolve` **throws away exactly what §1 needs**: it keeps
`client` + `model` and drops `ResolvedModel::settings` and `missing_key_env`
(`llm/mod.rs:432-435`).

Widen it. `ModelOverride` gains `pub settings: ModelSettings` and
`pub missing_key_env: Option<String>`. There is precisely **one** construction
site (`llm/mod.rs:434`), so this is a two-line change; the eight call sites in
`worker.rs`/`fanout.rs`/`main.rs`/`tui.rs` only read `.client`/`.model` and are
unaffected.

Then `ActiveModel::from_override(ov, &config)` builds the set the same way
startup does (`main.rs:210-228`), so a switched-to model is configured
identically to a started-on one:

- `context_limit = ov.settings.context.unwrap_or_else(|| config.context_limit())`
- `temperature = ov.settings.temperature.or(config.temperature)`
- `top_p`/`top_k` = `ov.settings.*` (unconditional — `None` clears)

**`missing_key_env` matters for the TUI.** `client_for` currently `eprintln!`s
that warning (`llm/mod.rs:391`), which in a running TUI paints garbage over the
frame. Surfacing it on `ModelOverride` lets `/model` push it as a `Kind::Notice`
instead. (`/spawn --model` at `tui.rs:1844` has the same latent bug; this fixes
it there too.)

## 3. The command

```
/model                       → list: current (marked), then each [models."…"]
                                 entry with its context and price
/model <provider/model>      → switch (session-scoped)
/model default               → revert to the config default (`config.model`)
```

- **Listing source:** `config.models` keys. If empty, show the current model
  and say the list comes from `[models."provider/model"]` — an empty list with
  no explanation reads like a broken command. Mark entries whose provider is
  not configured rather than hiding them; "why isn't it listed" is a worse
  question than "why is it greyed out".
- **Switch:** `ModelOverride::resolve(config, spec)` → `ActiveModel` →
  `agent.set_model(…)`. On failure (bad provider, unknown prefix) push the
  error and leave the model unchanged. `resolve_model` already writes good
  errors (`config.rs:504-529`); do not wrap them.
- **Session-scoped only** — do *not* write back to `config.toml`. A `/model` in
  one session changing the global default is the same "silently swaps your
  backend" surprise the `/route` comment warns about (`tui.rs:2058`).
  Persistence is a separate, explicit decision if ever wanted.
- **While a turn is running:** the switch is queued by construction — `run_turn`
  snapshots at the top, so a switch lands on the *next* turn. Say that in the
  notice ("takes effect next turn") rather than letting it look like nothing
  happened.
- Register in `COMMANDS` (`tui.rs:125-140`) and the `/help` MODEL block
  (`tui.rs:1774-1777`), next to `/fast`, `/think`, `/route`.

## 4. Record it

Add `Event::ModelChanged { from: String, to: String }` (`event.rs:14`) and emit
it through `Agent::emit` so it lands in the session JSONL and `/history`.

Three matches are exhaustive and will fail to compile until updated — that is
the feature, not a chore:

- `App::apply` (`tui.rs:859`)
- the `/history` renderer (`tui.rs:2476`)
- `main.rs:1326` area (`--mode json` / print mode)

Per-message provenance already exists: `with_trace(…, Some(model))`
(`agent.rs:613`) stamps every assistant message with the model that produced it,
so a mixed session is already readable back.

## 5. Cost accounting — fix it now, it's five lines

`app.total_in_tokens` / `total_out_tokens` are running totals *billed*
(`tui.rs:249-251`), and the footer multiplies them by one price
(`tui.rs:3420`). After a switch that number is simply wrong — old tokens priced
at the new model's rate. The draft deferred this; don't. Segment it:

```rust
/// Cost banked from models used earlier this session. `prices` only applies
/// to tokens billed since the last /model switch.
cost_prior: f64,
seg_in_tokens: u64,
seg_out_tokens: u64,
```

On switch: `cost_prior += prices.cost(seg_in, seg_out).unwrap_or(0.0)`, zero the
segment counters, replace `prices`. Footer shows
`cost_prior + prices.cost(seg…).unwrap_or(0.0)`, and keeps the existing
"show nothing at zero" rule — so a free local model still shows nothing, and a
session that spent $0.40 before switching to local still shows $0.40. Which is
the truth.

## 6. Front-ends

**TUI** (`tui.rs`, the `/route` arm at ~2058 is the model to copy). Beyond
`agent.set_model`, four pieces of `App` are stale after a switch:

- `app.model` — footer name (`tui.rs:3438`)
- `app.context_limit` — the footer's **percentage denominator**
  (`tui.rs:3406-3407`). Missed in the draft; without it the gauge reads a 32k
  model against a 256k window.
- `app.prices` — §5
- `app.last_prompt_tokens` — zero it alongside the agent's, or the footer shows
  90% of a window nothing has been sent to yet.

Then push the notice, the `missing_key_env` warning if any, and the
over-context warning if any.

**REPL** (`main.rs:596` area, alongside `/fast` / `/route`): same resolution,
`println!` instead of a notice. Its `Agent` is owned by value; `set_model(&self)`
works directly.

## 7. The open question, settled

*Does a switch to a model with no `[models."…"]` entry reset sampling, or carry
the previous model's numbers over?*

**Reset** — but not as a new rule: as the *same* rule startup already follows.
`main.rs:210-228` passes `config.temperature` as the base and lets
`resolved.settings` override it, with `top_p`/`top_k` set unconditionally from
the entry. §2's `from_override` is that expression verbatim. So a model reached
by `/model` is configured exactly as it would be if you had started on it — no
second code path, nothing to keep in sync. Inheriting Qwen's 0.6 into an unknown
model would also be the "claim rather than a fact" the `prices` comment warns
about (`tui.rs:254`).

## 8. Tests

- `agent`: build with a mock client, `set_model` to a second mock, assert the
  next request goes to the new client **and** carries the new model name,
  temperature, and context limit.
- `agent`: `set_model` zeroes `last_prompt_tokens`.
- `agent`: a switch during a turn does not change the model mid-turn (snapshot
  holds) — drive `run_turn` with a client that calls `set_model` from its first
  step and assert step two still went to the original.
- `agent`: `fork_with(None)` after a switch takes the *current* model, and a
  later switch does not retarget the fork.
- `tui`: `/model` updates `footer_string` — name, ctx denominator, cost — using
  the existing footer tests (`tui.rs:3637+`).
- `tui`: cost across a switch = old-price segment + new-price segment.
- `config`/`llm`: `ModelOverride::resolve` carries `settings` and
  `missing_key_env` through.

## 9. Order of work

1. **`ActiveModel` + the `Agent` refactor** — cell, `set_model`, `current()`,
   snapshot in `run_turn`, resets from §1. Load-bearing. `cargo test` green with
   behavior unchanged **before** anything else lands.
2. Widen `ModelOverride` (§2) — small, and everything downstream needs it.
3. `Event::ModelChanged` + the three match sites.
4. TUI command, footer state, cost segmentation.
5. REPL command.
6. Tests, then `cargo test` + `cargo clippy` clean.

Steps 1 and 2 are the real work. 3-5 are wiring.

## 10. Workers: what `/model` does to `/spawn` and `agents.model`

The precedence chain already exists (`worker.rs:319`):

```rust
let model = model.or_else(|| self.default_model.clone());
```

`--model <spec>` beats `agents.model` beats `None`, and `None` reaches
`fork_with(None)` (`agent.rs:305`) — "run what the parent runs." So `/model`
adds no rung. It changes what the bottom rung **means**:

- **`--model` and `agents.model` are pinned.** Both are explicit choices, both
  are already-resolved `ModelOverride`s, and `agents.model` is resolved once at
  startup (`tui.rs:963`) into a value `WorkerManager` holds. `/model` must not
  touch either — `agents.model` *is* the cheap-workers/smart-parent split, and
  a session switch overriding a config choice would break exactly the setup it
  exists for. No work needed here; it falls out of the design.
- **Inherit follows the session.** A bare `/spawn` after `/model` gets the model
  you just switched to. That is the whole point.

Three things break at the seam, all fixable in `worker.rs`.

### 10a. The queue splits a fan-out across two models

`spawn_in` stores `PendingTask { model: None, … }` when inheriting, and `None`
is not resolved until `start()` — which for a queued task runs from `pump()`,
minutes later. Fan out 5 with a cap of 3: w1-w3 fork on model A, the user runs
`/model`, `pump` starts w4-w5 on model B. One command, two models, and
`GroupInfo` then synthesizes them as a single answer (`tui.rs:1063`).

**Fix: bind at spawn time, not start time.** In `spawn_in`, resolve the
inherit case before the cap check:

```rust
let model = model
    .or_else(|| self.default_model.clone())
    .or_else(|| Some(self.template.current().into()));  // snapshot, once
```

Then `PendingTask.model` is always `Some`, `fork_with(None)` never fires from
the manager, and a queued worker runs the model that was current when the user
asked for it. **One command, one model** — the rule for all of §10.

This is the concrete reason `fork_with` copies the `ActiveModel` value instead
of sharing the `Arc` (§1). Sharing would make every running worker re-target on
the next `/model`, mid-request.

### 10b. The planner runs on the parent, off-thread

`plan_fanout` is `agent.ask` (`fanout.rs:271`) on a cloned parent `Arc`, spawned
off the UI task (`tui.rs:1189-1196`) so rendering keeps up. `ask` reads
`current()` per call (§1), so a `/model` typed while the planner is thinking
changes which model plans the fan-out.

Low stakes — it is one 2048-token call — but it violates the same rule, and the
fix is free: `PendingFanOut` already carries the resolved worker model
(`fanout.rs:18`). Give it the planner's snapshot too, and have `plan_fanout`
take an `ActiveModel` rather than reading the parent's live cell.

### 10c. `/agents` stops being able to say what ran

`start()` labels a worker with `model.as_ref().map(|m| m.model.clone())`
(`worker.rs:440`) → `Worker.model: Option<String>` → the `/agents` list
(`worker.rs:559`). Inherited workers are labelled `None` today, which is fine
when a session has exactly one model and the footer names it.

Once a session can hold two, "which model did w3 run on?" is a real question
with a blank answer. After 10a the override is always `Some`, so the label is
never `None` — take it from the resolved value and show it. Worth a line in the
`/agents` output for the inherited case too, not just the `--model` case.

### 10d. Synthesis follows the session — deliberately

`app.synthesize` starts a normal parent turn to combine a group's reports
(`tui.rs:1063-1073`), so it runs on whatever the session is *now*, not on
whatever the workers ran on. That is correct and worth keeping: three cheap
drafters and one smart judge is the documented pattern (`worker.rs:344`), and
`/model` makes the judge selectable mid-session — spawn on the cheap model,
switch up, let the good model do the combining.

### Tests

- A queued inherited worker started by `pump` after a `/model` runs the model
  that was current at spawn time, not at start time.
- `agents.model` and `/spawn --model` are unaffected by a `/model` switch.
- A bare `/spawn` after a switch runs the new model.
- `/agents` labels an inherited worker with its actual model.
