# Requirements Traceability Matrix

> **Authority date:** 2026-09-03  
> **Product:** Modbit — clean-slate implementation dossier  
> **Completion rule:** code is not “done” until it is wired through the real runtime and passes the release-gate real-system test with evidence.  
> **No-placeholder rule:** production code paths may not contain fake implementations, TODO return values, hard-coded success, disabled security checks, or UI-only simulations of unavailable behavior.


| Requirement | Architecture owner | Primary implementation | Release proof |
|---|---|---|---|
| Single agent-first Modbit/no IDE | SurfaceProtocol + desktop | `apps/desktop`, `packages/ui` | E2E-001 + UI audit |
| Durable session/task | Core domain/event store | `domain`, `event-store`, `protocol-state` | E2E-003/004 |
| Exact restart/resume | recovery spine | `checkpoint`, `compaction`, protocol state | E2E-004..008 |
| Memory separate from recovery | memory + state | `memory`, event/checkpoint stores | promotion/restart tests |
| Real local coding | workspace/tool/runtime | `workspace`, `git`, `terminal`, providers | E2E-001/002 |
| Dynamic tool projection | Prompt Compiler/tools | `prompt-compiler`, `tools` | E2E-011 |
| Procedural code-mode runtime | tools/procedural | `procedural-runtime` | E2E-012 |
| Subagents transactionally isolated | Core scheduler | `core-runtime`, `git`, `policy` | E2E-009/010 |
| Hybrid structural retrieval | Context Engine | `retrieval`, `context`, `diagnostics` | retrieval benchmark |
| Live same-session browser/takeover | browser/main | `browser`, Electron main | E2E-013..015 |
| Screenshot as fallback | browser semantic compiler | `browser` | E2E-014 + fallback-rate metric |
| Prompt injection isolation | context/policy/browser | `browser`, `context`, `policy` | E2E-016 |
| Protected effects/receipts | policy/effects | `effects`, `policy` | E2E-005/023 + chain test |
| Durable terminal/replay | exec broker | `modbit-execd`, `terminal` | E2E-008/021 |
| MicroVM cloud isolation | gateway/sandbox | cloud worker, gateway, guest | E2E-017/018/024 |
| No raw secrets in guest/renderer | secrets/policy | `secrets`, gateway/main | security leak suite |
| Same local/cloud runtime semantics | Core | shared Rust crates | local/cloud parity test |
| Real completion evidence | verification/release | `verification`, evidence-check | RC evidence bundle |
| Retrieval benchmark discipline | context/bench | `benchmarks/retrieval` | A/B/C report |

## Traceability rule

Every implementation task/PR names at least one requirement/decision ID; every locked requirement has at least one automated proof. An orphan feature without requirement owner is architecture drift and should not merge.


## V2 traceability extension

Source research traceability is normalized by `40_EVIDENCE_DERIVED_REQUIREMENT_LEDGER.md`: each source row carries owner + `IMP-EV-*` task + `QUAL-EV-*` qualification. Do not duplicate all rows in this high-level matrix; CI joins the two files and fails on missing IDs.
