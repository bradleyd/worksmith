# MCP in Worksmith

Status: parent-only implementation completed, 2026-09-09. This document preserves
the design; see `docs/mcp.md` for usage and limitations.
M11 worker isolation is a priority before expanding worker execution. Parent-only
MCP can proceed independently. This plan supersedes
§6's initial MCP sketch and §10a's older ordering where they disagree.

## Product intent

Worksmith's advantage is helping a modest model finish verifiable work. MCP
should extend that loop: find the relevant capability, perform a bounded action,
inspect the evidence, and run the task's check. Success is more completed tasks
with understandable effects, not more tools advertised to the model.

Example: “Fix the regression described in issue 42.” The parent activates the
issue-reading tool, retrieves the relevant acceptance criteria, and gives an
isolated worker the task and evidence. The worker edits and runs the check in its
own tree. The parent receives the patch and validation result. Posting a reply to
the issue is a separate, visible action requiring its own permission. Reading an
issue never implicitly authorizes posting to it.

The experience should have four recognizable properties:

- Small model context: compact discovery, selected schemas, bounded results.
- Visible scope: which tools are active, who can call them, and where they run.
- Evidence carried through the existing validation and worker report paths.
- Explainable failures: denied, unavailable, invalid arguments, execution error,
  and uncertain external outcome have different recovery paths.

Avoid a marketplace, automatic server installation, a second agent loop, or a
large orchestration framework. Skills remain the place for task-specific guidance;
MCP provides callable capabilities. Retrieved instructions are external data and
do not acquire the authority of project or user instructions.

## Implementation order and actual dependencies

There are two independent initial tracks:

- **Parent-only MCP, stdio only:** discovery, activation, permissions, execution,
  lifecycle, and observability. It does not depend on worktrees.
- **M11 isolation and review:** separate worker workspaces, scoped validation,
  reviewable results, conflict-safe application, and cleanup.

**MCP in workers waits for both tracks**, with explicitly delegated tools and
workspace-bound server instances. M12 explicit role routing follows as a product
priority, though its configuration plumbing does not technically require M11.

The earlier blanket “M11 before MCP” ordering was too strict. M11 is a priority
for worker safety, not a prerequisite for parent-only integration. The suggested
fresh-start slice is parent-only MCP; keep M11 as a separate implementation branch
and do not expand shared-tree workers in the meantime. See `FRESH_START.md`.

Session-store cleanup remains a separate maintenance task and should move up if
resume reliability blocks dogfooding.

## M11 contract required by MCP

- Capture one starting snapshot for a worker batch, including intended tracked
  edits, deletions, and untracked non-ignored files. Specify treatment of symlinks,
  submodules, ignored build dependencies, and secrets before implementation.
- Assign each worker a workspace identity and root. Builtin tools, validators,
  and workspace-bound MCP processes use that root consistently.
- Record the snapshot/base identity with each result. Validate inside the worker
  tree and report the command, result, and diff together.
- Accept changes through an explicit parent-side operation. Check whether the
  destination changed since the snapshot; surface conflicts and never overwrite
  newer user edits silently. Preserve failed/conflicting results for inspection.
- Define ownership and cleanup for running, completed, cancelled, and abandoned
  workspaces. A process destructor alone is not crash recovery.
- If isolation cannot be created, fail the isolated spawn clearly. Do not silently
  fall back to shared writes. Non-Git scratch copies require their own design;
  until then, offer an explicitly chosen shared mode with its limitation visible.

A Git worktree isolates normal edit locations, not process authority. Absolute
paths, symlinks, shell commands, and external APIs can escape it. OS-level
filesystem/network confinement remains a separate project. Documentation and UI
must not describe worktrees or MCP roots as an enforced security sandbox.

## A small, deliberate tool surface

Keep existing builtin tools. Add one compact discovery tool for enabled MCP
servers, with search/inspect/activate actions. It returns bounded matches with
server identity and short descriptions. It does not inject every server schema or
full catalog into the system prompt.

Selected tools become native definitions in the existing registry, using stable
`mcp__<server>__<tool>` identities. Activation makes a definition available; it
never grants permission to execute it. Activation through the browser and through
the model use the same backend. Calling inactive tools produces a useful error.

Resolve the discovery/startup dependency explicitly: on a fresh installation,
listing a server's tools requires launching and initializing it. Do this on an
explicit refresh, browser inspection, or discovery request, after launch trust is
resolved. Never claim that an unknown catalog can be discovered without starting
the server. Cached metadata can support later browsing while disconnected, but
must be marked stale and revalidated before activation/execution.

For the first slice:

