import csv
import io

from money import parse_amount


def parse_line(line: str) -> dict:
    f = next(csv.reader(io.StringIO(line)))
    if len(f) != 4:
        raise ValueError(f"want 4 fields, got {len(f)}")
    return {"date": f[0], "category": f[1].strip().lower(),
            "description": f[2], "amount": parse_amount(f[3])}


def parse_file(path: str) -> list[dict]:
    with open(path) as fh:
        lines = fh.read().splitlines()
    rows = []
    for i, line in enumerate(lines):
        if i == 0 and line.lower().startswith("date,"):
            continue
        rows.append(parse_line(line))
    return rows
