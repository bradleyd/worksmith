#!/usr/bin/env python3
"""End-to-end check for `expenses`. IDENTICAL across every backlog.

It runs `cli.py` as a subprocess and compares stdout byte for byte, so it knows
nothing about how the work was divided or which modules exist. That is the whole
point: the granularity sweep must be graded by something that cannot tell the
granularities apart.

It also writes a second CSV of its own covering shapes the sample file does not
(no header, no-decimal amounts, an empty file), because a check that only ever
sees the fixture the model was handed measures memorisation.
"""
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
FAILURES = []


def run(*args):
    return subprocess.run(
        [sys.executable, str(HERE / "cli.py"), *args],
        capture_output=True, text=True, cwd=HERE,
    )


def expect(label, got, want):
    if got != want:
        FAILURES.append(f"{label}\n  want: {want!r}\n  got:  {got!r}")


SAMPLE = """\
CATEGORY         TOTAL
transit      $1,309.56
groceries       $11.00
dining           $0.00
----------------------
TOTAL        $1,320.56
"""

r = run("expenses.csv")
expect("sample report", r.stdout, SAMPLE)
expect("sample exit code", r.returncode, 0)

# No header row, an amount with no decimal part, a blank line, a quoted
# description with a comma, and a tie broken by category name.
with tempfile.TemporaryDirectory() as d:
    other = Path(d) / "other.csv"
    other.write_text(
        '2026-02-01,Books,"Vol 1, hardback",$5\n'
        "\n"
        "2026-02-02,books,Vol 2,$5.00\n"
        "2026-02-03,Art,Pencils,$10\n"
    )
    r = run(str(other))
    expect("no-header report", r.stdout, """\
CATEGORY         TOTAL
art             $10.00
books           $10.00
----------------------
TOTAL           $20.00
""")

    empty = Path(d) / "empty.csv"
    empty.write_text("date,category,description,amount\n")
    r = run(str(empty))
    expect("empty report", r.stdout, """\
CATEGORY         TOTAL
----------------------
TOTAL            $0.00
""")

    missing = Path(d) / "nope.csv"
    r = run(str(missing))
    expect("missing-file exit code", r.returncode, 2)
    expect("missing-file stdout is empty", r.stdout, "")
    expect("missing-file stderr", r.stderr.strip(), f"error: no such file: {missing}")

if FAILURES:
    print("\n\n".join(FAILURES), file=sys.stderr)
    print(f"\n{len(FAILURES)} failed", file=sys.stderr)
    sys.exit(1)
print("ok")
