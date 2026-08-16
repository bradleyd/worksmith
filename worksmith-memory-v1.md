# Worksmith — Memory Layer v1

## Purpose

This document defines the first version of the memory layer for a terminal-based agent harness written in Rust.

The tool is named **Worksmith** and the CLI command is `worksmith`. All persistent local state should use `.worksmith` as the application directory name.

The system may run multiple CLI sessions and may spawn multiple sub-agents/workers. The model may have a relatively small context window, so useful state must survive outside the model context.

The memory system must:

1. Preserve durable information across sessions.
2. Share useful project state between workers.
3. Keep global user/tool preferences separate from project-specific information.
4. Avoid saving transient or low-value agent output.
5. Support both exact lookup and semantic retrieval.
6. Be simple enough to implement locally with SQLite.
7. Remain understandable and inspectable by humans.
8. Allow memory to be corrected or superseded rather than silently overwritten.

The core rule is:

> Knowledge is source material. Memory is distilled state about the user, project, decisions, constraints, and lessons learned.

---

# 1. High-Level Architecture

The first version uses two durable memory databases:

```text
~/.worksmith/
├── config.toml
└── memory.db                  # global memory

repo/
└── .worksmith/
    ├── memory.db              # project memory
    └── knowledge.db           # optional/project knowledge index
```

Global and project memory have the same logical schema but different scope and lifecycle.

```text
                         Agent
                           |
                   Memory Service API
                           |
             +-------------+-------------+
             |                           |
             v                           v
      Global Memory DB             Project Memory DB
   ~/.worksmith/memory.db       repo/.cli-tool/memory.db
             |                           |
             +-------------+-------------+
                           |
                    rank + merge
                           |
                           v
                     Agent Context
```

Project knowledge is related but separate:

```text
MEMORY                               KNOWLEDGE
------                               ---------
What we decided                      What source material says
What user prefers                    Repository files
Constraints                          Architecture documents
Lessons learned                      Rust best-practice docs
Important discoveries                API documentation
Current durable state                Indexed README/docs/code
```

A knowledge database should be rebuildable.

A memory database should not be casually deleted.

---

# 2. Memory Scopes

Use explicit scopes.

## Global

Global memory survives across repositories and CLI sessions.

Examples:

- User prefers Rust over Python for the CLI implementation.
- User prefers concise terminal output.
- User normally wants tests run before an agent reports completion.
- The CLI should not automatically push Git branches.
- The user uses a particular coding convention across projects.

Global memory lives at:

```text
~/.worksmith/memory.db
```

## Project

Project memory belongs to one repository/workspace.

Examples:

- This project uses SQLite for durable memory.
- Workers communicate through actor-style mailboxes.
- The project requires Rust 2024 edition.
- `knowledge.db` is derived and may be rebuilt.
- The scheduler implementation was moved from polling to event-driven execution.

Project memory lives at:

```text
repo/.worksmith/memory.db
```

## Session

Session state is temporary.

Examples:

- Worker 7 is currently reviewing `src/memory.rs`.
- The current debugging hypothesis is a deadlock in the mailbox.
- Three tasks remain in this run.
- The current user request is to implement FTS retrieval.

Session state should normally live in process memory or a temporary session store.

It should not automatically become durable memory.

## Worker Scratch

Worker scratch is the least durable state.

Examples:

- Candidate files to inspect.
- Intermediate reasoning.
- Search results being evaluated.
- Temporary hypotheses.
- Partial tool output.

Worker scratch should disappear when the worker terminates unless selected information is promoted.

```text
worker scratch
      |
      | propose_memory()
      v
memory candidate
      |
      | accept/reject
      v
global/project memory
```

---

# 3. Memory Is Not a Transcript

Do not save every model message.

Bad architecture:

```text
every prompt
every response
every tool call
every search result
       |
       v
   memory.db
```

This creates a large collection of low-value text and makes retrieval worse over time.

Instead:

