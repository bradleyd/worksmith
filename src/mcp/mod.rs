//! Parent-only MCP. Discovery, activation, permission and connection are separate.
mod connection;
mod results;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, atomic::Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rmcp::{model::*, service::PeerRequestOptions};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    llm::ToolDef,
    tools::{ToolContext, ToolOutput, approval::Approval},
};
use connection::Connection;

/// Local program configuration. A project entry replaces the entire global entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct ServerConfig {
    pub enabled: bool,
    pub command: String,
    pub args: Vec<String>,
    /// Child variable -> parent variable name. Values are resolved only on launch.
    pub env: BTreeMap<String, String>,
    pub timeout_ms: u64,
    pub frame_bytes: usize,
    pub catalog_bytes: usize,
    pub max_pages: usize,
    pub schema_bytes: usize,
    pub active_bytes: usize,
}
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            command: String::new(),
            args: vec![],
            env: BTreeMap::new(),
            timeout_ms: 30_000,
            frame_bytes: 1_048_576,
            catalog_bytes: 262_144,
            max_pages: 16,
            schema_bytes: 16_384,
            active_bytes: 32_768,
        }
    }
}

#[derive(Debug, Clone)]
struct Entry {
    server: String,
    remote: String,
    def: ToolDef,
    revision: String,
}

#[derive(Default)]
struct State {
    session: String,
    connections: BTreeMap<String, Connection>,
    catalog: BTreeMap<String, Entry>,
    active: BTreeMap<String, Entry>,
    results: Option<results::Store>,
    /// A possibly dispatched call must not be repeated merely because no reply arrived.
    uncertain: BTreeSet<String>,
}

/// Shared parent backend. No processes start until an explicitly requested refresh.
pub struct Manager {
    configs: BTreeMap<String, ServerConfig>,
    state: tokio::sync::Mutex<State>,
    // Request assembly is synchronous and must never wait for a remote operation.
    definitions: Mutex<(String, Vec<ToolDef>)>,
    browser: Mutex<(String, Vec<BrowserItem>)>,
}

impl Manager {
    pub fn new(configs: BTreeMap<String, ServerConfig>) -> Result<Self> {
        for (name, config) in &configs {
            if !identifier(name) || name.len() > 20 {
                bail!("MCP server name must be 1–20 ASCII letters, digits or single underscores");
            }
            if config.enabled
                && (!std::path::Path::new(&config.command).is_absolute()
                    || config.timeout_ms == 0
                    || config.frame_bytes < 1024
                    || config.frame_bytes > 16_777_216
                    || config.max_pages == 0
                    || config.max_pages > 128
                    || config.catalog_bytes == 0
                    || config.catalog_bytes > 4_194_304
                    || config.schema_bytes == 0
                    || config.schema_bytes > 65_536
                    || config.active_bytes == 0
                    || config.active_bytes > 262_144)
            {
                bail!(
                    "MCP {name}: use an absolute installed executable and bounded nonzero limits"
                );
            }
        }
        Ok(Self {
            configs,
            state: Default::default(),
            definitions: Default::default(),
            browser: Default::default(),
        })
    }

    pub fn definitions(&self, ctx: &ToolContext) -> Vec<ToolDef> {
        if ctx.is_worker || self.configs.is_empty() {
            return vec![];
        }
        let mut defs = vec![discovery_def()];
        let view = self.definitions.lock().unwrap();
        if view.0 == ctx.session_id {
            defs.extend(view.1.clone());
        }
        defs
    }

    fn publish(&self, state: &State) {
        *self.browser.lock().unwrap() = (state.session.clone(), self.browser_items(state));
        *self.definitions.lock().unwrap() = (
            state.session.clone(),
            state
                .active
                .values()
                .map(|entry| entry.def.clone())
                .collect(),
        );
    }

