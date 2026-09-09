//! Bounded, cancellable commands owned by the harness.
use anyhow::{Context, Result, bail};
use std::{process::Output, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};
use tokio_util::sync::CancellationToken;

/// Run a command and stop its process group on cancellation, timeout, or drop.
pub(crate) async fn run(
    mut command: Command,
    timeout: Duration,
    cancel: &CancellationToken,
    limit: usize,
) -> Result<Output> {
    use std::process::Stdio;
    ensure_active(cancel)?;
    let deadline = tokio::time::Instant::now() + timeout;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().context("starting command")?;
    let mut stdout = child
        .stdout
        .take()
        .context("missing stdout")?
        .take(limit as u64 + 1);
    let mut stderr = child
        .stderr
        .take()
        .context("missing stderr")?
        .take(limit as u64 + 1);
    let mut out = Vec::new();
    let mut err = Vec::new();
    // Keep the child unreaped while killing its descendants, so its ID cannot be reused.
    struct Group(Option<u32>);
    impl Drop for Group {
        fn drop(&mut self) {
            #[cfg(unix)]
            if let Some(id) = self.0 {
                // SAFETY: this group belongs to our still-owned child.
                unsafe {
                    libc::kill(-(id as i32), libc::SIGKILL);
                }
            }
        }
    }
    let group = Group(child.id());
    let reading = async {
        tokio::try_join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err))?;
        anyhow::Ok(())
    };
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(anyhow::anyhow!("command cancelled")),
        result = tokio::time::timeout_at(deadline, reading) => result.context("command timed out").and_then(|r| r),
    };
    if result.is_err() || out.len() > limit || err.len() > limit {
        drop(group);
        let _ = child.kill().await;
        let _ = child.wait().await;
        result?;
        bail!("command output exceeds {limit} bytes");
    }
    // Observe exit without reaping on Unix. This leaves the group ID reserved
    // until descendants have been stopped, including background jobs with closed pipes.
    let waiting = async {
        #[cfg(unix)]
        wait_unreaped(child.id().context("missing child identity")?).await?;
        #[cfg(not(unix))]
        child.wait().await?;
        anyhow::Ok(())
    };
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(anyhow::anyhow!("command cancelled")),
        result = tokio::time::timeout_at(deadline, waiting) => result.context("command timed out").and_then(|r| r),
    };
    drop(group);
    if let Err(error) = result {
        let _ = child.kill().await;
        let _ = child.wait().await;
        return Err(error);
    }
    let status = child.wait().await?;
    Ok(Output {
        status,
        stdout: out,
        stderr: err,
    })
}

#[cfg(unix)]
async fn wait_unreaped(id: u32) -> Result<()> {
    loop {
        // SAFETY: zero is a valid initial siginfo_t; waitid writes it and WNOWAIT
        // leaves our direct child owned until tokio performs the final wait.
        let exited = unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            if libc::waitid(
                libc::P_PID,
                id as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            ) != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            info.si_pid() != 0
        };
        if exited {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn ensure_active(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        bail!("command cancelled");
    }
    Ok(())
}
