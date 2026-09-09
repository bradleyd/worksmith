//! Bounded stdio framing with SDK protocol handling and explicit child ownership.
use std::process::Stdio;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt, future};
use rmcp::{
    ClientHandler, RoleClient, ServiceExt,
    model::*,
    service::{NotificationContext, RunningService, RxJsonRpcMessage, TxJsonRpcMessage},
    transport::async_rw::JsonRpcMessageCodec,
};
use tokio::process::{Child, Command};
use tokio_util::{
    codec::{FramedRead, FramedWrite},
    sync::CancellationToken,
};

use super::ServerConfig;

#[derive(Debug, Clone, Default)]
pub(super) struct Handler {
    pub changed: Arc<AtomicBool>,
}

impl ClientHandler for Handler {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::new("worksmith", env!("CARGO_PKG_VERSION")),
        )
        .with_protocol_version(ProtocolVersion::V_2025_11_25)
    }

    async fn on_tool_list_changed(&self, _: NotificationContext<RoleClient>) {
        self.changed.store(true, Ordering::Release);
    }
}

struct OwnedChild(Child);
impl OwnedChild {
    fn kill_group(&mut self) {
        #[cfg(unix)]
        if let Some(id) = self.0.id() {
            // SAFETY: this is the process group created for our own child.
            unsafe {
                libc::kill(-(id as i32), libc::SIGKILL);
            }
        }
        let _ = self.0.start_kill();
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        self.kill_group();
    }
}

pub(super) struct Connection {
    pub service: RunningService<RoleClient, Handler>,
    pub changed: Arc<AtomicBool>,
    child: OwnedChild,
}

impl Connection {
    pub async fn start(
        config: &ServerConfig,
        cwd: &std::path::Path,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .current_dir(cwd)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        // No HOME, cloud credentials, package-manager configuration, or ambient tokens.
        for key in ["PATH", "LANG", "LC_ALL", "TMPDIR", "SystemRoot"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        for (key, source) in &config.env {
            let value = std::env::var_os(source)
                .with_context(|| format!("missing credential environment reference {source}"))?;
            command.env(key, value);
        }
        #[cfg(unix)]
        command.process_group(0);
        let mut child = OwnedChild(command.spawn().context("launch MCP program")?);
        let stdout = child.0.stdout.take().context("missing MCP stdout")?;
        let stdin = child.0.stdin.take().context("missing MCP stdin")?;
        let read = FramedRead::new(
            stdout,
            JsonRpcMessageCodec::<RxJsonRpcMessage<RoleClient>>::new_with_max_length(
                config.frame_bytes,
            ),
        )
        .take_while(|item| future::ready(item.is_ok()))
        .filter_map(|item| future::ready(item.ok()));
        let write = FramedWrite::new(
            stdin,
            JsonRpcMessageCodec::<TxJsonRpcMessage<RoleClient>>::new(),
        )
        .sink_map_err(std::io::Error::from);
        let handler = Handler::default();
        let changed = handler.changed.clone();
        let service = tokio::select! {
            biased;
            _ = cancel.cancelled() => { child.kill_group(); let _ = child.0.wait().await; bail!("cancelled before initialization"); },
            result = tokio::time::timeout(Duration::from_millis(config.timeout_ms), handler.serve((write, read))) => {
                match result {
                    Ok(Ok(service)) => service,
                    _ => { child.kill_group(); let _ = child.0.wait().await; bail!("MCP initialization failed or timed out"); }
                }
            }
        };
        let mut connection = Self {
            service,
            changed,
            child,
        };
        if !connection.service.peer_info().is_some_and(|info| {
            info.protocol_version == ProtocolVersion::V_2025_11_25
                && info.capabilities.tools.is_some()
        }) {
            connection.stop().await;
            bail!("server must support protocol 2025-11-25 and tools");
        }
        Ok(connection)
    }

    pub async fn stop(&mut self) {
        // Close stdin first, then kill the group while its leader is still owned.
        // Killing descendants before reaping the leader avoids a reused group id.
        let _ = self
            .service
            .close_with_timeout(Duration::from_millis(200))
            .await;
        self.child.kill_group();
        let _ = tokio::time::timeout(Duration::from_secs(2), self.child.0.wait()).await;
    }
}