    /// Cached browsing never starts a process or changes request definitions.
    pub fn browser(&self, ctx: &ToolContext) -> Vec<BrowserItem> {
        if ctx.is_worker {
            return vec![];
        }
        let mut rows = if let Ok(state) = self.state.try_lock() {
            if state.session == ctx.session_id {
                self.browser_items(&state)
            } else {
                self.browser_items(&State::default())
            }
        } else {
            let cached = self.browser.lock().unwrap();
            if cached.0 == ctx.session_id {
                cached.1.clone()
            } else {
                self.browser_items(&State::default())
            }
        };
        for row in &mut rows {
            let allowed = row
                .scope
                .as_ref()
                .is_some_and(|scope| ctx.approver.has_scope(scope));
            row.detail.push_str(if allowed {
                "\nAuthorization: allowed this session for future arguments."
            } else {
                "\nAuthorization: approval required."
            });
            row.detail.push_str(&format!(
                "\nWorkspace: {}\nWorkers: disabled",
                ctx.cwd.display()
            ));
        }
        rows
    }

    fn browser_items(&self, state: &State) -> Vec<BrowserItem> {
        let active_bytes: usize = state
            .active
            .values()
            .map(|e| serde_json::to_vec(&e.def).map_or(0, |s| s.len()))
            .sum();
        let mut rows = vec![];
        for (server, config) in &self.configs {
            let status = match state.connections.get(server) {
                _ if !config.enabled => "disabled",
                Some(c) if c.service.is_closed() => "failed",
                Some(c) if c.changed.load(Ordering::Acquire) => {
                    "connected; catalog stale — refresh required"
                }
                Some(_) => "connected",
                None => "stopped",
            };
            rows.push(BrowserItem { name: format!("server:{server}"), description: status.into(), detail: format!("Server: {server}\nConnection: {status}\nProgram: {}\nEnter or r refreshes after launch approval.\nTotal active schema bytes: {active_bytes}", config.command), active: false, scope: None });
            for entry in state.catalog.values().filter(|e| e.server == *server) {
                let active = state.active.contains_key(&entry.def.name);
                rows.push(BrowserItem { name: entry.def.name.clone(), description: entry.def.description.clone(), detail: format!("Server: {server}\nConnection: {status}\nActivation: {}\nSchema revision: {}\nArguments (JSON Schema):\n{}", if active { "active" } else { "available" }, entry.revision, serde_json::to_string_pretty(&entry.def.parameters).unwrap_or_default()), active, scope: Some(format!("tool:{}", entry.revision)) });
            }
        }
        rows
    }

    pub async fn shutdown(&self) {
        let mut state = self.state.lock().await;
        for connection in state.connections.values_mut() {
            connection.stop().await;
        }
        *state = State::default();
        self.publish(&state);
    }

