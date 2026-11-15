"""Orders: lines priced at the moment they are added."""
from .catalog import Catalog
from .discounts import unit_price
from .models import OrderLine
from .pricing import price_of
from .stock import take


class Order:
    def __init__(self, catalog: Catalog):
        self.catalog = catalog
        self.lines: list[OrderLine] = []

    def add(self, sku: str, qty: int = 1) -> OrderLine:
        base = price_of(self.catalog, sku)
        line = OrderLine(sku=sku, qty=qty, unit_cents=unit_price(base, sku, qty))
        self.lines.append(line)
        return line

    def total_cents(self) -> int:
        return sum(l.total_cents for l in self.lines)

    def skus(self) -> list[str]:
        return [l.sku for l in self.lines]

    def fulfil(self) -> None:
        for line in self.lines:
            take(self.catalog, line.sku, line.qty)
