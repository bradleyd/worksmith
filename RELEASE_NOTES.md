# Worksmith v0.8.0

Most of this release came out of one long session building a POS app on Qwen
3.8 27B. It ran for two days and kept hitting the same walls: stopping every 50
steps, reasoning until it ran out of tokens, and losing the turn whenever the
laptop slept.

## Long runs no longer stop every 50 steps

A run that was writing code stopped at 50 steps and waited for someone to type
`continue`. That caught nothing. The checks that actually catch a lost model
(going in circles, 50 steps with nothing written, the token budget) were
already there.

Now `max-steps` is a progress check. A stretch of 50 steps that made real edits
inside the project gets another one, up to 4 per turn. Scratch files in `/tmp`
and edits that failed don't count. Neither does a stretch where you had to step
in: if you sent a message mid-turn, the next check stops and asks.

When it does stop, it stops loudly. The stop shows up like a checkpoint, the
terminal alerts, and an empty Enter continues.

The real bound on a turn is now a token budget of 150,000 completion tokens.
Across 758 recorded turns, the 99th percentile used 57k and the biggest one
that finished on its own used 100k, so this cuts nothing real and still stops a
runaway. Set `token-budget = 0` to turn it off.

## `/think low` now caps reasoning

On vLLM, Qwen 3.8 at `low` reasoned straight to a 6,000-token cap and returned
nothing. The effort level only adds a sentence to the prompt telling the model
to keep its thinking brief. On a hard problem the model ignores it.

`thinking_token_budget` is what the server enforces, and it held exactly at
300, 1,000 and 2,500 tokens. So each effort level now sends one too:

| Level | Budget |
| --- | --- |
| `low` | 1,024 |
| `medium` | 4,096 |
| `high` | 8,192 |
| `xhigh` | 16,384 |
| `max` | no cap |

The budget never goes above three quarters of `max-tokens`, so there's always
room left for the answer. This only applies to providers with a
`reasoning-budget-param`. The same question at `/think low` now stops reasoning
at 1,023 tokens and answers.

Qwen 3.8 only knows `low`, `medium` and `xhigh`. Ask for `high` and the server
says so.

## A laptop sleeping mid-reply no longer ends the turn

The connection drops, and the error comes back as "error decoding response
body" with "Connection reset by peer" underneath. Worksmith didn't count that as
a temporary failure. It also refused to retry once any text was on screen, so
the answer wouldn't print twice.

Now a dropped connection is retried at any point. The partial reply comes off
the screen first, and the retry waits 2, 4, 8 and 16 seconds, about 30 in all,
which is long enough for wifi to come back after the laptop wakes. Nothing was
lost before either: the partial reply was never saved, so `continue` always
worked.

## Projects stay in their own lane

The POS session was started in `~/Projects`, which holds 78 repos. That
directory had its own memory, left over from writing a novel, and the model
went off researching the book in the middle of the POS build. Two things were
wrong.

Memory got pulled into a turn on a single shared word. A 200-word brief matched
the novel's notes on "people", "things" and "use". Now a memory has to share
15% of the words of the shorter text, with common words ignored.

Launching in a folder of projects now gets a warning at startup, and the
`knowledge` tool refuses to index there. Start in the project's own folder.

## Smaller fixes

- The transcript groups by turn, with live status on each turn's row. Reasoning
  is hidden by default; Ctrl+T shows it.
- Checkpoint discussions label who said what.
- When Worksmith is stuck and asks you something, the question now comes after
  the evidence. A long tool dump used to push it off screen.
- Filing a decision outside a git repo no longer prints git's error over the
  input box.
- The terminal is restored when Worksmith is killed or panics.
- Building needs Rust 1.89 or newer, and `Cargo.toml` says so.

No configuration changes are needed. A turn can now run to 200 steps, so if
you want it tighter, set `max-steps` or `token-budget`.
