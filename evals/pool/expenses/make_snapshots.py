#!/usr/bin/env python3
"""Generate a per-task snapshot of the workspace for `fine.toml`.

Snapshot N is the state the workspace should be in *after* task N — so task N+1
can be run on its own, from a known-good start, instead of only as link 14 of a
22-link chain that has already died at link 8.

Each stage gives the full text of the file it changes; unchanged files carry
forward. The stages are deliberately *incomplete* in the way the task list is:
stage 5 parses commas but still demands a decimal point, because task 6 is what
adds `$5`. That is what `verify.py --snapshots` checks — a snapshot must pass its
own task's check and **fail the next one's**. A snapshot that passes the next
check is the finished solution wearing a stage's name, and would hand the model
the answer it was being asked for.

Regenerate with: python3 evals/pool/expenses/make_snapshots.py
"""
import pathlib
import shutil

HERE = pathlib.Path(__file__).resolve().parent
OUT = HERE / "snapshots"

# --- money.py -------------------------------------------------------------
M1 = '''def format_cents(n: int) -> str:
    return f"${n // 100}.{n % 100:02d}"
'''
M2 = '''def format_cents(n: int) -> str:
    return f"${n // 100:,}.{n % 100:02d}"
'''
M3 = '''def format_cents(n: int) -> str:
    sign = "-" if n < 0 else ""
    n = abs(n)
    return f"{sign}${n // 100:,}.{n % 100:02d}"
'''
_PA_PLAIN = '''

def parse_amount(s: str) -> int:
    s = s.strip()
    if not s.startswith("$"):
        raise ValueError(f"bad amount: {s!r}")
    whole, dot, frac = s[1:].partition(".")
    if not dot:
        raise ValueError(f"bad amount: {s!r}")
    return int(whole) * 100 + int(frac)
'''
M4 = M3 + _PA_PLAIN
M5 = M3 + _PA_PLAIN.replace("int(whole) * 100", 'int(whole.replace(",", "")) * 100')
M6 = M3 + '''

def parse_amount(s: str) -> int:
    s = s.strip()
    if not s.startswith("$"):
        raise ValueError(f"bad amount: {s!r}")
    whole, dot, frac = s[1:].partition(".")
    return int(whole.replace(",", "")) * 100 + (int(frac) if dot else 0)
'''
M7 = M3 + '''

def parse_amount(s: str) -> int:
    s = s.strip()
    neg = s.startswith("-")
    if neg:
        s = s[1:]
    if not s.startswith("$"):
        raise ValueError(f"bad amount: {s!r}")
    whole, dot, frac = s[1:].partition(".")
    cents = int(whole.replace(",", "")) * 100 + (int(frac) if dot else 0)
    return -cents if neg else cents
'''
M8 = '''import re

_RE = re.compile(r"^(-)?\\$(\\d{1,3}(?:,\\d{3})*|\\d+)(?:\\.(\\d{2}))?$")


def parse_amount(s: str) -> int:
    m = _RE.match(s.strip())
    if not m:
        raise ValueError(f"bad amount: {s!r}")
    sign, whole, frac = m.groups()
    cents = int(whole.replace(",", "")) * 100 + int(frac or 0)
    return -cents if sign else cents


def format_cents(n: int) -> str:
    sign = "-" if n < 0 else ""
    n = abs(n)
    return f"{sign}${n // 100:,}.{n % 100:02d}"
'''

# --- records.py -----------------------------------------------------------
# Indexing, not unpacking, on purpose: a 3-field row must raise IndexError here
# rather than the ValueError task 12 is the one to introduce.
R9 = '''import csv
import io


def parse_line(line: str) -> dict:
    f = next(csv.reader(io.StringIO(line)))
    return {"date": f[0], "category": f[1], "description": f[2], "amount": f[3]}
'''
R10 = R9.replace('"category": f[1]', '"category": f[1].strip().lower()')
R11 = '''import csv
import io

from money import parse_amount


def parse_line(line: str) -> dict:
    f = next(csv.reader(io.StringIO(line)))
    return {"date": f[0], "category": f[1].strip().lower(),
            "description": f[2], "amount": parse_amount(f[3])}
'''
_PL_STRICT = '''import csv
import io

from money import parse_amount


def parse_line(line: str) -> dict:
    f = next(csv.reader(io.StringIO(line)))
    if len(f) != 4:
        raise ValueError(f"want 4 fields, got {len(f)}")
    return {"date": f[0], "category": f[1].strip().lower(),
            "description": f[2], "amount": parse_amount(f[3])}
'''
R12 = _PL_STRICT
R13 = _PL_STRICT + '''

def parse_file(path: str) -> list[dict]:
    with open(path) as fh:
        return [parse_line(line) for line in fh.read().splitlines()]
'''
R14 = _PL_STRICT + '''

def parse_file(path: str) -> list[dict]:
    with open(path) as fh:
        lines = fh.read().splitlines()
    rows = []
    for i, line in enumerate(lines):
        if i == 0 and line.lower().startswith("date,"):
            continue
        rows.append(parse_line(line))
    return rows
'''
R15 = _PL_STRICT + '''

def parse_file(path: str) -> list[dict]:
    with open(path) as fh:
        lines = fh.read().splitlines()
    rows = []
    for i, line in enumerate(lines):
        if not line.strip():
            continue
        if i == 0 and line.lower().startswith("date,"):
            continue
        rows.append(parse_line(line))
    return rows
'''

