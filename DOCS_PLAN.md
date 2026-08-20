# DOCS_PLAN — Worksmith documentation

Status: **plan, not yet built.** No code in this repo. The site lives in a
separate `worksmith-docs/` repo (see §3); this file is the contract for it.

The bet: **the docs run the product.** Worksmith already emits a typed event
stream (`--mode json`) and JSONL sessions, and `evals/` records real runs.
So "interactive documentation" is not a gimmick — it's a renderer for output
the tool already produces. The docs should *show* the validation loop earning
its keep, not just describe it.

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
  binary. Same instinct as the unknown-key hard-error in `config.rs`: rather
  than silently lie, fail loudly. Hand-written reference *will* drift;
  generated reference can't.

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
│       ├── config.md          # from the config schema + example comments
│       ├── tools.md           # from the tool registry
│       └── events.md          # from the `event.rs` enum
├── src/components/            # the interactive layer (framework-agnostic core)
│   ├── SessionPlayer.vue      # replays a JSONL session: 4 channels + diffs
│   ├── EventStream.vue        # the typed event stream, visualized
│   └── TerminalReplay.vue     # xterm.js playing a .cast of the real TUI
├── data/
│   ├── sessions/              # recorded JSONL — curated runs + eval outputs
│   └── casts/                 # asciinema captures of the TUI
├── scripts/
│   ├── gen-cli.mjs            # `worksmith --help` → cli.md
│   ├── gen-config.mjs         # config schema + example comments → config.md
│   ├── gen-tools.mjs          # tool registry → tools.md
│   └── gen-events.mjs         # event.rs enum → events.md
└── .github/workflows/         # build → GitHub Pages; gen job runs on tag
```

## 4. The interactive layer (the differentiator)

This is where "better visuals" stops being generic advice and becomes specific
to *this* product.

### A. Session Player — the crown jewel

A web component that consumes the existing `--mode json` / JSONL session and
replays it: the model thinking (the `↻` spend), each `tool_call` /
`tool_result`, diffs from `edit`/`write`, the validation check firing and
*failing*, the nudge, the re-plan, a worker spinning up, the supervisor
stopping a stuck one.

You are not building a demo — you're building a **renderer for output the tool
already produces.** Double value: it's the docs' centerpiece *and* a
standalone session-debugging tool for the product (a `/replay` in the TUI is a
natural later use of the same component logic).

### B. Curated "watch it work" examples

Ship 3–5 recorded sessions, each demonstrating one claim. These come straight
from `evals/` plus a few hand-run captures — the evals are already the content:

1. **Small model, no loop** → gives up / spins (the baseline failure).
2. **Small model, `--until "cargo test"`** → nudged back, the test passes
   (the +34 story).
3. **Fan-out** → one `/spawn` → three cheap workers → synthesized answer
   (the ~$0.05 newsletter story).
4. **Supervisor** → a worker gets stuck → nudge → escalate with the reason.
5. **Frontier model claims done** → the deterministic check catches the
   unverified verdict (the "reported a verdict it had not verified" result).

### C. Faithful TUI captures

For "what it *looks like*" — the four channels, the footer, the picker
overlay — asciinema `.cast` of the real TUI, played in `TerminalReplay`.
Cheaper and more honest than re-implementing the UI in the browser.

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

The loop closes: point Worksmith's own `knowledge` tool at the docs repo so
Worksmith can answer questions about its own docs. That's a one-liner in a demo
and a strong "it works on itself" signal. The existing `docs` skill is the
seed for this.

## 6. Tooling

Tension: on-brand minimalism (mdbook/Zola) vs. the interactive ambitions
(custom components, embedded terminal, players).

**Recommendation: VitePress.** Markdown-first, fast, GitHub-Pages-trivial, and
it supports custom components/slots — which the Session Player needs.
Crucially, **the interactive components are framework-agnostic** (a web app
consuming JSONL), so the SSG is swappable later if you'd rather be on-brand
with Zola. Don't over-invest in the SSG's features; the value is in the
content + the player, not the theme.

## 7. Phased rollout (smallest valuable slice first)

- **Phase 0 — stand up the spine.** New repo, VitePress, `index` (thesis + one
  embedded demo) + Quickstart + one concept page, deploy to Pages. *Days, not
  weeks. A real home page exists.*
- **Phase 1 — generated reference.** The four `gen-*.mjs` scripts + a CI job
  that rebuilds `reference/` from a tag. This is the "strong on documentation"
  backbone, and it's mostly mechanical.
- **Phase 2 — Session Player + 3–5 curated sessions.** The differentiator and
  the marketing.
- **Phase 3 — animated diagrams + dual-reader skill docs + dogfood** (knowledge
  indexes the docs).
- **Phase 4 (optional) — whitepaper PDF via the `doc` tool, i18n, versioning.**

Each phase is independently shippable and useful; none is blocked on the next.

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

## 9. Open questions

- **Where do recorded sessions come from long-term?** A `--record` flag on the
  TUI that writes a session to `data/sessions/`? Or is `evals/` the only
  source? Leaning: a `worksmith record` subcommand that captures a session's
  JSONL for embedding — it's the same file, just copied.
- **License for recorded sessions.** They may contain real file contents from
  eval fixtures. Keep fixtures synthetic so the recordings are shareable.
- **Player as a product feature vs. docs-only.** If the Session Player proves
  useful, it may belong in the core crate (a `worksmith replay <session>` that
  opens a browser, or an in-TUI `/replay`). Decide when it's built, not now.
- **Versioning.** Do we version the reference per release tag, or only "latest"?
  Start latest-only; add a versioned reference if integrators ask.

---

**One-sentence version:** a separate `worksmith-docs` repo where the prose is
hand-written, the reference is generated from the binary, and the centerpiece
is a Session Player that replays the tool's own `--mode json` output — so the
docs literally run the product.
