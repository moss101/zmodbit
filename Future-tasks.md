# Future Tasks: Recommended Work Order for Modbit

Audit date: 2026-09-06 (revised after the Phase 1 closure of 2026-09-05).
Method: full trace of every crate, app, package and workflow; `cargo tree` closure of `modbit-core`; `python3 tools/graph.py status`; parity check against Cursor (agent, cloud agents, 2.0 multi-agent) and OpenAI Codex (CLI, cloud, app) as of mid-2026.
Previous version of this file: the 2026-09-05 component audit and its section 4 governance fixes and Phase 1 plan. Those are closed (see section 1) and this file replaces them.

## 1. What closed since the last audit

| Item | Evidence |
|---|---|
| Section 4 governance: typed evidence, reachability lint, placement lint, derived status tables, M2 E2E gate, status reset | commit `b1c9fa1`; `tools/check_dossier.py`, `tools/evidence.py`, `tools/architecture-lint` |
| Phase 1.1 `HttpStreamTransport` + `SecretBroker` (ADR-0002, tokio/reqwest/rustls) | `crates/providers/src/transport.rs` |
| Phase 1.2 tool protocol: registry schemas to providers, fragmented tool-call assembly, tool_result turns | `crates/providers/src/gateway.rs` |
| Phase 1.3 the single scheduler: daemon-driven runs, worktrees, durable run events, outcome-based transitions | `crates/core-runtime/src/scheduler.rs`, `bin/modbit-core.rs` |
| Phase 1.4 worktree toolset: `shell.run` via `modbit-execd`, `change.propose/apply`, `search.grep`, `git.status/diff`, `test.run` | `scheduler.rs::build_worktree_registry` |
| Phase 1.5 surface protocol: `GetRunDetail`, `GetDiff`, `SteerTask`, `PauseTask`, `StopTask`, explicit `WorktreeSource` | `proto/modbit/protocol/v1/surface.proto` (14 requests) |
| Phase 1.6 desktop task workspace, SSE consumption, real tokens and UI components | `apps/desktop/src/task-workspace`, `packages/ui`, `packages/design-tokens` |
| Phase 1.7 daemon-driven E2E automation, nightly live job, rewritten `live_m2_close.sh` | `.github/workflows/nightly-live.yml`, `crates/core-runtime/tests/daemon_*_e2e.rs` |
| M2 → `E2E_PROVEN` against a live model (E2E-001/002/003) | `docs/evidence/m2-11-live-e2e-2026-09-05T20-25Z.log` |
| Phase 2.1 proper message roles — `Vec<ChatMessage>`, assistant turns carry `tool_calls`, tool results keyed by call id | commit `e80394d`; `crates/core-runtime/src/one_agent.rs`, `tests/daemon_roles_e2e.rs`, `docs/evidence/phase2-1-message-roles-2026-09-06.log` |
| Phase 2.2 token budget + hot-path compaction (oldest tool results first, `CompactionApplied` run event) + per-model request settings | commit `6ee7135`; `crates/compaction/src/hot_path.rs`, `one_agent.rs` |
| Phase 2.3 cancellation — `StopTask` aborts the in-flight stream and kills the broker run, `PauseTask` parks at the turn boundary, `SteerTask` injects on the next turn | commit `271dc35`; `scheduler.rs`, `one_agent.rs`, `tests/daemon_cancellation_e2e.rs` |
| Phase 2.4 repo rules files (`AGENTS.md` / `CLAUDE.md` / `.cursor/rules` / `.modbit/rules.md`) ride the compiled prompt with provenance hashes | commit `c5307da`; `scheduler.rs` rules loader, `docs/evidence/phase2-4-rules-files-2026-09-06.log` |
| Phase 2.5 M4 recovery — a Core kill mid-run resumes from the last committed checkpoint; `docs/54` kill points proven | commit `dabe36c`; `scheduler.rs`, `one_agent.rs`, `tests/daemon_resume_e2e.rs`, `docs/evidence/phase2-5-run-resume-2026-09-06.log` |
| Phase 2.6 shell correctness — `argv` array form, streamed output chunks as run events, paginated `OutputRef`, Core-spawned `modbit-execd` broker | commits `dfc923a`, `0bf3638`; `bin/modbit-core.rs`, `scheduler.rs`, `tests/daemon_output_e2e.rs` |
| Phase 2 exit: M2 + M4 `E2E_PROVEN` with typed evidence; nightly live job green 5 consecutive nights, one revision (main @ `ecf5e6a`) | runs `34002162962` (09-06), `34080928159` (09-07), `34186289909` (09-08), `34310197890` (09-09), `34436444933` (09-10) — typed `run:` refs on graph node `M2.11` |
| Phase 3 EXIT: M3 `E2E_PROVEN` at task level for every card Phase 3 scoped as build work — M3.1 (index surfaces), M3.2 (Tantivy BM25), M3.3 (tree-sitter symbols), M3.4 (headless LSP bridge), M3.6 (import/dependency graph + impact), M3.7 (L0-L3 planner, auto mode), M3.8 (context pack + retrieve-before-edit), M3.9 (benchmark gates in CI: recall@5 0.970 ≥ 0.90 floor, warm p95 2 ms < 300 ms). M3.5 (embeddings) resolved by DECISION per the phase item's own condition ("embeddings only if the gate misses"): the gate passed, so embeddings stay out of the M3 path — the card remains IMPLEMENTING behind recorded revisit triggers; a formal ACCEPTED ADR awaits user approval. Product closure 15→18 (retrieval, context, diagnostics all in). Defects found and fixed en route: `MerkleIndex` empty-dir root drift; docs/36-mandated deps admitted (tantivy, tree-sitter); a hardcoded `profile_experiment` flagged (never closed on) | commits `9bfc91a`, `b3e6159`, `fd366b1`, `bbcb07a`, `727814a`, `49ea54c`, `685047b`, `e3507eb`, M3.4 commit; CI runs `34443959131`, `34452032966`, `34455052293`, `34459967318`, `34470983632`, M3.4 CI run; evidence `docs/evidence/phase3-1-…` through `phase3-7-lsp-bridge-exit-2026-09-10.log` |
| Phase 4.1 repository picker: `RegisterRepo`/`ListRecentRepos` surface RPCs + `recent_repos` registry (migration v6) + clone-by-URL in the git crate + per-task `repo_id`/`base_branch` on `CreateTask` → worktrees allocated from the registered repo at the chosen base branch; desktop repository picker + base-branch field; unknown-repo tasks now fail durably (defect: they hung Running forever) | see the Phase 4 exit row when the phase closes; evidence `docs/evidence/phase4-1-repo-picker-<date>.log` |
| Phase 4.1 repository picker: `RegisterRepo`/`ListRecentRepos` surface RPCs + `recent_repos` registry (migration v6) + clone-by-URL in the git crate + per-task `repo_id`/`base_branch` on `CreateTask` → worktrees allocated from the registered repo at the chosen base branch; desktop repository picker + base-branch field; unknown-repo tasks now fail durably (defect: they hung Running forever) | see the Phase 4 exit row when the phase closes; evidence `docs/evidence/phase4-1-repo-picker-<date>.log` |
| Phase 4.3 keychain SecretBroker: `keyring` v3 admitted per docs/36 (macOS Keychain / Windows Credential Manager / Secret Service, env broker as fallback); api_key via settings goes straight to the OS keychain — never settings/env/events (E2E asserts 0 occurrences in the event store and that the run authenticates ONLY with the keychain key); run-scoped `MODBIT_KEYCHAIN_SERVICE` for tests/E2E (keychain ACLs are per-creating-binary) | commits in the Phase 4.3 changeset; `docs/evidence/phase4-3-5-6-keychain-cli-docs-2026-09-10.log` |
| Phase 4.5 headless CLI: `modbit run <repo> "<task>" --json` + `modbit settings` over the same SurfaceProtocol daemon (auto-spawn, terminal-state wait, JSON report); live smoke: bare fixture repo correctly parks at TASK_STATUS_WAITING (never self-certifies) | same changeset; `docs/evidence/phase4-3-5-6-keychain-cli-docs-2026-09-10.log` |
| Phase 4.6 quick-start docs + README status fix: README's "no product code" claim replaced with the real status + headless quick-start; docs/30/31/32 updated | same changeset; README.md |
| Phase 4.6 quick-start docs + README status fix: README's "no product code" claim replaced with the real status + headless quick-start; docs/30/31/32 updated | same changeset; README.md |
| Phase 4.4 packaging — SPLIT per operator directive: (1) packaging implementation + verification DONE with unsigned/development artifacts (`tools/release/package.sh` build→assemble→SBOM→provenance→SHA256SUMS, `verify-bundle.sh`, `release-receipt.sh`; release flow: build→test→package→SBOM/hash/provenance→OPERATOR SIGNING→verification→receipt); (2) production signing/notarization = `BLOCKED_EXTERNAL_CREDENTIAL / RELEASE_GATE` — an OPERATOR step on an operator-controlled machine; no Apple/Windows private credentials may ever enter agent/runtime/repo | `tools/release/` + `tools/release/operator-signing.md` |
| Phase 5 (items 1,2,3,6): durable approval loop (kernel denial → persisted pending approval → blocking gate → ApproveEffect/DenyEffect RPCs → live provisional grant; first decision wins; survives Core kill), `crates/effects` receipt ledger (hash-chained, tamper-evident JSONL, canonical owner), `execution_mode=approvals` real consumer, kernel path-traversal hardening, docs/52 attack suite always-on (4 tests) | commits in the Phase 5 changeset (53c615d); `docs/evidence/phase5-approvals-receipts-2026-09-11.log` |
| Phase 3.1 walker + incremental Merkle (IMP-EV-0004): index built at task start, refreshed from the change journal on `change.apply` + turn boundary; `index_updated` events carry only-affected-segments evidence; empty-dir prune defect fixed; `crates/retrieval` enters the closure | commit `9bfc91a`; `docs/evidence/phase3-1-repository-index-2026-09-10.log` |
| Phase 3.2 `context.query` + `search.symbol` (M3.2+M3.3): real Tantivy 0.26 + tree-sitter 0.27 admitted per docs/35/36 (orphan `bm25.rs` deleted); fused ranking with provenance; journal-refresh before answering | commit `b3e6159`; `docs/evidence/phase3-2-context-query-symbols-2026-09-10.log` |
| Phase 3.3 context pack + retrieve-before-edit gate (M3.8 + MOD-CTX-001): pack compiled by `modbit-context` (token budget, validated provenance, recently-changed section, index-seeded fresh heads); `change.propose` gated on retrieval evidence; `crates/context` enters the closure | commit `fd366b1`; `docs/evidence/phase3-3-context-pack-gate-2026-09-10.log` |
| Phase 3.4 benchmark gates in CI (M3.9): deterministic 2,600-file corpus through the real pipeline; recall/latency gates always-on; ADR-D embeddings decision: NOT required (gate passed) | commit `bbcb07a`; `docs/evidence/phase3-4-retrieval-benchmark-2026-09-10.log` + `.json` |
| Phase 3.5 M3.1+M3.7: exact/regex/path surfaces + L0-L3 planner auto mode (minimum-sufficient-level proven, no over-escalation); both mutation-checked | commits `727814a`, `49ea54c`; `docs/evidence/phase3-5-planner-surfaces-2026-09-10.log` |
| Phase 3.6 M3.6 dependency graph: tree-sitter import edges resolved against the corpus, maintained incrementally, `impact` mode + L3 `impact_of_top_hit`; Windows path-normalization fix caught by CI | commits `685047b`, `e3507eb`; `docs/evidence/phase3-6-deps-planner-surfaces-2026-09-10.log` |
| Phase 3.7 M3.4 headless LSP bridge: JSON-RPC/stdio client, REAL wire-protocol fixture peer for CI, lazy per-language sessions, `search.symbol` LSP enrichment with fail-soft degradation; `crates/diagnostics` enters the closure (18/26); rust-analyzer smoke when installed | M3.4 commit; `docs/evidence/phase3-7-lsp-bridge-exit-2026-09-10.log` |

