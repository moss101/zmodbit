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

Current facts:

| Fact | Value |
|---|---|
| Crates in the `modbit-core` dependency closure | 12 of 26 (`browser`, `checkpoint`, `compaction`, `context`, `diagnostics`, `procedural-runtime`, `protocol-state`, `retrieval`, `sandbox`, `skills` + 4 empty crates unlinked) |
| Empty canonical crates | `effects`, `secrets`, `memory`, `observability` |
| Stub binaries (`fn main() {}`) | `apps/cloud-api`, `apps/cloud-worker`, `apps/sandbox-gateway`, `services/modbit-guest` |
| Rust / TS tests | 437 / 53 |
| Desktop screens | 2 (fleet, task workspace) |
| Milestones | M0, M1 COMPLETE; M2 E2E_PROVEN; M3–M10 IN_PROGRESS with 0 IMP tasks closed |

## 2. Open defects found in the live path (fix before anything else)

These are inside code that is already "proven"; they cap the quality of every live run.

1. **Conversation roles are wrong.** `one_agent.rs` keeps `conversation: Vec<String>` and sends every entry as `ChatMessage::user`, including the model's own prior text and tool results serialized as `tool <name> → {json}` strings (`crates/core-runtime/src/one_agent.rs` ~L200 and ~L373). The gateway already supports `assistant_with_tool_calls` and tool-result messages; the loop never uses them, so the model loses call-id linkage.
2. **No context-window management.** The conversation grows unbounded; `crates/compaction` is not linked; `max_output_tokens` is fixed at 4096, `temperature` at 0.2; no reasoning/thinking or prompt-cache controls.
3. **Context pack is a directory listing.** `build_context_pack` emits title, prompt and the first 50 top-level entries. `workspace_rules` is always empty: AGENTS.md / CLAUDE.md / `.cursor/rules` in the target repo are never read.
4. **`shell.run` splits argv on whitespace** (no quoting), no PTY, no streamed output, 600 s cap, 8 KB tail. Electron never spawns `modbit-execd`, so the desktop path has no working shell unless `MODBIT_EXECD_ADDR` is exported by hand.
5. **Stop/Pause do not cancel.** They write events; no cancellation token reaches the in-flight model stream or tool. Steer notes are stored but not injected into the next turn.
6. **All configuration is environment variables** (`MODBIT_REPO_ROOT`, `MODBIT_PROVIDER`, `MODBIT_MODEL`, `MODBIT_BASE_URL`, `*_API_KEY`, `MODBIT_MAX_TURNS`). No repo picker, no provider/model settings, `EnvSecretBroker` only.
7. **README.md line 5** still says the repository contains no product code.

## 3. Parity snapshot (Cursor / Codex / Modbit)

Modbit is not an IDE; editor features (tab completion, inline edit) are out of scope. The comparison is against Cursor's agent and cloud-agent side and Codex CLI/cloud/app.

| Capability | Cursor | Codex | Modbit today |
|---|---|---|---|
| Agentic loop with proper tool messages | yes | yes | broken roles (§2.1) |
| Context compaction / long sessions | yes | yes | no |
| Semantic codebase index | embeddings + Merkle | repo map, grep | logic only, unlinked |
| Repo rules files | `.cursor/rules`, AGENTS.md | AGENTS.md hierarchy | not read |
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

Each phase ends with `python3 tools/check_dossier.py`, the nightly live job green, the named E2E scenarios, and regenerated README / `docs/98`. A node closes only through production routing with typed evidence and only if its module is in the binary closure.

### Phase 2: fix the loop (M2 hardening, M4 link)

1. **Proper message roles.** Replace `Vec<String>` with `Vec<ChatMessage>`; assistant turns carry `tool_calls`, tool results go back as tool-result messages keyed by call id, on both providers. Update `daemon_scripted_e2e` fixtures.
2. **Token budget and compaction on the hot path.** Link `crates/compaction`; count tokens per message (provider usage frames already parsed); compact oldest tool results first, then summarize epochs; emit a `CompactionApplied` run event; make `max_output_tokens` and reasoning/thinking effort per-model config.
3. **Cancellation.** A `CancellationToken` per run threaded through `LiveGatewayTransport` and `execd.run_capture`; `StopTask` aborts the stream and kills the broker run; `PauseTask` parks at the next turn boundary; `SteerTask` notes are injected as a user message on the next turn.
4. **Rules files.** Read AGENTS.md, CLAUDE.md, `.cursor/rules/*.mdc`, `.modbit/rules.md` from repo root down to the touched directory into `workspace_rules`, with provenance hashes in the prompt compiler.
5. **M4 recovery for in-flight runs.** Persist turn/step checkpoints in the event store runtime tables, link `protocol-state` and `checkpoint` into the scheduler, resume a run at the last committed step after a Core kill (unknown-outcome tool calls surface as attention items). Run the `docs/54` kill points on a real turn and a real tool call. Target: E2E-004.
6. **Shell correctness.** Accept `argv` as a JSON array (keep the string form with shell-words parsing), stream output chunks as run events, raise the tail to a paginated `OutputRef`, spawn `modbit-execd` from the Core (not Electron) so every host path has a broker.

Exit: M2 and M4 `E2E_PROVEN` with typed evidence; nightly live job green for 5 consecutive nights.

### Phase 3: context that beats grep (M3)

1. Filesystem walker with `.gitignore` respect and incremental Merkle updates feeding `crates/retrieval`; index built on task start and refreshed on `change.apply`.
2. `context.query` tool over BM25 + path + symbol chunks; `search.symbol` via tree-sitter (Rust, TS/JS, Python first) with definitions/references.
3. Context pack compiled by `crates/context` with token budgets, provenance and a "recently changed files" section; retrieve-before-edit gate on `change.propose`.
4. Benchmark at a fixed revision (`docs/53`) with a latency and recall gate in CI; embeddings decision (ADR-D, local model per `docs/72`) only if BM25 + symbols miss the gate.

Exit: M3 `E2E_PROVEN`; benchmark numbers in `docs/evidence/`.

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