- Keep tools active until explicitly deactivated; no speculative relevance model
  and no silent eviction. Workers receive an independent activation snapshot.
- Bound catalog pages, description bytes, schema bytes, and total active-schema
  bytes. Make initial limits configurable and measure them before choosing defaults.
  Reject oversized activation with a clear explanation and the tools consuming
  the budget. Do not recreate an unexplained skill-count limit.
- Preserve builtin order; canonicalize and sort MCP definitions deterministically.
  Activation changes the next request snapshot; in-flight requests remain intact.
- Browsing, filtering, permission prompts, and health updates do not alter model
  inputs. A catalog-change notification marks a refresh pending; it does not
  rewrite schemas mid-request. Changed tool identity/schema invalidates approvals
  tied to the old definition and requires reactivation at a safe request boundary.
- Do not silently translate unsupported JSON Schema into a weaker contract.
  Explain provider incompatibilities and test representative schemas.

This deliberately trades a discovery turn for a smaller active tool set. Compare
it with eager schema exposure on the same tasks before claiming it saves tokens
or helps Qwen-class models. Keep production memory placement unchanged.

## Trust and permissions

Project configuration follows existing content-based trust. Trusting a project
file authorizes using its configuration; the UI must explain that enabled stdio
servers are executable programs and can act as soon as they start. Do not launch
untrusted project servers during startup or metadata browsing. Prefer an installed
executable plus explicit arguments; omit automatic package installation in v1.

Use explicit environment-variable references for credentials and a documented
minimal inherited environment. Resolve secrets only when launching a server;
never write their values into events, config fingerprints, or UI previews.

Unknown MCP tools require approval. A server's read-only or destructive annotation
is informational, not an authorization rule. Users may explicitly allow a named
operation for the session; show that this covers future arguments too. Do not
remember an approval under a generic “MCP tool” reason: the current approver caches
by reason string, so introduce a structured scope or an equivalently precise key.
Bind grants to server configuration identity, tool/schema identity, and parent or
worker scope. One-call approval binds to the exact pending arguments. Workers
never inherit broader grants or credentials merely because the parent has them.

Show server, tool, workspace, arguments, and reason before execution. Redact known
secret fields without hiding meaningful destinations. Headless behavior uses the
existing unattended-refusal policy; explicit approve-all remains an explicit
bypass, not a default. Denial is a recoverable tool result and must not trigger a
retry loop. Approval waits use the existing awaiting-approval signal so the
supervisor does not mistake a person reading the prompt for a hung worker.

## Process and protocol boundary

Evaluate the official Rust SDK, `rmcp`, with a small fixture client first. Pin a
released compatible dependency and negotiated protocol versions after checking
MSRV, cancellation, child ownership, schema handling, and testability. Do not
hand-roll JSON-RPC because the happy-path transport looks small. The protocol
baseline must be chosen during implementation; this plan does not assume the
latest draft is the release to target.

A manager outside `tui.rs` owns connections and process lifetimes. Its states are
disabled, stopped, starting, ready, failed, and stopping. Keep server connections
session-scoped in the parent. Workspace-bound workers get separate instances;
shared remote-service instances are deferred until concurrency and credential
scope are explicitly designed.

Implement initialization/capability negotiation, paginated tool discovery, calls,
progress where supported, timeouts, and cancellation. Bound page count, frame and
result size, stderr capture, and concurrent calls. Start conservatively with one
call per connection. Advertise only supported client features. Server-initiated
sampling and elicitation are unsupported in v1; do not allow hidden model calls
or unexpected interactions. Resources, prompt catalogs, remote transports, OAuth,
and protocol task extensions are later slices.

On cancellation, request protocol cancellation where applicable, then enforce a
bounded shutdown if the process is unresponsive. Own and reap the process and
managed descendants. Cancellation cannot undo an external mutation. A lost
connection after dispatch has an uncertain outcome: do not automatically replay
that call. Reconnect for later operations with bounded attempts, never an endless
restart loop.

A worker's cwd and advertised roots point to its own workspace. These are routing
information, not containment. An external account mutation stays external even
when invoked from an isolated worker.

## Results, validation, and metrics

Adapt MCP results into the existing tool-result path with explicit execution-error
status. Preserve text and supported structured content, retaining provenance.
For v1, unsupported images/audio/resource content produces an explicit marker,
not a false success or silently dropped data. Do not fetch embedded URLs or load
server instructions automatically.

Add bounded session-local result storage when output exceeds the prompt budget.
Return a concise excerpt, total size, and an opaque handle for paged retrieval.
The handle must resolve only within that session; reuse must not rerun the remote
operation. Apply a disk cap and retention policy, report expiry, and treat stored
results as sensitive session data. MCP output needs its own truncation guidance:
the builtin cap currently suggests local grep/read, which cannot recover a remote
result. Do not generate an extra LLM summary by default.