```text
conversation / tools / worker activity
                 |
                 v
          candidate memory
                 |
          should persist?
             /       \
            no       yes
            |         |
         discard   normalize
                      |
                      v
                  memory.db
```

A durable memory should normally represent one of these:

- decision
- constraint
- preference
- fact
- lesson

These five types are enough for v1.

---

# 4. Memory Types

## 4.1 Decision

A choice that should guide future work.

Example:

```text
type: decision
subject: memory.storage
content: Use separate SQLite databases for global and project memory.
scope: project
importance: high
```

Good decisions usually answer:

> What choice did we make that future agents should follow?

---

## 4.2 Constraint

A requirement or boundary that future work must respect.

Example:

```text
type: constraint
subject: workers.communication
content: Workers communicate through actor-style mailboxes. Logical broadcast must be brokered rather than waking every LLM worker.
scope: project
importance: high
```

---

## 4.3 Preference

A preference that changes how future work should be performed.

Example:

```text
type: preference
subject: implementation.complexity
content: Prefer simple local Rust components over distributed infrastructure unless scale requires it.
scope: global
importance: medium
```

---

## 4.4 Fact

A durable fact that is not easily or cheaply rediscovered from knowledge sources.

Example:

```text
type: fact
subject: deployment.gpu
content: The agent harness is normally run on a Runpod GPU instance.
scope: project
importance: medium
```

Facts that already exist clearly in repository files normally belong in project knowledge rather than memory.

---

## 4.5 Lesson

A conclusion learned from experience that should affect future behavior.

Example:

```text
type: lesson
subject: memory.worker-writes
content: Allowing workers to directly persist all findings creates noisy durable memory. Workers should propose memories and a memory manager should promote only durable findings.
scope: project
importance: high
```

---

# 5. SQLite Schema

A simple v1 schema:

```sql
CREATE TABLE memories (
    id              TEXT PRIMARY KEY,
    scope           TEXT NOT NULL,
    kind            TEXT NOT NULL,
    subject         TEXT NOT NULL,
    content         TEXT NOT NULL,

    source_type     TEXT NOT NULL,
    source_id       TEXT,

    importance      INTEGER NOT NULL DEFAULT 50,
    confidence      REAL NOT NULL DEFAULT 1.0,

    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    last_used_at    INTEGER,
    use_count       INTEGER NOT NULL DEFAULT 0,

    expires_at      INTEGER,

    supersedes_id   TEXT,
    status          TEXT NOT NULL DEFAULT 'active',

    FOREIGN KEY (supersedes_id) REFERENCES memories(id)
);
```

Recommended values:

```text
scope:
  global
  project

kind:
  decision
  constraint
  preference
  fact
  lesson

source_type:
  user
  agent
  worker
  tool
  system

status:
  candidate
  active
  superseded
  rejected
  expired
```

Indexes:

```sql
CREATE INDEX idx_memories_subject
ON memories(subject);

CREATE INDEX idx_memories_kind
ON memories(kind);

CREATE INDEX idx_memories_status
ON memories(status);

CREATE INDEX idx_memories_updated
ON memories(updated_at);
```

---

# 6. Events Versus Memories

Keep an append-only event stream separate from durable memories.

```sql
CREATE TABLE memory_events (
    id            TEXT PRIMARY KEY,
    event_type    TEXT NOT NULL,
    actor_id      TEXT,
    session_id    TEXT,
    payload       TEXT NOT NULL,
    created_at    INTEGER NOT NULL
);
```

Events may include:

```text
worker.spawned
worker.completed
memory.proposed
memory.accepted
memory.rejected
memory.superseded
memory.retrieved
session.started
session.completed
```

The distinction is important:

```text
EVENT
"Worker 4 proposed that actor mailboxes should be durable."

MEMORY
"Worker mailboxes are ephemeral; durable decisions belong in project memory."
```

Events answer:

> What happened?

Memory answers:

> What should future agents know?

---

# 7. What Triggers a Memory Write?

