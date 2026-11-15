"""A book of balanced entries, and the questions you can ask it."""
from __future__ import annotations

import datetime as dt
from dataclasses import dataclass, field

from .accounts import is_debit_normal, under
from .money import Money


@dataclass(frozen=True)
class Posting:
    account: str
    amount: Money  # debit-positive: a credit is a negative amount


@dataclass(frozen=True)
class Entry:
    date: dt.date
    memo: str
    postings: tuple[Posting, ...]

    def __post_init__(self):
        total = sum((p.amount for p in self.postings), Money())
        if total:
            raise ValueError(f"{self.date} {self.memo!r} does not balance (off by {total})")


@dataclass
class Book:
    entries: list[Entry] = field(default_factory=list)

    def add(self, entry: Entry) -> None:
        self.entries.append(entry)

    def _sum(self, path: str, keep) -> Money:
        total = Money()
        for e in self.entries:
            if not keep(e.date):
                continue
            for p in e.postings:
                if under(p.account, path):
                    total += p.amount
        return total

    def balance(self, path: str, *, through: dt.date | None = None,
                natural: bool = False) -> Money:
        """Everything posted to `path` and its subaccounts, up to and including
        `through`. Debit-positive unless `natural`, which flips credit-normal
        accounts so a healthy income account reads positive."""
        total = self._sum(path, lambda d: through is None or d <= through)
        return self._signed(path, total, natural)

    def movement(self, path: str, start: dt.date, end: dt.date, *,
                 natural: bool = False) -> Money:
        """What was posted to `path` and its subaccounts from `start` up to,
        but not including, `end`. Same sign rule as `balance`."""
        total = self._sum(path, lambda d: start <= d < end)
        return self._signed(path, total, natural)

    @staticmethod
    def _signed(path: str, total: Money, natural: bool) -> Money:
        if natural and not is_debit_normal(path):
            return -total
        return total
