"""Find items by sku prefix, name words or tag."""
from .catalog import Catalog
from .models import Item


def by_sku_prefix(catalog: Catalog, prefix: str) -> list[Item]:
    return [i for i in catalog if i.sku.startswith(prefix.upper())]


def by_name(catalog: Catalog, words: str) -> list[Item]:
    wanted = words.lower().split()
    return [i for i in catalog if all(w in i.name.lower() for w in wanted)]


def by_tag(catalog: Catalog, tag: str) -> list[Item]:
    return [i for i in catalog if tag in i.tags]