Memory writes should be event-driven and deliberate.

There are six primary triggers.

## Trigger 1 — Explicit User Request

Examples:

```text
"Remember that I always want cargo fmt run before completion."

"From now on, don't automatically push branches."

"Use SQLite for project memory going forward."
```

These should normally become memory immediately.

The system still determines whether the scope is global or project.

---

## Trigger 2 — A Decision Is Made

Example conversation:

```text
User:
Let's keep the knowledge index separate because it should be rebuildable.

Agent:
Agreed.
```

Candidate:

```text
kind: decision
subject: knowledge.storage
content: Store project knowledge separately from durable project memory because the knowledge index is rebuildable.
scope: project
```

---

## Trigger 3 — A Durable Constraint Appears

Example:

```text
"The GPU only has 24 GB, so workers cannot each load a separate model."
```

Candidate:

```text
kind: constraint
subject: workers.model-runtime
content: Workers must share the model runtime because the target GPU cannot support one model instance per worker.
scope: project
```

---

## Trigger 4 — A Worker Discovers Something Non-Obvious

Example:

A worker spends ten minutes debugging and discovers:

```text
SQLite writes were failing because every worker opened the database
with a different locking configuration.
```

This may be worth remembering if it changes future implementation behavior.

Candidate:

```text
kind: lesson
subject: sqlite.connection-policy
content: All worker SQLite connections must use the same WAL and busy-timeout configuration to avoid intermittent lock failures.
scope: project
```

---

## Trigger 5 — Task or Session Completion

At completion, perform a memory extraction pass.

Ask:

```text
What was learned during this task that:

1. changes future behavior?
2. records a decision?
3. captures a durable constraint?
4. prevents expensive rediscovery?
5. corrects existing memory?
```

Expected output:

```text
0-3 memory candidates
```

Most tasks should produce zero, one, or two memories.

Ten or twenty memory writes from a normal task is a warning sign.

---

## Trigger 6 — Existing Memory Is Corrected

Existing memory:

```text
Use one SQLite database for both project memory and project knowledge.
```

Later decision:

```text
Keep knowledge in knowledge.db so it remains independently rebuildable.
```

Do not silently overwrite the old row.

Create the new memory:

```text
new memory
    |
    +-- supersedes_id --> old memory
```

Mark old memory:

```text
status = superseded
```

This gives the system historical traceability.

---

# 8. Worker Memory Protocol

Workers should not have unrestricted durable memory writes.

Workers may call:

```rust
propose_memory(candidate)
```

Example:

```rust
MemoryCandidate {
    scope: Scope::Project,
    kind: MemoryKind::Lesson,
    subject: "sqlite.connection-policy",
    content: "All workers should use WAL mode and a common busy timeout.",
    confidence: 0.92,
    importance: 80,
    evidence: "...",
}
```

The memory service then decides:

```text
worker finding
     |
     v
propose_memory()
     |
     v
Memory Manager
     |
     +-- duplicate? ------> reject / merge
     |
     +-- transient? ------> reject
     |
     +-- knowledge fact? -> reject or route to knowledge
     |
     +-- useful later? ---> accept
                              |
                              v
                          memory.db
```

For v1, the "Memory Manager" can be:

1. deterministic rules;
2. one small-model classification call;
3. deduplication lookup;
4. SQLite write.

It does not need to be a separate running service.

---

# 9. Explicit Memory Evaluation Prompt

A smaller model should receive explicit instructions.

Suggested classifier prompt:

