"""Read and write the catalog as CSV with a header row."""
import csv
from pathlib import Path

from .catalog import Catalog
from .models import Item

FIELDS = ["sku", "name", "price_cents", "qty", "tags"]


def load_catalog(path) -> Catalog:
    catalog = Catalog()
    with open(path, newline="") as f:
        for row in csv.DictReader(f):
            catalog.add(Item(
                sku=row["sku"],
                name=row["name"],
                price_cents=int(row["price_cents"]),
                qty=int(row["qty"]),
                tags=[t for t in row.get("tags", "").split("|") if t],
            ))
    return catalog


def save_catalog(catalog: Catalog, path) -> None:
    with open(Path(path), "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=FIELDS)
        w.writeheader()
        for item in catalog:
            w.writerow({"sku": item.sku, "name": item.name,
                        "price_cents": item.price_cents, "qty": item.qty,
                        "tags": "|".join(item.tags)})
