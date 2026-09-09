# Parent MCP over stdio

Worksmith can discover and activate tools from explicitly configured local MCP
programs. This implementation is parent-only. Worker discovery and dispatch reject
MCP, including workers forked from a parent with active tools. M11 remains a
separate prerequisite for worker MCP; a worktree is not a security sandbox.

> **Use MCP at your own risk. MCP servers are not sandboxed.** An approved
> server runs with your user account's permissions and can read, modify, or delete
> accessible files and make network requests, including during startup. Commands
> it runs internally bypass Worksmith's command safety checks. Only launch servers
> you trust. OS-enforced process sandboxing is planned future work; it is not
> implemented today. Approval prompts, timeouts, and worktrees do not provide it.

## Configure and use

Add a server to global configuration or a trusted project configuration:

```toml
[mcp.issues]
enabled = true
command = "/absolute/path/to/installed-issues-server"
args = ["--stdio"]

[mcp.issues.env]
API_TOKEN = "ISSUES_API_TOKEN"
```

A project server entry replaces the entire global entry with the same name. This
avoids combining a different executable with inherited arguments or credentials.
Untrusted project entries never reach the manager. Configuration alone launches
nothing; refreshing the catalog requests launch approval, since a program can act
as soon as it starts. Worksmith does not install servers. Launch arguments are
shown in the approval preview.

Only `PATH`, `LANG`, `LC_ALL`, `TMPDIR`, and `SystemRoot` are inherited. Other
variables require explicit name-to-name references in `env`; their values are
resolved at launch and excluded from config identity and launch previews. Stderr
is discarded in this initial implementation to avoid leaking ambient credentials
or corrupting the terminal. Startup failures are consequently generic.

`/mcp` opens the TUI browser. Enter on a server refreshes its catalog; Enter on a
tool activates it; `u` deactivates it; `r` refreshes its server. Tab switches panes,
`/` filters from either pane, and Escape leaves filtering or closes the browser.
Controls that can launch programs run outside the terminal event loop. Equivalent
plain REPL commands are:

```text
/mcp list
/mcp refresh issues
/mcp inspect mcp__issues__read_issue
/mcp activate mcp__issues__read_issue
/mcp deactivate mcp__issues__read_issue
/mcp read <result-handle> <byte-offset>
```

The model has a compact `mcp` discovery tool with these same backend operations.
Listing an unknown server does not pretend its catalog is known; refresh must
initialize it first. Browsing and health changes do not change prompts. Activation
changes the next request's native tool definitions, after the builtin definitions,
in sorted order. A call to a tool absent from that request snapshot is refused.

Connection, activation and authorization are separate. A server's read-only
annotation cannot grant permission. Session grants bind the session, workspace,
server configuration and exact tool definition; refresh retires operation grants.
One-call approval applies to the pending argument object. Headless runs refuse
approvals unless `--approve-all` was explicitly supplied. The plain REPL retains
that existing unattended policy; it has no interactive approval channel.

## Bounds and failure behavior

Per-server settings and initial defaults:

| Setting | Default |
| --- | ---: |
| `timeout-ms` | 30,000 |
| `frame-bytes` | 1,048,576 |
| `catalog-bytes` | 262,144 |
| `max-pages` | 16 |
| `schema-bytes` | 16,384 |
| `active-bytes` | 32,768 |

`schema-bytes` includes the native definition's description and schema.
`active-bytes` checks the total active definitions when activating a tool from
that server. Launches and operations are serialized conservatively. Catalog
pagination cycles, duplicates, excessive names, oversized frames and budgets fail
explicitly. Names use ASCII letters, digits and single underscores; native names
must fit 64 bytes. Input schemas are forwarded intact, with an object root and no
external references. Provider-specific JSON Schema support still varies; this is
not a promise that every provider supports every schema keyword. No schema is
silently weakened, and no schema URL is fetched.

Text and structured results preserve the server's execution-error flag.
Unsupported images, audio or resources produce an explicit error marker; they are
not fetched or silently discarded. Results above 7,000 bytes have a 6,000-byte
excerpt and a session-local handle. Full output is stored beside the session in
`<session-id>.mcp-results/`, with an 8 MiB per-session disk cap and seven-day
retention. Directories/files are created with Unix modes 0700/0600. Expired files
are pruned when that session's result store is accessed; inactive stores are not
swept in the background. Handles work across resume of the same session and never
rerun the remote action. Reads accept any byte offset, rounding back to include a
character when needed. Pages report `next_offset` and `eof`; stop when `eof=true`. Full storage or
expired handles return an error rather than encouraging a mutation replay.

After a potentially dispatched call loses its response, times out, or is
cancelled, its outcome is uncertain. Cancellation does not undo external effects.
The connection is stopped and the tool remains blocked for the session, even with
changed arguments. Refresh cannot clear this block. Recorded dispatched/uncertain
events restore it on session resume. Inspect external state using another
operation; the initial interface deliberately has no model-accessible unblock.
A new session is a new permission scope. Explicit invalid-argument/method errors
and completed execution errors are distinguished from missing responses.

Worksmith owns the child process, gives it a process group on Unix, sends protocol
cancellation when a request handle exists, and bounds shutdown before killing and
reaping the child. Group cleanup reaches descendants that remain in the group;
it does not confine a program that deliberately escapes it. Connections are
closed on frontend exit and before `/new` changes sessions.
Structural MCP events flow through `Agent::emit` into session history. They carry
identity, revision, phase, elapsed time and result size, without raw arguments or
results. Existing tool transcripts still retain tool content and need the same
care as other session data. Tool completion is evidence of execution, not proof
that the user's task passed its validator.

## SDK decision and verification

Pinned [`rmcp` 3.2.0](https://docs.rs/rmcp/3.2.0/rmcp/) with only `client` and
`transport-async-rw`. The [official SDK](https://github.com/modelcontextprotocol/rust-sdk)
declares Rust 1.88 as its MSRV; this checkout was built with Rust 1.96. A complete
MSRV matrix has not been run. Worksmith explicitly negotiates and requires
protocol `2025-11-25` and the tools capability. It advertises no sampling,
elicitation, resources, subscription, or task capabilities.

The SDK's default async reader has no frame bound, and its high-level call helper
can drive additional exchanges. This adapter instead uses the SDK's bounded
`JsonRpcMessageCodec`, a single cancellable request, and Worksmith-owned process
lifecycle. JSON-RPC itself remains SDK code. Server initialization instructions
are never appended to model prompts.

`tests/fixtures/mcp_server.py` is a deterministic local peer (Python 3 is required
for these offline tests). Scripted-model tests exercise the real agent/registry,
request snapshots and session recording. No live LLM, API credentials or service
is required. No completed-task or token-efficiency advantage over eager schemas
has yet been measured. Remote transports, OAuth, worker delegation, marketplaces,
and broader protocol features remain deferred.