```text
You are evaluating whether information should be stored as durable agent memory.

Durable memory is information that will materially improve future work.

Valid memory types:
- decision
- constraint
- preference
- fact
- lesson

SAVE information when it:
- records a durable decision;
- records a persistent user preference;
- records a requirement or constraint;
- captures a non-obvious lesson likely to prevent repeated work;
- records a durable fact that is not already obvious from project source material;
- corrects or supersedes an existing memory.

DO NOT SAVE:
- intermediate reasoning;
- temporary hypotheses;
- ordinary tool output;
- file contents that can be retrieved from project knowledge;
- line numbers;
- routine implementation details;
- completed one-time actions;
- generic programming knowledge;
- duplicate information;
- verbose transcripts;
- facts unlikely to matter again.

Prefer ZERO memories when nothing durable was learned.

A normal completed task should produce 0-3 memory candidates.

Each memory must contain exactly one durable idea.

Return:
SAVE or DISCARD
scope
kind
subject
content
importance
reason
```

---

# 10. Good Memory Writes

## Good Example 1 — Architecture Decision

Source:

```text
"We'll keep global memory and project memory in separate SQLite databases."
```

Memory:

```text
scope: project
kind: decision
subject: memory.database-layout
content: Use separate SQLite databases for global memory and project memory.
```

Why good:

- Durable.
- Changes future architecture.
- Hard to infer if conversation context disappears.
- Concise.

---

## Good Example 2 — User Preference

Source:

```text
"I want spawned workers to be called workers rather than sub-agents."
```

Memory:

```text
scope: global
kind: preference
subject: terminology.worker
content: Refer to spawned agent processes as workers.
```

Why good:

- Repeatedly affects output and API terminology.
- Small and precise.

---

## Good Example 3 — Constraint

Source:

```text
"Do not start Cassandra or another distributed database. This needs to run locally."
```

Memory:

```text
scope: project
kind: constraint
subject: memory.infrastructure
content: The memory layer must run locally and must not require distributed database infrastructure.
```

---

## Good Example 4 — Debugging Lesson

Source:

A worker discovers through debugging that simultaneous writes are unreliable unless WAL is enabled.

Memory:

```text
scope: project
kind: lesson
subject: sqlite.wal
content: Enable WAL mode for project memory because multiple concurrent workers may read and write the database.
```

---

## Good Example 5 — Superseding Decision

Old:

```text
subject: project.index-storage
content: Store knowledge chunks in memory.db.
```

New:

```text
subject: project.index-storage
content: Store rebuildable project knowledge in knowledge.db rather than memory.db.
supersedes: <old-id>
```

---

# 11. Bad Memory Writes

## Bad Example 1 — File Location

```text
src/memory.rs contains the MemoryStore implementation.
```

Why bad:

The repository index can answer this.

This belongs in project knowledge, not memory.

---

## Bad Example 2 — Temporary Work

```text
Worker 7 is currently checking SQLite locking behavior.
```

Why bad:

This is session/worker state.

---

## Bad Example 3 — Tool Output

```text
cargo test returned 47 passed tests.
```

Why bad:

Usually temporary.

It may be useful in a task event log, but not durable memory.

Exception:

If the project has a persistent known baseline such as:

```text
The integration suite is expected to contain exactly 47 tests.
```

that could potentially become a fact.

---

## Bad Example 4 — Generic Knowledge

```text
Rust ownership prevents data races.
```

Why bad:

This is general knowledge, not project memory.

If the project needs Rust reference material, put it in knowledge/RAG.

---

## Bad Example 5 — Model Reasoning

```text
I think the deadlock might be inside the mailbox implementation.
```

Why bad:

A hypothesis is not a durable fact.

---

## Bad Example 6 — Verbose Transcript

```text
The user asked about memory and then we discussed SQLite and then
talked about Cassandra and eventually decided...
```

Why bad:

Store the final decision, not the conversational path.

Good replacement:

```text
Use SQLite for v1 durable memory; do not introduce Cassandra.
```

---

# 12. Memory Candidate Quality Rules

Before accepting a memory, test it against these questions.

```text
1. Will this probably matter in another session?
2. Would losing it cause repeated work, inconsistency, or a bad decision?
3. Is it something that cannot simply be retrieved from repository knowledge?
4. Is it sufficiently certain?
5. Can it be expressed in one or two concise sentences?
```

If most answers are no, discard it.