Current facts:

| Fact | Value |
|---|---|
| Crates in the `modbit-core-runtime` dependency closure | 18 of 26 (`checkpoint`, `compaction`, `context`, `core-runtime`, `diagnostics`, `domain`, `event-store`, `git`, `policy`, `prompt-compiler`, `protocol`, `protocol-state`, `providers`, `retrieval`, `terminal`, `tools`, `verification`, `workspace`) |
| Empty canonical crates | `effects`, `secrets`, `memory`, `observability` |
| Stub binaries (`fn main() {}`) | `apps/cloud-api`, `apps/cloud-worker`, `apps/sandbox-gateway`, `services/modbit-guest` |
| Rust / TS tests | 516 / 53 |
| Desktop screens | 2 (fleet, task workspace) |
| Surface RPCs | 17 requests in the `surface.proto` oneof (Phase 4.1 added RegisterRepo + ListRecentRepos) |
| Nightly live workflow | `.github/workflows/nightly-live.yml` active (cron 03:43Z; five-night gate 2026-09-06..10 green, see section 1) |
| Milestones | M0, M1 COMPLETE; M2 + M4 `E2E_PROVEN` at task level (M2: 11/11 milestone_tasks and 20 IMPs `E2E_PROVEN`, 42 cross-phase IMPs WIRED with audit notes — never paper-closed; M4: 11/11 tasks `E2E_PROVEN`); **M3: 8/9 milestone_tasks `E2E_PROVEN` — M3.5 decision-deferred (gate passed, revisit triggers recorded)**; M5–M10 IN_PROGRESS |

