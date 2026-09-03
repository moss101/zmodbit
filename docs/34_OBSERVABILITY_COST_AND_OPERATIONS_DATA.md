# Observability, Cost, and Operations Data

> **Authority date:** 2026-09-03  
> **Product:** Modbit — clean-slate implementation dossier  
> **Completion rule:** code is not “done” until it is wired through the real runtime and passes the release-gate real-system test with evidence.  
> **No-placeholder rule:** production code paths may not contain fake implementations, TODO return values, hard-coded success, disabled security checks, or UI-only simulations of unavailable behavior.


## Principles

Observability must explain **what happened, why the runtime decided it, what it cost, and what evidence proves it** without turning logs into a second source of truth.

## Tracing

OpenTelemetry spans use IDs: tenant (hashed/pseudonymous where exported), session, task, run, turn, step, tool call and sandbox. Trace propagation crosses Core→provider, Core→Sandbox Gateway→guest and Core→browser bridge.

Do not put raw prompts, source code, secrets or terminal contents into metrics labels.

## Metrics

### Runtime
- active/queued/waiting tasks;
- admitted subagents and capacity utilization;
- turn duration;
- tool success/application-failure/infra-failure/unknown-outcome;
- approval wait time;
- recovery/resume count and duration.

### Context
- retrieval latency by level L0-L3;
- candidates/read tokens/injected tokens;
- Context Pack precision proxy;
- prompt cache hit rate;
- compaction latency/stale rejection/fallback count;
- index freshness lag.

### Execution
- terminal attach/replay latency;
- sandbox provisioning/recovery duration;
- browser semantic snapshot/delta size;
- screenshot fallback rate;
- OutputRef spill/read volume.

### Model/cost
- input/output/cached tokens;
- cost per task/turn/provider;
- model first-token/total latency;
- provider error/rate-limit/failover rate.

### Reliability/security
- stale lease write rejection;
- protected effect approvals/denials;
- secret-handle use;
- cross-tenant authorization denials;
- emergency stops.

## User-visible evidence

Task review shows human-scale data, not telemetry internals: model used, commands/tests, changed files, verification status, browser/external effects, approvals and receipts. Cost can be shown per task with estimated/actual provider usage.

## Logs

Structured JSON logs have severity, component, event ID and error code. Raw content requires debug opt-in and redaction. Security audit records and Effect Ledger are separate from normal application logs.

## SLOs

Initial targets:
- 99.9% cloud control API availability monthly excluding planned maintenance;
- no acknowledged event loss;
- 99.9% successful resume for crash tests from supported durable states;
- 100% protected external effects either have receipt or explicit `UnknownOutcome` reconciliation record;
- zero cross-tenant resource access in security suite.