    /// Runs through the same backend for model calls and frontend controls.
    pub async fn run(
        &self,
        name: &str,
        args: Value,
        ctx: &ToolContext,
        advertised: Option<&ToolDef>,
    ) -> ToolOutput {
        if ctx.is_worker {
            return ToolOutput::error("MCP is disabled for workers");
        }
        if ctx.session_id.is_empty()
            || !ctx
                .session_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return ToolOutput::error("MCP requires a valid session identity");
        }
        let mut state = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return ToolOutput::error("MCP cancelled before dispatch"),
            state = self.state.lock() => state,
        };
        if state.session != ctx.session_id {
            for connection in state.connections.values_mut() {
                connection.stop().await;
            }
            let path = match ctx
                .mcp_session_path
                .clone()
                .map(Ok)
                .unwrap_or_else(|| crate::session::Session::path_for_id(&ctx.session_id))
            {
                Ok(path) => path,
                Err(error) => return ToolOutput::error(format!("MCP session path: {error}")),
            };
            let mut uncertain = BTreeSet::new();
            if let Ok(events) = crate::session::events(&path) {
                for item in events {
                    if let crate::event::Event::McpOperation {
                        server,
                        tool: Some(tool),
                        phase,
                        ..
                    } = item.event
                    {
                        let key = format!("mcp__{server}__{tool}");
                        if phase == "dispatched" || phase == "uncertain" {
                            uncertain.insert(key);
                        } else if phase == "completed"
                            || phase == "execution_error"
                            || phase == "invalid_arguments"
                        {
                            uncertain.remove(&key);
                        }
                    }
                }
            }
            *state = State {
                session: ctx.session_id.clone(),
                results: Some(results::Store::new(&path)),
                uncertain,
                ..State::default()
            };
            self.publish(&state);
        }
        let result = if name == "mcp" {
            self.discover(&mut state, &args, ctx).await
        } else {
            self.call(&mut state, name, args, ctx, advertised).await
        };
        self.publish(&state);
        match result {
            Ok(out) => out,
            Err(error) => ToolOutput::error(format!("MCP: {error}")),
        }
    }

    async fn discover(
        &self,
        state: &mut State,
        args: &Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
        match action {
            "disconnect" => {
                for (server, connection) in &mut state.connections {
                    record(ctx, server, None, "stopping", None, 0, 0);
                    connection.stop().await;
                    record(ctx, server, None, "stopped", None, 0, 0);
                }
                for entry in state.catalog.values() {
                    ctx.approver
                        .forget_scope(&format!("tool:{}", entry.revision));
                }
                state.connections.clear();
                state.active.clear();
                state.catalog.clear();
                Ok(ToolOutput::ok("MCP servers stopped; tools deactivated"))
            }
            "refresh" => {
                let server = field(args, "server")?;
                self.refresh(state, server, ctx).await?;
                Ok(ToolOutput::ok(format!(
                    "{server}: connected; catalog refreshed. Tools require explicit activation and approval."
                )))
            }
            "activate" => {
                let name = field(args, "tool")?;
                let entry = state
                    .catalog
                    .get(name)
                    .context("unknown tool; refresh its server first")?;
                let config = &self.configs[&entry.server];
                let connection = state
                    .connections
                    .get(&entry.server)
                    .context("server disconnected; refresh first")?;
                if connection.service.is_closed() || connection.changed.load(Ordering::Acquire) {
                    bail!("stale catalog; refresh and reactivate");
                }
                validate_schema(&entry.def.parameters)?;
                let size = serde_json::to_vec(&entry.def)?.len();
                if size > config.schema_bytes {
                    bail!(
                        "tool schema/description exceeds {} bytes",
                        config.schema_bytes
                    );
                }
                let total = state
                    .active
                    .iter()
                    .filter(|(key, _)| *key != name)
                    .map(|(_, value)| {
                        serde_json::to_vec(&value.def).map_or(usize::MAX, |bytes| bytes.len())
                    })
                    .sum::<usize>();
                if total.saturating_add(size) > config.active_bytes {
                    bail!(
                        "active schema budget {} bytes exceeded; deactivate tools: {}",
                        config.active_bytes,
                        state.active.keys().cloned().collect::<Vec<_>>().join(", ")
                    );
                }
                state.active.insert(name.into(), entry.clone());
                Ok(ToolOutput::ok(format!(
                    "{name}: active from the next request; approval still required"
                )))
            }
            "deactivate" => {
                state.active.remove(field(args, "tool")?);
                Ok(ToolOutput::ok("tool deactivated"))
            }
            "read" => {
                let handle = field(args, "handle")?;
                let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0);
                let store = state.results.as_ref().context("no session result store")?;
                Ok(ToolOutput::ok(store.page(handle, offset)?))
            }
            "inspect" => {
                let entry = state
                    .catalog
                    .get(field(args, "tool")?)
                    .context("unknown tool; refresh first")?;
                let text = serde_json::to_string(&entry.def)?;
                Ok(store_result(state, text, false))
            }
            "list" => {
                let query = args
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_lowercase();
                let mut text =
                    String::from("MCP is parent-only. Available ≠ active ≠ authorized.\n");
                for (name, config) in &self.configs {
                    let status = match state.connections.get(name) {
                        _ if !config.enabled => "disabled",
                        Some(c) if c.service.is_closed() => "failed; refresh required",
                        Some(c) if c.changed.load(Ordering::Acquire) => "connected; catalog stale",
                        Some(_) => "connected",
                        None => "stopped; refresh requires launch approval",
                    };
                    text.push_str(&format!("{name}: {status}\n"));
                }
                let matches: Vec<_> = state
                    .catalog
                    .values()
                    .filter(|entry| {
                        entry.def.name.to_lowercase().contains(&query)
                            || entry.def.description.to_lowercase().contains(&query)
                    })
                    .collect();
                for entry in matches.iter().take(20) {
                    let active = if state.active.contains_key(&entry.def.name) {
                        "active"
                    } else {
                        "available"
                    };
                    text.push_str(&format!(
                        "{} [{active}; approval required]: {}\n",
                        entry.def.name,
                        &entry.def.description[..boundary(&entry.def.description, 160)]
                    ));
                }
                text.push_str(&format!(
                    "{} matches; showing at most 20. Narrow query to find more.\n",
                    matches.len()
                ));
                Ok(store_result(state, text, false))
            }
            _ => bail!("actions: list, refresh, inspect, activate, deactivate, read"),
        }
    }

    async fn refresh(&self, state: &mut State, server: &str, ctx: &ToolContext) -> Result<()> {
        let config = self.configs.get(server).context("unknown server")?;
        if !config.enabled {
            bail!("server is disabled in trusted configuration");
        }
        // Refresh retires definitions before touching the remote catalog.
        for entry in state
            .catalog
            .values()
            .filter(|entry| entry.server == server)
        {
            ctx.approver
                .forget_scope(&format!("tool:{}", entry.revision));
        }
        state.active.retain(|_, entry| entry.server != server);
        state.catalog.retain(|_, entry| entry.server != server);
        self.publish(state);
        if let Some(mut old) = state.connections.remove(server) {
            old.stop().await;
        }
        let scope = crate::trust::fingerprint(&serde_json::to_vec(&(
            ctx.session_id.as_str(),
            &ctx.cwd,
            server,
            config,
        ))?);
        let preview = format!(
            "Start MCP server {server} in {}\n{}\nThis program can act immediately on launch. Credential references: {:?}",
            ctx.cwd.display(),
            format_args!(
                "{} {}",
                config.command,
                serde_json::to_string(&config.args)?
            ),
            config.env
        );
        record(
            ctx,
            server,
            None,
            "awaiting_launch_approval",
            Some(&scope),
            0,
            0,
        );
        if !approve(
            ctx,
            &preview,
            "launches an MCP program; session approval covers this configuration",
            &format!("launch:{scope}"),
        )
        .await
        {
            record(ctx, server, None, "launch_denied", Some(&scope), 0, 0);
            bail!("launch denied; no process started");
        }
        record(ctx, server, None, "starting", Some(&scope), 0, 0);
        let mut connection = match Connection::start(config, &ctx.cwd, &ctx.cancel).await {
            Ok(connection) => connection,
            Err(error) => {
                record(ctx, server, None, "failed", Some(&scope), 0, 0);
                return Err(error);
            }
        };
        let result = self
            .catalog(server, config, &connection, &scope, &ctx.cancel)
            .await;
        match result {
            Ok(entries) => {
                state.catalog.extend(entries);
                state.connections.insert(server.into(), connection);
                record(ctx, server, None, "ready", Some(&scope), 0, 0);
            }
            Err(error) => {
                connection.stop().await;
                record(ctx, server, None, "failed", Some(&scope), 0, 0);
                return Err(error);
            }
        }
        Ok(())
    }

    async fn catalog(
        &self,
        server: &str,
        config: &ServerConfig,
        connection: &Connection,
        scope: &str,
        cancel: &CancellationToken,
    ) -> Result<BTreeMap<String, Entry>> {
        let mut entries = BTreeMap::new();
        let mut cursor = None;
        let mut seen = BTreeSet::new();
        let mut bytes = 0usize;
        for _ in 0..config.max_pages {
            let params = Some(PaginatedRequestParams::default().with_cursor(cursor));
            let page = tokio::select! {
                biased;
                _ = cancel.cancelled() => bail!("discovery cancelled"),
                result = tokio::time::timeout(Duration::from_millis(config.timeout_ms), connection.service.list_tools(params)) =>
                    result.context("discovery timed out")?.map_err(|_| anyhow::anyhow!("discovery failed"))?,
            };
            bytes = bytes.saturating_add(serde_json::to_vec(&page)?.len());
            if bytes > config.catalog_bytes {
                bail!("catalog byte limit exceeded");
            }
            for tool in page.tools {
                if !identifier(&tool.name) {
                    bail!(
                        "unsupported tool name; names must be ASCII letters, digits or single underscores"
                    );
                }
                let name = format!("mcp__{server}__{}", tool.name);
                if name.len() > 64 || entries.contains_key(&name) {
                    bail!("tool name too long or duplicated");
                }
                let def = ToolDef {
                    name: name.clone(),
                    description: tool.description.unwrap_or_default().into_owned(),
                    parameters: Value::Object((*tool.input_schema).clone()),
                };
                let revision = crate::trust::fingerprint(&serde_json::to_vec(&(scope, &def))?);
                entries.insert(
                    name,
                    Entry {
                        server: server.into(),
                        remote: tool.name.into_owned(),
                        def,
                        revision,
                    },
                );
            }
            cursor = page.next_cursor;
            let Some(next) = &cursor else {
                return Ok(entries);
            };
            if !seen.insert(next.clone()) {
                bail!("catalog pagination loop");
            }
        }
        bail!("catalog page limit exceeded")
    }

    async fn call(
        &self,
        state: &mut State,
        name: &str,
        args: Value,
        ctx: &ToolContext,
        advertised: Option<&ToolDef>,
    ) -> Result<ToolOutput> {
        let entry = state
            .active
            .get(name)
            .context("inactive tool; discover and activate it for the next request")?
            .clone();
        if let Some(advertised) = advertised {
            if serde_json::to_value(advertised)? != serde_json::to_value(&entry.def)? {
                bail!("tool definition changed since request; reactivate and request again");
            }
        } else {
            bail!("tool was not active in this request snapshot");
        }
        let arguments = args
            .as_object()
            .context("tool arguments must be an object")?
            .clone();
        if serde_json::to_vec(&args)?.len() > 16_384 {
            bail!("tool arguments exceed 16384 bytes");
        }
        let operation = name.to_string();
        if state.uncertain.contains(&operation) {
            bail!(
                "uncertain prior outcome: this call will not be replayed; inspect the external state before taking another action"
            );
        }
        let connection = state
            .connections
            .get_mut(&entry.server)
            .context("server disconnected; refresh required")?;
        if connection.service.is_closed() || connection.changed.load(Ordering::Acquire) {
            bail!("server unavailable or catalog stale; refresh and reactivate");
        }
        let preview = format!(
            "MCP {} / {}\nWorkspace: {}\nArguments: {}",
            entry.server,
            entry.remote,
            ctx.cwd.display(),
            redact(args.clone())
        );
        record(
            ctx,
            &entry.server,
            Some(&entry.remote),
            "awaiting_operation_approval",
            Some(&entry.revision),
            0,
            0,
        );
        if !approve(
            ctx,
            &preview,
            "external MCP operation; session approval covers future arguments for this tool",
            &format!("tool:{}", entry.revision),
        )
        .await
        {
            record(
                ctx,
                &entry.server,
                Some(&entry.remote),
                "denied",
                Some(&entry.revision),
                0,
                0,
            );
            bail!("operation denied; no call dispatched");
        }
        if ctx.cancel.is_cancelled() {
            bail!("cancelled before dispatch");
        }
        if connection.changed.load(Ordering::Acquire) {
            bail!("catalog changed during approval; refresh and reactivate");
        }
        let params = CallToolRequestParams::new(entry.remote.clone()).with_arguments(arguments);
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(params));
        let started = Instant::now();
        let timeout = Duration::from_millis(self.configs[&entry.server].timeout_ms);
        // Conservatively record before enqueue: a cancelled caller cannot prove non-dispatch.
        state.uncertain.insert(operation.clone());
        record(
            ctx,
            &entry.server,
            Some(&entry.remote),
            "dispatched",
            Some(&entry.revision),
            0,
            0,
        );
        let response = async {
            let mut handle = tokio::time::timeout(timeout, connection.service.send_cancellable_request(request, PeerRequestOptions::default())).await.map_err(|_| rmcp::ServiceError::Timeout { timeout })??;
            tokio::select! {
                biased;
                _ = ctx.cancel.cancelled() => {
                    let _ = tokio::time::timeout(Duration::from_millis(100), handle.cancel(Some("user cancelled".into()))).await;
                    Err(rmcp::ServiceError::Timeout { timeout })
                }
                _ = tokio::time::sleep(timeout) => {
                    let _ = tokio::time::timeout(Duration::from_millis(100), handle.cancel(Some("deadline exceeded".into()))).await;
                    Err(rmcp::ServiceError::Timeout { timeout })
                }
                result = &mut handle.rx => result.unwrap_or(Err(rmcp::ServiceError::TransportClosed)),
            }
        }.await;
        match response {
            Err(rmcp::ServiceError::McpError(error))
                if error.code == rmcp::model::ErrorCode::INVALID_PARAMS
                    || error.code == rmcp::model::ErrorCode::METHOD_NOT_FOUND =>
            {
                state.uncertain.remove(&operation);
                record(
                    ctx,
                    &entry.server,
                    Some(&entry.remote),
                    "invalid_arguments",
                    Some(&entry.revision),
                    started.elapsed().as_millis() as u64,
                    0,
                );
                // Server-provided error data is untrusted and may include credentials.
                Ok(ToolOutput::error(
                    "MCP request rejected: invalid arguments or unsupported method",
                ))
            }
            Ok(ServerResult::CallToolResult(result)) => {
                state.uncertain.remove(&operation);
                let mut content = format!("MCP {name} ({} ms)\n", started.elapsed().as_millis());
                let mut unsupported = false;
                for part in &result.content {
                    if let Some(text) = part.as_text() {
                        content.push_str(&text.text);
                        content.push('\n');
                    } else {
                        unsupported = true;
                        content.push_str("[unsupported MCP content; not fetched or rendered]\n");
                    }
                }
                if let Some(structured) = result.structured_content {
                    content.push_str(&serde_json::to_string(&structured)?);
                }
                let failed = result.is_error.unwrap_or(false) || unsupported;
                record(
                    ctx,
                    &entry.server,
                    Some(&entry.remote),
                    if failed {
                        "execution_error"
                    } else {
                        "completed"
                    },
                    Some(&entry.revision),
                    started.elapsed().as_millis() as u64,
                    content.len(),
                );
                Ok(store_result(state, content, failed))
            }
            _ => {
                record(
                    ctx,
                    &entry.server,
                    Some(&entry.remote),
                    "uncertain",
                    Some(&entry.revision),
                    started.elapsed().as_millis() as u64,
                    0,
                );
                connection.stop().await;
                state.active.retain(|_, value| value.server != entry.server);
                bail!(
                    "uncertain outcome for {name}: connection failed, response unsupported, timeout or cancellation after dispatch; no automatic replay. Verify external state"
                )
            }
        }
    }
}

