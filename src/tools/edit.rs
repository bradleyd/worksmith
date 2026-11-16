//! `edit` — exact-match string replacement, one or more disjoint edits per call.
//!
//! Semantics (the rules that make the agent reliable):
//! - `old_string` must match **exactly** (including whitespace).
//! - Without `replace_all`, the match must be **unique**: 0 matches is an
//!   error (not found), >1 is an error (ambiguous — add more context).
//! - With `replace_all: true`, every occurrence is replaced (>=1 required).
//! - Multiple edits apply in order to the in-memory buffer, then the file is
//!   written once. If any edit fails, nothing is written.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolContext, ToolOutput, resolve_path, unified_diff};

pub struct EditTool;

struct EditOp {
    old: String,
    new: String,
    replace_all: bool,
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Replace exact text in a file. Provide `old_string`/`new_string` for a \
         single edit, or `edits` for several disjoint edits in one call. Each \
         `old_string` must match uniquely unless `replace_all` is set. All \
         edits are applied atomically (file is written only if all succeed)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path (absolute or relative to cwd)." },
                "old_string": { "type": "string", "description": "Exact text to replace (single-edit form)." },
                "new_string": { "type": "string", "description": "Replacement text (single-edit form)." },
                "replace_all": { "type": "boolean", "description": "Replace all occurrences (single-edit form)." },
                "edits": {
                    "type": "array",
                    "description": "Multiple disjoint edits, applied in order.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_string": { "type": "string" },
                            "new_string": { "type": "string" },
                            "replace_all": { "type": "boolean" }
                        },
                        "required": ["old_string", "new_string"]
                    }
                }
            },
            "required": ["path"]
        })
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
            return ToolOutput::error("missing required argument: path");
        };
        let full = resolve_path(ctx, path);
        if let Some(refusal) = super::approve_write_outside_cwd(ctx, &full).await {
            return ToolOutput::error(refusal);
        }

        let ops = match collect_ops(&args) {
            Ok(ops) if ops.is_empty() => {
                return ToolOutput::error(
                    "no edits provided (need old_string/new_string or edits[])",
                );
            }
            Ok(ops) => ops,
            Err(e) => return ToolOutput::error(e),
        };

        let original = match std::fs::read_to_string(&full) {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(format!("cannot read {}: {e}", full.display())),
        };
        let mut content = original.clone();

        let mut applied = 0usize;
        for (i, op) in ops.iter().enumerate() {
            let count = content.matches(&op.old).count();
            if count == 0 {
                let mut msg = format!("edit {}: old_string not found in {}", i + 1, full.display());
                match closest_match(&content, &op.old) {
                    Some(hint) => msg.push_str(&hint),
                    None => msg.push_str(". Nothing in the file is close; read it again."),
                }
                return ToolOutput::error(msg);
            }
            if count > 1 && !op.replace_all {
                return ToolOutput::error(format!(
                    "edit {}: old_string is ambiguous ({count} matches) — add surrounding context or set replace_all",
                    i + 1
                ));
            }
            if op.replace_all {
                content = content.replace(&op.old, &op.new);
                applied += count;
            } else {
                content = content.replacen(&op.old, &op.new, 1);
                applied += 1;
            }
        }

        if let Err(e) = std::fs::write(&full, &content) {
            return ToolOutput::error(format!("cannot write {}: {e}", full.display()));
        }
        let label = format!(
            "edited {} ({} edit(s), {applied} replacement(s))",
            full.display(),
            ops.len()
        );
        ToolOutput::ok(unified_diff(&label, &original, &content))
    }
}

/// Files longer than this get no hint rather than a slow one.
const HINT_MAX_LINES: usize = 20_000;
/// Below this the "closest" lines are a guess, and a wrong guess sends the
/// model editing the wrong place.
const HINT_MIN_SCORE: f32 = 0.6;
/// Windows refined with a full comparison after the cheap pass.
const HINT_CANDIDATES: usize = 40;
const HINT_MAX_SHOWN: usize = 40;

