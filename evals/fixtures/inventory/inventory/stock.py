"""Stock movements. Every change goes through here so it can be audited."""
from .audit import record
from .catalog import Catalog


class OutOfStock(Exception):
    def __init__(self, sku: str, wanted: int, have: int):
        super().__init__(f"{sku}: wanted {wanted}, have {have}")
        self.sku, self.wanted, self.have = sku, wanted, have


def receive(catalog: Catalog, sku: str, qty: int, note: str = "") -> int:
    if qty <= 0:
        raise ValueError("receive a positive quantity")
    item = catalog.get(sku)
    item.qty += qty
    record("receive", sku=sku, qty=qty, note=note)
    return item.qty


def take(catalog: Catalog, sku: str, qty: int) -> int:
    item = catalog.get(sku)
    if qty > item.qty:
        raise OutOfStock(sku, qty, item.qty)
    item.qty -= qty
    record("take", sku=sku, qty=qty)
    return item.qty


def low_stock(catalog: Catalog, threshold: int = 3) -> list[str]:
    return [i.sku for i in catalog if i.qty <= threshold]
