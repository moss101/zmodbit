# Future Tasks: Component-Level Audit and Completion Plan for Modbit

Audit date: 2026-09-05 (revised after a full code trace of every crate, app, package and test).
Method: `cargo test --workspace` (400 passed), `pnpm -r test` (28 passed), `tools/check_dossier.py` (OK); then, for each of the 133 Rust modules, recorded what it claims, whether it touches a real boundary (filesystem, process, socket, SQLite), whether it has integration tests, and whether the product binary can reach it. Classification uses the vocabulary of `docs/84` and `docs/93`.

## 1. Executive summary

The repository holds far more real, tested engineering than "scaffold", and far less product than "all 10 milestones complete". The accurate statement is:

**Modbit today is a set of well-built components with one thin composition point.** The `modbit-core` binary composes 9 crates (domain, event-store, policy, prompt-compiler, protocol, providers, terminal, tools, workspace) into a durable task store with an authenticated protocol, a fleet screen, and a code view. The other 17 crates are not in the binary's dependency closure at all, and the agent runtime that does exist inside `core-runtime` has no production caller.

| Fact | Evidence |
|---|---|
| Crates reachable from the product binary | 9 of 26 (`crates/core-runtime/Cargo.toml`); `browser`, `checkpoint`, `compaction`, `context`, `diagnostics`, `git` (only transitively), `procedural-runtime`, `protocol-state`, `retrieval`, `sandbox`, `skills`, `verification` and the four empty crates are never linked into `modbit-core` |
| Production callers of the agent runtime | 0 (`grep OneAgentRuntime` outside tests returns nothing) |
| Modules that touch a real boundary | 24 of 133; all in event-store, protocol, terminal, workspace, git, tools/media, verification, skills discovery, protocol-state journal, agent-fleet journal, context/ports, daemon |
| Integration test files | 26, concentrated in event-store (7), core-runtime (6), git (3), workspace (3), terminal (2), protocol (2) |
| Real-effect evidence logs | 2 (`docs/evidence/`), both M2 live-provider runs |
| Surface protocol requests | 9 (`proto/modbit/protocol/v1/surface.proto`) |
| Desktop screens | 1 (fleet + new task) |

The graph is inconsistent with `README.md` and `docs/98_BUILD_MANIFEST.md` (both say M2 to M10 not started) and `check_dossier.py` does not detect that.

## 2. Milestone-by-milestone classification

Classes: **PRODUCTION-WORKING** (reachable from the binary, real effector, tested), **WIRED** (real effector and tests, but not reachable from the binary), **LOGIC-ONLY** (pure in-memory types and functions, well tested, no effector), **SCAFFOLDED** (empty or canned), **NOT-FOUND**.

### M0 Repository and authority: PRODUCTION-WORKING
Graph tooling, architecture lint, decision guard, coverage guard, examples runner, three-OS CI with clippy `-D warnings`, generated-binding drift check. All real. Gaps: lint checks dependency direction only, evidence field is untyped, manifest/README not derived from graph (see section 4).

### M1 Durable local shell and Core: PRODUCTION-WORKING
- Event store on SQLite: append-only envelope, idempotent commands, transactional projections, migrations, leases and fencing, index store, runtime records (output refs, background tasks, tool-call pairs). 7 integration test files.
- Local SurfaceProtocol: HMAC boot handshake, length-prefixed protobuf over `interprocess`, Rust and TS clients with wire-compat tests.
- `modbit-core` daemon: boot channel, request dispatch, HTTP+SSE multi-client daemon with replay and 413 bounds, SIGKILL crash/restart test recovering the fleet exactly.
- Electron main with IPC schema validation, sandboxed renderer, fleet screen, new-task form, real e2e test spawning the binary.
- Gaps: renderer polls every 1.5 s instead of using the SSE path; no session UI; `packages/ui` and `packages/design-tokens` are one-line scaffolds.

