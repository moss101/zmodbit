# Authority, Decision Register, and Conflict Resolution

> **Authority date:** 2026-09-03  
> **Product:** Modbit — clean-slate implementation dossier  
> **Status vocabulary:** **LOCKED**, **PROVISIONAL**, **EXPERIMENT**, **DEFERRED**, **REJECTED**  
> **Source-of-truth rule:** latest explicit Modbit decision > locked decisions > current dossier > older project documents. Older Code-OSS/Modbit Lite material is historical only when it conflicts with this dossier.


## Current authority

This dossier supersedes older Modbit plans wherever those plans assume Code-OSS as the primary shell, an IDE-first product, or a separate Modbit Lite. Older v5-era PRDs and scope locks are historical provenance only.

## Decision register

| ID | Decision | Status | Implementation consequence |
|---|---|---|---|
| MOD-PROD-001 | Single Modbit product: agent-first Work + Code workspace | **LOCKED** | No Modbit Lite branch or duplicated product runtime |
| MOD-SURF-001 | No Code-OSS foundation | **LOCKED** | No VS Code workbench/extension host dependency in product path |
| MOD-SURF-002 | No built-in full IDE / Monaco architecture | **LOCKED** | Code shown through revision-bound trusted review surfaces; editing is not the primary UI |
| MOD-CORE-001 | One Modbit-owned canonical runtime | **LOCKED** | Same domain/event contracts for desktop local tasks and remote cloud tasks |
| MOD-STATE-001 | Memory is not recovery | **LOCKED** | Seven separated durability layers; no transcript reconstruction as resume mechanism |
| MOD-STATE-002 | compaction epochs + protocol persistence | **LOCKED** | Versioned compaction, stale result rejection, fork/revert cancellation, bounded sync fallback |
| MOD-STATE-003 | checkpoint epochs + delta recovery | **LOCKED** | Monotonic fencing generation, baseline+delta checkpoints, cursor replay and full rehydrate fallback |
| MOD-TOOL-001 | Dynamic task-scoped tool projection | **LOCKED** | Model sees only capabilities required by current step/task |
| MOD-TOOL-002 | Procedural Tool Runtime | **LOCKED** | Minimal model-visible composition interface backed by governed `tools.*` bindings |
| MOD-EXEC-001 | Command failure is not turn failure | **LOCKED** | Exit status becomes typed tool result and can feed repair loops |
| MOD-EXEC-002 | Durable terminal + replay window | **LOCKED** | Terminal broker survives UI detachment; offset/cursor replay required |
| MOD-AGENT-001 | Transactional subagent admission + capacity tickets | **LOCKED** | No worker starts until capacity, worktree/write-set and capability admission commits atomically |
| MOD-EFFECT-001 | Protected-effect receipt chain | **LOCKED** | High-risk writes/external effects produce hash-linked immutable receipts |
| MOD-BROWSE-001 | Live embedded browser as platform primitive | **LOCKED** | Structural CDP/AX/network control; screenshot only fallback; user observe/takeover in same session |
| MOD-SBX-001 | Hardened MicroVM-class cloud sandbox substrate | **LOCKED** | Sandbox policy remains Modbit-owned; no secrets in guest image or static env |
| MOD-CTX-001 | Retrieval-before-edit and evidence-backed Context Packs | **LOCKED** | Exact/BM25/vector/AST/graph/Git/diagnostics/runtime evidence; never vector-only |
| MOD-CTX-002 | Prompt-cache-aware compaction | **LOCKED** | Stable prompt prefix and cache key accounting are part of context economics |
| MOD-UX-001 | Attention-first fleet UX | **LOCKED** | Needs Attention / Ready for Review / Running / Waiting / Completed / Failed are first-class views |
| MOD-VERIFY-001 | Real-system evidence required for completion | **LOCKED** | Status ladder ends at COMPLETE only after release-gate E2E proof |
| MOD-DESK-001 | Electron + React/TypeScript native shell for v1 | **PROVISIONAL** | Chosen for mature Chromium embedding and desktop integration; SurfaceProtocol prevents architectural lock-in |
| MOD-EMB-001 | Local non-generative embedding model for semantic retrieval | **PROVISIONAL** | Embedding is infrastructure, not reasoning; exact/BM25/AST remains correctness fallback |
| MOD-AUTH-001 | OIDC-compatible hosted identity for cloud account plane | **PROVISIONAL** | Desktop uses authorization-code + PKCE; enterprise SSO can attach behind same identity interface |
| MOD-MOBILE-001 | Mobile/simulator control | **DEFERRED** | Architecture keeps tool namespace available; no P0 implementation |
| MOD-AUTO-001 | Broad consumer automations/scheduling | **DEFERRED** | Only task queue/remote continuation in P0; recurring automation product comes later |
| MOD-IDE-001 | Code-OSS / VS Code workbench as fallback product surface | **REJECTED** | May exist only in a separate future adapter repo, never as a dependency of Core |
| MOD-IDE-002 | Build a new general-purpose code editor | **REJECTED** | Not strategic; review/inspection surface only |
| MOD-ORCH-001 | Hidden fixed planner→builder→reviewer hierarchy copied from external references | **REJECTED** | Orchestration is explicit, typed and task-dependent; no unsupported claims about external reference internals |
| MOD-CTX-003 | Vector retrieval as sole context mechanism | **REJECTED** | Hybrid and structural retrieval required |
| MOD-CLOUD-001 | Cloud-only brain | **REJECTED** | Local Core is first-class; remote Core uses same domain/runtime contracts |

