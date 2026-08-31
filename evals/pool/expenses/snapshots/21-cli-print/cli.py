import sys

from records import parse_file
from report import format_report


def main() -> int:
    print(format_report(parse_file(sys.argv[1])))
    return 0


if __name__ == "__main__":
    sys.exit(main())
