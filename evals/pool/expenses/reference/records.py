import csv
import io

from money import parse_amount


def parse_line(line: str) -> dict:
    fields = next(csv.reader(io.StringIO(line)))
    if len(fields) != 4:
        raise ValueError(f"want 4 fields, got {len(fields)}")
    date, category, description, amount = fields
    return {
        "date": date,
        "category": category.strip().lower(),
        "description": description,
        "amount": parse_amount(amount),
    }


def parse_file(path: str) -> list[dict]:
    rows = []
    with open(path) as fh:
        lines = fh.read().splitlines()
    for i, line in enumerate(lines):
        if not line.strip():
            continue
        if i == 0 and line.lower().startswith("date,"):
            continue
        rows.append(parse_line(line))
    return rows
