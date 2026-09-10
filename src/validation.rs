//! Validation — how a task decides it's actually *done*, not just when the model
//! stops talking. This is the core of the thesis (PLAN.md §0/§7a): the loop
//! terminates on a passing check, and its failure output becomes a re-plan
//! directive for the next attempt.
//!
//! The [`Validator`] trait is deliberately small so M6 workers and the M7
//! supervisor reuse it unchanged.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use tokio::process::Command;

/// A success predicate for a task.
#[async_trait]
pub trait Validator: Send + Sync {
    /// A completed check retains its output; execution errors are separate.
    async fn validate(&self) -> Result<CheckOutput, String>;
    /// Short human description (e.g. the command), for events and directives.
    fn describe(&self) -> String;
}

/// Validate by running a shell command; exit 0 = pass. The command's output
/// becomes the failure reason. This backs `--until "cargo test"`.
pub struct CommandValidator {
    command: String,
    cwd: PathBuf,
    timeout: Duration,
    cancel: tokio_util::sync::CancellationToken,
}

impl CommandValidator {
    pub fn new(command: impl Into<String>, cwd: PathBuf, timeout: Duration) -> Self {
        Self {
            command: command.into(),
            cwd,
            timeout,
            cancel: Default::default(),
        }
    }
    pub fn with_cancel(mut self, cancel: tokio_util::sync::CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }
}

#[async_trait]
impl Validator for CommandValidator {
    async fn validate(&self) -> Result<CheckOutput, String> {
        let mut cmd = Command::new("bash");
        cmd.arg("-lc").arg(&self.command).current_dir(&self.cwd);
        let out = crate::process::run(cmd, self.timeout, &self.cancel, 16 * 1024 * 1024)
            .await
            .map_err(|e| format!("validation command: {e}"))?;

        Ok(CheckOutput {
            passed: out.status.success(),
            exit_code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn describe(&self) -> String {
        self.command.clone()
    }
}

/// Keep the last `max` bytes of `s` (on a char boundary), with a leading notice.
fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut start = s.len() - max;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…(earlier output truncated)…\n{}", &s[start..])
}

/// Captured subprocess output, bounded by the process runner.
#[derive(Debug)]
pub struct CheckOutput {
    pub passed: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Small durable evidence for display, separate from the model's summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckReport {
    pub command: String,
    pub passed: bool,
    pub exit_code: Option<i32>,
    pub output: String,
    pub log_path: Option<PathBuf>,
}

impl CheckReport {
    /// Save each attempt beside its session; keep only an excerpt in events.
    pub async fn record(
        command: String,
        result: Result<CheckOutput, String>,
        session: &Path,
    ) -> Self {
        let (passed, exit_code, body) = match result {
            Ok(out) => {
                let mut body = out.stdout;
                if !out.stderr.is_empty() {
                    if !body.is_empty() {
                        body.push('\n');
                    }
                    body.push_str(&out.stderr);
                }
                if body.is_empty() {
                    body.push_str("(no output)");
                }
                (out.passed, out.exit_code, body)
            }
            Err(error) => (false, None, error),
        };
        let directory = crate::session::store::artifact_path(session, "checks", "checks");
        let path = directory.join(format!("{}.log", uuid::Uuid::new_v4()));
        let code = exit_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "unavailable".into());
        let saved = async {
            tokio::fs::create_dir_all(&directory).await?;
            tokio::fs::write(
                &path,
                format!("$ {command}\npassed: {passed}\nexit code: {code}\n\n{body}"),
            )
            .await
        }
        .await;
        let mut output = tail(body.trim(), 4000);
        let log_path = match saved {
            Ok(()) => Some(path),
            Err(error) => {
                output.push_str(&format!("\nCould not save validation log: {error}"));
                None
            }
        };
        Self {
            command,
            passed,
            exit_code,
            output,
            log_path,
        }
    }

    /// Human-readable evidence; also used as feedback after a failed check.
    pub fn display(&self) -> String {
        let code = self
            .exit_code
            .map(|code| format!(" · exit {code}"))
            .unwrap_or_default();
        let mut text = format!(
            "Validation · {}{code}\n$ {}\n{}",
            if self.passed { "Passed" } else { "Failed" },
            self.command,
            self.output
        );
        if let Some(path) = &self.log_path {
            text.push_str(&format!("\nValidation log: {}", path.display()));
        }
        text
    }

    /// Stable retry feedback excludes per-attempt log paths.
    pub fn failure_reason(&self) -> String {
        format!(
            "exit code {}\n{}",
            self.exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "unavailable".into()),
            self.output
        )
    }
}
