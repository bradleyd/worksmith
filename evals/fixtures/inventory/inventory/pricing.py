"""Prices, in cents, with per-sku overrides for promotions."""
from .catalog import Catalog

_OVERRIDES: dict[str, int] = {}


def set_promo(sku: str, cents: int) -> None:
    if cents < 0:
        raise ValueError("a promo price cannot be negative")
    _OVERRIDES[sku] = cents


def clear_promo(sku: str) -> None:
    _OVERRIDES.pop(sku, None)


def price_of(catalog: Catalog, sku: str) -> int:
    if sku in _OVERRIDES:
        return _OVERRIDES[sku]
    return catalog.get(sku).price_cents


def price_list(catalog: Catalog) -> dict[str, int]:
    return {sku: price_of(catalog, sku) for sku in catalog.skus()}
