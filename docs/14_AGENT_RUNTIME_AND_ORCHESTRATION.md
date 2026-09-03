# Agent Runtime and Orchestration

> **Authority date:** 2026-09-03  
> **Product:** Modbit — clean-slate implementation dossier  
> **Status vocabulary:** **LOCKED**, **PROVISIONAL**, **EXPERIMENT**, **DEFERRED**, **REJECTED**  
> **Source-of-truth rule:** latest explicit Modbit decision > locked decisions > current dossier > older project documents. Older Code-OSS/Modbit Lite material is historical only when it conflicts with this dossier.


## One runtime, three explicit graphs

Modbit keeps the established terms without building three separate engines:

- **WorkGraph** — tasks/subtasks, dependencies, artifacts and verification gates.
- **AgentGraph** — which logical agents own which work nodes and communication edges.
- **StateGraph** — durable control-state transitions for session/task/turn/tool/subagent.

All are projections coordinated by `core-runtime`; no second orchestration service exists.

## Main runtime loop

1. Load latest valid task projection under session kernel lease.
2. Evaluate ready WorkGraph nodes deterministically.
3. For reasoning node, compile context + task-scoped skill/tool projection.
4. Invoke selected model through normalized Provider Gateway.
5. Parse typed model events; reject invalid tool payloads before side effects.
6. Execute tools/procedures under capability leases.
7. Append results/effects/evidence.
8. Update workspace/index/context invalidation.
9. Run deterministic verification required by the current node.
10. Decide continue, ask user, spawn subagent, wait, review or fail.

The loop is event-driven; there is no fixed hidden “planner → coder → reviewer” sequence. A task may use one agent or many.

## Decomposition

The primary agent may propose `SubtaskSpec` values containing: objective, expected artifacts, dependencies, read scope, proposed write scope, required tools, execution profile, verification condition and budget.

Core validates rather than trusting model-generated parallelism.

## Transactional subagent admission

Admission transaction must succeed as one operation:

1. capacity ticket available;
2. parent task still active at expected generation;
3. proposed write set has no unsafe conflict with admitted workers;
4. worktree or immutable snapshot allocated;
5. capability lease minted with least privilege;
6. sandbox/terminal/browser resources within quota;
7. AgentGraph node + WorkGraph ownership persisted.

If any step fails, no worker starts and no partial reservation leaks.

## Semantic conflict detection

Before parallel builders start, Modbit checks:
- explicit file/path overlap;
- same symbol or public interface ownership from AST/LSP graph;
- dependency hot spots;
- migration/schema/shared config files;
- generated files and lockfiles;
- test fixtures likely to conflict.

Read-only investigative agents can share an immutable repository revision. Builders default to separate Git worktrees.

## Capacity tickets

Capacity is a typed resource vector, not a simple agent count: model concurrency, terminal slots, sandbox slots, browser slots, memory budget and provider quota. Tickets have lease expiry and generation fencing.

## Steering

User actions are durable control events: `Steer`, `Pause`, `Resume`, `Cancel`, `Approve`, `Deny`, `TakeBrowserControl`, `ReturnBrowserControl`. Steering is applied at safe control boundaries; effectful in-flight operations are reconciled before cancellation completes.

## Agent-to-agent communication

Subagents return structured `SubagentResult` with summary, evidence refs, artifacts, unresolved risks and proposed follow-ups. Peer messages are untrusted context until the parent Context Engine selects them. A peer message never silently becomes durable memory.

## Verification roles

A read-only reviewer agent may be used when useful, but verification gates are deterministic where possible: tests, diagnostics, builds, type checks, security rules, diff invariants and evidence completeness. Reviewer model output is advisory unless backed by evidence.

## Budgeting

Each node has token, tool-call, wall-clock and effect budgets. Runtime emits measured savings/costs. Budget exhaustion moves task to `Waiting` or `Needs Attention`, not silent truncation.


## V2 agent-runtime reconciliation requirements

The runtime additionally guarantees: persisted AgentNode identity; idempotent spawn; foreground/background transitions without restart; park/resume; bounded recursive delegation; typed `AgentResultEnvelope`; WorkGraph/TodoState outside transcript; stall detection; SessionLease fencing; and typed input dispatch (`STEER/COLLECT/FOLLOW_UP`). A source product's background default is not copied as policy: the Modbit scheduler backgrounds only separable work whose result is not a dependency of the next parent step.