fn identifier(name: &str) -> bool {
    !name.is_empty()
        && !name.contains("__")
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}
fn field<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing {key}"))
}
fn boundary(text: &str, limit: usize) -> usize {
    let mut end = limit.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}
fn store_result(state: &mut State, content: String, is_error: bool) -> ToolOutput {
    if content.len() <= 7000 {
        return ToolOutput {
            content,
            is_error,
            fatal: false,
        };
    }
    let length = content.len();
    let excerpt = content[..boundary(&content, 6000)].to_string();
    let stored = state
        .results
        .as_ref()
        .context("no session result store")
        .and_then(|store| store.put(&content));
    match stored {
        Ok(handle) => ToolOutput {
            content: format!(
                "{excerpt}\n[{length} total bytes; session handle {handle}. Use mcp action=read handle={handle} offset={}, then follow next_offset until eof=true. Retrieval never repeats the operation; handles expire after seven days.]", excerpt.len()
            ),
            is_error,
            fatal: false,
        },
        Err(error) => ToolOutput::error(format!(
            "{excerpt}\n[result {length} bytes; storage unavailable: {error}; do not replay a mutation to recover omitted content]"
        )),
    }
}

async fn approve(ctx: &ToolContext, preview: &str, reason: &str, scope: &str) -> bool {
    struct Wait<'a>(&'a std::sync::atomic::AtomicBool);
    impl Drop for Wait<'_> {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Relaxed);
        }
    }
    ctx.awaiting_approval.store(true, Ordering::Relaxed);
    let _wait = Wait(&ctx.awaiting_approval);
    tokio::select! {
        biased;
        _ = ctx.cancel.cancelled() => false,
        answer = ctx.approver.ask_scoped(preview, reason, scope) => answer != Approval::Deny,
    }
}