## 2. Open defects found in the live path (fix before anything else)

Status after the Phase 2 closure (2026-09-10): defects 1–5 are **cleared** by Phase 2 items 2.1–2.6 (see section 1 for the per-item commits and evidence). Defects 6–7 remain open and are deferred to Phase 4 (desktop a user can run).

1. ~~**Conversation roles are wrong.**~~ **Cleared** by Phase 2.1 (commit `e80394d`): the loop sends typed `ChatMessage` turns — assistant `tool_calls` and tool-result messages keyed by call id.
2. ~~**No context-window management.**~~ **Cleared** by Phase 2.2 (commit `6ee7135`): `crates/compaction` hot path compacts oldest tool results first, emits `CompactionApplied`, per-model `max_output_tokens`/settings.
3. ~~**Context pack is a directory listing / rules never read.**~~ **Cleared** by Phase 2.4 (commit `c5307da`): AGENTS.md / CLAUDE.md / `.cursor/rules` / `.modbit/rules.md` are read into `workspace_rules` with provenance. (The pack now compiles through `modbit-context` with budgets/provenance/retrieval seeding — Phase 3.3; pull-based diagnostics-in-context remains future work.)
4. ~~**`shell.run` splits argv on whitespace / no streaming / desktop has no broker.**~~ **Cleared** by Phase 2.6 (commits `dfc923a`, `0bf3638`): argv arrays, streamed run events, paginated `OutputRef`, Core-spawned `modbit-execd`.
5. ~~**Stop/Pause do not cancel.**~~ **Cleared** by Phase 2.3 (commit `271dc35`): cancellation reaches the in-flight stream and broker run; Steer injects on the next turn.
6. **All configuration is environment variables** (`MODBIT_REPO_ROOT`, `MODBIT_PROVIDER`, `MODBIT_MODEL`, `MODBIT_BASE_URL`, `*_API_KEY`, `MODBIT_MAX_TURNS`). No repo picker, no provider/model settings, `EnvSecretBroker` only. **Deferred to Phase 4.**
7. **README.md line 5** still says the repository contains no product code. **Deferred to Phase 4.**

