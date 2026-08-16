//! Validation — how a task decides it's actually *done*, not just when the model
//! stops talking. This is the core of the thesis (PLAN.md §0/§7a): the loop
//! terminates on a passing check, and its failure output becomes a re-plan
//! directive for the next attempt.
//!
//! The [`Validator`] trait is deliberately small so M6 workers and the M7
//! supervisor reuse it unchanged.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tokio::process::Command;

/// A success predicate for a task.
#[async_trait]
pub trait Validator: Send + Sync {
    /// `Ok(())` = the task validated. `Err(reason)` = not yet; `reason` is fed
    /// back to the model as a re-plan directive.
    async fn validate(&self) -> Result<(), String>;
    /// Short human description (e.g. the command), for events and directives.
    fn describe(&self) -> String;
}

/// Validate by running a shell command; exit 0 = pass. The command's output
/// becomes the failure reason. This backs `--until "cargo test"`.
pub struct CommandValidator {
    command: String,
    cwd: PathBuf,
    timeout: Duration,
}

impl CommandValidator {
    pub fn new(command: impl Into<String>, cwd: PathBuf, timeout: Duration) -> Self {
        Self { command: command.into(), cwd, timeout }
    }
}

#[async_trait]
impl Validator for CommandValidator {
    async fn validate(&self) -> Result<(), String> {
        let mut cmd = Command::new("bash");
        cmd.arg("-lc")
            .arg(&self.command)
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let child = cmd.spawn().map_err(|e| format!("failed to run validation command: {e}"))?;

        let out = match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return Err(format!("validation command error: {e}")),
            Err(_) => {
                return Err(format!(
                    "validation command timed out after {}s",
                    self.timeout.as_secs()
                ));
            }
        };

        if out.status.success() {
            return Ok(());
        }

        let code = out.status.code().unwrap_or(-1);
        let mut body = String::new();
        body.push_str(&String::from_utf8_lossy(&out.stdout));
        body.push_str(&String::from_utf8_lossy(&out.stderr));
        // Keep the tail (compilers/tests put the useful errors last).
        let body = tail(body.trim(), 4000);
        Err(format!("exit code {code}\n{body}"))
    }

    fn describe(&self) -> String {
        format!("`{}`", self.command)
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
