# DOCS_PLAN — Worksmith documentation

Status: **plan, not yet built.** No code in this repo. The site lives in a
separate `worksmith-docs/` repo (see §3); this file is the contract for it.

The bet: **the docs run the product.** Worksmith emits a typed event stream
(`--mode json`) and JSONL sessions, and `evals/` records real runs. So
"interactive documentation" is not a gimmick — it's a renderer for output the
tool already produces. The docs should *show* the validation loop earning its
keep, not just describe it.

## 0. Core-crate dependencies (decisions made)

The docs plan is not code in this repo, but it rests on data the binary must
expose. Review found four gaps; each gets a decision now so Phase 1–2 aren't
built on assumptions:

- **Sessions ≠ event streams.** The JSONL session file stores *messages*
  (user/assistant/tool results + reasoning traces). `Nudge`, `Validation`,
  `Compaction`, `Usage`, and `TurnComplete` are published on the event bus but
  **never written to the session file**. The Session Player's whole story —
  "the check fired, failed, the nudge, the re-plan" — lives only in a captured
  `--mode json` stream. **Decision: the player consumes captured event
  streams, not session files.** Capture is `worksmith --mode json "task" >
  run.jsonl` — the shell does it, no new verb. Session files stay as they are;
  enriching them with event entries is a core change to make only if an
  in-TUI `/replay` later needs it.
- **Worker events never reach the parent stream.** Worker activity goes to the
  worker's own bus, so a fan-out replay would show three workers appear and a
  synthesis block with nothing between. Demos #3 (fan-out) and #4 (supervisor)
  are *about* worker activity, so the parent must re-emit **worker lifecycle
  events** (`worker_started`, `worker_finished`, `worker_stopped { reason }`) —
  the supervisor's reason is the whole story of #4 and today exists only in
  the worker's bounded log. This is a small core change and a precondition for
  Phase 2, not an open question.
- **There is no binary introspection surface.** `--help` is the only thing a
  Rust binary gives you for free. Generating `config.md` and `tools.md` "from
  a tagged binary" requires the binary to expose them. **Decision: add
  `worksmith config schema --json`** (known keys serialized from the same
  `deny_unknown_fields` structs in `config.rs`) **and `worksmith tools
  list --json`** (name + description from the registry). `events.md` is
  generated from `src/event.rs` *source* (the enum is Rust, not a runtime
  surface) — the CI job checks out the tag, runs the two subcommands against
  the built binary, and extracts the enum from source. That's what "generated
  from a tag" actually means.
