//! `doc` — first-class document handling (PDF, DOCX, …) by shelling out to
//! well-proven CLI engines (poppler, pandoc, LibreOffice). One tool with an
//! `action`: read / info / convert / extract / create. Engines are detected on
//! use; a missing one yields a clear install hint. See PLAN.md §5.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{Tool, ToolContext, ToolOutput, resolve_path};

pub struct DocTool;

#[async_trait]
impl Tool for DocTool {
    fn name(&self) -> &str {
        "doc"
    }

    fn description(&self) -> &str {
        "Work with documents (PDF, DOCX, ODT, etc.) via proven CLI engines. \
         Prefer this over `bash`/`read` for documents — it returns clean text. \
         Actions: `read` (path[, pages, format, offset, limit]) → text/markdown \
         (use offset/limit to page through large documents); `info` (path) \
         → metadata; `convert` (path, out) → convert by extension; `extract` \
         (path, out) → extract PDF images to a directory; `create` (path, out) \
         → build a .docx/.pdf from a markdown/text source. Requires the relevant \
         engine installed (pdftotext/poppler, pandoc, or LibreOffice's soffice)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["read", "info", "convert", "extract", "create"],
                    "description": "What to do."
                },
                "path": { "type": "string", "description": "Input document (read/info/convert/extract) or source file (create)." },
                "out": { "type": "string", "description": "Output file (convert/create) or output directory (extract)." },
                "pages": { "type": "string", "description": "PDF page range, e.g. '1-5' or '3' (read/extract)." },
                "format": { "type": "string", "enum": ["markdown", "text"], "description": "read: output format for DOCX (default markdown)." },
                "offset": { "type": "integer", "description": "read: 1-based line to start from in the extracted text. Use with limit to page through large documents.", "minimum": 1 },
                "limit": { "type": "integer", "description": "read: max number of lines to return from the extracted text.", "minimum": 1 }
            },
            "required": ["action"]
        })
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let timeout = ctx.bash_timeout;
        match action {
            "read" => read(&args, ctx, timeout).await,
            "info" => info(&args, ctx, timeout).await,
            "convert" => convert(&args, ctx, timeout).await,
            "create" => convert(&args, ctx, timeout).await, // create is convert(source → out)
            "extract" => extract(&args, ctx, timeout).await,
            "" => ToolOutput::error("missing required argument: action"),
            other => ToolOutput::error(format!("unknown doc action: {other}")),
        }
    }
}

// ---- actions --------------------------------------------------------------

async fn read(args: &Value, ctx: &ToolContext, timeout: Duration) -> ToolOutput {
    let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
        return ToolOutput::error("missing required argument: path");
    };
    let full = resolve_path(ctx, path);
    if let Some(refusal) = super::approve_read_outside_cwd(ctx, &full).await {
        return ToolOutput::error(refusal);
    }
    if !full.exists() {
        return ToolOutput::error(format!("no such file: {}", full.display()));
    }
    let pages = args.get("pages").and_then(|v| v.as_str());
    let fmt = args.get("format").and_then(|v| v.as_str()).unwrap_or("markdown");
    let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(1).max(1) as usize;
    let limit = args.get("limit").and_then(|v| v.as_u64()).map(|v| v as usize);

    // Extract the full text, then page it with offset/limit so large documents
    // (which the tool-output cap would truncate) can be read in chunks.
    let text: Result<String, String> = match ext(&full).as_str() {
        "pdf" => {
            let mut pt = vec!["-layout".to_string()];
            if let Some((f, l)) = parse_pages(pages) {
                pt.extend(["-f".into(), f.to_string(), "-l".into(), l.to_string()]);
            }
            pt.push(full.display().to_string());
            pt.push("-".to_string());
            let jobs = [
                ("pdftotext", pt),
                ("mutool", vec!["draw".into(), "-F".into(), "text".into(), full.display().to_string()]),
            ];
            run_first(&jobs, &ctx.cwd, timeout).await
        }
        "docx" | "doc" | "odt" | "rtf" | "epub" => {
            let to = if fmt == "text" { "plain" } else { "gfm" };
            let jobs = [
                ("pandoc", vec![full.display().to_string(), "-t".into(), to.into()]),
                ("docx2txt", vec![full.display().to_string(), "-".into()]),
            ];
            run_first(&jobs, &ctx.cwd, timeout).await
        }
        "md" | "markdown" | "txt" | "text" | "" => {
            std::fs::read_to_string(&full).map_err(|e| format!("cannot read {}: {e}", full.display()))
        }
        other => {
            return ToolOutput::error(format!(
                "doc read doesn't handle .{other}; use the `read` tool for plain text or `bash` directly"
            ));
        }
    };

    match text {
        Ok(t) if t.trim().is_empty() => ToolOutput::ok("(no text extracted)"),
        Ok(t) => ToolOutput::ok(slice_lines(&t, offset, limit)),
        Err(e) => ToolOutput::error(e),
    }
}