## 3. Parity snapshot (Cursor / Codex / Modbit)

Modbit is not an IDE; editor features (tab completion, inline edit) are out of scope. The comparison is against Cursor's agent and cloud-agent side and Codex CLI/cloud/app.

| Capability | Cursor | Codex | Modbit today |
|---|---|---|---|
| Agentic loop with proper tool messages | yes | yes | real (Phase 2.1) |
| Context compaction / long sessions | yes | yes | real (Phase 2.2) |
| Semantic codebase index | embeddings + Merkle | repo map, grep | real: Merkle + Tantivy BM25 + tree-sitter + import graph, wired (embeddings deferred) |
| Repo rules files | `.cursor/rules`, AGENTS.md | AGENTS.md hierarchy | read with provenance (Phase 2.4) |
| Multi-provider / local models | many + custom endpoints | OpenAI + OSS via Ollama | OpenAI-compat, Anthropic, env only |
| Approval modes | ask / auto / YOLO | read-only / auto / full | static grants, no approve UI |
| OS sandbox for commands | sandboxed terminals | Seatbelt / Landlock / seccomp | policy only, no OS sandbox |
| PTY terminal, streamed output | yes | yes | no PTY, tail only |
| Parallel agents in worktrees | yes | yes | one agent per task, no subagents |
| Cloud / background agents | yes | yes | stubs |
| Browser control | built-in browser | limited | logic only |
| MCP client | yes | client + server | types only |
| Skills / procedures | rules, commands | skills, plugins | discovery only |
| Memory | memories | project memory | empty crate |
| PR / code review | Bugbot, review | `/review`, GitHub reviews | numstat diff |
| Session resume after crash | yes | yes | tasks yes, runs no |
| Headless / CI | CLI | `codex exec`, SDK | HTTP daemon only |
| Image / multimodal input | yes | yes | media pipeline, not exposed |
| Packaging, updates, auth | yes | yes | none |
| Receipted effects, revision-bound review, fleet supervision | partial | partial | designed, partly real |

