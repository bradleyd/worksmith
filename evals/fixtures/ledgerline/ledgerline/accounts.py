"""Account paths and which side of the ledger they live on.

A path is colon-separated, `income:sales:online`. The first segment is the
root, and the root decides the account's normal side: assets and expenses grow
with debits, the rest grow with credits.
"""

ROOTS = ("assets", "liabilities", "equity", "income", "expenses")
_DEBIT_NORMAL = {"assets", "expenses"}


def root_of(path: str) -> str:
    root = path.split(":", 1)[0]
    if root not in ROOTS:
        raise ValueError(f"unknown root in {path!r}; roots are {', '.join(ROOTS)}")
    return root


def is_debit_normal(path: str) -> bool:
    return root_of(path) in _DEBIT_NORMAL


def under(path: str, parent: str) -> bool:
    """`income:sales:online` is under `income:sales` and `income`, not `inc`."""
    return path == parent or path.startswith(parent + ":")
