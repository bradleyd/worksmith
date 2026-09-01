use std::path::Path;

use crate::config::Config;
use crate::memory::{MemoryStore, short_id};

use super::{COMMANDS, Overlay, OverlayItem};

/// Active Tab-completion state (candidates for the current token).
struct Completion {
    candidates: Vec<String>,
    idx: usize,
    token_start: usize,
}

#[derive(Default)]
pub(super) struct Composer {
    pub(super) input: String,
    /// Cursor position as a char index into `input` (0..=char_count).
    pub(super) cursor: usize,
    /// Submitted-prompt history and the current navigation position.
    pub(super) history: Vec<String>,
    pub(super) history_idx: Option<usize>,
    /// The in-progress line stashed while browsing history.
    pub(super) draft: String,
    /// Tab-completion state for the composer.
    completion: Option<Completion>,
    /// The as-you-type command hint. Unlike `overlay` it is *not* modal: the
    /// composer keeps the keyboard and this just follows what is typed, the way
    /// a shell completion menu does.
    pub(super) hint: Option<Overlay>,
}

impl Composer {
    pub(super) fn byte_at(&self, char_idx: usize) -> usize {
        self.input
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(self.input.len())
    }

    pub(super) fn char_len(&self) -> usize {
        self.input.chars().count()
    }

    pub(super) fn wrapped_rows(&self, width: usize) -> (Vec<String>, usize, usize) {
        wrap_input(&self.input, width, self.cursor)
    }

    pub(super) fn render_height(&self) -> u16 {
        let lines = self.input.split('\n').count().clamp(1, MAX_INPUT_ROWS);
        (lines + 2) as u16
    }

    pub(super) fn clear_completion(&mut self) {
        self.completion = None;
    }

    pub(super) fn insert_str(&mut self, s: &str) {
        let at = self.byte_at(self.cursor);
        self.input.insert_str(at, s);
        self.cursor += s.chars().count();
        self.completion = None;
    }

    pub(super) fn paste(&mut self, text: &str) {
        self.insert_str(text);
        self.refresh_hint();
    }

    pub(super) fn insert_char(&mut self, c: char) {
        let at = self.byte_at(self.cursor);
        self.input.insert(at, c);
        self.cursor += 1;
        self.completion = None;
    }

    pub(super) fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = self.byte_at(self.cursor - 1);
        let end = self.byte_at(self.cursor);
        self.input.replace_range(start..end, "");
        self.cursor -= 1;
    }

    /// Delete the word (and preceding whitespace) before the cursor.
    pub(super) fn delete_word(&mut self) {
        let mut i = self.cursor;
        let chars: Vec<char> = self.input.chars().collect();
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        let start = self.byte_at(i);
        let end = self.byte_at(self.cursor);
        self.input.replace_range(start..end, "");
        self.cursor = i;
    }

    pub(super) fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub(super) fn move_right(&mut self) {
        if self.cursor < self.char_len() {
            self.cursor += 1;
        }
    }

    /// Move to the start / end of the current logical line.
    pub(super) fn move_home(&mut self) {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = self.cursor;
        while i > 0 && chars[i - 1] != '\n' {
            i -= 1;
        }
        self.cursor = i;
    }

    pub(super) fn move_end(&mut self) {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = self.cursor;
        while i < chars.len() && chars[i] != '\n' {
            i += 1;
        }
        self.cursor = i;
    }

    pub(super) fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.history_idx = None;
        self.completion = None;
    }

    pub(super) fn set_input(&mut self, text: String) {
        self.cursor = text.chars().count();
        self.input = text;
        self.history_idx = None;
        self.completion = None;
    }

    /// Take the composed input for submission, resetting the composer and
    /// recording history.
    pub(super) fn take_input(&mut self) -> String {
        let text = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.history_idx = None;
        self.completion = None;
        let trimmed = text.trim();
        if !trimmed.is_empty() && self.history.last().map(String::as_str) != Some(trimmed) {
            self.history.push(trimmed.to_string());
        }
        text
    }

    /// Recall the previous / next history entry into the composer.
    pub(super) fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_idx {
            None => {
                self.draft = self.input.clone();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_idx = Some(next);
        self.input = self.history[next].clone();
        self.cursor = self.char_len();
        self.completion = None;
    }

    pub(super) fn history_next(&mut self) {
        match self.history_idx {
            None => {}
            Some(i) if i + 1 < self.history.len() => {
                self.history_idx = Some(i + 1);
                self.input = self.history[i + 1].clone();
                self.cursor = self.char_len();
            }
            Some(_) => {
                // Past the newest entry -> restore the stashed draft.
                self.history_idx = None;
                self.input = std::mem::take(&mut self.draft);
                self.cursor = self.char_len();
            }
        }
        self.completion = None;
    }

    pub(super) fn complete(
        &mut self,
        cwd: &Path,
        mem: &MemoryStore,
        config: &Config,
    ) -> Option<String> {
        if let Some(c) = &mut self.completion {
            if c.candidates.len() > 1 {
                c.idx = (c.idx + 1) % c.candidates.len();
                self.input.truncate(c.token_start);
                self.input.push_str(&c.candidates[c.idx]);
                let status = completion_status(c);
                self.cursor = self.char_len();
                return Some(status);
            }
            return None;
        }

        let (start, candidates) = compute_completions(&self.input, cwd, mem, config)?;
        self.input.truncate(start);
        self.input.push_str(&candidates[0]);
        let compl = Completion { candidates, idx: 0, token_start: start };
        let status = completion_status(&compl);
        self.cursor = self.char_len();
        self.completion = Some(compl);
        Some(status)
    }

    /// Show the command list while a `/command` is being typed, and hide it
    /// once the command is complete (a space means arguments now, and the
    /// argument completions are a different thing).
    pub(super) fn refresh_hint(&mut self) {
        let typed = self.input.trim_start();
        let showing = typed.starts_with('/')
            && !typed.contains(char::is_whitespace)
            && !self.input.contains('\n');
        if !showing {
            self.hint = None;
            return;
        }
        let items: Vec<OverlayItem> = COMMANDS
            .iter()
            .filter(|(name, _)| name.starts_with(typed))
            .map(|(name, desc)| OverlayItem {
                label: (*name).to_string(),
                description: (*desc).to_string(),
            })
            .collect();
        if items.is_empty() {
            self.hint = None;
            return;
        }
        // Keep the highlighted row where it was if it is still in range, so
        // typing one more character doesn't jump the selection around.
        let selected = self.hint.as_ref().map(|h| h.selected).unwrap_or(0).min(items.len() - 1);
        let mut ov = Overlay::new("commands", items);
        ov.selected = selected;
        self.hint = Some(ov);
    }
}

