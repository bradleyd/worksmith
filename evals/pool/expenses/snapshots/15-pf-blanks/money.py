import re

_RE = re.compile(r"^(-)?\$(\d{1,3}(?:,\d{3})*|\d+)(?:\.(\d{2}))?$")


def parse_amount(s: str) -> int:
    m = _RE.match(s.strip())
    if not m:
        raise ValueError(f"bad amount: {s!r}")
    sign, whole, frac = m.groups()
    cents = int(whole.replace(",", "")) * 100 + int(frac or 0)
    return -cents if sign else cents


def format_cents(n: int) -> str:
    sign = "-" if n < 0 else ""
    n = abs(n)
    return f"{sign}${n // 100:,}.{n % 100:02d}"
