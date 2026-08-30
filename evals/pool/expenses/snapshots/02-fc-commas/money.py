def format_cents(n: int) -> str:
    return f"${n // 100:,}.{n % 100:02d}"
