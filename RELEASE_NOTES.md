# Worksmith v0.8.1

This one is about what a model does with its time: how it finds things, and
what it remembers when the conversation gets squeezed. Most of it came from
going back through a month of real sessions and counting.

## Search that models will actually use

Since October, models in my sessions ran about 1,800 searches through bash
(`grep -rn`, `find`, `cat`). They used Worksmith's own `grep` tool 47 times and
the `knowledge` index 3 times. The index answered in 50 to 70 milliseconds with
the right file when it was used. It just wasn't.

Three things were in the way:

- **The `grep` tool was slow and incomplete.** It walked one directory at a
  time and gave up after 10,000 files without saying so. It now runs on
  ripgrep's walker. Over 45,000 files it takes under a second and finds every
  match, where bash `grep -rn` takes over 6 seconds and the old tool found 2 of
  8. It skips anything in `.gitignore` and takes `glob` and `ignore_case`.
- **Knowledge hits didn't say where they were.** A hit read `src/main.rs (chunk
  27)`, so the model went and grepped for it anyway. Hits now read
  `src/app/admin_login.rs:1-24`, ready to open.
- **The prompt never pointed at it.** The first line listed every tool but
  `knowledge`. Now it's listed, with a plain rule: topic words to `knowledge`
  when you don't know the file, the `grep` tool for an exact string.

The index also builds in the background when a session starts. Turn that off
with `[knowledge] auto-index = false`.

On the 9B, asking where the admin PIN was checked went from 13 tool calls
(including bash `find`) to 7, starting with `knowledge`. That is one run, not a
benchmark. Your sessions over the next few weeks are the real test.

## Compaction keeps what's left to do

When a long session fills the window, Worksmith summarises the older part and
carries on from the summary. In two long POS sessions, all ten summaries were
cut off mid-sentence. The list of files came second and was told to name every
one, so it ate the 1,024-token budget, and none of them got as far as what was
still unfinished. After every compaction the model had lost its to-do list.

Summaries now go Goal, Unfinished, Established, Locations, with what can't be
re-read from disk first and the file list capped at 15 lines. The budget grows
with the window, from 1,024 up to 3,072 tokens. A summary that comes back
without Goal and Unfinished is thrown out and the history kept. Run against the
same transcripts, the old prompt cut off on all three tried; the new one
finished all three, in 1,556 to 2,105 tokens.

The prompt also stopped assuming you're writing code. A session on a book keeps
its decisions and `chapter.md § section` locations in its own terms.

## Reasoning budgets that leave room for an answer

- A fixed `thinking = 2000` was sent as-is even when the window left only a
  few hundred tokens for the whole reply, and the reply came back all
  reasoning, no answer. Every budget is now capped at three quarters of what
  the request has room for.
- When a provider ignores the budget, Worksmith now says so, once. OpenRouter's
  qwen3.5-9b went past a 2,000-token budget on 24 of 587 calls, up to 8,329.

## Smaller fixes

- A reply cut off by the token limit no longer has tool calls fished out of
  its unfinished thinking. A 9B once brainstormed 240 calls as text, 112 of
  them `rm -f chapter.docx`, and every one got promoted.
- `.worksmith/.gitignore` keeps the search index out of commits.
- `install.sh` is gone. Build with `cargo install --path .` or use Homebrew.
- A configuration recipes page, with every example checked by `cargo test`.

No configuration changes are needed. The knowledge index is rebuilt once on
first use, which takes under a second for a typical repo.
