# ADR-0003: defer semantic embeddings (ADR-D / MOD-EMB-001) out of the M3 retrieval path

- **ID:** ADR-0003
- **Status:** PROPOSED
- **Date:** 2026-09-10
- **Affects:** Future-tasks.md section 4 Phase 3 item 4, graph node M3.5, docs/72 (MOD-EMB-001), docs/18
- **Decides:** The M3 retrieval stack ships WITHOUT semantic embeddings. The shipped context.query fusion is Tantivy BM25 + exact path + tree-sitter symbols (+ the L0–L3 planner over them). The USearch HNSW index, an embedding model, and changed-chunk embedding updates (graph card M3.5) are NOT built in Phase 3 and are not required for the M3 benchmark gate. MOD-EMB-001 stays **PROVISIONAL**; docs/36's "Semantic ANN — USearch (PROVISIONAL)" row is unchanged and remains unrealized.

## Trigger / Evidence

Future-tasks.md Phase 3 item 4 conditions the embeddings decision on the benchmark gate: "embeddings decision (ADR-D, local model per docs/72) only if BM25 + symbols miss the gate." The gate PASSED — `crates/retrieval/tests/retrieval_benchmark.rs` (always-on, 3-OS CI, run 34470983632): recall@5 fused 0.970 ≥ exact baseline 0.970 ≥ 0.90 floor; warm hybrid p95 2 ms < 300 ms (docs/53 L1 bound); evidence in `docs/evidence/phase3-4-retrieval-benchmark-2026-09-10.log` + `.json`. The gate did not miss, so the condition for pulling embeddings in was not met.

## Current Behavior

`crates/retrieval` carries no vector/ANN code. The fusion layer (`TaskIndex::context_query`) consumes BM25, path and symbol candidates only; `crates/context` has no embedding backlog. docs/18 already locks the fallback invariant this decision relies on: "Embedding is an infrastructure feature, not reasoning. If semantic embedding is unavailable, exact/BM25/AST/LSP remain fully functional."

## Proposed Replacement

No code change. The decision records:
1. M3.5 (USearch embeddings + changed-chunk incremental update) stays open on the graph, **IMPLEMENTING, gated behind this ADR** — it is not closed, not descoped from the dossier, and not claimed by any other node.
2. The revisit triggers, in order of strength: (a) the retrieval benchmark gate misses on a real corpus (recall floor), (b) M3.7's planner shows L1-hybrid recall below the fused floor in the nightly agent benchmark (docs/53), (c) an explicit product decision to pursue profile B parity.
3. On revisit, M3.5 must satisfy docs/72's checklist (licensing, CPU/GPU latency, package size, multilingual/code retrieval quality, update compatibility) before any dependency is admitted via the dependency-admission skill; semantic results must never become a correctness dependency (docs/18, docs/72).

## Migration

None. The benchmark harness already measures the shipped profiles honestly (A exact vs F fused) and documents that docs/53's full profile B (semantic) and C (structural evidence beyond imports) signals are M3.4/M3.5 work.

## Compatibility

No protocol/schema/API/event changes. `context.query`'s fusion contract is unchanged; a future embedding source would enter through the same fused-candidate layer with its own provenance tag (docs/18 § Fusion: rank-based fusion, deterministic boosts).

## Security Impact

None. Fewer dependencies (no model runtime, no ANN index) shrink the supply-chain surface; no embeddings model means no model-asset provenance burden at this stage.

## Test Impact

The always-on benchmark gates (recall floor, fused ≥ exact, warm p95 < 300 ms) run in CI on every push and ARE the regression tripwire for this decision: if any gate starts failing as the corpus/queries grow, revisit trigger (a) fires.

## Explicit User Approval

**PROPOSED — not yet accepted.** Per docs/decisions/README.md, ACCEPTED requires the named human approval (who, when, how) to be recorded here. Until accepted, M3.5 remains IMPLEMENTING on the graph and the M3 exit condition stays open; this ADR is filed in the same changeset as the M3.5 status note that references it.
