def totals_by_category(rows) -> dict:
    totals: dict[str, int] = {}
    for r in rows:
        totals[r["category"]] = totals.get(r["category"], 0) + r["amount"]
    return totals
