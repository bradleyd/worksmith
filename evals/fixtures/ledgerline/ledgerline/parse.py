"""Read a ledger file.

    2026-01-03 Coffee sale
      income:sales        -4.50
      assets:cash          4.50

An entry is a date and memo, then indented postings. Amounts are
debit-positive, so a credit is written negative. Lines starting with `;` are
comments.
"""
from __future__ import annotations

import datetime as dt
from pathlib import Path

from .book import Book, Entry, Posting
from .money import Money


def load(path: str | Path) -> Book:
    book = Book()
    date = memo = None
    postings: list[Posting] = []

    def flush():
        if date is not None:
            book.add(Entry(date, memo, tuple(postings)))

    for n, raw in enumerate(Path(path).read_text().splitlines(), 1):
        line = raw.rstrip()
        if not line or line.lstrip().startswith(";"):
            continue
        if not line[0].isspace():
            flush()
            head, _, rest = line.partition(" ")
            try:
                date = dt.date.fromisoformat(head)
            except ValueError:
                raise ValueError(f"line {n}: expected a date, got {head!r}") from None
            memo, postings = rest.strip(), []
        else:
            account, _, amount = line.strip().rpartition(" ")
            postings.append(Posting(account.strip(), Money.parse(amount)))
    flush()
    return book
