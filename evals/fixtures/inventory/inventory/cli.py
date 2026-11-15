"""`python3 -m inventory.cli stock.csv report|value|label <sku>`."""
import sys

from .csv_io import load_catalog
from .labels import label
from .reports import stock_report, stock_value_cents


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        print(__doc__)
        return 2
    catalog = load_catalog(argv[0])
    cmd = argv[1]
    if cmd == "report":
        print(stock_report(catalog))
    elif cmd == "value":
        print(stock_value_cents(catalog))
    elif cmd == "label" and len(argv) == 3:
        print(label(catalog, argv[2]))
    else:
        print(__doc__)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