### M2 Real local engineering loop: WIRED (component-complete, integration missing)
Real and tested in isolation:
- `git`: init, commit, branch, linked worktrees, numstat diff, typed merge with conflict evidence. 3 integration test files against real git.
- `workspace`: file service with safe paths and revisions, change engine, merge transactions, session lineage, capsules, handoff bundles. 3 integration test files.
- `terminal` + `modbit-execd`: durable process broker with offset-addressed output, detach/reattach test, structured command contract, environment hierarchy, replay window.
- `tools`: registry with fail-closed policy decision, args hash evidence, `fs.list`, `fs.read`, `shell.run`, `git`, `web.fetch`; media pipeline with real file I/O; output pagination, schemas, families, toolsets, turn surface (logic).
- `policy`: capability kernel, approvals, protected paths, mediation, device policy, question service, UI risk, patch gate, admin precedence. 11 unit tests in lib plus per-module tests.
- `providers`: request bodies and SSE parsers for OpenAI-compatible and Anthropic, routing and health, envelope, media split; live streaming call recorded in `docs/evidence/m2-6-live-qualification.log`.
- `prompt-compiler` and `one_agent.rs`: compile, stream, typed events, tool dispatch under policy, repair loop, verification stage; live run recorded in `docs/evidence/m2-7-live-one-agent.log`.
- `verification`: deterministic gates that spawn real commands, verification plane, diagnostics regression comparison (integration test), evidence index, browser evidence normalisation (logic).
- Code view served over the protocol (`code_surface.rs` test).

Missing for M2's own proof (E2E-001 to E2E-003):
- No production caller of the runtime; `StartTask` only emits `TaskStarted` (`crates/core-runtime/src/surface.rs:128`).
- No HTTP client in the product; the transport is a `curl` subprocess inside two test files.
- `tools` never sends tool definitions to the provider; `openai_request_body`/`anthropic_request_body` have no `tools` field.
- `shell.run` calls `Command::new` directly (`crates/tools/src/lib.rs:204`) and bypasses `modbit-execd`, so E2E-003 output survival cannot hold.
- `verification` is not a dependency of `core-runtime`; the runtime's verify step is stub-tested.
- No `change.apply`, `search.grep`, `git.diff`, `test.run` tools registered.
- No worktree allocation on task start.
- No streamed events to the desktop; no task workspace or diff screen.

### M3 Context intelligence: LOGIC-ONLY, not linked into the binary
- `retrieval`: in-memory index with exact/regex/path search, BM25, Merkle tree, AST/symbol chunk representation, engineering and history context, hydration, rerank, index benchmark. Nothing walks a real filesystem; callers must feed bytes. No embeddings or vector index (`knowledge.rs:6` says "plug in later").
- `context`: query planner, pack compiler with token budgets, provenance, recoverability, cache economy, savings, metrics, inspector, fast subagent, benchmark harness, ports. `ports.rs` reads the repository's own crate list from disk for a self-test; otherwise pure.
- `compaction`: epoch-based history compaction, pure.
- `diagnostics`: pull-based diagnostics types; no LSP process, no adapters.
- `workspace/bridge.rs`, `edit_gate.rs`: editor context bridge and retrieve-before-edit gate, pure.
- No benchmark has been run on a fixed revision; no latency gate enforced.

### M4 Durable recovery spine: WIRED for journals, LOGIC-ONLY for checkpoints, not linked into the binary
- `protocol-state`: file-backed protocol journal with torn-write recovery; `kill_points.rs` really kills between writes and recovers pending tool calls and approvals.
- `checkpoint`: epoch fencing, delta journal, failure taxonomy, cursor metadata, kernel lease. In-memory structures; nothing persists checkpoints to the event store or disk except through `protocol-state`.
- `core-runtime` depends on neither crate, so no in-flight run can be recovered because no run exists.
- M1's crash/restart test covers task and session state only.

### M5 Procedural runtime and skills: LOGIC-ONLY with real skill discovery, not linked into the binary
- `skills/lib.rs` discovers `SKILL.md` files on disk with SHA-256 provenance and detects removal (real fs, tested). Layering, packaging, compact mode, eval harness, evolution lab, impact log, profile evolution, SDK validation, wiki index are pure logic.
- `procedural-runtime`: a minimal code-mode interface (`exec`, `wait`, `request_user_input`, evidence) with policy callback, and composition over existing tools. No isolate (no QuickJS, no WASM); `exec` records intent rather than running scripts.
- `tools/multimodal.rs`: multimodal read and PDF fallback, pure.