/// Max visible rows for the multi-line composer before it scrolls internally.
const MAX_INPUT_ROWS: usize = 8;

fn completion_status(c: &Completion) -> String {
    if c.candidates.len() == 1 {
        return String::new();
    }
    let preview: Vec<String> = c
        .candidates
        .iter()
        .take(8)
        .map(|s| s.trim().trim_start_matches('@').to_string())
        .collect();
    format!("⇥ {}/{}  {}", c.idx + 1, c.candidates.len(), preview.join("  "))
}

/// Compute completion candidates for the current (last) token. Returns the byte
/// offset where the token starts and the replacement strings.
pub(super) fn compute_completions(
    input: &str,
    cwd: &Path,
    mem: &MemoryStore,
    config: &Config,
) -> Option<(usize, Vec<String>)> {
    let token_start = input.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
    let token = &input[token_start..];

    // @path references anywhere.
    if let Some(rest) = token.strip_prefix('@') {
        let cands: Vec<String> =
            complete_path(rest, cwd).into_iter().map(|p| format!("@{p}")).collect();
        return (!cands.is_empty()).then_some((token_start, cands));
    }

    // /command in the first-token position.
    if token_start == 0 {
        let rest = token.strip_prefix('/')?;
        let cands: Vec<String> = COMMANDS
            .iter()
            .filter(|(c, _)| c[1..].starts_with(rest))
            .map(|(c, _)| format!("{c} "))
            .collect();
        return (!cands.is_empty()).then_some((token_start, cands));
    }

    // Subcommand / argument completion for a /command.
    let tokens: Vec<&str> = input.split_whitespace().collect();
    let first = *tokens.first()?;
    if !first.starts_with('/') {
        return None;
    }
    let prev = input[..token_start].split_whitespace().count();
    let cands = arg_completions(first, prev, token, &tokens, cwd, mem, config)?;
    (!cands.is_empty()).then_some((token_start, cands))
}

/// Complete a subcommand or argument for a `/command`.
#[allow(clippy::too_many_arguments)]
fn arg_completions(
    first: &str,
    prev: usize,
    token: &str,
    tokens: &[&str],
    cwd: &Path,
    mem: &MemoryStore,
    config: &Config,
) -> Option<Vec<String>> {
    let opts: &[&str] = match first.trim_start_matches('/') {
        "agents" | "workers" if prev == 1 => {
            &["list", "show", "tail", "kill", "nudge", "drop-queued"]
        }
        "spawn" if prev == 1 => &["-n", "--each-files", "--model"],
        "knowledge" | "know" if prev == 1 => &["index", "search", "status"],
        "skill" | "skills" if prev == 1 => return Some(skill_names(token, cwd)),
        "fast" | "lucky" if prev == 1 => &["on", "off", "auto"],
        "help" | "h" if prev == 1 => &["footer"],
        "think" if prev == 1 => {
            // Servers disagree about which levels exist: OpenRouter documents
            // minimal..max, and some vLLM builds accept only xhigh/medium/low.
            &["on", "off", "auto", "minimal", "low", "medium", "high", "xhigh", "max", "2000"]
        }
        "model" if prev == 1 => {
            // Config-driven, so it cannot go stale the way a hardcoded list
            // would. `default` is offered alongside, since reverting is the
            // other thing you do with this command.
            let mut names: Vec<String> = config
                .models
                .keys()
                .filter(|k| k.starts_with(token))
                .cloned()
                .collect();
            if "default".starts_with(token) {
                names.push("default".to_string());
            }
            names.sort();
            return Some(names);
        }
        "mouse" if prev == 1 => &["on", "off"],
        "route" if prev == 1 => &["throughput", "latency", "price", "auto"],
        "trust" if prev == 1 => &["revoke"],
        "validate" if prev == 1 => &["off"],
        "memory" | "mem" => match prev {
            1 => &[
                "list", "global", "project", "search", "show", "pending", "approve",
                "extract", "mine", "forget", "add", "help",
            ],
            // Ids are UUIDs. Completing them is the difference between the
            // review loop being usable and the user retyping 36 characters from
            // a terminal they cannot even select text in.
            2 if matches!(tokens.get(1), Some(&"approve")) => {
                let mut out = memory_id_candidates(mem, token, true);
                if "all".starts_with(token) {
                    out.insert(0, "all ".to_string());
                }
                return Some(out);
            }
            2 if matches!(tokens.get(1), Some(&"forget") | Some(&"show")) => {
                return Some(memory_id_candidates(mem, token, false));
            }
            2 if tokens.get(1) == Some(&"add") => &["global", "project"],
            3 if tokens.get(1) == Some(&"add") => {
                &["decision", "constraint", "preference", "fact", "lesson"]
            }
            _ => return None,
        },
        _ => return None,
    };
    Some(opts.iter().filter(|o| o.starts_with(token)).map(|o| format!("{o} ")).collect())
}

