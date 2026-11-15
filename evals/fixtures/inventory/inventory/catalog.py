"""The catalog: every item, looked up by sku."""
from .models import Item


class Catalog:
    def __init__(self, items=None):
        self._by_sku: dict[str, Item] = {}
        for item in items or []:
            self.add(item)

    def add(self, item: Item) -> None:
        if item.sku in self._by_sku:
            raise KeyError(f"duplicate sku {item.sku}")
        self._by_sku[item.sku] = item

    def get(self, sku: str) -> Item:
        try:
            return self._by_sku[sku]
        except KeyError:
            raise KeyError(f"no item with sku {sku}") from None

    def remove(self, sku: str) -> Item:
        return self._by_sku.pop(sku)

    def skus(self) -> list[str]:
        return sorted(self._by_sku)

    def __iter__(self):
        return iter(self._by_sku[s] for s in self.skus())

    def __len__(self) -> int:
        return len(self._by_sku)
