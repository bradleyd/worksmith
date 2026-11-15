"""Exact money. Amounts are whole minor units (cents); floats never enter."""
from __future__ import annotations

import re

_AMOUNT = re.compile(r"^(-)?(\d+)(?:\.(\d{1,2}))?$")


class Money:
    __slots__ = ("minor",)

    def __init__(self, minor: int = 0):
        if not isinstance(minor, int):
            raise TypeError("Money holds whole minor units; use Money.parse for text")
        self.minor = minor

    @classmethod
    def parse(cls, text: str) -> "Money":
        m = _AMOUNT.match(text.strip())
        if not m:
            raise ValueError(f"not an amount: {text!r}")
        sign, whole, frac = m.groups()
        minor = int(whole) * 100 + int((frac or "0").ljust(2, "0"))
        return cls(-minor if sign else minor)

    def __add__(self, other: "Money") -> "Money":
        return Money(self.minor + other.minor)

    def __sub__(self, other: "Money") -> "Money":
        return Money(self.minor - other.minor)

    def __neg__(self) -> "Money":
        return Money(-self.minor)

    def __eq__(self, other) -> bool:
        return isinstance(other, Money) and other.minor == self.minor

    def __hash__(self) -> int:
        return hash(self.minor)

    def __bool__(self) -> bool:
        return self.minor != 0

    def __str__(self) -> str:
        sign = "-" if self.minor < 0 else ""
        whole, frac = divmod(abs(self.minor), 100)
        return f"{sign}{whole}.{frac:02d}"

    def __repr__(self) -> str:
        return f"Money({self})"