fn redact(value: Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(key, value)| {
                    let lower = key.to_ascii_lowercase();
                    let value = if [
                        "password",
                        "secret",
                        "token",
                        "api_key",
                        "apikey",
                        "authorization",
                    ]
                    .iter()
                    .any(|part| lower.contains(part))
                    {
                        json!("[redacted]")
                    } else {
                        redact(value)
                    };
                    (key, value)
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(redact).collect()),
        value => value,
    }
}

fn validate_schema(schema: &Value) -> Result<()> {
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        bail!("provider tool schema must have an object root; schema was not translated");
    }
    // Remote references must never result in network retrieval during activation.
    fn remote_ref(value: &Value) -> bool {
        match value {
            Value::Object(fields) => fields.iter().any(|(key, value)| {
                ((key == "$ref" || key == "$dynamicRef")
                    && value.as_str().is_some_and(|s| !s.starts_with('#')))
                    || remote_ref(value)
            }),
            Value::Array(values) => values.iter().any(remote_ref),
            _ => false,
        }
    }
    if remote_ref(schema) {
        bail!("external JSON Schema references unsupported; schema was not translated");
    }
    Ok(())
}

fn discovery_def() -> ToolDef {
    ToolDef { name: "mcp".into(), description: "Discover configured parent MCP tools. Refresh starts a server after launch approval. Activate selects a native schema for the next request, not permission. Read retrieves a stored result without replay.".into(), parameters: json!({"type":"object","properties":{
        "action":{"type":"string","enum":["list","refresh","inspect","activate","deactivate","read","disconnect"]},
        "server":{"type":"string"},"tool":{"type":"string"},"query":{"type":"string"},"handle":{"type":"string"},"offset":{"type":"integer","minimum":0,"description":"Byte offset; defaults to 0. Offsets inside UTF-8 characters round back safely. Follow next_offset; stop when eof=true."}},"required":["action"],"additionalProperties":false}) }
}