/// Return lines `[offset, offset+limit)` (1-based) with a header when the slice
/// isn't the whole document, so the model knows there's more to page through.
fn slice_lines(text: &str, offset: usize, limit: Option<usize>) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let start = offset.saturating_sub(1);
    if start >= total {
        return format!("(offset {offset} is past end of document — {total} lines total)");
    }
    let end = match limit {
        Some(l) => (start + l).min(total),
        None => total,
    };
    let body = lines[start..end].join("\n");
    if start == 0 && end == total {
        body
    } else {
        format!(
            "[lines {}-{} of {} — pass offset/limit to read more]\n{}",
            start + 1,
            end,
            total,
            body
        )
    }
}

async fn info(args: &Value, ctx: &ToolContext, timeout: Duration) -> ToolOutput {
    let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
        return ToolOutput::error("missing required argument: path");
    };
    let full = resolve_path(ctx, path);
    if let Some(refusal) = super::approve_read_outside_cwd(ctx, &full).await {
        return ToolOutput::error(refusal);
    }
    if !full.exists() {
        return ToolOutput::error(format!("no such file: {}", full.display()));
    }
    if ext(&full) == "pdf" {
        let jobs = [
            ("pdfinfo", vec![full.display().to_string()]),
            ("mutool", vec!["info".into(), full.display().to_string()]),
        ];
        return out_or_err(run_first(&jobs, &ctx.cwd, timeout).await);
    }
    // Generic metadata for non-PDF.
    match std::fs::metadata(&full) {
        Ok(m) => ToolOutput::ok(format!("{}\nsize: {} bytes\ntype: .{}", full.display(), m.len(), ext(&full))),
        Err(e) => ToolOutput::error(format!("cannot stat {}: {e}", full.display())),
    }
}

async fn convert(args: &Value, ctx: &ToolContext, timeout: Duration) -> ToolOutput {
    let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
        return ToolOutput::error("missing required argument: path");
    };
    let Some(out) = args.get("out").and_then(|v| v.as_str()) else {
        return ToolOutput::error("missing required argument: out");
    };
    let full_in = resolve_path(ctx, path);
    let full_out = resolve_path(ctx, out);
    if let Some(refusal) = super::approve_read_outside_cwd(ctx, &full_in).await {
        return ToolOutput::error(refusal);
    }
    // `convert` (and `create`, which is convert with a different name) writes a
    // file at a model-chosen path. Same rule as `write` and `edit`: leaving the
    // project is a different act than editing what you were pointed at.
    if let Some(refusal) = super::approve_write_outside_cwd(ctx, &full_out).await {
        return ToolOutput::error(refusal);
    }
    if !full_in.exists() {
        return ToolOutput::error(format!("no such file: {}", full_in.display()));
    }
    if let Some(parent) = full_out.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let from = ext(&full_in);
    let to = ext(&full_out);

    let result: Result<String, String> = if from == "pdf" {
        if to == "txt" || to == "text" {
            run_first(
                &[("pdftotext", vec!["-layout".into(), full_in.display().to_string(), full_out.display().to_string()])],
                &ctx.cwd,
                timeout,
            )
            .await
        } else {
            Err("converting FROM pdf only supports text output; use `extract` for images".into())
        }
    } else if to == "pdf" {
        // LibreOffice gives best fidelity; pandoc is the fallback (needs LaTeX).
        match soffice_to_pdf(&full_in, &full_out, &ctx.cwd, timeout).await {
            Ok(s) => Ok(s),
            Err(e1) => run_first(
                &[("pandoc", vec![full_in.display().to_string(), "-o".into(), full_out.display().to_string()])],
                &ctx.cwd,
                timeout,
            )
            .await
            .map_err(|e2| format!("{e1}\n{e2}")),
        }
    } else {
        // pandoc handles the text-format matrix (md/docx/html/rtf/odt/epub/…).
        run_first(
            &[("pandoc", vec![full_in.display().to_string(), "-o".into(), full_out.display().to_string()])],
            &ctx.cwd,
            timeout,
        )
        .await
    };

    match result {
        Ok(_) if full_out.exists() => {
            ToolOutput::ok(format!("converted {} → {}", full_in.display(), full_out.display()))
        }
        Ok(_) => ToolOutput::error(format!("engine reported success but {} was not created", full_out.display())),
        Err(e) => ToolOutput::error(e),
    }
}

