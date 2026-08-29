# expenses — specification

A small expense-report tool. Four modules, then a CLI.

Money is **integer cents everywhere**. Floats are not permitted to touch an
amount: `0.1 + 0.2` is why.

## §1 `money.py`

`parse_amount(s: str) -> int` — a text amount to cents.

- `"$12.50"` → `1250`
- `"$1,234.56"` → `123456` (commas are thousands separators)
- `"$5"` → `500` (no decimal part is legal)
- `"-$4.75"` → `-475` (the minus comes before the `$`)
- surrounding whitespace is stripped
- anything else raises `ValueError`, including `"$-5.00"`, `"12.5"` with one
  decimal place, `"$1.234"`, and `""`

`format_cents(n: int) -> str` — cents to text, the inverse of the above.

- `1250` → `"$12.50"`
- `123456` → `"$1,234.56"`
- `-475` → `"-$4.75"`
- `0` → `"$0.00"`

## §2 `records.py`

`parse_line(line: str) -> dict` — one CSV row to a record.

Fields are `date,category,description,amount`, in that order.

- parsed with the `csv` module, not `str.split`: a description may be quoted and
  contain commas (`"Milk, eggs"`)
- `date` and `description` are kept verbatim
- `category` is stripped and lowercased (`"GROCERIES"` and `" Groceries "` are
  the same category)
- `amount` goes through `parse_amount` and is an `int`
- any row that is not exactly four fields raises `ValueError`

`parse_file(path: str) -> list[dict]` — a file to records, in file order.

- a first line beginning `date,` (case-insensitive) is a header and is skipped
- blank lines are skipped
- a file with no records returns `[]`

## §3 `report.py`

`totals_by_category(rows) -> dict[str, int]` — net cents per category. A
category whose net is zero is still present in the result.

`ranked(totals) -> list[tuple[str, int]]` — `(category, total)` sorted by total
descending, ties broken by category name ascending.

`format_report(rows) -> str` — the whole report as text, no trailing newline.

Every line is a category left-justified to width 12 followed by an amount
right-justified to width 10 — 22 characters. In order:

1. `CATEGORY` / `TOTAL` as that header pair
2. one line per category, in `ranked` order, amounts via `format_cents`
3. exactly 22 `-` characters
4. `TOTAL` and the grand total, as that same pair

For the example file this is:

```
CATEGORY         TOTAL
transit      $1,309.56
groceries       $11.00
dining           $0.00
----------------------
TOTAL        $1,320.56
```

## §4 `cli.py`

`python3 cli.py <path>` prints `format_report(parse_file(path))` and a trailing
newline, exiting 0.

If the file does not exist, print `error: no such file: <path>` to **stderr**
and exit **2**. Nothing goes to stdout in that case.