A useful memory should be understandable without the original conversation.

Bad:

```text
Use the second option we discussed.
```

Good:

```text
Use separate SQLite databases for durable project memory and rebuildable project knowledge.
```

---

# 13. Memory Subjects

Subjects should act like stable semantic keys.

Use dotted names:

```text
memory.database-layout
memory.write-policy
workers.communication
workers.broadcast
workers.lifecycle
rag.retrieval-policy
project.database
project.build-command
user.output-style
user.git-policy
```

Avoid overly specific keys:

```text
discussion.2026-08-16.sqlite.option-two
worker7.response14
```

The subject allows exact retrieval before semantic retrieval.

---

# 14. Retrieval

Memory retrieval should be hybrid.

Do not rely only on embeddings.

Recommended order:

```text
agent/task query
      |
      +--> exact subject/key matches
      |
      +--> FTS/BM25 matches
      |
      +--> vector similarity
      |
      +--> recency/importance weighting
      |
      v
  merge + dedupe
      |
      v
  top memories
      |
      v
 agent context
```

Possible score:

```text
score =
    semantic_similarity * 0.40
  + text_match          * 0.25
  + importance          * 0.20
  + recency             * 0.10
  + prior_use           * 0.05
```

Do not treat these weights as permanent. They are merely a reasonable v1 starting point.

---

# 15. RAG and Memory

RAG is a retrieval mechanism, not the memory itself.

```text
memory row
    |
    +--> SQLite row = source of truth
    |
    +--> FTS index
    |
    +--> embedding index
```

If the vector index disappears, it can be rebuilt from memory rows.

Similarly:

```text
knowledge document/chunk
    |
    +--> knowledge.db = source/index
```

Do not put embeddings in place of canonical text.

---

# 16. Global + Project Retrieval

An agent working inside a repository should normally query both databases.

```text
                    query
                      |
              +-------+-------+
              |               |
              v               v
        global memory    project memory
              |               |
              +-------+-------+
                      |
                rank + merge
                      |
                      v
                  context
```

Project memories should usually receive a ranking boost for project-specific tasks.

Example:

```text
Global:
"Prefer simple local infrastructure."

Project:
"Use SQLite for durable project memory."

Current task:
"Implement MemoryStore."

Both are relevant.
```

---

# 17. Context Injection

Do not load the entire memory database into the model context.

Construct a small memory section.

Example:

```text
<MEMORY>

Relevant global memories:
- Prefer simple local Rust components over distributed infrastructure.
- Call spawned agents "workers."

Relevant project memories:
- Use separate SQLite databases for global and project memory.
- Workers propose durable memories; they do not directly persist arbitrary findings.
- Project knowledge is rebuildable and stored separately from durable memory.

</MEMORY>
```

A small-context model benefits from concise retrieved memory much more than a large memory dump.

---

# 18. Spawned Worker Behavior

When `/spawn` starts a worker:

```text
/spawn "investigate SQLite concurrency"
```

The supervisor should provide:

```text
worker system instructions
        +
task
        +
relevant global memory
        +
relevant project memory
        +
relevant project knowledge
```

Not:

```text
all memory
+
all project docs
+
entire parent conversation
```

Worker lifecycle:

```text
             /spawn
                |
                v
        create worker identity
                |
                v
        retrieve relevant state
                |
                v
           execute task
                |
         +------+------+
         |             |
         v             v
      result       memory candidates
         |             |
         |             v
         |        memory manager
         |             |
         +-------------+
                |
                v
          worker exits
```

---

# 19. Broadcast and Memory

Broadcast messages are coordination, not durable memory.

Example:

```text
broadcast("Who has information about SQLite WAL behavior?")
```

The broker can search:

1. active worker capabilities;
2. worker mailboxes/state;
3. project memory;
4. project knowledge.

```text
broadcast request
       |
       v
     broker
       |
   +---+------------------+
   |          |           |
   v          v           v
workers     memory     knowledge
```

