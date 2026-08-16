# Skill: Documents (PDF, DOCX, …)

Guidance for working with documents using the built-in `doc` tool. (Bundled for
the future skill-loading system; until then, the `doc` tool's own description
carries the essentials.)

## When to use `doc` vs `read`/`bash`

- **Use `doc read`** for `.pdf`, `.docx`, `.odt`, `.rtf`, `.epub` — it returns
  clean text/markdown via the right engine. Do **not** `read` or `cat` these
  (they're binary/zip and will be garbage).
- **Use the plain `read` tool** for source code and plain text (`.md`, `.txt`,
  `.rs`, …).
- Drop to `bash` only for engine features `doc` doesn't wrap.

## Actions

- `doc read {path, pages?, format?, offset?, limit?}` — extract text. `pages`
  is a PDF range (`"1-5"` or `"3"`). `format` is `markdown` (default) or `text`
  for DOCX. `offset`/`limit` page through the extracted text by line — use them
  for large DOCX/text that would otherwise be truncated by the tool-output cap.
- `doc info {path}` — metadata (PDF: page count, size, producer; else file stat).
- `doc convert {path, out}` — convert by file extension (e.g. `report.docx` →
  `report.pdf`, `notes.md` → `notes.docx`).
- `doc create {path, out}` — build a `.docx`/`.pdf` from a markdown/text source
  (same as convert, framed as authoring).
- `doc extract {path, out}` — extract a PDF's images into directory `out`.

## Engines (installed separately)

| Job | Primary | Fallback |
|---|---|---|
| PDF → text | `pdftotext -layout` (poppler) | `mutool` (mupdf) |
| PDF info / images | `pdfinfo` / `pdfimages`,`pdftoppm` (poppler) | `mutool` |
| DOCX/ODT/… ↔ text/markdown | `pandoc` | `docx2txt` |
| anything → PDF | `soffice` (LibreOffice) | `pandoc` (needs LaTeX) |

If an engine is missing, `doc` returns an install hint. Vision-capable models
can `doc extract` a scanned PDF to images, then `read` the PNGs.

## Tips

- For a scanned/image PDF, `doc read` may return little text → `doc extract` to
  images and read those.
- Prefer `pages` on large PDFs to keep output focused and within context.