Modbit's differentiators (receipts, exact recovery, one Core local and cloud, evidence-first review) are exactly the parts still unwired. The order below closes the loop-quality gap first, then builds the differentiators, then chases breadth.

## 4. Recommended work order

Phase 3 closed (see section 1). Each phase ends with `python3 tools/check_dossier.py`, the nightly live job green, the named E2E scenarios, and regenerated README / `docs/98`. A node closes only through production routing with typed evidence and only if its module is in the binary closure.

### Phase 4: the desktop a user can run (M1 polish, M10 packaging)

1. Repository picker (recent repos, clone by URL) replacing `MODBIT_REPO_ROOT`; per-task base branch selection.
2. Settings screen: providers, models, base URLs, max turns, execution mode; presets for OpenAI, Anthropic, OpenAI-compatible (Ollama, vLLM, z.ai), Gemini and Bedrock adapters after the first two are stable.
3. `crates/secrets`: keychain-backed `SecretBroker` (macOS Keychain, Windows Credential Manager, Secret Service) with the env broker as fallback; keys never enter the event store or logs.
4. Packaging: electron-builder with the Rust binaries as sidecars, code signing and notarization, an update channel, SBOM (`cargo cyclonedx`, `pnpm sbom`), `cargo audit`, `cargo deny`, `pnpm audit` in CI.
5. Headless CLI (`modbit run <repo> "<task>" --json`) over the same daemon for CI use.
6. Quick-start doc and provider setup guide; fix README status text.

Exit: a signed build that a new user can open, point at a repo, add a key, and run a task end to end.

### Phase 5: approvals, receipts and review (M9 hot path, M2.9 depth)

1. Approval loop end to end: kernel decision → bound intent persisted → task `Waiting(Approval)` → Needs Attention card → `ApproveIntent` / `DenyIntent` RPCs → receipt → resume; survives a Core kill. Bulk approve/deny of identical intents; per-session effect budgets.
2. Approval modes as a first-class setting (read-only / edits-only / auto with protected effects / full) mapped onto capability grants.
3. `crates/effects`: tamper-evident receipt chain moved out of `checkpoint`, written on every protected effect; `crates/memory` and `crates/observability` populated or removed with an ADR.
4. Review surface: hunk-level diff content over `GetDiff`, accept/reject per hunk, inline revision-bound comments, generated review checklist, merge/apply/export actions gated on verification.
5. `tracing` + OpenTelemetry export, real cost from usage frames, SLO ladder consumer.
6. `docs/52` attack suite (prompt injection through tool results, path escape, secret exfiltration) as always-on tests.