A useful answer may be returned without waking every worker.

If a worker discovers a durable fact during the broadcast, it may propose a memory afterward.

---

# 20. Memory Deduplication

Before writing:

```text
candidate
    |
    v
search same subject
    |
    +--> identical meaning ------> do nothing
    |
    +--> improved wording -------> update existing
    |
    +--> contradiction ----------> supersede existing
    |
    +--> genuinely different ----> insert
```

Example existing memory:

```text
Use SQLite for memory.
```

Candidate:

```text
Use SQLite as the local persistence layer for agent memory.
```

Do not create a second row.

Candidate:

```text
Use RocksDB instead of SQLite because write contention is unacceptable.
```

If accepted as a new decision, supersede the old memory.

---

# 21. Memory Expiration

Most durable memories do not need an expiration date.

But temporary durable facts may.

Example:

```text
kind: fact
subject: migration.current-phase
content: The migration is currently blocked waiting for the new schema.
expires_at: ...
```

Prefer not to store rapidly changing status as memory unless future sessions genuinely need it.

Task tracking may eventually deserve its own subsystem.

Do not turn memory into a task database.

---

# 22. Memory Importance

Simple range:

```text
0-100
```

Guideline:

```text
90-100  foundational constraint or architecture decision
70-89   important decision/lesson
40-69   useful preference/fact
10-39   low-value durable detail
0-9     probably should not have been saved
```

Importance should influence retrieval, not determine truth.

---

# 23. Confidence

Confidence indicates certainty of the memory.

```text
1.0   explicitly stated by user / verified
0.8   strong conclusion from tools or worker investigation
0.5   uncertain but potentially useful
<0.5  generally do not persist as durable memory
```

Do not persist speculative model guesses as facts.

---

# 24. Suggested Rust API

Keep the first API small.

```rust
pub enum MemoryScope {
    Global,
    Project,
}

pub enum MemoryKind {
    Decision,
    Constraint,
    Preference,
    Fact,
    Lesson,
}

pub struct MemoryCandidate {
    pub scope: MemoryScope,
    pub kind: MemoryKind,
    pub subject: String,
    pub content: String,
    pub importance: u8,
    pub confidence: f32,
    pub source_type: String,
    pub source_id: Option<String>,
}

pub struct Memory {
    pub id: String,
    pub candidate: MemoryCandidate,
    pub created_at: i64,
    pub updated_at: i64,
    pub supersedes_id: Option<String>,
}
```

Service:

```rust
pub trait MemoryStore {
    fn propose(&self, candidate: MemoryCandidate) -> Result<ProposalResult>;
    fn remember(&self, candidate: MemoryCandidate) -> Result<Memory>;
    fn forget(&self, id: &str) -> Result<()>;
    fn supersede(&self, old_id: &str, candidate: MemoryCandidate)
        -> Result<Memory>;

    fn get_by_subject(&self, subject: &str) -> Result<Vec<Memory>>;
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Memory>>;
}
```

Eventually:

```rust
memory.search(
    query,
    scopes = [Global, Project],
    limit = 10
)
```

---

# 25. CLI Commands for Debugging

Make memory observable.

Suggested commands:

```bash
/memory
/memory list
/memory search "SQLite"
/memory show <id>
/memory forget <id>
/memory global
/memory project
/memory candidates
```

Useful development command:

```bash
/memory explain "how should workers communicate?"
```

Output:

```text
Query: how should workers communicate?

1. [project][constraint][0.91]
   workers.communication
   Workers communicate through actor-style mailboxes.

2. [project][decision][0.84]
   workers.broadcast
   Logical broadcast is broker-mediated rather than sent to every LLM worker.
```

This will make retrieval bugs much easier to diagnose.

---

# 26. First-Version Write Policy

For v1, be conservative.

Persist automatically only when:

```text
A. the user explicitly requests memory;
B. a project decision is clearly made;
C. a durable project constraint is clearly established;
D. end-of-task extraction identifies a high-confidence durable lesson.
```

