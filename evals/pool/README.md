# Pool backlogs

Hand-written task backlogs for the granularity sweep (`POOL_PLAN.md` §4). No
planner is involved — that is the point. Phase 1 asks only whether a small model
executes small tasks reliably; whether anything can *write* these lists is a
separate question, and mixing the two makes a bad number unattributable.

## The confound this format exists to remove

The obvious way to write a fine-grained backlog is to spell out every function
name, signature and edge case, and the obvious way to write a coarse one is to
gesture at the job. Run those against each other and a win for fine-grained is
unreadable: it may be the small tasks, or it may be that the fine backlog simply
*told the model more*. The eval already had to guard the same thing once —
guided beat raw on the 9B and the finding only counted because guided leaked no
information the goal did not already carry.

So: **`SPEC.md` is shared, complete, and identical for every backlog.** Every
module, signature, edge case, and the exact report layout live there. Backlog
prompts point at spec sections; they do not add requirements. The only thing
that varies across `coarse.toml`, `medium.toml` and `fine.toml` is **how the
same specified work is cut up and handed out.**

For the same reason every backlog gets per-task checks at its own granularity.
Giving fine-grained tasks checks and coarse ones none would move enforcement and
size together, which is two variables again.

## Layout

```
expenses/
  SPEC.md          the specification — shared, identical, the only source of requirements
  files/           dropped into each scratch dir: expenses.csv, check.py
  coarse.toml      3 tasks
  medium.toml      8 tasks
  fine.toml        22 tasks
  reference/       a correct solution, used only by verify.py
```

`files/check.py` is the shared end-to-end check. It drives `cli.py` as a
subprocess and compares stdout byte for byte, so it knows nothing about which
modules exist or how the work was divided — a grader that could tell the
granularities apart would not be grading the same thing three times. It also
writes CSVs of its own (no header, `$5` with no decimal part, an empty file) so
it is not only checking the fixture the model was handed.

## verify.py

```
python3 evals/pool/verify.py
```

Runs every per-task check and every end-to-end check against `reference/`. An
answer key nobody graded is how a day gets spent discovering the model was right
and the check was wrong. It also refuses a cycle or a dangling `needs`.

It cannot prove a check is *strict* enough. Nothing can; that is what the runs
are for.

## Known cost of fine granularity, recorded before the runs

`fine.toml` is mostly a chain — twenty-two tasks that repeatedly extend the same
four files, so `needs` serialises nearly all of them. Pool concurrency buys
little there and wall clock will likely be *worse* than coarse. That is expected
and is not a failure: the bet is on per-task accuracy, and the price is being
written down in advance so it cannot later be reported as a surprise.