async fn extract(args: &Value, ctx: &ToolContext, timeout: Duration) -> ToolOutput {
    let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
        return ToolOutput::error("missing required argument: path");
    };
    let Some(out) = args.get("out").and_then(|v| v.as_str()) else {
        return ToolOutput::error("missing required argument: out (output directory)");
    };
    let full_in = resolve_path(ctx, path);
    let out_dir = resolve_path(ctx, out);
    if let Some(refusal) = super::approve_read_outside_cwd(ctx, &full_in).await {
        return ToolOutput::error(refusal);
    }
    // `extract` writes N files into a directory, so the directory is the thing
    // to ask about.
    if let Some(refusal) = super::approve_write_outside_cwd(ctx, &out_dir).await {
        return ToolOutput::error(refusal);
    }
    if !full_in.exists() {
        return ToolOutput::error(format!("no such file: {}", full_in.display()));
    }
    if ext(&full_in) != "pdf" {
        return ToolOutput::error("extract currently supports PDF input only");
    }
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        return ToolOutput::error(format!("cannot create {}: {e}", out_dir.display()));
    }

    let img_prefix = out_dir.join("img");
    let page_prefix = out_dir.join("page");
    let jobs = [
        ("pdfimages", vec!["-all".into(), full_in.display().to_string(), img_prefix.display().to_string()]),
        ("pdftoppm", vec!["-png".into(), full_in.display().to_string(), page_prefix.display().to_string()]),
    ];
    match run_first(&jobs, &ctx.cwd, timeout).await {
        Ok(_) => {
            let files = list_dir(&out_dir);
            if files.is_empty() {
                ToolOutput::ok(format!("no images found in {}", full_in.display()))
            } else {
                ToolOutput::ok(format!("extracted {} file(s) to {}:\n{}", files.len(), out_dir.display(), files.join("\n")))
            }
        }
        Err(e) => ToolOutput::error(e),
    }
}

// ---- engine plumbing ------------------------------------------------------

/// Try each (binary, args) in order; return the first success's stdout. If all
/// fail, return the joined errors (with install hints for missing engines).
async fn run_first(jobs: &[(&str, Vec<String>)], cwd: &Path, timeout: Duration) -> Result<String, String> {
    let mut errs = Vec::new();
    for (bin, args) in jobs {
        if !have(bin) {
            errs.push(format!("`{bin}` not found — install: {}", hint(bin)));
            continue;
        }
        match run_capture(bin, args, cwd, timeout).await {
            Ok((0, stdout, _)) => return Ok(stdout),
            Ok((code, _, stderr)) => errs.push(format!("`{bin}` exited {code}: {}", stderr.trim())),
            Err(e) => errs.push(e),
        }
    }
    Err(errs.join("\n"))
}

/// LibreOffice writes `<stem>.pdf` into an out dir; convert then rename to the
/// requested output path.
async fn soffice_to_pdf(full_in: &Path, full_out: &Path, cwd: &Path, timeout: Duration) -> Result<String, String> {
    if !have("soffice") {
        return Err(format!("`soffice` not found — install: {}", hint("soffice")));
    }
    let out_dir = full_out.parent().unwrap_or(Path::new("."));
    let args = vec![
        "--headless".to_string(),
        "--convert-to".into(),
        "pdf".into(),
        "--outdir".into(),
        out_dir.display().to_string(),
        full_in.display().to_string(),
    ];
    let (code, _out, err) = run_capture("soffice", &args, cwd, timeout).await?;
    if code != 0 {
        return Err(format!("`soffice` exited {code}: {}", err.trim()));
    }
    let produced = out_dir.join(format!(
        "{}.pdf",
        full_in.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
    ));
    if produced != full_out && produced.exists() {
        std::fs::rename(&produced, full_out)
            .map_err(|e| format!("converted but could not rename to {}: {e}", full_out.display()))?;
    }
    Ok(String::new())
}

async fn run_capture(bin: &str, args: &[String], cwd: &Path, timeout: Duration) -> Result<(i32, String, String), String> {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd.spawn().map_err(|e| format!("failed to run {bin}: {e}"))?;
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(o)) => Ok((
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        )),
        Ok(Err(e)) => Err(format!("{bin} error: {e}")),
        Err(_) => Err(format!("{bin} timed out after {}s", timeout.as_secs())),
    }
}

/// Is `bin` an executable on PATH?
fn have(bin: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else { return false };
    std::env::split_paths(&path).any(|dir| {
        let p = dir.join(bin);
        p.is_file()
    })
}

fn hint(bin: &str) -> &'static str {
    match bin {
        "pdftotext" | "pdfinfo" | "pdftoppm" | "pdfimages" => {
            "poppler (brew install poppler / apt install poppler-utils)"
        }
        "pandoc" => "brew install pandoc / apt install pandoc",
        "soffice" => "LibreOffice (brew install --cask libreoffice)",
        "mutool" => "mupdf (brew install mupdf)",
        "docx2txt" => "brew install docx2txt / apt install docx2txt",
        _ => "see the tool's documentation",
    }
}

fn out_or_err(r: Result<String, String>) -> ToolOutput {
    match r {
        Ok(s) if s.trim().is_empty() => ToolOutput::ok("(no text extracted)"),
        Ok(s) => ToolOutput::ok(s),
        Err(e) => ToolOutput::error(e),
    }
}

fn ext(path: &Path) -> String {
    path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

/// Parse "1-5" → (1,5), "3" → (3,3).
fn parse_pages(pages: Option<&str>) -> Option<(u32, u32)> {
    let p = pages?.trim();
    if let Some((a, b)) = p.split_once('-') {
        Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
    } else {
        let n: u32 = p.parse().ok()?;
        Some((n, n))
    }
}

fn list_dir(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}
