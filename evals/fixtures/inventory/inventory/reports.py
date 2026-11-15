"""Plain-text reports for the end of the day."""
from .catalog import Catalog
from .pricing import price_of
from .stock import low_stock


def stock_value_cents(catalog: Catalog) -> int:
    return sum(price_of(catalog, i.sku) * i.qty for i in catalog)


def stock_report(catalog: Catalog) -> str:
    rows = [f"{'sku':10} {'qty':>5}  name"]
    for item in catalog:
        rows.append(f"{item.sku:10} {item.qty:>5}  {item.name}")
    low = low_stock(catalog)
    if low:
        rows.append("reorder: " + ", ".join(low))
    return "\n".join(rows)