/// Where `old` most likely went, for a not-found error. Measured: 6% of edits
/// missed, almost always because the file had changed since the model last
/// read it, and 72% of misses were followed by re-reading the whole file to
/// find a few lines. Showing those lines lets the next edit go straight in.
fn closest_match(file: &str, old: &str) -> Option<String> {
    let want: Vec<&str> = old.trim_matches('\n').lines().collect();
    let have: Vec<&str> = file.lines().collect();
    if want.is_empty() || have.is_empty() || have.len() > HINT_MAX_LINES {
        return None;
    }
    let n = want.len().min(have.len());
    let windows = have.len() - n + 1;
    let line_sim = |a: &str, b: &str| -> f32 {
        let (a, b) = (a.trim(), b.trim());
        if a == b {
            1.0
        } else {
            similar::TextDiff::from_chars(a, b).ratio()
        }
    };

    // Cheap pass: windows sharing exact lines, and windows whose first or last
    // line resembles the wanted one. Comparing every line of every window
    // would be windows × n character diffs.
    let mut cands: Vec<usize> = Vec::new();
    let mut exact: Vec<(usize, usize)> = (0..windows)
        .map(|s| {
            let hits = (0..n)
                .filter(|&k| have[s + k].trim() == want[k].trim())
                .count();
            (hits, s)
        })
        .filter(|&(hits, _)| hits > 0)
        .collect();
    exact.sort_by_key(|&(hits, _)| std::cmp::Reverse(hits));
    cands.extend(exact.iter().take(HINT_CANDIDATES).map(|&(_, s)| s));
    for k in [0, n - 1] {
        let mut by_line: Vec<(f32, usize)> = (0..windows)
            .map(|s| (line_sim(have[s + k], want[k]), s))
            .collect();
        by_line.sort_by(|a, b| b.0.total_cmp(&a.0));
        cands.extend(by_line.iter().take(HINT_CANDIDATES / 2).map(|&(_, s)| s));
    }
    cands.sort_unstable();
    cands.dedup();

    let score = |s: usize| (0..n).map(|k| line_sim(have[s + k], want[k])).sum::<f32>() / n as f32;
    let (best, start) = cands
        .into_iter()
        .map(|s| (score(s), s))
        .max_by(|a, b| a.0.total_cmp(&b.0).then(b.1.cmp(&a.1)))?;
    if best < HINT_MIN_SCORE {
        return None;
    }

    let only_space = (0..n).all(|k| have[start + k].trim() == want[k].trim());
    let mut out = format!(
        ". Closest is lines {}-{} ({:.0}% similar{}); the file has likely changed since \
         you read it. Current text:\n",
        start + 1,
        start + n,
        best * 100.0,
        if only_space {
            ", differs only in whitespace"
        } else {
            ""
        }
    );
    for (k, line) in have[start..start + n]
        .iter()
        .enumerate()
        .take(HINT_MAX_SHOWN)
    {
        out.push_str(&format!("{:>6}\t{line}\n", start + k + 1));
    }
    if n > HINT_MAX_SHOWN {
        out.push_str(&format!("  ... ({} more lines)\n", n - HINT_MAX_SHOWN));
    }
    out.push_str("Copy old_string from these lines, without the line-number prefix.");
    Some(out)
}

fn collect_ops(args: &Value) -> Result<Vec<EditOp>, String> {
    let mut ops = Vec::new();

    // Single-edit form.
    if let Some(old) = args.get("old_string").and_then(|v| v.as_str()) {
        let new = args
            .get("new_string")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let replace_all = args
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        ops.push(EditOp {
            old: old.to_string(),
            new: new.to_string(),
            replace_all,
        });
    }

    // Multi-edit form.
    if let Some(arr) = args.get("edits").and_then(|v| v.as_array()) {
        for (i, e) in arr.iter().enumerate() {
            let old = e
                .get("old_string")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("edits[{i}]: missing old_string"))?;
            let new = e.get("new_string").and_then(|v| v.as_str()).unwrap_or("");
            let replace_all = e
                .get("replace_all")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            ops.push(EditOp {
                old: old.to_string(),
                new: new.to_string(),
                replace_all,
            });
        }
    }

    Ok(ops)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> String {
        let mut f = String::new();
        for i in 1..=200 {
            f.push_str(&format!("let value_{i} = compute({i});\n"));
        }
        f.push_str("fn tie_break(refs: &[Ref]) -> Option<usize> {\n");
        f.push_str("    // Prefer the listing whose caption sits below the code.\n");
        f.push_str("    refs.iter().position(|r| r.below)\n");
        f.push_str("}\n");
        f
    }

    #[test]
    fn a_stale_old_string_gets_the_current_lines() {
        // The model remembers the comment before it was reworded.
        let old = "fn tie_break(refs: &[Ref]) -> Option<usize> {\n    // Prefer the caption below the code.\n    refs.iter().position(|r| r.below)";
        let hint = closest_match(&file(), old).expect("close enough to hint");
        assert!(hint.contains("lines 201-203"), "{hint}");
        assert!(
            hint.contains("   202\t    // Prefer the listing whose caption sits below the code."),
            "{hint}"
        );
        assert!(!hint.contains("whitespace"));
    }

    #[test]
    fn an_indentation_slip_is_named() {
        let old = "  refs.iter().position(|r| r.below)";
        let hint = closest_match(&file(), old).unwrap();
        assert!(
            hint.contains("lines 203-203") && hint.contains("whitespace"),
            "{hint}"
        );
    }

    #[test]
    fn nothing_close_gives_no_hint() {
        assert!(closest_match(&file(), "struct Kubernetes { pods: Vec<Pod> }").is_none());
        assert!(closest_match("", "anything").is_none());
        assert!(closest_match(&file(), "\n\n").is_none());
    }

    #[test]
    fn an_old_string_longer_than_the_file_still_works() {
        let old = "a\nb\nc\nd";
        let hint = closest_match("a\nb\n", old).unwrap();
        assert!(hint.contains("lines 1-2"), "{hint}");
    }

    #[test]
    fn a_big_file_answers_quickly() {
        let mut f = String::new();
        for i in 0..19_000 {
            f.push_str(&format!(
                "    let field_{i} = record.get(\"col_{i}\")?.parse::<f64>()?;\n"
            ));
        }
        let old: String = (9_000..9_030)
            .map(|i| format!("    let field_{i} = record.get(\"col_{i}\")?.parse::<f32>()?;\n"))
            .collect();
        let t = std::time::Instant::now();
        let hint = closest_match(&f, &old).unwrap();
        assert!(hint.contains("lines 9001-9030"), "{}", &hint[..120]);
        assert!(t.elapsed().as_secs_f32() < 2.0, "took {:?}", t.elapsed());
    }
}
