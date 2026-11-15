"""Move stock between two shops' catalogs."""
from .catalog import Catalog
from .models import Item
from .stock import receive, take


def transfer(src: Catalog, dst: Catalog, sku: str, qty: int) -> None:
    take(src, sku, qty)
    try:
        dst.get(sku)
    except KeyError:
        item = src.get(sku)
        dst.add(Item(sku=sku, name=item.name, price_cents=item.price_cents))
    receive(dst, sku, qty, note="transfer")