Exit: M9 hot-path items and M2.9 `E2E_PROVEN`; E2E approval scenario proven through the desktop.

### Phase 6: terminal, sandbox, external tools (M5, M7 prerequisites, M9)

1. PTY via `portable-pty` (ConPTY on Windows); `shell.attach/input/cancel`; terminal panel in the task workspace streaming from `modbit-execd`.
2. OS sandbox for `shell.run`: Seatbelt profile on macOS, Landlock + seccomp on Linux, restricted token on Windows; network deny-by-default with allowlist; labelled as part of the capability grant.
3. `crates/mcp`: stdio and streamable-HTTP MCP client, `external.list/call/cancel` tools through policy and receipts, per-workspace transport pool (types already exist in `checkpoint/mcp_memory.rs`).
4. `skill.list/load` tools over the existing SKILL.md discovery; procedural isolate after ADR-C (QuickJS or WASM) over the single registry.
5. Multimodal input: expose the media pipeline through `fs.read` for images and PDFs, and image attachments on task creation.

Exit: M5 `E2E_PROVEN`; MCP conformance and sandbox escape tests green.

### Phase 7: fleet and browser (M6, M7)

1. Children through the scheduler with admission tickets and isolated worktrees; `agent.spawn/steer/park/resume/cancel/wait/result`; parent-child event linkage; fleet UI showing children under parents; conflict proof on merge.
2. Parallel independent tasks on the same repo with the write coordinator; "run N variants" from New Task.
3. CDP bridge and Chromium launch reusing the fingerprint and lease code; `browser.navigate/snapshot/action/network/console/capture`; live view and takeover UI; hostile-page test.
4. Scheduled and event-triggered tasks (automations) on the daemon.

Exit: M6 and M7 `E2E_PROVEN`.

### Phase 8: cloud (M8), only after local Release Zero steps 1 to 15 pass

Guest RPC over vsock/TCP; `sandbox-gateway`, `cloud-worker` hosting Core against a remote store; `cloud-api` with OIDC (ADR-E); substrate adapter (Firecracker or Cloud Hypervisor); GitHub-linked background tasks that open PRs; conformance suite on a real guest; tenant isolation and loss/recovery tests.

### Phase 9: release (M10)

Release Zero on the packaged build as the release-candidate gate with the full evidence bundle; diagnostics export and report-a-problem flow; end-user docs.

## 5. Enhancement backlog (not phase-blocking)

| Enhancement | Earliest phase | Why |
|---|---|---|
| Ollama / vLLM preset with a small local model for the nightly job | 2 | free live proof, offline mode |
| Cost/latency-aware routing with fallback (`routing.rs` exists) | 2 | robustness of live runs |
| Prompt-cache-aware context reuse (`cache_economy.rs` exists) | 3 | cost |
| Plan mode: model drafts a plan the user approves before edits | 5 | Cursor 2.0 / Codex parity |
| Voice input on New Task | 7 | parity, low cost |
| Skill marketplace trust UI | 6 | data already modelled |
| Team policies and shared model settings | 8 | enterprise persona |

Keep deferred (per `docs/72`): pricing, marketplace economics, mobile, consumer automations, embedded editor, Code-OSS adapter.

## 6. Working rules

- Phase 2 items 1 to 4 are the first tasks of the next session; nothing in later phases starts before the loop sends correct roles.
- No logic in `crates/checkpoint` that is not checkpointing; no new crate without an owner in `docs/81`.
- Every phase adds at least one always-on daemon-driven E2E test, not only a live-gated one.
- Update this file when a phase closes: move its items to section 1 with evidence references.