### M6 Subagents and fleet: LOGIC-ONLY with a real journal, inside core-runtime but not composed into the surface
- `agent_fleet.rs`: persisted AgentNodes with a file-backed journal (real fs, 8 tests). `fleet_admission.rs`: transactional admission with rollback, capacity tickets, task contracts, isolation bundles, write coordinator, attention aggregator. `agent_runtime_batch2/3/final.rs`, `agent_profiles_plans.rs`, `delegation.rs`, `reminder_engine.rs`: profiles, plan mode, plan versions, todo graph, stall watchdog, child prompt isolation, leaf profiles, background handles.
- No child ever executes a runtime; no spawn path from a parent turn; not reachable through the surface protocol. The desktop has fleet grouping and supervision functions only.

### M7 Live browser and computer use: LOGIC-ONLY
- `browser`: session identity, control lease with watchdog, deterministic → accessibility → visual ladder, semantic elements with stable fingerprints and deltas, page/action/state graph, computer-use safety (preemption, cooldown, safe typing, clipboard guard), peer boundary, Chromium run record.
- Zero process spawns, zero sockets, no CDP dependency, no Electron embedding. `record_chromium_run` stores a version string.

### M8 Cloud isolated execution: SCAFFOLDED
- `sandbox`: `ExecutionBackend` trait with a real `LocalBackend` (spawns processes) and a `CloudMicroVmBackend` returning `Ok((0, format!("microvm[..]")))`. Deny-by-default policy, control-plane never-grantable, typed guest RPC messages with version/capability rejection, tenant-bound gateway records, credential-handle injection, worker negotiation, conformance suite: all in-memory logic (10 tests).
- `apps/cloud-api`, `apps/cloud-worker`, `apps/sandbox-gateway`, `services/modbit-guest`: `fn main() {}`.
- No identity (OIDC), sync, tenancy or offline behaviour from `docs/24`.

### M9 Memory, effects, security hardening: LOGIC-ONLY, in the wrong crates
- `checkpoint/security_hardening.rs`: tool schema secrets outside arguments, MCP install trust and credential gates, marketplace trust with quarantine, tamper-evident receipt hash chain, dynamic credential handles.
- `checkpoint/mcp_memory.rs`: MCP list/call/cancel lifecycle with identities, scoped auth, organisational memory with supersession chains. `checkpoint/hook_bus.rs`, `importers_plugins.rs`: hook bus with timeouts, plugin registrations, importers with migration report.
- `policy/approvals.rs`: effect ledger reversibility classes.
- No MCP transport (no stdio, no JSON-RPC), no keychain, no receipt written by any real tool execution. `crates/effects`, `crates/secrets`, `crates/memory` are empty; `docs/81` ownership is violated and `architecture-lint` does not detect placement.

### M10 Release hardening: LOGIC-ONLY except the headless daemon
- `verification/release_hardening.rs`: dual error channels with redaction, SLO event ladder, per-run usage ledger with reconciliation, diagnostics export with credential masking, shadow harness generation, bounded repair with static fallback. Pure functions, 7 tests.
- Headless mode (REQ-EV-0126) is genuinely served by the M1 HTTP+SSE daemon, which is real.
- Multi-level qualification "incl. real API" refers to the env-gated live tests, never run in CI.
- No packaging, signing, notarisation, update channel, SBOM, or Release Zero run. `docs/59` steps 2 to 15 cannot execute.

## 3. Honest status table

