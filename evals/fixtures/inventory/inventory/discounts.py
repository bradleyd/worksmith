"""Quantity discounts: buy enough of one sku and the unit price drops."""
TIERS: dict[str, list[tuple[int, int]]] = {}


def add_tier(sku: str, min_qty: int, percent_off: int) -> None:
    if not 0 < percent_off < 100:
        raise ValueError("percent_off must be between 1 and 99")
    TIERS.setdefault(sku, []).append((min_qty, percent_off))
    TIERS[sku].sort()


def unit_price(base_cents: int, sku: str, qty: int) -> int:
    off = 0
    for min_qty, pct in TIERS.get(sku, []):
        if qty >= min_qty:
            off = pct
    return base_cents * (100 - off) // 100