- **Session id is invisible on exit.** The TUI shows the id only in the
  `started new session <id>` notice (and the footer doesn't show it at all),
  so a user who quits has no way to `--resume` later without `ls`-ing
  `~/.worksmith/sessions/`. **Decision: print the session id on exit** — the
  TUI prints `session <id>` to stdout when it leaves raw mode (so
  `worksmith | tee` and scripts see it too), and the plain REPL prints it on
  `/quit`. Pair with **shell completions** (`worksmith completions
  bash|zsh|fish`; clap has `clap_complete` for this): `--resume <TAB>`
  completes session ids from `~/.worksmith/sessions/`, sorted newest first,
  with a short preview (cwd + first user message) in zsh/fish where the
  completion style allows. Completions are a small core addition (one
  subcommand, ~50 lines) and make `--resume` usable without memorizing UUIDs.

## 1. Goals & audiences

Three readers, one source. The thesis (§0 of `PLAN.md`) is the content for all
of them — the harness, not the model, does the work.

| Audience | Wants | Surface |
|---|---|---|
| **User** (adopting it) | "Is this for me? How do I run it? What does it actually do?" | Landing + Quickstart + "watch it work" |
| **Integrator** (embedding/CI) | Event-stream shape, config keys, CLI flags, exit behavior | Generated **Reference** |
| **Contributor** | Architecture, the loop, where things live | Concept guides + `AGENTS.md` (already good) |

## 2. The two kinds of content

The split that keeps the docs from rotting:

- **Hand-write the prose** — the *why*: why validate, why the loop is worth
  +34 points on a small model, why trust is a gate, why knowledge ≠ memory.
  This is the durable, high-value content. It changes when the product's
  intent changes.
- **Generate the reference** — the *what*: CLI flags, config keys, tool
  names/descriptions, the event enum. A CI job rebuilds it from a tagged
  binary: `--help` for the CLI, `config schema --json` / `tools list --json`
  for the rest (§0), the event enum from source. Same instinct as the
  unknown-key hard-error in `config.rs`: rather than silently lie, fail
  loudly. Hand-written reference *will* drift; generated reference can't.

## 3. Architecture — separate repo, docs-as-code

**A separate `worksmith-docs/` repo.** Keeps the core crate minimal (on-brand),
isolates site build tooling and deps from the Rust build, and gives the
interactive assets (recorded sessions, casts, the player bundle) a home. The
one real risk is **drift** between the binary and the reference — the
mitigation is that the reference is *generated*, never hand-typed.

```
worksmith-docs/
├── docs/                      # hand-written prose (the "why")
│   ├── index.md               # landing: thesis + one embedded live demo
│   ├── quickstart/            # install → first run → one real task
│   ├── guide/                 # the concepts, one page each:
│   │   ├── validation-loop.md
│   │   ├── workers.md         # spawn / fan-out / supervisor
│   │   ├── memory-knowledge.md
│   │   ├── trust.md
│   │   └── thinking-cost.md
│   └── reference/             # GENERATED — never hand-edited
│       ├── cli.md             # from `worksmith --help` (clap)
│       ├── config.md          # from `worksmith config schema --json`
│       ├── tools.md           # from `worksmith tools list --json`
│       └── events.md          # from the `event.rs` enum (source)
├── src/components/            # the interactive layer (framework-agnostic core)
│   ├── SessionPlayer.vue      # replays a captured --mode json stream
│   ├── EventStream.vue        # the typed event stream, visualized
│   └── TerminalReplay.vue     # xterm.js playing a .cast of the real TUI
├── data/
│   ├── streams/               # captured --mode json output — curated runs + evals
│   └── casts/                 # asciinema captures of the TUI
├── scripts/
│   ├── gen-cli.mjs            # `worksmith --help` → cli.md
│   ├── gen-config.mjs         # `worksmith config schema --json` → config.md
│   ├── gen-tools.mjs          # `worksmith tools list --json` → tools.md
│   └── gen-events.mjs         # src/event.rs enum (from source) → events.md
└── .github/workflows/         # build → GitHub Pages; gen job runs on tag
```

## 4. The interactive layer (the differentiator)

This is where "better visuals" stops being generic advice and becomes specific
to *this* product.

### A. Session Player — the crown jewel

A web component that consumes a **captured `--mode json` event stream** and
replays it: the model thinking (the `↻` spend), each `tool_call` /
`tool_result`, diffs from `edit`/`write`, the validation check firing and
*failing*, the nudge, the re-plan, a worker spinning up, the supervisor
stopping a stuck one (via the worker lifecycle events from §0 — without them
the worker half of the replay is empty).

You are not building a demo — you're building a **renderer for output the tool
already produces.** Double value: it's the docs' centerpiece *and* a
standalone session-debugging tool for the product (a `/replay` in the TUI is a
natural later use of the same component logic — at which point the session
file may need the event entries it doesn't have today, §0).

### B. Curated "watch it work" examples

Ship 3–5 recorded streams, each demonstrating one claim. These come straight
from `evals/` plus a few hand-run captures — the evals are already the content
(`run.py` already captures `--mode json` output per run):

1. **Small model, no loop** → declares done and is wrong (the baseline
   failure — evals show all 10 raw failures ended `outcome: done`, so the
   replay ends on the unverified "I'm done"; the best single demo of the
   thesis).
2. **Small model, `--until "cargo test"`** → validation fails, re-plan,
   the test passes (the +34 story). Keep the honest caveat from the evals
   alongside it: on the 27B the loop is dead weight — the quickstart inherits
   the README's "default-on for small models" framing, not a universal
   `--until` pitch.
3. **Fan-out** → one `/spawn` → three cheap workers → synthesized answer
   (the ~$0.05 newsletter story).
4. **Supervisor** → a worker gets stuck → nudge → escalate with the reason.
5. **Frontier model claims done** → the deterministic check catches the
   unverified verdict (the "reported a verdict it had not verified" result).

### C. Faithful TUI captures

For "what it *looks like*" — the four channels, the footer, the picker
overlay — asciinema `.cast` of the real TUI, played in `TerminalReplay`.
Cheaper and more honest than re-implementing the UI in the browser. Note:
ratatui needs a real PTY, so these are **manual captures** (`script -q
recording.cast worksmith`), not CI-able — the same trick the smoke tests use.

### D. Animated concept diagrams

One per guide page, for the five ideas above: the loop as a state machine,
fan-out as a tree, memory-vs-knowledge as two stores, trust as a gate, the
event bus as one stream everything subscribes to. Mermaid for the static
ones; a small canvas/SVG animation only where the *motion* teaches (the loop,
fan-out).

### E. (Horizon, optional) "Try it."

A sandboxed headless `worksmith` running a canned task, streaming live
`--mode json` into the player. The ultimate "the docs run the product," but it
needs a backend/sandbox — a stretch, not v1.

## 5. One source, two readers (dogfood)

Author the guide pages in markdown whose frontmatter doubles as **Agent
Skills** metadata — the format Worksmith already implements. Then:

- **Humans** read the full page on the site.
- **The model** gets the condensed skill version in-context (progressive
  disclosure: name+description in the prompt, body fetched on demand).

One constraint this imposes: **the skill body is not the page body.** A
2000-word guide page is not a skill — a skill body is short and imperative.
So each concept gets a small `SKILL.md` (the condensed version) plus the
guide page (the human expansion), linked both ways. The frontmatter
"doubling" only works if you accept that split; don't try to make the page
body *be* the skill body.

The loop closes: point Worksmith's own `knowledge` tool at the docs repo so
Worksmith can answer questions about its own docs. That's a one-liner in a
demo and a strong "it works on itself" signal. The existing `docs` skill is
the seed for this.

## 6. Tooling

Tension: on-brand minimalism (mdbook/Zola) vs. the interactive ambitions
(custom components, embedded terminal, players).

**Recommendation: Zola.** The value is in the prose and the player — both
framework-agnostic by design — so the SSG is the least important piece and
shouldn't buy a JS toolchain (node, vite, vue, its CI image) for a site that's
90% static markdown. Zola gets prose + GitHub Pages with zero JS deps, and the
Session Player embeds as a self-contained web component or iframe (it's a web
app consuming JSONL anyway). The plan's own escape hatch is the decision: if
the player later needs slots into the page, *that's* the moment to switch to
VitePress — not now. Don't over-invest in the SSG's features; the value is in
the content + the player, not the theme.

## 7. Phased rollout (smallest valuable slice first)

- **Phase 0 — stand up the spine.** New repo, Zola, `index` (thesis + one
  embedded demo) + Quickstart + one concept page, deploy to Pages. *Days, not
  weeks. A real home page exists.*
- **Phase 0.5 — core-crate prerequisites (from §0).** `config schema --json`,
  `tools list --json`, worker lifecycle events on the parent bus, session id
  printed on exit, `worksmith completions` (bash/zsh/fish, `--resume` tab-
  completes ids newest-first). Small, testable, and unblocks Phase 1–2.
- **Phase 1 — generated reference.** The four `gen-*.mjs` scripts + a CI job
  that rebuilds `reference/` from a tag (subcommands against the built
  binary, event enum from source). This is the "strong on documentation"
  backbone.
- **Phase 2 — Session Player + 3–5 curated streams.** The differentiator and
  the marketing. The real deliverable is the **capture + curate workflow**
  (synthetic fixtures, one stream per claim) — the player is the easy half.
- **Phase 3 — animated diagrams + dual-reader skill docs + dogfood** (knowledge
  indexes the docs).
- **Phase 4 (optional) — whitepaper PDF via the `doc` tool, i18n, versioning.**

Each phase is independently shippable and useful; none is blocked on the next
except Phase 1–2 on Phase 0.5.

## 8. What NOT to do (keep it minimal)

- **Don't** make the in-app `/help` picker the source of truth — it stays a
  complement; the site is canonical.
- **Don't** hand-write CLI/config/tool/event reference — generate it or it
  rots.
- **Don't** duplicate the README wholesale. The README is the teaser; the site
  is the depth; README links out.
- **Don't** build the "try it" sandbox in v1.
- **Don't** add a plugin/component API to the site for the sake of it. The
  interactive value is the player, not a theming system.
- **Don't** add a `worksmith record` subcommand — the shell redirect
  (`--mode json > run.jsonl`) is the capture tool, and `evals/run.py` already
  does it. A verb that copies a file adds nothing.

## 9. Open questions

- ~~Where do recorded sessions come from long-term?~~ **Decided (§0):**
  captured `--mode json` streams; the shell redirect is the capture tool, and
  `evals/run.py` already does it. The only addition worth making is a
  `--record` that also writes the *worker* streams (per the worker lifecycle
  events) — only if hand-curating a fan-out proves painful.
- **License for recorded streams.** They may contain real file contents from
  eval fixtures. Keep fixtures synthetic so the recordings are shareable.
- **Player as a product feature vs. docs-only.** If the Session Player proves
  useful, it may belong in the core crate (a `worksmith replay <stream>` that
  opens a browser, or an in-TUI `/replay` — the latter needs session files to
  carry the event entries, §0). Decide when it's built, not now.
- **Versioning.** Do we version the reference per release tag, or only
  "latest"? Start latest-only; add a versioned reference if integrators ask.
- **Completions distribution.** `worksmith completions` prints the script;
  users source it (`eval "$(worksmith completions zsh)"`). Worth a line in the
  Quickstart and a `/help` hint? Cheap either way — decide in Phase 0.5.

---

**One-sentence version:** a separate `worksmith-docs` repo where the prose is
hand-written, the reference is generated from a tagged binary (via small new
introspection subcommands), and the centerpiece is a Session Player that
replays captured `--mode json` streams — so the docs literally run the
product. The binary gets four small additions to make this true: schema/tools
dumps, worker lifecycle events, a session id on exit, and shell completions
that tab-complete `--resume`.