/// Short ids matching `token`. `pending_only` narrows to proposals, which is
/// what `approve` can act on — offering ids it would reject is worse than
/// offering none.
fn memory_id_candidates(mem: &MemoryStore, token: &str, pending_only: bool) -> Vec<String> {
    let ids = if pending_only {
        mem.pending_ids()
    } else {
        mem.list(None).map(|rows| rows.into_iter().map(|r| r.id).collect())
    };
    ids.unwrap_or_default()
        .iter()
        .map(|id| short_id(id).to_string())
        .filter(|id| id.starts_with(token))
        .map(|id| format!("{id} "))
        .collect()
}

/// Installed skill names matching `token` — completion has to read the disk
/// here, since skills are discovered rather than compiled in.
fn skill_names(token: &str, cwd: &Path) -> Vec<String> {
    crate::skill::SkillCatalog::discover(cwd)
        .skills()
        .iter()
        .filter(|s| s.name.starts_with(token))
        .map(|s| format!("{} ", s.name))
        .collect()
}

/// Prefix-complete a relative file path against the filesystem. Directories get
/// a trailing `/`. Hidden entries are shown only when the prefix starts with `.`.
fn complete_path(prefix: &str, cwd: &Path) -> Vec<String> {
    let (dir_rel, file_start) = match prefix.rfind('/') {
        Some(i) => (&prefix[..=i], &prefix[i + 1..]),
        None => ("", prefix),
    };
    let dir = cwd.join(dir_rel);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(file_start) {
            continue;
        }
        if name.starts_with('.') && !file_start.starts_with('.') {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let mut p = format!("{dir_rel}{name}");
        if is_dir {
            p.push('/');
        }
        out.push(p);
    }
    out.sort();
    out.truncate(50);
    out
}

/// Hard-wrap the composer to `width`, and say where the cursor lands in the
/// wrapped text.
///
/// One function for both, because the composer did not wrap at all before this:
/// wrapping and cursor position were two calculations that could disagree, so
/// the safe move was to clip. Clipping meant a pasted `/spawn` line vanished
/// past the right edge with the cursor pinned there, typing blind.
///
/// The breaks are inserted here rather than left to ratatui's `Wrap`, which
/// breaks on word boundaries — no char-index arithmetic can predict where those
/// land, which is the disagreement the clipping avoided. Hard breaks are ugly
/// mid-word and exactly predictable, and predictable is what a cursor needs.
pub(super) fn wrap_input(input: &str, width: usize, cursor: usize) -> (Vec<String>, usize, usize) {
    let w = width.max(1);
    let mut rows: Vec<String> = vec![String::new()];
    let (mut row, mut col) = (0usize, 0usize);
    let (mut crow, mut ccol) = (0usize, 0usize);
    let mut n = 0usize;

    for (i, ch) in input.chars().enumerate() {
        // Wrap before placing the cursor, so a cursor sitting exactly on a
        // break belongs to the start of the next row and not to a column that
        // is off the edge.
        if ch != '\n' && col == w {
            rows.push(String::new());
            row += 1;
            col = 0;
        }
        if i == cursor {
            crow = row;
            ccol = col;
        }
        if ch == '\n' {
            rows.push(String::new());
            row += 1;
            col = 0;
        } else {
            rows[row].push(ch);
            col += 1;
        }
        n = i + 1;
    }
    // Cursor past the last char — the common case, since it usually trails the
    // text being typed.
    if cursor >= n {
        if col == w {
            rows.push(String::new());
            row += 1;
            col = 0;
        }
        crow = row;
        ccol = col;
    }
    (rows, crow, ccol)
}
