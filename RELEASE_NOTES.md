# Worksmith v0.7.0

Worksmith now makes pairing decisions and tool activity easier to follow, with
expandable discussion, compact tool results, and recorded tool timings.

## Pairing that waits for your direction

`/pair on` adds collaboration guidance to model requests. At a checkpoint, you
can ask a follow-up question before choosing a direction; Worksmith answers and
keeps the decision open. End follow-up questions with `?` so they are treated as
questions rather than directions. Tool calls queued after a blocking checkpoint are
deferred so the next request can incorporate your answer. Questions are not
filed as decisions, and exhausting the four-round discussion limit stops the
turn instead of treating silence as approval.

The checkpoint's question, discussion, and answer appear in one expandable
entry, without duplicate raw tool JSON. Pairing remains guidance supported by
bounded interruption handling, rather than a guarantee that a model will spot
every important decision.

## Expandable tool activity

Tool calls and results share one entry. Successful output starts collapsed;
failures start expanded. An explicit expansion choice survives completion.
Expanded edits retain diff colors, search reveals matching hidden output, and
copying includes the full entry. This works with pairing off too.

In transcript navigation mode (`jj`, or Esc from an empty idle composer), press
Enter to expand or collapse the selected entry. Press `i` to return to typing;
composer Enter still sends your message or checkpoint answer. Ctrl+O toggles
all tool output.

## Tool timings in the terminal and JSON

Completed tool entries show measured milliseconds. `tool_result` events include
optional `elapsed_ms` in JSON output and saved sessions:

```sh
worksmith --think off --mode json "Search the web for Rust release notes"
```

This measures wall time inside tool dispatch, including approval and retry
delays. For checkpoints, it also includes follow-up model calls and time waiting
for the user. The initiating model request and result rendering are outside the
interval. Calls skipped before dispatch and older records omit the field; `0`
is valid for a call finishing in less than a millisecond. Existing sessions
remain readable without migration.

## Reliability fixes

- Bash tools and `--until` checks enable `pipefail`: an earlier failing pipeline
  stage no longer appears successful because a trailing command such as `tail`
  succeeded. Explicit recovery such as `|| true` still works. A consumer such as
  `head` can close a pipe early and cause an upstream SIGPIPE; capture the full
  check output when its exit status matters.
- Web fetching preserves article text after `<header>` elements and skipped
  scripts, and handles Unicode offsets correctly. Pages yielding no readable
  text now report an actionable error. Fetching remains static HTML extraction;
  it does not execute JavaScript.

No new configuration is required. Turn grouping, worker trees, tabs, workflows,
and separate model routing for internal helpers remain future work.
