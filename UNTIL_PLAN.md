# Plan: `--until` should accept a real shell command

## The bug, and how it hid

`/spawn --until "cd docs && zola check"` does not work. `parse_spawn`
(`fanout.rs`) takes one whitespace-delimited token per flag with no quote
handling — its comment says so plainly: *"Flags take a single token, so no
quoting rules are needed."* So the check became the literal string `"cd`, and
the rest of the command was silently absorbed into the task text.

What the user saw, fifteen steps later, inside a worker:

```
exit code 2
bash: -c: line 0: unexpected EOF while looking for matching "
```

Which reads as *the docs failed to build*. The real answer was that the
harness had never run a check at all, in that fan-out or any earlier one.

**The flag is the problem, not the parser.** `-n` and `--each-files` are
genuinely single tokens. But `--until` is a *shell command*, and every real one
is multi-word: `cargo test`, `zola check`, `npm run lint`. A single-token
`--until` cannot express any useful check, so the flag is unusable in the TUI
today while working fine on the CLI, where the shell does the quoting.

## What runs, and with what privileges

Worth stating plainly before adding conveniences, because the answer is
"quite a lot":

`CommandValidator::validate` (`validation.rs`) runs `bash -lc <command>` in the
working directory. **No `dangerous_command` check. No approval gate.** It runs
after every turn *and* after every re-plan retry, unattended, by design — that
is the differentiator working. `-l` means it sources the login profile, so it
inherits PATH, aliases, and env.

Where the command can come from:

| source | trust | gate today |
|---|---|---|
| `--until` typed by the user (TUI or CLI) | the user's own command | none, and none needed |
| `[agent] validate` in a **project** config | someone else's repo | `trust.rs`, by content hash |
| the model | **no path exists** — there is no spawn tool, and the fan-out planner supplies task text only | n/a |

That last row is the one that matters for this change: making `--until` easier
to type adds **no new trust boundary**, because the model cannot reach it. This
is a usability fix, not a security one. It should not grow a security story it
does not need.

## The change

**1. Quote-aware flag values.** If a value opens with `"` or `'`, read to the
matching close; otherwise keep today's single-token behaviour. Applies to every
flag, but `--until` and `--model` are the ones that need it.

**2. An unterminated quote is an error at parse time.** Today it produces a
command that fails inside a worker minutes later with a shell error that looks
like the task failing. `/spawn: --until has an unterminated quote` costs
nothing and is the whole difference between a five-second fix and an autopsy.

**3. Refuse a validation command that `policy::classify` refuses.** Not a new
gate — the same regexes `bash` already uses, applied at the one other place
worksmith runs a shell. A validator cannot prompt, so refusal is the only
option, and it belongs at parse time where it can be reported. This costs one
call and closes the mistyped-`rm -rf` case for a command that runs on a loop.

**4. Say the check out loud when a fan-out starts.** `/spawn -n 3 --until X`
should print X with the task list. A check nobody sees is a check nobody
notices the absence of, which is exactly what happened here.

## The hazard this does not fix

**N workers run the check concurrently in one directory.** `worker.rs:466`
already calls this "a known hazard for a fan-out", which is why per-worker
validation is opt-in rather than inherited. Making `--until` usable makes the
hazard easier to reach.

`zola build` is the worked example: it *deletes the output directory* before
building, so three workers running it would delete each other's output
mid-flight. `zola check` does not render and is safe. That distinction is
invisible from the command string, and no heuristic will reliably tell a
read-only check from a destructive one.

So: **do not try to detect it.** Document that a fan-out check must be
read-only, note it in `/help`, and let M11's tree-per-worker be the real answer.
A wrong guess here is worse than none, because a check that is silently skipped
for safety is the same failure this plan exists to fix.

## Tests

- `--until "cd docs && zola check"` parses to the whole command.
- Single-token `--until "cargo"` still works, quoted or not.
- An unterminated quote is a parse error naming the flag, not a broken command.
- A refused command (`rm -rf /`) is rejected at parse time.
- `--model` and `--until` together, in either order, with quotes on both.
- The task text after a quoted flag value is intact — the failure here was the
  remainder of the command being silently absorbed into the task.