| Milestone | Graph | Honest | Remaining work in one line |
|---|---|---|---|
| M0 | COMPLETE | COMPLETE | tighten gates (section 4) |
| M1 | COMPLETE | COMPLETE | switch renderer to SSE, session UI |
| M2 | COMPLETE | WIRED | transport in product, scheduler in daemon, tools to provider, shell via execd, verification linked, 4 more tools, task screen, E2E-001..003 |
| M3 | COMPLETE | LOGIC-ONLY | filesystem walker feeding the index, link into runtime context pack, run benchmark, embeddings decision |
| M4 | COMPLETE | WIRED (journals) / LOGIC-ONLY (checkpoints) | persist checkpoints, link into scheduler, kill-point suite on a real run |
| M5 | COMPLETE | LOGIC-ONLY | isolate decision and implementation, `skill.*` tools, link registry into runtime |
| M6 | COMPLETE | LOGIC-ONLY | child execution through the scheduler, `agent.*` tools, surface requests, fleet UI |
| M7 | COMPLETE | LOGIC-ONLY | CDP bridge, Chromium launch, `browser.*` tools, live view, takeover UI |
| M8 | COMPLETE | SCAFFOLDED | everything except the contract types |
| M9 | COMPLETE | LOGIC-ONLY | move to owner crates, MCP transport, keychain broker, receipts on the hot path |
| M10 | COMPLETE | LOGIC-ONLY | packaging, signing, updates, Release Zero |

## 4. Root cause and governance fixes (Step 0, do first)

The docs and graph forbid what happened (`AGENTS.md` forbidden shortcuts; `docs/82`). The tooling let it through:

1. **Evidence is untyped.** `check_dossier.py` accepts any non-empty evidence list; 709 of the entries are CI run URLs or commit hashes. Fix: a `COMPLETE` node needs `log:docs/evidence/<file>`, `scenario:E2E-0nn`, `receipt:<sha256>`, or `run:` plus an integration/live test name. Enforce in `check_dossier.py` and `coverage-guard.py`.
2. **No reachability check.** Nothing verifies that a closed requirement's code is in the product binary's dependency closure. Fix: `architecture-lint` computes the closure of `modbit-core` (and later `cloud-worker`) and fails any `COMPLETE` IMP node whose module is outside it.
3. **No placement check.** Fix: every `REQ-EV-*` tag in a source file must belong to the subsystem that owns that crate (`docs/81`); flag the `checkpoint` crate's M9 modules.
4. **Empty canonical crates pass.** Fix: a crate whose milestone is `COMPLETE` must have `pub` items and tests.
5. **Status sources drift.** Fix: `build_manifest.py` derives `docs/98` and the README status table from the graph; `check_dossier.py` fails on mismatch.
6. **Row-by-row closure beat vertical slices.** Fix: add graph edges so M3 to M10 IMP nodes are not `ready` until M2 is `E2E_PROVEN` through the daemon.
7. **Reset statuses** to section 3 with `tools/graph.py set`.

Done when: `check_dossier.py` fails on today's tree, passes after the reset, CI green.

## 5. Completion plan

### Phase 1: close M2 for real, then M4 (resume point)

Concrete changes, smallest first:
1. `crates/providers/src/transport.rs`: `HttpStreamTransport` implementing `ModelTransport` with incremental SSE, timeouts, cancel token, retry on 429/5xx, usage capture. Needs an HTTP client; ADR-A chooses `tokio`+`reqwest`+`rustls` (recommended) or sync `ureq`. Credentials via a `SecretBroker` trait (env-backed now, keychain in Phase 2).
2. Tool protocol: emit `tools` from `ToolRegistry` schemas; parse `tool_use`/`tool_calls`; append `tool_result` messages; extend `StreamEvent` with `ToolCallDelta` and `Usage`.
3. `crates/core-runtime/src/scheduler.rs`: the single scheduler. On `TaskStarted`: allocate worktree and revision (`workspace` + `git`), build context pack, run `OneAgentRuntime` on a worker, write `RunStep`/`Turn`/tool events and receipts, transition task from real outcomes. Wire into `bin/modbit-core.rs` and `daemon.rs`.
4. Route `shell.run` through `modbit-execd`; add `change.propose`/`change.apply` (edit gate + change engine), `search.grep` (index or ripgrep), `git.status`/`git.diff`, `test.run` (link `verification`, add cargo/vitest/pytest runner adapters).
5. Surface protocol: `SubscribeEvents`, `GetRunDetail`, `GetDiff`, `SteerTask`, `PauseTask`, `StopTask`. Regenerate bindings.
6. Desktop: task workspace screen (conversation and steering, timeline, diff bound to revisions, test output); switch to SSE; first real tokens and components in `packages/*`.
7. M4: persist checkpoints in the event store's runtime tables, link `protocol-state` and `checkpoint` into the scheduler, run the `docs/54` kill points against a real turn and a real tool call (unknown-outcome handling).
8. Live proof through the daemon: rewrite `tools/live_m2_close.sh` to drive the desktop protocol; nightly CI job with a frozen low-cost task and a repository secret, log to `docs/evidence/`; automate E2E-001 to E2E-003 (E2E-004 with M4).