## Conflicts with older material

### Code-OSS
Older Modbit dossiers treated Code-OSS as the desktop substrate. The latest decision removes IDE scope and Code-OSS entirely from the product foundation. **Resolution: replace.** Retain only mechanism knowledge—workspace lifecycle, Git/worktrees, diagnostics/LSP, diff UX, secure IPC patterns—implemented independently.

### Monaco / editor buffer ownership
Some intermediate plans introduced a native React workspace with Monaco. Earlier and later agent-first decisions remove the full editor architecture. **Resolution: retain the no-IDE direction.** Filesystem + Git revision are canonical; code review is revision-bound and read-oriented.

### Modbit Lite
Older cross-product documents still name Modbit Lite. Latest product strategy consolidates it into Modbit. **Resolution: retire.** Shared state/sandbox lessons survive as Modbit architecture.

### External reference parity
External reference behavior is used as evidence for mechanisms only. No proprietary service, binary or implementation becomes an architectural dependency. **Resolution: retain mechanism-level inspiration; reject dependency or cloning.**

## Change control

Any future change to a **LOCKED** item requires a Decision Record containing: trigger/evidence, current behavior, proposed replacement, migration, compatibility, security impact, test impact, rollback and explicit user approval. A PR that silently changes a locked invariant fails architecture CI.


## V2 decisions added after source reconciliation

| ID | Decision | Status | Consequence |
|---|---|---|---|
| MOD-COV-001 | Mechanism-level requirement coverage is a build authority gate | **LOCKED** | No accepted requirement may disappear through summarization |
| MOD-MEDIA-001 | Typed `MediaEnvelope` and provider-neutral multimodal tool results | **LOCKED** | Images/PDF/audio/video are first-class artifacts/results with budgets and provenance |
| MOD-MEDIA-002 | Bounded PDF text→vision fallback with explicit lossy/untrusted label | **LOCKED** | No silent full-document claim or unbounded rasterization |
| MOD-INPUT-001 | Typed `STEER / COLLECT / FOLLOW_UP` input dispatch | **LOCKED** | Concurrency semantics live in Core, not clients |
| MOD-SKILL-001 | evolution-lab-style trace/wiki/skill separation inside Skill Evolution Lab | **PROVISIONAL / EXPERIMENT-GATED** | No second general memory; no production self-modification |
| MOD-SKILL-002 | Skill promotion requires real qualification + rollback | **LOCKED** | Active skill head changes atomically only after gates pass |
| MOD-MM-001 | Multimodal/media requirements extend existing Modbit owners; no second runtime | **LOCKED** | Multimodal/subagent/daemon lessons map to existing owners |
| MOD-TOOL-003 | External-tool compatibility is capability parity, not name parity | **LOCKED** | Exact private tool names are never invented; canonical Modbit intent/effect schemas prevail |
| MOD-JIT-001 | Task-conditioned adaptive harness/profile generation stays shadow/eval | **EXPERIMENT** | Cannot replace Core or control production before measured gates |
