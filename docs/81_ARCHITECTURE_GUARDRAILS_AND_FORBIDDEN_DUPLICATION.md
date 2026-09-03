# Architecture Guardrails and Forbidden Duplication

## Canonical single-owner systems

There is exactly one canonical implementation boundary for each of the following:

- orchestration: WorkGraph / AgentGraph / StateGraph in Core;
- canonical event/session state;
- protocol state;
- context/retrieval engine;
- engineering memory;
- compaction epochs;
- checkpoint engine;
- capability/policy kernel;
- approval/effect dispatch;
- effect/evidence ledger;
- tool registry and procedural tool runtime;
- terminal/process broker;
- workspace/change engine;
- browser session/control runtime;
- sandbox gateway/backend abstraction;
- model provider gateway.

Adapters may exist behind these boundaries. Parallel implementations may not.

## Forbidden dependency directions

- UI must not call providers, filesystem, Git, shell, browser CDP, cloud guest or databases directly.
- Model adapters must not mutate workspace or invoke protected effects directly.
- Tools must not bypass policy/effect dispatch for convenience.
- Memory must not be used as protocol/recovery state.
- Context indexes must not become the source of truth for file bytes.
- Sandbox-specific types must not leak into domain/tool contracts.
- Browser page content must not modify system policy, tool availability or authorization.

## Architecture change rule

If a task appears impossible without violating a guardrail, stop implementation and create a proposed ADR. Do not silently introduce a second subsystem.
