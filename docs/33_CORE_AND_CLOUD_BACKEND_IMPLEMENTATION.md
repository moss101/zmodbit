# Core and Cloud Backend Implementation

> **Authority date:** 2026-09-03  
> **Product:** Modbit — clean-slate implementation dossier  
> **Completion rule:** code is not “done” until it is wired through the real runtime and passes the release-gate real-system test with evidence.  
> **No-placeholder rule:** production code paths may not contain fake implementations, TODO return values, hard-coded success, disabled security checks, or UI-only simulations of unavailable behavior.


## Rust runtime composition

`modbit-core` uses Tokio for async orchestration and explicit bounded channels. Every background worker has cancellation token, owner aggregate, capacity class and shutdown deadline. Unbounded `mpsc` channels are forbidden in event/tool/output paths.

## Startup sequence

1. Load config and installation identity.
2. Open/migrate SQLite with integrity checks.
3. Initialize object store and verify writable space.
4. Acquire Core singleton lock for user profile.
5. Start event/projection services.
6. Reconcile unfinished sessions/protocol state.
7. Start terminal broker client and diagnostics supervisors.
8. Initialize provider registry and health probes.
9. Initialize repository index manager.
10. Bind authenticated local IPC.
11. Emit `CoreReady` only after recovery completes.

## Shutdown

Stop accepting new mutating commands, checkpoint active tasks if policy allows, flush event store, detach rather than kill durable terminal/sandbox resources that should survive, revoke expired leases and persist final cursor. Forced shutdown path is separately chaos-tested.

## Scheduler

A single scheduler evaluates ready WorkGraph nodes and capacity tickets. It never directly executes privileged I/O; it issues typed commands to tool/execution components. Scheduler decisions are events so recovery can explain why a node started or waited.

## Session kernel lease

Each active session has one execution owner guarded by generation-fenced lease. Local mode uses DB/file lock; cloud mode uses Postgres lease with expiry/heartbeat. A stale owner can append audit events but cannot advance state after lease loss.

## Verification engine

Verification plan is materialized from task type, changed files, repository config and agent proposals. Deterministic steps include build/typecheck/lint/test/diagnostics/diff/security rules. Agent can propose extra checks but cannot delete mandatory policy checks.

## Cloud API implementation

Rust HTTP service with structured auth middleware, request IDs, tenant context and rate limits. It writes commands/events to Postgres and streams committed events. It does not run model/tool code in request handlers.

## Cloud worker lifecycle

Worker claims ready session lease, loads projection/protocol state, executes Core runtime, renews lease, writes heartbeat/metrics and releases lease on safe shutdown. If heartbeat expires, another worker waits fencing grace period and then resumes with a higher generation.

## Sandbox Gateway

Gateway validates bearer/mTLS worker identity + tenant/session/sandbox lease. It compiles capability into MicroVM substrate network/mount/resource policy, starts `modbit-guest`, exchanges ephemeral guest credential and returns a sandbox handle. All guest calls are signed/nonce-protected or carried on mutually authenticated channel.

## Provider requests

Provider Gateway records request metadata before network dispatch and model usage after completion. Raw hidden reasoning is not persisted unless explicitly exposed by provider and permitted; normal model messages/tool calls are stored as protocol/evidence according to retention policy.

## Backpressure

- Model deltas: coalesce small text fragments before UI event publication.
- Terminal/browser streams: cursor + bounded ring + OutputRef spill.
- Tool payloads: inline size ceiling; larger objects by ref.
- Event stream: per-client bounded queue; slow clients reconnect/replay rather than block Core.

## Idempotency

All mutating commands and tool dispatches have stable IDs. Cloud API stores command result or accepted event pointer. Retried HTTP requests never create duplicate tasks/approvals/effects.
