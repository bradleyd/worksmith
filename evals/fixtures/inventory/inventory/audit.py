"""An in-memory audit trail of stock changes, newest last."""
_LOG: list[dict] = []


def record(action: str, *, sku: str, qty: int, note: str = "") -> None:
    _LOG.append({"action": action, "sku": sku, "qty": qty, "note": note})


def history(sku: str | None = None) -> list[dict]:
    return [e for e in _LOG if sku is None or e["sku"] == sku]


def clear() -> None:
    _LOG.clear()
