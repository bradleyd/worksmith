import csv
import io

from money import parse_amount


def parse_line(line: str) -> dict:
    f = next(csv.reader(io.StringIO(line)))
    return {"date": f[0], "category": f[1].strip().lower(),
            "description": f[2], "amount": parse_amount(f[3])}
