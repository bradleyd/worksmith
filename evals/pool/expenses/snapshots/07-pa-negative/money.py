def format_cents(n: int) -> str:
    sign = "-" if n < 0 else ""
    n = abs(n)
    return f"{sign}${n // 100:,}.{n % 100:02d}"


def parse_amount(s: str) -> int:
    s = s.strip()
    neg = s.startswith("-")
    if neg:
        s = s[1:]
    if not s.startswith("$"):
        raise ValueError(f"bad amount: {s!r}")
    whole, dot, frac = s[1:].partition(".")
    cents = int(whole.replace(",", "")) * 100 + (int(frac) if dot else 0)
    return -cents if neg else cents
