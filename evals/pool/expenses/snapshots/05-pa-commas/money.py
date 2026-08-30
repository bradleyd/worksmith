def format_cents(n: int) -> str:
    sign = "-" if n < 0 else ""
    n = abs(n)
    return f"{sign}${n // 100:,}.{n % 100:02d}"


def parse_amount(s: str) -> int:
    s = s.strip()
    if not s.startswith("$"):
        raise ValueError(f"bad amount: {s!r}")
    whole, dot, frac = s[1:].partition(".")
    if not dot:
        raise ValueError(f"bad amount: {s!r}")
    return int(whole.replace(",", "")) * 100 + int(frac)