An MCP success response is tool execution evidence, not proof the task is done.
Reuse the existing validator and bounded correction loop. Checks come from the
user/project task contract, not untrusted server descriptions. Worker reports
link the source operation, local diff, and validator result without promoting
remote content into durable memory automatically.

Emit structural events through `Agent::emit`, retaining parent/worker association.
Record server/tool identity, schema revision, start/finish, approval wait, execution
latency, result bytes delivered/omitted, failure category, and uncertain outcomes.
Keep secrets and raw arguments/results out of diagnostic events by default.
Existing session tool transcripts still need the same data-retention care.
Separate external tool latency from model latency and cost. Do not infer third-party
service charges from model pricing or declare tool success a solved task.

## Terminal experience

Reuse the skill browser's accessible two-pane layout for `/mcp`:

- Left: configured servers and filtered tools, with textual states such as
  disconnected, available, active, approval required, and disabled for workers.
- Right: selected tool purpose, arguments, server identity, workspace scope,
  permission, and last outcome. Full schema/details are inspectable on demand.
- Connection and activation are separate states. “Connected” never means every
  tool is in context or authorized. Display active-schema bytes alongside counts.
- Underlined heading plus `>` for focus; wrapped help/status, no composer overlap,
  and no reliance on color or emoji. `/` filters the list from either pane, using
  the existing skill browser's Escape behavior.

Provide equivalent inspect/activate/deactivate/refresh operations in the plain
REPL. Keep routine progress collapsed; expand a call for evidence and errors.
Do not build a second dashboard or put operational notices into the conversation.

## Delivery slices and acceptance gates

| Slice | Deliverable | Required evidence |
| --- | --- | --- |
| M11 | Workspace snapshot, scoped checks, reviewed apply, cleanup | Two workers edit the same path without collision; parent edits survive conflicting apply; cancellation and crash recovery preserve reviewable results |
| MCP A | Parent fixture server through the real adapter and approver | Discovery/activation/call works in TUI and plain REPL; untrusted config never launches; denied calls never dispatch |
| MCP B | Bounded results, stable schemas, lifecycle and events | Large output remains retrievable without replay; schema changes invalidate grants; timeout/crash never duplicates a mutation |
| MCP C | Task-scoped browser and isolated worker delegation | Narrow-terminal navigation works; worker cannot select undelegated tools; separate workspace-bound processes receive distinct roots/cwds |
| Measurement | Completed-task comparison | Objective checks, approval behavior, tool failures, input tokens, elapsed time, and cost reported together |

A and B are one release gate; do not ship a connectivity-only feature without
permission and process handling. C's parent browser can be developed alongside A;
worker enablement waits for M11 and the delegation tests.

Use a deterministic local fixture server and scripted `LlmClient` tests: no API
keys, network services, or live model required in CI. Include pagination loops,
malformed/oversized results, name collisions, unsupported schemas/content,
prompt-injection text, secret redaction, stale metadata, approval cancellation,
server crashes before/after dispatch, and process cleanup. Test both positive and
negative grant matching, not only the permission prompt. All session tests use
`common::isolate_home()`. Run `cargo test`, warning-clean Clippy, and PTY smoke tests
for implementation slices.

For live evaluation, use synthetic fixture tasks with executable validators:
retrieve an acceptance criterion, edit a file, run its check, and report evidence;
add a denied publish step and an interrupted mutation case. Compare eager versus
selected schemas on identical tasks/provider settings with multiple repetitions.
Record failed and uncertain outcomes as well as successes. Use OpenRouter while
the local B70 is occupied. Set the run budget before evaluation; no claimed quality
or cache benefit without data. M12 then measures role routing against this baseline.

## Sources and deferred decisions

The [official Rust SDK](https://github.com/modelcontextprotocol/rust-sdk) is the
first implementation candidate. MCP's [tool specification](https://modelcontextprotocol.io/specification/2025-06-18/server/tools)
explicitly treats untrusted annotations as unsuitable authorization evidence.
The [2025-11-25 protocol overview](https://modelcontextprotocol.io/specification/2025-11-25/basic)
describes capability negotiation and the distinction between stdio and HTTP
credential handling. Verify the chosen release's exact requirements at implementation.

Decide during the fixture spike: SDK/version/MSRV, provider schema compatibility,
initial byte/time/disk limits, exact config spelling, and structured approval-key
migration. These are bounded engineering choices; none requires a marketplace,
automatic tool selection model, or a new workflow language.
