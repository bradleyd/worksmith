//! `bash` — run a shell command with a timeout and session env passthrough.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{Tool, ToolContext, ToolOutput};

pub struct BashTool;

/// Does this command hit the refuse tier? This is NOT a sandbox — real isolation
/// is "run in a container" (PLAN M11) — but it hard-stops the classic disasters.
pub fn dangerous_command(cmd: &str) -> Option<String> {
    // Kept as the public name this has always had; the patterns themselves live
    // in `policy`, which also owns the softer ask-tier. Two copies of a security
    // rule is one copy too many.
    match crate::tools::policy::classify(cmd) {
        crate::tools::policy::Decision::Refuse(reason) => Some(reason),
        _ => None,
    }
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

        // Two tiers: catastrophic commands are refused outright, outward-facing
        // ones are put to the user. See `policy` for why the ask-list is short.
        match crate::tools::policy::classify(command) {
            crate::tools::policy::Decision::Allow => {}
            crate::tools::policy::Decision::Refuse(reason) => {
                return ToolOutput::blocked(format!(
                    "refused to run a destructive command ({reason}): {command}"
                ));
            }
            crate::tools::policy::Decision::Ask(reason) => {
                use crate::tools::approval::Approval;
                match ctx.approver.ask(command, &reason).await {
                    Approval::Once | Approval::AlwaysThisSession => {}
                    // Not fatal: the model should be able to take a different
                    // route (say, leave the commit unpushed and report that)
                    // rather than have the turn killed under it.
                    Approval::Deny => {
                        return ToolOutput::error(format!(
                            "the user did not approve this command ({reason}): {command}\n\
                             Do not retry it. Continue without it, and say what was skipped."
                        ));
                    }
                }
            }
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
            .stderr(Stdio::piped())
            // Without this the timeout below abandons the process instead of
            // ending it. `wait_with_output` consumes the child, so when the
            // timeout future is dropped the child is simply orphaned and keeps
            // running — measured live: a `cargo test` capped at 120s was still
            // compiling four and a half minutes later.
            //
            // That is not merely untidy, it is what makes a worker appear
            // stuck. The orphan holds the build lock, so every retry blocks on
            // it, and a model with no way to see why reaches for `pkill` — one
            // did, with a pattern broad enough to kill every cargo and rustc on
            // the machine, and matched its own shell in the process.
            .kill_on_drop(true);

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

#[cfg(test)]
mod kill_tests {
    use super::*;

    /// A timed-out command does not outlive its timeout.
    ///
    /// It used to. `wait_with_output` consumes the child, so dropping the
    /// timeout future orphaned it — measured live at four and a half minutes
    /// for a `cargo test` the model had capped at 120s. The orphan then held
    /// the build lock, every retry blocked on it, and the worker looked stuck
    /// for a reason nothing on screen could explain. The model's answer was
    /// `pkill -9 -f "cargo|rustc"`.
    #[tokio::test]
    async fn a_timed_out_command_is_actually_killed() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext {
            cwd: dir.path().to_path_buf(),
            bash_timeout: Duration::from_millis(300),
            ..Default::default()
        };
        // Writes a marker a second in. If the process survives its own timeout,
        // the marker appears.
        let marker = dir.path().join("survived");
        let out = BashTool
            .run(
                json!({ "command": format!("sleep 1; touch {}", marker.display()) }),
                &ctx,
            )
            .await;
        assert!(out.content.contains("timed out"), "{}", out.content);

        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert!(
            !marker.exists(),
            "the command outlived its timeout — orphaned, not killed"
        );
    }
}
