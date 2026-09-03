# Status Vocabulary and Lifecycle Reconciliation

> **Authority date:** 2026-09-03  
> **Purpose:** the dossier previously carried three overlapping status ladders. This file is the single normative mapping. Where another file disagrees, this file wins and the other file must be corrected in the same change.

## Three different things carry status

| Thing | Ladder | Recorded in |
|---|---|---|
| **Decision** (an architecture/product choice) | `LOCKED`, `PROVISIONAL`, `EXPERIMENT`, `DEFERRED`, `REJECTED` | `02_AUTHORITY_AND_DECISIONS.md`, `72_RISK_REGISTER_AND_OPEN_DECISIONS.md` |
| **Requirement row** (`REQ-EV-*`) disposition | `ADOPT`, `ADAPT`, `EXPERIMENT`, `ALREADY COVERED`, `DEFERRED`, `REJECT` | `40_EVIDENCE_DERIVED_REQUIREMENT_LEDGER.md` |
| **Work item** (task card, `IMP-EV-*`, milestone task `Mx.y`, milestone `Mx`) | agent task lifecycle below | task cards, `98_BUILD_MANIFEST.md`, `../graph/project-graph.json` |
| **Feature/capability** (what a release can claim) | feature depth ladder below | `83_DEFINITION_OF_DONE_AND_ACCEPTANCE.md`, `82_NO_PLACEHOLDER_PRODUCTION_EVIDENCE_GATE.md`, release evidence bundle |
| **Existing code** found during audit | `PRODUCTION-WORKING`, `IMPLEMENTED-PARTIAL`, `SCAFFOLDED`, `DOCUMENTED-ONLY`, `BROKEN-DRIFTED`, `NOT-FOUND` | audit note on the task card (`84_EXISTING_CODE_FEATURE_AUDIT_PROTOCOL.md`) |

## Agent task lifecycle (work items)

`NOT_STARTED → AUDITING → IMPLEMENTING → WIRED → REAL_TESTING → E2E_PROVEN → COMPLETE`, plus `BLOCKED` from any state.

| State | Entry condition | Evidence required to leave |
|---|---|---|
| `NOT_STARTED` | task exists in graph/manifest | assigned agent + task card created |
| `AUDITING` | agent owns task | audit note with existing-code classification and first missing link |
| `IMPLEMENTING` | audit note recorded | code reaches a real effector/storage boundary |
| `WIRED` | production caller reaches implementation through real registration/routing/policy | integration test through production routing passes |
| `REAL_TESTING` | integration passes | linked `QUAL-EV-*` passes on real substrate; fault case exercised |
| `E2E_PROVEN` | qualification passes | linked E2E scenario passes on packaged/production-equivalent build with evidence bundle |
| `COMPLETE` | E2E proven | manifest row points to evidence refs; PR evidence template filled; remaining work empty |
| `BLOCKED` | any | blocker recorded with reproduction and next safe action |

Only `COMPLETE` means done. A milestone is `COMPLETE` only when every task in it is `COMPLETE` and the milestone proof in `43_IMPLEMENTATION_ROADMAP_AND_TASK_GRAPH.md` has evidence.

## Feature depth ladder (capabilities)

`DECLARED → IMPLEMENTED → WIRED → E2E_PROVEN → COMPLETE`

This ladder describes the **state of the feature**, not the agent's activity. It is what `tools/evidence-check` and the release gate evaluate.

## Mapping between the two ladders

| Agent task state | Feature depth state it can prove at most |
|---|---|
| `NOT_STARTED`, `AUDITING` | `DECLARED` |
| `IMPLEMENTING` | `IMPLEMENTED` |
| `WIRED`, `REAL_TESTING` | `WIRED` |
| `E2E_PROVEN` | `E2E_PROVEN` |
| `COMPLETE` | `COMPLETE` |

A task may never claim a feature depth higher than its own state allows. A feature at `COMPLETE` requires every applicable task that contributes to it to be `COMPLETE`.

## Evidence expiry

If architecture, dependency version, protocol major version or the linked qualification test changes materially, the task moves back to `REAL_TESTING` and the feature back to `WIRED` until evidence is regenerated (`87_HANDOFF_AND_MANIFEST_PROTOCOL.md`).

## Machine encoding

The project graph (`../graph/project-graph.json`) stores `status` on every work-item node using exactly the agent task lifecycle strings above. `tools/check_dossier.py` rejects any other value and rejects `COMPLETE` without at least one `evidence` reference on the node.
