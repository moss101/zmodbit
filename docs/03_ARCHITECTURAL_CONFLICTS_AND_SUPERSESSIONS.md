# Architectural Conflicts and Supersessions

> **Purpose:** prevent historical project authority from silently re-entering the clean-slate Modbit build.

## Authority rule

Latest explicit user decision wins over older locks. A historical **LOCKED** row does not remain normative when a later explicit product decision replaces its premise. The mechanism knowledge may survive even when the old component is retired.

| Historical decision / assumption | Current disposition | Resolution | What survives |
|---|---|---|---|
| Code-OSS-derived desktop is foundational | **SUPERSEDED / REPLACED** | Clean-slate Modbit has no Code-OSS runtime dependency | Git/worktree, diagnostics, diff, IPC and workflow lessons only |
| Full IDE is primary product surface | **SUPERSEDED / REPLACED** | Agent-first Work + Code workspace | revision-bound code review/inspection; no full editor ownership |
| Monaco/native editor as new shell | **REJECTED** | Do not build a general editor | code rendering/diff components only |
| Separate Modbit Lite | **SUPERSEDED / RETIRED** | One Modbit product | durable-state/sandbox lessons merge into Modbit |
| Code-OSS fallback inside main product | **REJECTED** | Any future editor adapter is external/optional and cannot own Core | protocol compatibility only |
| Firecracker-specific cloud backend as architecture | **SUPERSEDED** | Current substrate is a hardened MicroVM-class sandbox behind the Modbit Sandbox Gateway | replaceable SandboxBackend interface |
| Memory can represent resumed session truth | **REJECTED** | Memory is not recovery | Engineering Memory remains separate curated layer |
| General local SLM runtime as product subsystem | **REJECTED / CANCELLED** | No production SLM subsystem | non-generative embeddings or explicit provider vision bridge are infrastructure, not local reasoning authority |
| Vector search as main/sole retrieval | **REJECTED** | Hybrid + structural retrieval planner | vector signal remains one candidate source |
| Eager expose all tools | **REJECTED** | task-scoped projection + deferred tool tail | full registry remains host-side |
| Unbounded parallel agents | **REJECTED** | transactional admission, capacity, worktree/conflict controls | bounded parallelism for separable work |
| Mandatory LLM judge on every task | **REJECTED** | deterministic/evidence verification baseline | optional semantic verifier remains eval/policy-gated |
| External reference code/service dependency | **REJECTED** | clean-room reimplementation of mechanisms | source provenance and benchmark references |
| Evolution-lab research as new general memory/runtime | **REJECTED FORM** | Skill Evolution Lab is isolated evaluation subsystem under existing Skill Registry/Eval architecture | trace/wiki/skill separation and gated promotion |
| External-reference tool namespaces as canonical | **REJECTED FORM** | normalize to Modbit tool intent/effect contracts | capabilities and conformance tests |

## Retain / modify / replace / reconsider calls

- **RETAIN:** Core-authoritative state, WorkGraph/AgentGraph/StateGraph, Capability Kernel, Context Engine, Effect/Evidence Ledger, durable terminal, live browser, worktree isolation and real E2E completion semantics.
- **MODIFY:** skills now include an eval-gated evolution pipeline; file/tool results now have a first-class MediaEnvelope; input steering is explicit `STEER/COLLECT/FOLLOW_UP`.
- **REPLACE:** all Code-OSS/IDE-foundation decisions with the clean-slate Work + Code shell and Core APIs.
- **RECONSIDER only with evidence:** adaptive/JIT profile generation, cross-model skill evolution defaults, private-worker reverse connect and enterprise TLS interception.

## Migration rule for old implementation code

Old source is **donor material only**. To enter the new repository it must be architecture-independent, license-clean, covered by tests, free of Code-OSS assumptions at its boundary and conform to the new canonical contracts. Existing implementation does not inherit old authority simply because it already exists.
