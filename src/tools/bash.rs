//! `bash` — run a shell command with a timeout and session env passthrough.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{Tool, ToolContext, ToolOutput};

pub struct BashTool;

/// Best-effort guard against catastrophic commands. This is NOT a sandbox — real
/// isolation is "run in a container" — but it hard-stops the classic disasters
/// (recursive rm of /, ~, ., or *; fork bombs; dd/mkfs to devices; piping a
/// remote script into a shell; recursive chmod/chown of / or home).
pub fn dangerous_command(cmd: &str) -> Option<String> {
    let checks: &[(&str, &str)] = &[
        (r":\s*\(\s*\)\s*\{", "fork bomb"),
        (r"\bdd\b[^|;&]*\bof=/dev/", "dd writing to a device"),
        (r"\bmkfs\b", "filesystem format (mkfs)"),
        (r">\s*/dev/(sd|nvme|disk|hd|mmcblk)", "write to a block device"),
        (
            r"(?:curl|wget)\b[^|]*\|\s*(?:sudo\s+)?(?:sh|bash|zsh|fish)\b",
            "piping a remote script straight into a shell",
        ),
        (
            r"\bch(?:mod|own)\b[^|;&]*-R[^|;&]*\s(?:/|~|\$HOME)(?:\s|$)",
            "recursive permission change on / or home",
        ),
        (
            r"\brm\b[^|;&]*\s-\S*[rR]\S*[^|;&]*\s(?:-\S+\s+)*(?:/|/\*|~|~/|\$HOME|\.|\.\.|\*|/etc|/usr|/bin|/sbin|/var|/lib|/System|/Library|/boot|/dev)(?:/\s|\s|$)",
            "recursive rm of a dangerous path (/, ~, ., .., *, or a system dir)",
        ),
    ];
    for (pat, reason) in checks {
        // Patterns are static and known-valid.
        if Regex::new(pat).map(|re| re.is_match(cmd)).unwrap_or(false) {
            return Some((*reason).to_string());
        }
    }
    None
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Run a shell command in the working directory and return its combined \
         stdout/stderr and exit status. Has a timeout; long-running or \
         interactive commands will be killed."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The shell command to run." },
                "timeout_secs": { "type": "integer", "description": "Override the default timeout (seconds).", "minimum": 1 }
            },
            "required": ["command"]
        })
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let Some(command) = args.get("command").and_then(|v| v.as_str()) else {
            return ToolOutput::error("missing required argument: command");
        };

        // Hard-stop destructive commands before they run.
        if let Some(reason) = dangerous_command(command) {
            return ToolOutput::blocked(format!(
                "refused to run a destructive command ({reason}): {command}"
            ));
        }

        let timeout = args
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .map(Duration::from_secs)
            .unwrap_or(ctx.bash_timeout);

        let mut cmd = Command::new("bash");
        cmd.arg("-lc")
            .arg(command)
            .current_dir(&ctx.cwd)
            .env("WORKSMITH_SESSION_ID", &ctx.session_id)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(format!("failed to spawn command: {e}")),
        };

        let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => return ToolOutput::error(format!("command error: {e}")),
            Err(_) => {
                return ToolOutput::error(format!(
                    "command timed out after {}s",
                    timeout.as_secs()
                ));
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let code = output.status.code().unwrap_or(-1);

        let mut body = String::new();
        if !stdout.is_empty() {
            body.push_str(&stdout);
        }
        if !stderr.is_empty() {
            if !body.is_empty() && !body.ends_with('\n') {
                body.push('\n');
            }
            body.push_str(&stderr);
        }
        if body.is_empty() {
            body.push_str("(no output)");
        }
        let body = format!("exit code: {code}\n{body}");

        if output.status.success() {
            ToolOutput::ok(body)
        } else {
            ToolOutput::error(body)
        }
    }
}
