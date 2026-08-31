from money import format_cents


def totals_by_category(rows) -> dict:
    totals: dict[str, int] = {}
    for r in rows:
        totals[r["category"]] = totals.get(r["category"], 0) + r["amount"]
    return totals


def ranked(totals) -> list[tuple[str, int]]:
    return sorted(totals.items(), key=lambda kv: (-kv[1], kv[0]))


def _row(left: str, right: str) -> str:
    return f"{left:<12}{right:>10}"


def format_report(rows) -> str:
    totals = totals_by_category(rows)
    lines = [_row("CATEGORY", "TOTAL")]
    for cat, cents in ranked(totals):
        lines.append(_row(cat, format_cents(cents)))
    lines.append("-" * 22)
    lines.append(_row("TOTAL", format_cents(sum(totals.values()))))
    return "\n".join(lines)
