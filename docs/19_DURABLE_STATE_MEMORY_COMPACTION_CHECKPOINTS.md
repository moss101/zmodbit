# Durable State, Memory, Compaction, and Checkpoints

> **Authority date:** 2026-09-03  
> **Product:** Modbit — clean-slate implementation dossier  
> **Status vocabulary:** **LOCKED**, **PROVISIONAL**, **EXPERIMENT**, **DEFERRED**, **REJECTED**  
> **Source-of-truth rule:** latest explicit Modbit decision > locked decisions > current dossier > older project documents. Older Code-OSS/Modbit Lite material is historical only when it conflicts with this dossier.


## Seven-layer durability invariant

1. **Canonical Event / Session Store** — authoritative state transitions.
2. **Protocol State** — tool calls, approvals, questions, terminal/browser/control/subagent lifecycle needed for exact resume.
3. **Context Projection** — bounded model-visible working state for each turn.
4. **Compaction Epochs** — versioned compressed conversation/context history.
5. **Workspace Checkpoints** — recoverable files/worktrees/runtime metadata using baseline + deltas.
6. **Engineering / Semantic Memory** — durable learned knowledge.
7. **Evidence Archive / Effect Ledger** — immutable proof and side-effect provenance.

No layer may silently substitute for another.

## Local persistence

SQLite WAL databases are used for event/projection/protocol/memory metadata with strict migrations and foreign keys. Large immutable payloads, terminal output, browser captures and checkpoint blobs use a content-addressed object store on disk. Core commits event + critical projection changes in one transaction.

## Compaction epochs

Each compaction request captures:
- session/branch generation;
- source event range;
- previous epoch ID;
- prompt/compiler version;
- target token budget.

Async compaction result is accepted only if the source branch/generation is still current. Fork/revert/cancel invalidates incompatible pending compactions. If context reaches hard pressure before async result arrives, Core executes bounded synchronous compaction. The compacted text is context material, **not semantic memory**.

The synchronous hot-path compaction (`crates/compaction::hot_path`) applies stages in order and stops as soon as the projection fits the budget: (1) truncate the oldest tool-result payloads in place — the message and its call-id linkage always survive; (2) summarize whole assistant+tool-result blocks after the initial prompt into single epoch lines; (3) as a last resort, truncate even the most recent block's results, because the repair turn still needs its linkage. Every application extends the epoch lineage and emits a durable `compaction_applied` run event carrying the epoch id, affected message count, reclaimed-token estimate and the sha256 manifest digest.

## Checkpoint epochs

Checkpoint metadata includes monotonic epoch, base revision, delta object refs, Git HEAD/worktree state, index generation, terminal/browser/sandbox reattachment metadata and integrity hash. A stale epoch can never overwrite newer checkpoint state.

Use periodic full baseline + intermediate deltas. Restore validates every object hash before making the checkpoint current.

## Protocol state examples

- outstanding ToolCall and unknown outcome reconciliation;
- pending Approval with bound intent hash;
- pending user question;
- subagent admission/running state;
- terminal session ID + last acknowledged output cursor;
- browser session/control lease;
- sandbox lease + generation;
- model stream attempt and safe resume boundary.

## Engineering Memory

Scopes: Run, Session, User, Agent Profile, Repository, Space, Organization. Record types: decision, convention, fact, procedure, failure pattern, dependency knowledge, user preference. Every item stores source provenance, author/actor, confidence, TTL/expiry, scope, sensitivity, supersedes/conflicts links and last validation revision.

Promotion rules:
- transcript summary alone cannot promote;
- web/tool/peer content is untrusted until validated;
- repository facts bind to revision and can stale;
- sensitive memory requires policy-permitted scope;
- user can inspect/delete/supersede where allowed.

## Resume algorithm

1. Acquire session kernel lease with new fencing generation.
2. Load latest projection + event tail and validate hashes.
3. Reconstruct protocol state.
4. Validate latest checkpoint chain against workspace/Git.
5. Reattach terminal/browser/sandbox resources by lease and cursor.
6. Reconcile any `UnknownOutcome` tool calls with Effect Ledger/target state.
7. Reject stale compaction/checkpoint workers.
8. Build fresh Context Projection from current state.
9. Continue from the exact waiting/executing/review boundary.

This flow is release-tested by process kill at every major state.