fn record(
    ctx: &ToolContext,
    server: &str,
    tool: Option<&str>,
    phase: &str,
    revision: Option<&str>,
    elapsed_ms: u64,
    result_bytes: usize,
) {
    if let Some(sender) = &ctx.mcp_events {
        let _ = sender.send(crate::event::Event::McpOperation {
            server: server.into(),
            tool: tool.map(str::to_owned),
            phase: phase.into(),
            revision: revision.map(str::to_owned),
            elapsed_ms,
            result_bytes,
        });
    }
}

/// One server or tool in the shared browser backend.
#[derive(Debug, Clone)]
pub struct BrowserItem {
    pub name: String,
    pub description: String,
    pub detail: String,
    pub active: bool,
    scope: Option<String>,
}

/// Parse frontend controls without constructing a model turn.
pub fn command_args(words: &[&str]) -> Result<Value> {
    match words {
        [] | ["list"] => Ok(json!({"action":"list"})),
        ["disconnect"] => Ok(json!({"action":"disconnect"})),
        ["list", rest @ ..] => Ok(json!({"action":"list","query":rest.join(" ")})),
        ["refresh", server] => Ok(json!({"action":"refresh","server":server})),
        [action @ ("inspect" | "activate" | "deactivate"), tool] => {
            Ok(json!({"action":action,"tool":tool}))
        }
        ["read", handle, offset] => {
            Ok(json!({"action":"read","handle":handle,"offset":offset.parse::<usize>()?}))
        }
        _ => bail!(
            "usage: /mcp [list [query] | refresh <server> | inspect <tool> | activate <tool> | deactivate <tool> | read <handle> <offset>]"
        ),
    }
}
