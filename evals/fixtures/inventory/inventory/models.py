"""The two records everything else passes around."""
from dataclasses import dataclass, field


@dataclass
class Item:
    """One stocked product. `sku` is the shop's own product code."""
    sku: str
    name: str
    price_cents: int
    qty: int = 0
    tags: list[str] = field(default_factory=list)

    def __post_init__(self):
        if self.price_cents < 0:
            raise ValueError(f"{self.sku}: price cannot be negative")
        if self.qty < 0:
            raise ValueError(f"{self.sku}: quantity cannot be negative")


@dataclass
class OrderLine:
    sku: str
    qty: int
    unit_cents: int

    @property
    def total_cents(self) -> int:
        return self.qty * self.unit_cents
