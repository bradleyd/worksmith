//! Dated session discovery. JSONL remains authoritative; no database index.
use super::{SessionEntry, sessions_dir};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::{Duration, UNIX_EPOCH},
};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Date {
    year: u32,
    month: u32,
    day: u32,
}
impl Date {
    pub fn parse(text: &str) -> Result<Self> {
        let parts: Vec<_> = text.split('-').collect();
        ensure!(
            parts
                .iter()
                .all(|part| part.bytes().all(|b| b.is_ascii_digit()))
                && parts.len() == 3
                && parts[0].len() == 4
                && parts[1].len() == 2
                && parts[2].len() == 2,
            "expected YYYY-MM-DD"
        );
        let date = Self {
            year: parts[0].parse()?,
            month: parts[1].parse()?,
            day: parts[2].parse()?,
        };
        ensure!(
            (1970..=9999).contains(&date.year) && (1..=12).contains(&date.month),
            "invalid date"
        );
        ensure!(
            (1..=month_days(date.year, date.month)).contains(&date.day),
            "invalid date"
        );
        Ok(date)
    }
    pub fn from_unix(seconds: u64) -> Self {
        // Bound corrupt legacy timestamps to the supported calendar range.
        let mut days = seconds.min(253402300799) / 86400;
        let mut year = 1970;
        while days >= u64::from(year_days(year)) {
            days -= u64::from(year_days(year));
            year += 1;
        }
        let mut month = 1;
        while days >= u64::from(month_days(year, month)) {
            days -= u64::from(month_days(year, month));
            month += 1;
        }
        Self {
            year,
            month,
            day: days as u32 + 1,
        }
    }
    pub fn directory(self) -> PathBuf {
        PathBuf::from(format!(
            "{:04}/{:02}/{:02}",
            self.year, self.month, self.day
        ))
    }
}
impl std::fmt::Display for Date {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}
fn year_days(year: u32) -> u32 {
    if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) {
        366
    } else {
        365
    }
}
fn month_days(year: u32, month: u32) -> u32 {
    match month {
        2 => {
            if year_days(year) == 366 {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DateRange {
    pub from: Option<Date>,
    pub to: Option<Date>,
}
impl DateRange {
    pub fn new(from: Option<&str>, to: Option<&str>) -> Result<Self> {
        let range = Self {
            from: from.map(Date::parse).transpose()?,
            to: to.map(Date::parse).transpose()?,
        };
        if let (Some(from), Some(to)) = (range.from, range.to) {
            ensure!(from <= to, "--from must not be after --to");
        }
        Ok(range)
    }
    fn contains(self, date: Date) -> bool {
        self.from.is_none_or(|from| date >= from) && self.to.is_none_or(|to| date <= to)
    }
}

/// Small listing metadata; recoverable from the transcript if missing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metadata {
    pub id: String,
    pub cwd: String,
    pub created: u64,
    #[serde(default)]
    pub title: String,
}
#[derive(Debug)]
pub struct Listing {
    pub metadata: Metadata,
    pub path: PathBuf,
    pub modified: std::time::SystemTime,
}

pub fn is_managed(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name == "transcript.jsonl")
        && path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
}
pub fn artifact_path(session: &Path, name: &str, legacy_extension: &str) -> PathBuf {
    if is_managed(session) {
        session.with_file_name(name)
    } else {
        session.with_extension(legacy_extension)
    }
}
pub fn write_metadata(path: &Path, metadata: &Metadata) -> Result<()> {
    if !is_managed(path) {
        return Ok(());
    }
    let target = path.with_file_name("meta.json");
    use std::io::Write;
    let temp = path.with_file_name(format!("meta-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(&serde_json::to_vec(metadata)?)?;
        std::fs::rename(&temp, target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}
pub fn metadata(path: &Path) -> Result<Metadata> {
    use std::io::{BufRead, BufReader};
    if is_managed(path)
        && path
            .with_file_name("meta.json")
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_file())
        && let Ok(bytes) = std::fs::read(path.with_file_name("meta.json"))
        && let Ok(meta) = serde_json::from_slice(&bytes)
    {
        return Ok(meta);
    }
    for line in BufReader::new(std::fs::File::open(path)?).lines() {
        let line = line?;
        if let Ok(entry) = serde_json::from_str::<SessionEntry>(&line)
            && entry.kind == "meta"
        {
            return Ok(Metadata {
                id: entry.data["id"]
                    .as_str()
                    .context("session has no id")?
                    .into(),
                cwd: entry.data["cwd"]
                    .as_str()
                    .context("session has no project")?
                    .into(),
                created: entry.ts,
                title: String::new(),
            });
        }
    }
    anyhow::bail!("session has no metadata: {}", path.display())
}

/// Traverse only the known layout; never follow symlinks or enter artifacts.
pub fn transcripts(range: DateRange) -> Result<Vec<PathBuf>> {
    let root = sessions_dir()?;
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&root)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry.path().extension().is_some_and(|ext| ext == "jsonl")
            && (range.from.is_none() && range.to.is_none()
                || metadata(&entry.path())
                    .is_ok_and(|meta| range.contains(Date::from_unix(meta.created))))
        {
            out.push(entry.path());
        }
    }
    for year in directories(&root, 4)? {
        for month in directories(&year, 2)? {
            for day in directories(&month, 2)? {
                let text = day
                    .strip_prefix(&root)?
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "-");
                if !Date::parse(&text).is_ok_and(|date| range.contains(date)) {
                    continue;
                }
                for entry in std::fs::read_dir(day)? {
                    let entry = entry?;
                    if entry.file_type()?.is_dir()
                        && entry
                            .file_name()
                            .to_str()
                            .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
                    {
                        let path = entry.path().join("transcript.jsonl");
                        if path
                            .symlink_metadata()
                            .is_ok_and(|meta| meta.file_type().is_file())
                        {
                            out.push(path);
                        }
                    }
                }
            }
        }
    }
    out.sort();
    Ok(out)
}
fn directories(root: &Path, digits: usize) -> Result<Vec<PathBuf>> {
    let mut result = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && entry.file_name().to_str().is_some_and(|name| {
                name.len() == digits && name.bytes().all(|b| b.is_ascii_digit())
            })
        {
            result.push(entry.path());
        }
    }
    Ok(result)
}
pub fn list(range: DateRange, project: Option<&Path>) -> Result<Vec<Listing>> {
    let project = project.map(|path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf()));
    let mut result = Vec::new();
    for path in transcripts(range)? {
        let Ok(meta) = metadata(&path) else {
            continue;
        };
        if project.as_ref().is_some_and(|project| {
            let recorded = Path::new(&meta.cwd);
            recorded != project && recorded.canonicalize().ok().as_ref() != Some(project)
        }) {
            continue;
        }
        let modified = path.metadata()?.modified().unwrap_or(UNIX_EPOCH);
        result.push(Listing {
            metadata: meta,
            path,
            modified,
        });
    }
    result.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(result)
}
/// Ripgrep searches transcript content, returning matching sessions once each.
pub async fn search(
    pattern: &str,
    range: DateRange,
    project: Option<&Path>,
) -> Result<Vec<Listing>> {
    let candidates = list(range, project)?;
    let mut matches = HashSet::new();
    for chunk in candidates.chunks(64) {
        let mut command = Command::new("rg");
        command.args([
            "--no-config",
            "--files-with-matches",
            "--null",
            "--text",
            "--no-ignore",
            "--hidden",
            "--color",
            "never",
            "-e",
            pattern,
            "--",
        ]);
        command.args(chunk.iter().map(|item| &item.path));
        let output = crate::process::run(
            command,
            Duration::from_secs(60),
            &CancellationToken::new(),
            16 * 1024 * 1024,
        )
        .await
        .context("session search requires ripgrep (rg)")?;
        ensure!(
            matches!(output.status.code(), Some(0 | 1)),
            "ripgrep: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        for item in chunk {
            if output
                .stdout
                .split(|b| *b == 0)
                .any(|path| path == item.path.as_os_str().as_encoded_bytes())
            {
                matches.insert(item.path.clone());
            }
        }
    }
    Ok(candidates
        .into_iter()
        .filter(|item| matches.contains(&item.path))
        .collect())
}