Everything else should be proposed or discarded.

This is intentionally conservative.

Bad memory is worse than missing memory because bad memory silently contaminates future agent behavior.

---

# 27. First-Version End-of-Task Flow

After a main task or worker task:

```text
task completes
     |
     v
extract candidate memories
     |
     v
0-3 candidates
     |
     v
dedupe against existing memory
     |
     +--> duplicate -> discard
     |
     +--> contradiction -> supersede candidate
     |
     +--> new durable item -> save
     |
     v
return task result
```

Example extraction input:

```text
Task:
Implement SQLite project memory.

Result:
Added MemoryStore using rusqlite. WAL is required because workers may
write concurrently. We chose separate global and project DBs. FTS is
not implemented yet.
```

Correct memories:

```text
1.
kind: decision
subject: memory.database-layout
content: Global and project memory use separate SQLite databases.

2.
kind: constraint
subject: memory.sqlite-wal
content: SQLite memory databases must use WAL mode because workers may access them concurrently.
```

Do NOT save:

```text
FTS is not implemented yet.
```

That is task/project status and belongs elsewhere.

---

# 28. Knowledge Example

Suppose the repository contains:

```text
docs/architecture.md
docs/rust-style.md
README.md
src/memory.rs
```

These are chunked into `knowledge.db`.

Example knowledge rows:

```text
source: docs/rust-style.md
chunk: "Prefer Result<T, E> ..."

source: docs/architecture.md
chunk: "Workers are supervised by ..."

source: src/memory.rs
chunk: "pub struct MemoryStore ..."
```

If an agent asks:

```text
"What does our architecture document say about worker supervision?"
```

Search knowledge.

If an agent asks:

```text
"What did we decide about durable worker memory after testing it?"
```

Search memory.

Often both searches can run and be merged.

---

# 29. Example: Full Workflow

User:

```text
Use SQLite for the first version. I don't want Redis or another service dependency.
```

System:

```text
candidate 1:
  scope: project
  kind: decision
  subject: memory.storage
  content: Use SQLite for the first version of durable memory.

candidate 2:
  scope: project
  kind: constraint
  subject: infrastructure.external-services
  content: The v1 memory layer must not require Redis or another external service.
```

Later:

```text
/spawn investigate-memory-concurrency
```

Worker discovers:

```text
WAL mode plus a shared busy timeout removes the lock failures in the test.
```

Worker returns result and proposes:

```text
kind: lesson
subject: memory.sqlite-concurrency
content: Configure SQLite memory connections with WAL mode and a common busy timeout for concurrent workers.
```

Memory manager:

```text
search existing subject
no duplicate
confidence high
durable
SAVE
```

Three weeks later a new CLI session asks:

```text
"Implement the second memory writer."
```

Retrieved context:

```text
- Use SQLite for v1 durable memory.
- Do not require Redis or another external service.
- Configure SQLite connections with WAL and a common busy timeout.
```

The model does not need the original conversation.

That is the goal.

---

# 30. Implementation Stages

## Stage 1 — Durable Store

Implement:

- global `memory.db`
- project `memory.db`
- schema migrations
- create/read/update/supersede
- exact subject lookup
- `/memory list`
- `/memory show`

No embeddings required yet.

---

## Stage 2 — FTS Retrieval

Add SQLite FTS5 over:

```text
subject
content
kind
```

Implement:

```bash
/memory search "worker communication"
```

Use keyword/BM25 ranking.

---

## Stage 3 — Memory Candidate Pipeline

Implement:

```text
propose_memory()
classify
dedupe
persist/reject
```

Add end-of-task extraction.

---

## Stage 4 — Semantic Retrieval

Add embeddings through a SQLite vector extension or another local index.

Search becomes:

```text
exact + FTS + vector + importance
```

Do not block v1 on vector retrieval.

---

## Stage 5 — Worker Integration

When `/spawn` creates a worker:

