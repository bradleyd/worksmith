# Milestones: agree what done looks like, check it, start fresh

Status: plan. Part 2 (the reviewer) is being built first; parts 1 and 3
follow once it is measured.

## The problem, measured

Long interactive work runs as one open-ended session with no agreed finish
line and nothing checking the result except the model itself.

- Togmoro (2026-11-15 to 11-19) was one session: 28 turns, 686 tool calls,
  757k generated tokens and 8 compactions, each squeezing about 80k tokens to
  about 27k. Its 14 commits were the real milestones (onboarding, capture
  flow, recommendation engine, nightly loop), but nothing marked them.
- No validation check was set. `/validate` and `[agent] validate` exist, but
  nobody is asked for one, and one fixed command cannot say whether a screen
  looks right or a requirement was met.
- The model is its own judge. Research says that does not work: an agent
  grading its own output approves about 95% of it (*When Do Agent Loops
  Mistake Stagnation for Progress?*, arXiv 2607.25152), and a completion-time
  self-check gains little at 1.6 to 1.76 times the cost (*Rethinking
  Verification in Agent Harnesses*, arXiv 2610.34577).

What did work in that study: an **independent reviewer** that sees only the
task and the code changes, not the transcript. +5.9 to +9.5 points on
Terminal-Bench and +3.9 to +6.6 on SWE-bench, for 0 to 25% more cost.

## The loop

1. **Agree what done looks like** before substantial work starts.
2. **Check it** when the model says it is done: commands where a command can
   decide, an independent reviewer where judgement is needed, the person
   where only they can see.
3. **Close the milestone** when it passes: commit, keep a handover note, and
   start the next milestone in a fresh context.

## Part 2 (first): the reviewer

When the model ends a turn and the turn changed files, one review runs before
the turn is reported done.

- **Input:** the person's request for this turn, and the diff of everything
  the turn changed. Not the transcript: the reviewer must not inherit the
  agent's framing of its own work.
- **Diff:** a tree snapshot at turn start and at turn end, built with a
  temporary git index (the same method isolated workers use), so new files
  count, commits made mid-turn are included, and the person's own index is
  never touched. Capped to fit the reviewer's window, with the cut stated.
- **Model:** `[agents] model` when set (the cheaper, faster one), else the
  session's model. One request, thinking off, no tools.
- **Output:** a verdict, `pass` or `revise`, and at most five findings, each a
  concrete problem with a file and line: a requirement not met, a bug that
  will break, work left half done. Style and taste are out of scope.
- **What happens:** `pass` ends the turn as before. `revise` hands the
  findings to the agent, which fixes the real ones and says in one line why
  it rejects any it disagrees with, then finishes. If a validation command
  is set, it runs again after the revision.
- **Bounds:** once per turn; skipped when nothing changed; a failed reviewer
  call warns and ends the turn normally. Never blocks on the person.
- **Switch:** `[agent] review = true` and `/review on|off`. Off by default
  until measured; the default flips if the evals show a gain.
- **Recorded:** every review lands in the session as an event (verdict,
  finding count, the findings), so it can be counted later.

### How it is measured

- Evals: the short and long suites with and without review, three repeats
  each, on the 27B and the 9B. Accept on the usual rule: no split worse, at
  least one better, beyond the noise floor (about ±2 of 6, ±4 of 48).
- Cost: tokens per solved task, with and without.
- Precision, from real sessions: of the findings raised, how many the agent
  fixed against how many it rejected, and how many of the rejections the
  person sided with. A reviewer that cries wolf costs more than it saves.

## Part 1: done looks like

At the start of a substantial request, in pairing mode, the model proposes
three to five acceptance criteria as a checkpoint. The person edits or
accepts. Each criterion is one of:

- **command:** a check a command decides (`xcodebuild test`, `cargo test`).
  These become the turn's validation command, so nobody has to remember
  `/validate`.
- **review:** judged from the code by the reviewer (part 2), which then works
  from the criteria instead of the raw request.
- **person:** only the person can confirm (it looks right in the simulator).
  Listed at the end of the turn as "for you to check".

"Substantial" is decided cheaply: a request that is more than a question or a
one-line change. Erring toward not asking; a checkpoint answered by reflex is
worse than none. Criteria are stored with the session and shown by
`/milestone`.

## Part 3: closing a milestone

When a milestone's criteria pass and the work is committed, Worksmith offers
to close it:

- write a handover note in the compaction format (Goal, Unfinished,
  Established, Locations), saved under `.worksmith/milestones/`;
- start a fresh session seeded with that note.

The next milestone then starts with a clean context instead of the compressed
remains of the last three. The offer is a checkpoint, never automatic.

## Risks

- **A confidently wrong reviewer** sends the agent after problems that do not
  exist. Mitigations: findings must cite a file and line, at most five, one
  round, and the agent may reject with a reason. Measured by precision above.
- **Cost on big diffs.** Togmoro turns changed hundreds of lines. The cap
  keeps a review to one request; a diff over the cap is reviewed in part and
  says so.
- **Asking too much.** Part 1 adds a question at the start of work. It must
  stay rare and skippable.

## Not doing

- Self-review at completion (the same model checking itself): measured as
  costly and weak.
- Several review rounds per turn: cost grows, gains do not.
- A reviewer with tools or the transcript: the point is independence.