# --- report.py ------------------------------------------------------------
P16 = '''def totals_by_category(rows) -> dict:
    totals: dict[str, int] = {}
    for r in rows:
        totals[r["category"]] = totals.get(r["category"], 0) + r["amount"]
    return totals
'''
P17 = P16 + '''

def ranked(totals) -> list[tuple[str, int]]:
    return sorted(totals.items(), key=lambda kv: -kv[1])
'''
_P_RANKED = P16 + '''

def ranked(totals) -> list[tuple[str, int]]:
    return sorted(totals.items(), key=lambda kv: (-kv[1], kv[0]))
'''
P18 = _P_RANKED
P19 = '''from money import format_cents

''' + _P_RANKED + '''

def _row(left: str, right: str) -> str:
    return f"{left:<12}{right:>10}"


def format_report(rows) -> str:
    totals = totals_by_category(rows)
    lines = [_row("CATEGORY", "TOTAL")]
    for cat, cents in ranked(totals):
        lines.append(_row(cat, format_cents(cents)))
    return "\\n".join(lines)
'''
P20 = P19.replace('''    return "\\n".join(lines)''',
                  '''    lines.append("-" * 22)
    lines.append(_row("TOTAL", format_cents(sum(totals.values()))))
    return "\\n".join(lines)''')

# --- cli.py ---------------------------------------------------------------
C21 = '''import sys

from records import parse_file
from report import format_report


def main() -> int:
    print(format_report(parse_file(sys.argv[1])))
    return 0


if __name__ == "__main__":
    sys.exit(main())
'''
C22 = '''import sys

from records import parse_file
from report import format_report


def main() -> int:
    path = sys.argv[1]
    try:
        rows = parse_file(path)
    except FileNotFoundError:
        print(f"error: no such file: {path}", file=sys.stderr)
        return 2
    print(format_report(rows))
    return 0


if __name__ == "__main__":
    sys.exit(main())
'''

# task id -> {filename: contents changed by that task}
STAGES = [
    ("fc-whole", {"money.py": M1}), ("fc-commas", {"money.py": M2}),
    ("fc-signs", {"money.py": M3}), ("pa-plain", {"money.py": M4}),
    ("pa-commas", {"money.py": M5}), ("pa-nodecimals", {"money.py": M6}),
    ("pa-negative", {"money.py": M7}), ("pa-reject", {"money.py": M8}),
    ("pl-fields", {"records.py": R9}), ("pl-category", {"records.py": R10}),
    ("pl-amount", {"records.py": R11}), ("pl-reject", {"records.py": R12}),
    ("pf-read", {"records.py": R13}), ("pf-header", {"records.py": R14}),
    ("pf-blanks", {"records.py": R15}),
    ("totals", {"report.py": P16}), ("ranked-sort", {"report.py": P17}),
    ("ranked-ties", {"report.py": P18}), ("fr-rows", {"report.py": P19}),
    ("fr-total", {"report.py": P20}),
    ("cli-print", {"cli.py": C21}), ("cli-missing", {"cli.py": C22}),
]


def main() -> None:
    if OUT.exists():
        shutil.rmtree(OUT)
    state: dict[str, str] = {}
    for i, (tid, changed) in enumerate(STAGES, 1):
        state.update(changed)
        d = OUT / f"{i:02d}-{tid}"
        d.mkdir(parents=True)
        for name, text in state.items():
            (d / name).write_text(text)
    print(f"wrote {len(STAGES)} snapshots to {OUT.relative_to(HERE.parents[2])}")


if __name__ == "__main__":
    main()
