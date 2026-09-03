# End-to-End System Architecture

> **Authority date:** 2026-09-03  
> **Product:** Modbit — clean-slate implementation dossier  
> **Status vocabulary:** **LOCKED**, **PROVISIONAL**, **EXPERIMENT**, **DEFERRED**, **REJECTED**  
> **Source-of-truth rule:** latest explicit Modbit decision > locked decisions > current dossier > older project documents. Older Code-OSS/Modbit Lite material is historical only when it conflicts with this dossier.


## Architecture principles

1. **One canonical runtime contract.** Desktop-local and cloud-remote modes use the same domain crate, event envelope, tool contracts and policy semantics.
2. **Surfaces are clients.** UI cannot own authoritative task state or hidden business logic.
3. **State before transcript.** Resume derives from structured state/event/protocol/checkpoint records, never LLM reconstruction.
4. **Tools are effects.** Every capability is typed, policy-checked and evidence-producing.
5. **Context is compiled.** Retrieval, provenance, token economics and compaction are explicit subsystems.
6. **Isolation is an execution concern, not the brain.** MicroVM substrate is a substrate behind Modbit policy, not the orchestrator.
7. **Real verification closes the loop.** The agent cannot self-declare implementation success.

## Logical architecture

```text
┌──────────────────────────── Desktop ──────────────────────────────┐
│ React/TS Renderer                                                  │
│  └─ Preload SurfaceProtocol                                       │
│       └─ Electron Main / Surface Host                              │
│            ├─ Local IPC client                                    │
│            ├─ Browser WebContents host                            │
│            └─ OS integrations/keychain                            │
└───────────────────────────┬───────────────────────────────────────┘
                            │ authenticated local framed protocol
                            ▼
┌──────────────────────── Modbit Core ──────────────────────────────┐
│ Domain + StateGraph + WorkGraph + AgentGraph                       │
│ Agent Runtime / Scheduler / Capacity                               │
│ Model Router + Provider Gateway                                    │
│ Tool Registry + Capability Kernel + Procedural Runtime             │
│ Context/Retrieval + Prompt/Skill Compiler                          │
│ Workspace/Git + Terminal + Browser adapters                        │
│ Event Store + Protocol State + Compaction + Checkpoint Manager     │
│ Engineering Memory + Evidence/Effect Ledger                        │
│ Verification Engine + Observability                                │
└───────┬───────────────────────┬───────────────────────┬───────────┘
        │                       │                       │
        ▼                       ▼                       ▼
 trusted local host      Sandbox Gateway         Model Providers
 filesystem/processes       │                  / embedding provider
 browser session            ▼
                       isolated MicroVM
                       guest RPC + PTY + Chrome

Cloud mode:
Desktop ─HTTPS/WSS─ Cloud API ─ durable DB/object store ─ Cloud Core Worker
                                      │
                                      └─ Sandbox Gateway → MicroVM
```

## Deployment units

### Desktop application
Unprivileged renderer plus hardened Electron main. Renderer has no Node integration and no direct filesystem/process access. All privileged operations go through SurfaceProtocol → Core.

### `modbit-core`
Rust daemon/service containing canonical local runtime. It owns persistent SQLite stores under the user data directory, repository index metadata, policy decisions and local execution coordination.

### `modbit-execd`
Small Rust PTY/process broker. It exists solely to keep durable process/terminal sessions attachable across renderer/Core detachment and to provide cursor-based replay. It is not a second scheduler or policy engine.

### `modbit-guest`
Guest agent image component in cloud MicroVMs. Implements typed capability-bound RPC for process, filesystem, Git, browser endpoint discovery and artifact transfer. It cannot mint capabilities or fetch secrets independently.

### Cloud API / Cloud Core Worker
Cloud API handles auth, session directory, remote task control, event streaming and artifact URLs. Cloud Core Worker runs the same agent runtime/domain crates as local Core. Cloud workers are horizontally scalable and session-leased.

### Sandbox Gateway
Tenant-authenticated boundary between Core workers and the MicroVM substrate. Validates sandbox lease, capability and effect class; brokers dynamic secret handles and network policy.

## Trust boundaries

1. **Renderer boundary** — assume web-rendered UI can be compromised; no raw secrets or privileged APIs.
2. **Browser content boundary** — all page content is untrusted data; browser instructions cannot alter system/tool policy.
3. **Model boundary** — model outputs are proposals until validated by typed tool schemas and policy.
4. **MCP/external tool boundary** — discovered schemas and returned content are untrusted; namespace + capability + size limits mandatory.
5. **Sandbox boundary** — guest is hostile/compromisable; deny internal network by default; no static credentials.
6. **Cloud tenant boundary** — every session, sandbox, object, event and secret reference is tenant-bound.

## Data flow for one coding step

1. Task Scheduler selects ready Agent node under capacity ticket.
2. Context Engine compiles a Context Pack for current immutable workspace revision.
3. Prompt/Skill Compiler produces model request with task-scoped tool projection.
4. Provider Gateway streams normalized ModelEvents.
5. Model emits direct tool call or Procedural Runtime program.
6. Tool Registry validates schema; Capability Kernel preflights effect and approval.
7. Executor performs operation locally or in sandbox.
8. Tool result and file change events append to Canonical Event Store; large output becomes OutputRef.
9. Effect Ledger records receipt where applicable.
10. Workspace revision/index invalidation occurs.
11. Verification Engine schedules deterministic checks.
12. Failure output returns as evidence for repair rather than crashing the entire turn.

## Failure containment

- Renderer failure: Core continues; UI re-subscribes by event cursor.
- Core failure: structured stores + checkpoints reconstruct state; terminal broker/sandbox sessions reattach by lease/cursor.
- Model provider failure: router applies bounded retry/failover according to policy; no duplicate effectful tool execution.
- Tool stream loss: call ID is idempotency key; Core queries effect receipt before retry.
- Sandbox loss: restore from latest valid checkpoint into a new MicroVM; effects outside sandbox remain ledgered and are never replayed blindly.
- Browser bridge loss: session state and control lease determine reconnect; no new browser session unless old one is irrecoverable.

## Architectural anti-duplication rule

Every proposed component must map to one owner listed above. A new subsystem is rejected if it duplicates scheduling, state, policy, memory, effect tracking, context retrieval or recovery semantics already owned by Core.


## V2 cross-cutting additions

Two cross-cutting services are now explicit but **do not become new authorities**:

- **Media Pipeline** — owned beside Artifact/File + Model Gateway boundaries. It validates, budgets, transforms and provenance-binds images/PDF/audio/video before provider egress. It owns no task state, policy or context selection.
- **Skill Evolution Lab** — an offline/eval workflow around the existing Skill Registry and Eval Harness. Its trace archive/wiki/candidate data cannot become Engineering Memory, protocol state or production authority. Promotion is an atomic registry transaction after qualification.

Input steering is also promoted to a Core contract: `STEER`, `COLLECT` and `FOLLOW_UP` operate through the same cancellation domains and event log used by desktop/web/headless clients.