- retrieve relevant global memory;
- retrieve relevant project memory;
- provide task-specific knowledge;
- allow `propose_memory`;
- perform end-of-worker extraction.

---

## Stage 6 — Memory Maintenance

Later add:

- merge duplicates;
- expire temporary memories;
- lower importance for unused memories;
- inspect conflicting memories;
- compact related lessons;
- export/import memory;
- memory debugging UI.

Do not implement these before the basic system is useful.

---

# 31. Recommended v1 Design

For the first working implementation:

```text
GLOBAL
~/.worksmith/memory.db

PROJECT
repo/.worksmith/memory.db

PROJECT KNOWLEDGE
repo/.worksmith/knowledge.db
(optional until RAG indexing exists)

WORKER SCRATCH
in memory / session state

MEMORY TYPES
decision
constraint
preference
fact
lesson

RETRIEVAL
exact subject lookup
+
SQLite FTS5

WRITES
explicit user memory
+
clear decisions/constraints
+
end-of-task extraction

WORKERS
propose durable memory
do not directly save arbitrary findings
```

Do not begin with Cassandra, Redis, a distributed message broker, or a separate vector database.

Those can be introduced when there is an observed scaling problem.

---

# 32. Core Rules for the Agent

These rules should eventually appear in the agent's own system instructions.

```text
MEMORY RULES

1. Do not treat the conversation transcript as durable memory.

2. Store only information likely to materially improve future work.

3. Prefer zero memory writes over saving weak or transient information.

4. A normal task should produce no more than 0-3 durable memories.

5. Store decisions, constraints, preferences, durable facts, and lessons.

6. Do not store information that can be easily retrieved from project knowledge.

7. Do not store temporary hypotheses, tool output, line numbers, progress updates,
   or intermediate reasoning.

8. Worker agents propose memories. They do not automatically persist arbitrary state.

9. Each memory contains one concise, independently understandable idea.

10. When a new memory contradicts an existing memory, supersede the old memory
    rather than silently overwriting it.

11. Global memory applies across projects. Project memory applies only to the
    current repository.

12. Retrieve only memories relevant to the current task. Never inject the entire
    memory database into model context.

13. SQLite rows are the source of truth. FTS and embedding indexes are retrieval
    indexes and must be rebuildable.

14. Bad durable memory is more harmful than missing memory. Be conservative.
```

---

# 33. Definition of Done for Memory v1

Memory v1 is complete when:

- [ ] A CLI session can open the global memory database.
- [ ] A CLI session inside a repo can open/create project memory.
- [ ] The agent can store an explicit memory.
- [ ] The agent can distinguish global from project memory.
- [ ] Memories have one of the five supported types.
- [ ] Memories can be searched by exact subject.
- [ ] Memories can be searched through FTS.
- [ ] An existing memory can be superseded.
- [ ] Workers can propose memory candidates.
- [ ] Worker scratch disappears when the worker exits.
- [ ] End-of-task extraction produces at most a few candidates.
- [ ] Duplicate memory writes are rejected or merged.
- [ ] Retrieved memory can be injected into a new worker's context.
- [ ] `/memory list`, `/memory search`, and `/memory show` exist.
- [ ] The system does not automatically save transcripts or routine tool output.

Semantic/vector retrieval can come immediately after this baseline works.

---

# Final Mental Model

```text
                 RAW INFORMATION
       conversation / tools / workers
                      |
             +--------+--------+
             |                 |
             v                 v
        KNOWLEDGE           CANDIDATE
      source material         MEMORY
             |                  |
          chunk/index       evaluate
             |                  |
             v             +----+----+
      knowledge.db         |         |
                         reject     persist
                                      |
                         +------------+------------+
                         |                         |
                         v                         v
                   global memory            project memory
                         |                         |
                         +------------+------------+
                                      |
                                  retrieve
                                      |
                                      v
                                agent context
```

The durable memory layer should remain small enough that every stored item feels intentional.
