import sys

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
