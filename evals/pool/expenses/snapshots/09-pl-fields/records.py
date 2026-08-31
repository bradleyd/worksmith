import csv
import io


def parse_line(line: str) -> dict:
    f = next(csv.reader(io.StringIO(line)))
    return {"date": f[0], "category": f[1], "description": f[2], "amount": f[3]}
