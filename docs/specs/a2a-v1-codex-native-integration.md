# A2A v1.0.0 Spec-Driven Design for Codex-Native Integration

> Goal: evolve `codex-a2a-rs` from a CLI-wrapping bridge into a spec-aligned A2A front end over Codex-native session and execution surfaces.

## Scope

This document defines the target protocol shape and migration path for `li-yifei/codex-a2a-rs`.

The immediate objective is stronger alignment with the latest A2A spec while preserving the useful Codex-specific capabilities already proven in the current bridge.

## Normative baseline

This design uses A2A `v1.0.0` as the protocol baseline.

Primary references checked during drafting:
- `a2aproject/A2A` tag `v1.0.0`
- `specification/a2a.proto`
- `docs/specification.md`
- `docs/topics/life-of-a-task.md`
- `docs/topics/extensions.md`

Relevant spec facts:
- Core send operation is `message/send`.
- Core streaming send operation is `message/stream`.
- Core task operations include `tasks/get`, `tasks/list`, and `tasks/cancel`.
- A2A multi-turn continuity is grouped by `contextId`; follow-up requests can also include `taskId` and `referenceTaskIds`.
- Agent Card security is described via `security_schemes` and `security_requirements`.
- Protocol extensions are first-class and are declared in `AgentCapabilities.extensions`.
- `Message` objects can also carry extension URIs via `message.extensions`.
- Extension methods are valid, but they should be explicitly named and advertised as extensions rather than silently replacing core behavior.

## Current server shape

Current `codex-a2a-rs` behavior in this repo:
- Supports discovery via `/.well-known/agent.json` and `/.well-known/agent-card.json`.
- Supports JSON-RPC over `POST /` and `POST /a2a/jsonrpc`.
- Supports `message/send` and the legacy alias `tasks/send`.
- Supports `tasks/get`.
- Returns `tasks/cancel` as unsupported.
- Adds non-core methods `sessions/list` and `sessions/resume`.
- Uses a local map from A2A `contextId` to Codex session/thread id.
- Uses Codex CLI as the execution backend.
- Exposes controlled write mode through request metadata.

## Main gaps against the v1.0.0 baseline

### 1. Core operation coverage is incomplete

The spec expects `message/send`, `message/stream`, `tasks/get`, `tasks/list`, and `tasks/cancel` as the core interaction set.

Current gaps:
- `message/stream` is missing.
- `tasks/list` is missing.
- `tasks/cancel` is declared but not implemented.

### 2. Agent Card metadata still reflects an older protocol shape

The current card still advertises `protocolVersion: 0.2.0`.

The next implementation round should explicitly move the public card to a v1-shaped declaration, including the v1 protocol version and the richer Agent Card fields required by the current spec.

### 3. Codex session features are useful but currently non-standardized

`sessions/list` and `sessions/resume` are valuable, especially for Codex continuity and operator workflows.

These methods should remain available, but they should move behind an explicit extension contract instead of living as undocumented custom RPCs.

### 4. Agent Card shape should move closer to the v1 schema

The current card is simple and works for lightweight clients.

The target card should explicitly describe:
- supported interfaces
- security schemes
- security requirements
- default input/output modes
- extension declarations
- skill-level descriptions for both core task handling and Codex session operations

### 5. The transport layer is more spec-aligned than the execution layer

The current server is an A2A server around `codex exec` and `codex exec resume`.

That is acceptable as a transitional implementation, but the long-term shape should treat Codex-native session/app-server primitives as the source of truth and keep A2A as a thin protocol layer.

## Design decision

Build the next version as a **spec-aligned A2A core plus a Codex session extension**.

That means:
1. Implement the A2A v1.0.0 core methods faithfully.
2. Keep Codex-specific session affordances as an explicit extension.
3. Preserve the current CLI-backed backend as an adapter layer for now.
4. Leave room to swap the backend from `codex exec` to Codex-native app-server or session APIs later.

### Proposed protocol model

### Core methods

The server should support these core JSON-RPC methods:
- `message/send`
- `message/stream`
- `tasks/get`
- `tasks/list`
- `tasks/cancel`

Behavior rules:
- `message/send` returns either a direct `Message` or a `Task`.
- For this server, default behavior can stay task-oriented for durable traceability.
- `message/stream` should stream status/artifact updates when streaming is enabled.
- `tasks/get` returns the latest task state.
- `tasks/list` lists tasks visible to the authenticated caller, ordered by last update time descending.
- `tasks/list` should also support an optional `contextId` filter so multi-session clients can query one conversational lane efficiently.
- `tasks/cancel` should attempt best-effort cancellation of in-flight Codex work and report terminal task state.

### Context and continuation

Use A2A `contextId` as the primary continuity handle.

Mapping rules:
- one A2A `contextId` maps to one Codex session/thread id
- `taskId` tracks one A2A unit of work inside that context
- follow-up requests use the same `contextId`
- `referenceTaskIds` can point at prior tasks for refinement
- `sessionId` is accepted only as a deprecated compatibility alias for `contextId` during migration
- `sessions/resume` can bind an existing Codex session id to a new or existing A2A `contextId`

This matches the spec's model better than treating task id as the main continuation key.

## Codex session extension

Define one explicit extension URI for Codex-only session affordances.

Working placeholder:
- `urn:codex-a2a:extensions:codex-sessions:v1`

Extension scope:
- `sessions/list`
- `sessions/resume`
- optional future `sessions/get` or `sessions/fork`
- structured metadata for exposing Codex-native session identifiers

