"""Shelf labels: name, price and the sku as a scannable line."""
from .catalog import Catalog
from .pricing import price_of


def label(catalog: Catalog, sku: str) -> str:
    item = catalog.get(sku)
    cents = price_of(catalog, sku)
    return f"{item.name}\n${cents // 100}.{cents % 100:02d}\n|{sku}|"


def labels_for(catalog: Catalog, skus: list[str]) -> list[str]:
    return [label(catalog, s) for s in skus]