Exit: M2 and M4 `E2E_PROVEN` with typed evidence.

### Phase 2: trust and review (M9 hot path, parts of M10)
Verification gate in the task state machine; approvals end to end (kernel decision → bound intent → Needs Attention → approve/deny UI → receipt) surviving a Core kill; keychain `SecretBroker`; move receipts to `crates/effects`, credentials to `crates/secrets`, memory to `crates/memory`; Review screen; `tracing` + OpenTelemetry + real cost from usage; `docs/52` attack suite; `cargo deny`, `cargo audit`, `pnpm audit`, license allowlist.

### Phase 3: terminal, browser, external tools (M7, M5, M9)
PTY via `portable-pty` with ConPTY, `shell.attach/input/cancel`, terminal surface; CDP bridge and Chromium launch reusing the existing fingerprint and lease code, `browser.*` tools, live view, takeover; MCP stdio client in a new `crates/mcp`, `external.*` tools through policy and receipts; procedural isolate after ADR-C (QuickJS or WASM) over the single registry.

### Phase 4: fleet, context, memory (M3, M5, M6, M9)
Children through the scheduler with admission tickets and isolated worktrees, `agent.*` tools, fleet surface requests and UI; filesystem walker and incremental Merkle updates feeding `retrieval`, context pack wired into the runtime, benchmark at a fixed revision with `docs/53` gates, embeddings after ADR-D; `memory.*` tools with promotion and provenance; `skill.*` tools and evolution loop.

### Phase 5: cloud (M8), only after local Release Zero steps 1 to 15 pass
Guest RPC over vsock/TCP; `sandbox-gateway` binary; `cloud-worker` hosting Core against a remote store; `cloud-api` with OIDC (ADR-E); substrate adapter (Firecracker or Cloud Hypervisor; container adapter labelled non-production); conformance suite on a real guest; tenant isolation and loss/recovery tests.

### Phase 6: release (M10)
Packaging, signing, notarisation, update channel, SBOM, reproducible build digest; headless CLI; Release Zero on the packaged build as the release-candidate gate with the full evidence bundle; diagnostics export flow; end-user docs.

## 6. Enhancement advice

| Enhancement | Phase | Why |
|---|---|---|
| Local model provider (Ollama/vLLM through the OpenAI-compatible adapter) | 1 | free live proof in CI, offline mode; adapter already takes a base URL |
| Cost/latency-aware routing with fallback | 2 | `routing.rs` exists; makes the nightly live job robust |
| Prompt-cache-aware context reuse | 2 | `cache_economy.rs` exists; needs real usage data |
| Generated review checklist and inline revision-bound comments | 2 | makes Review the differentiator |
| Telemetry opt-in and privacy settings | 2 | required before any export leaves the machine |
| Tree-sitter symbol index for `search.symbol` | 4 | highest retrieval gain per effort, no embeddings needed |
| Bulk approve/deny of identical intents, per-session budgets, SLO alerts | 4 | SLO ladder has no consumer today |
| Skill marketplace trust UI | 4 | data already modelled |
| Report-a-problem using the redacted diagnostics export | 6 | export exists |
| User quick start and provider setup guide | 1 | nothing user-facing exists |

Keep deferred (per `docs/72`): pricing, marketplace economics, mobile, consumer automations, embedded editor, Code-OSS adapter.

## 7. Working rules for the next sessions

- A node closes only through production routing with typed evidence, and only if its module is in the binary's dependency closure.
- No logic in `crates/checkpoint` that is not checkpointing; no new crate without an owner in `docs/81`.
- Each phase ends with `check_dossier.py`, the nightly live job, the relevant E2E scenarios, and regenerated README/`docs/98`.
- First task next session: section 4 items 1 to 5 and 7, then ADR-A, then Phase 1 item 1.