Agent Card requirements for this extension:
- declare it in `capabilities.extensions`
- mark it `required: false`
- document the RPC methods and parameters in extension docs

Client behavior:
- A plain A2A client can still use core task methods without knowing the extension.
- A Codex-aware client can opt into richer session continuity features.

## Controlled write mode

Keep controlled write mode as an explicit extension-scoped policy rather than an undocumented ad hoc contract.

Recommended direction:
- prefer a structured extension payload from the start so the Codex-specific write contract stays isolated from generic A2A metadata
- keep a short migration window where metadata aliases are accepted if compatibility with existing clients matters
- document one canonical extension payload shape and one deprecation timeline for legacy metadata keys

A practical model is a `codexPolicy` object carried in extension-scoped request metadata or extension params with fields such as:
- `writeMode`
- `workingDirectory`
- optional sandbox-related knobs

This keeps the extension boundary explicit while preserving room for compatibility shims during migration.

## Security model

Adopt the v1 Agent Card security fields explicitly.

For current bearer-token deployment:
- publish one named HTTP bearer security scheme
- declare corresponding `security_requirements`
- continue using transport-layer auth rather than payload-layer identity

The current Linux shim for `security find-generic-password` is a deployment detail.
It should stay outside the protocol spec and outside the extension contract.

## Storage model

Persist these records separately from process-local in-memory maps:

### Context registry
- `contextId -> codex_session_id`
- metadata: created_at, updated_at, backend_kind, resume_source

### Task registry
- `taskId -> task state`
- fields needed for `tasks/get`, `tasks/list`, cancellation, timestamps, artifacts, and trace data

### Optional task index by context
- `contextId -> [taskIds]`

This makes `tasks/list` a first-class capability and reduces overloading of Codex session files as the only source of truth.

## Streaming design

Implement `message/stream` with a long-lived response channel that emits incremental updates derived from Codex execution.

Transport note:
- the current implementation uses HTTP chunked streaming with newline-delimited JSON events
- `Content-Type` is `application/x-ndjson`
- a later backend or server refactor could also move this to a stronger event transport if Codex-native surfaces make that easier

Minimum acceptable behavior:
- initial `Task`
- zero or more `tasks/status-update` events
- one terminal `tasks/final` event
- stream closes on terminal state

Transitional source of events:
- start the same durable task path as `message/send`
- poll persisted task state and emit status/final events

Long-term source of events:
- consume Codex-native app-server or exec-server event streams directly

## Cancellation design

`tasks/cancel` should move from unsupported to best-effort support.

Transitional implementation:
- keep the child process id for in-flight Codex runs
- send `TERM`, then `KILL`, on cancel request
- mark task terminal state as canceled
- include cancellation details in task metadata or artifacts

Long-term implementation:
- use Codex-native session/app-server cancellation primitives when available

## Migration plan

### Phase 1: protocol cleanup
- keep existing server architecture
- add minimal persistent storage for context and task registries so `tasks/list` remains useful across restarts
- add v1-shaped Agent Card fields, including protocol version `1.0.0`
- add `tasks/list`
- add explicit extension declaration for Codex sessions
- keep `sessions/list` and `sessions/resume`
- accept `sessionId` only as a deprecated compatibility alias for `contextId`
- treat `tasks/send` as a deprecated alias

### Phase 2: streaming and cancellation
- implement `message/stream`
- refactor execution tracking so in-flight Codex runs retain child-process handles or equivalent cancellation handles
- implement best-effort `tasks/cancel`
- enrich persisted task state for reliable polling, listing, and cancellation reporting

### Phase 3: backend separation
- extract a Codex backend trait/interface
- keep current CLI backend as one implementation
- prepare a second backend for Codex-native app-server or exec-server integration

### Phase 4: Codex-native execution surface
- switch session continuity and streaming to Codex-native backend primitives
- minimize shelling out through CLI where native surfaces exist

## Non-goals for the next iteration

- full push notification support
- multi-tenant authenticated extended agent cards
- genericizing the server for non-Codex backends
- inventing a new orchestration protocol beyond the A2A extension mechanism

## Acceptance criteria

A next-round implementation should satisfy all of these:
- discovery returns a v1-oriented Agent Card with explicit security and extension declarations
- Agent Card protocol version is `1.0.0`
- `message/send` is the primary documented method
- `tasks/send` remains only as a deprecated compatibility alias
- `sessionId` remains only as a deprecated compatibility alias for `contextId` during migration
- `tasks/get` and `tasks/list` work against persisted task state
- `message/stream` is implemented or clearly marked unavailable in capabilities until finished
- `tasks/cancel` has defined behavior
- `sessions/list` and `sessions/resume` are documented as a Codex session extension
- A2A `contextId` is the primary continuity key
- backend code is structured so Codex-native app-server integration can replace CLI wrapping later without rewriting the A2A protocol layer

## Open questions for review

1. Should this server remain task-first, or should trivial interactions return direct `Message` objects?
2. Should the Codex session extension include `sessions/fork` from the start?
3. Should controlled write mode stay in request metadata or move immediately into a more structured extension payload?
4. Is `tasks/list` enough for operator visibility, or should task search/history become a second extension later?
5. Should `message/stream` and `tasks/cancel` ship together, or is there a strong reason to stage one before the other?

## Recommendation

The cleanest path is:
- ship a spec-aligned A2A core
- formalize Codex-only features as an extension
- keep the CLI backend temporarily
- design the backend seam so a future Codex-native app-server integration becomes a backend swap instead of another protocol rewrite
