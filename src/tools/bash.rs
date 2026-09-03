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
                match super::ask_approval(ctx, command, &reason).await {
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

        if let Some(refusal) = super::approve_command_outside_cwd(ctx, command).await {
            return ToolOutput::error(refusal);
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

        // `kill_on_drop` reaches the command we spawned and nothing it spawned
        // in turn. `cargo test` starts rustc; killing cargo leaves those
        // running, still holding the build lock and the pipes. So the command
        // gets its own process group and the whole group is killed together.
        //
        // This is also the answer to why a model reaches for `pkill`: it has no
        // pid. The tool returns stdout, stderr and an exit code and no handle,
        // so a pattern is the only thing the model can name. The layer that
        // *does* have the pid is the one that timed out, and it was not
        // cleaning up.
        #[cfg(unix)]
        cmd.process_group(0);

        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(format!("failed to spawn command: {e}")),
        };
        // Captured before `wait_with_output` consumes the child. With
        // `process_group(0)` the child's pid *is* the group id.
        #[cfg(unix)]
        let pgid = child.id().map(|id| id as i32);

        // Raced against cancellation as well as the timeout. `kill_on_drop`
        // above means either arm ends the process rather than orphaning it —
        // which is what `/agents kill` needs in order to mean anything, since
        // otherwise a command answers to nobody until its own timeout expires.
        // Ends the whole group, not just the process we hold. Best-effort by
        // design: the group may already be gone, and failing to reap a dead
        // process must not fail the tool call.
        #[cfg(unix)]
        let reap = || {
            if let Some(pgid) = pgid {
                // SAFETY: a plain kill(2) on a group we created ourselves.
                unsafe { libc::kill(-pgid, libc::SIGKILL) };
            }
        };
        #[cfg(not(unix))]
        let reap = || {};

        let output = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => {
                reap();
                return ToolOutput::error("command cancelled".to_string());
            }
            r = tokio::time::timeout(timeout, child.wait_with_output()) => match r {
                Ok(Ok(out)) => out,
                Ok(Err(e)) => return ToolOutput::error(format!("command error: {e}")),
                Err(_) => {
                    reap();
                    return ToolOutput::error(format!(
                        "command timed out after {}s",
                        timeout.as_secs()
                    ));
                }
            },
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

    /// A timed-out command does not leave *grandchildren* behind either.
    ///
    /// `kill_on_drop` reaches the process we spawned and nothing it spawned in
    /// turn — and the real case is exactly that shape: `cargo test` starts
    /// rustc, so killing cargo leaves the build running, still holding the lock
    /// and the pipes. A worker then sees every retry block, has no pid to name
    /// because the tool never returns one, and reaches for `pkill`.
    #[tokio::test]
    async fn a_timeout_takes_the_whole_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext {
            cwd: dir.path().to_path_buf(),
            bash_timeout: Duration::from_millis(300),
            ..Default::default()
        };
        // The shell exits immediately; the *grandchild* is what outlives it and
        // writes the marker. Backgrounded so nothing waits on it.
        let marker = dir.path().join("grandchild");
        let out = BashTool
            .run(
                json!({
                    "command":
                        format!("( sleep 1; touch {} ) & sleep 5", marker.display()),
                }),
                &ctx,
            )
            .await;
        assert!(out.content.contains("timed out"), "{}", out.content);

        tokio::time::sleep(Duration::from_millis(2_000)).await;
        assert!(
            !marker.exists(),
            "a grandchild outlived the timeout — the group was not killed"
        );
    }

    /// A cancelled command stops, rather than running to its own timeout.
    ///
    /// This is what `/agents kill` needs in order to mean anything. Without it
    /// the tool watched only its timeout — up to `bash-timeout-secs`, 600 in a
    /// real config — and neither the user nor the supervisor could interrupt
    /// it. Observed: "killing w1" printed while the worker stayed `[running]`,
    /// and the supervisor's own escalation was ignored the same way.
    #[tokio::test]
    async fn a_cancelled_command_stops_instead_of_running_to_its_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let ctx = ToolContext {
            cwd: dir.path().to_path_buf(),
            // Far longer than the test: only cancellation can end this.
            bash_timeout: Duration::from_secs(60),
            cancel: cancel.clone(),
            ..Default::default()
        };

        let marker = dir.path().join("survived");
        let cmd = format!("sleep 1; touch {}", marker.display());
        let running = tokio::spawn(async move {
            BashTool.run(json!({ "command": cmd }), &ctx).await
        });

        tokio::time::sleep(Duration::from_millis(200)).await;
        cancel.cancel();

        let out = tokio::time::timeout(Duration::from_secs(5), running)
            .await
            .expect("it must return promptly, not wait out its 60s timeout")
            .unwrap();
        assert!(out.content.contains("cancelled"), "{}", out.content);

        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert!(!marker.exists(), "cancelling must end the process, not orphan it");
    }
}
