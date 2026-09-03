# Start Here for Build Agents

> **Build edition authority date:** 2026-09-03

This dossier is the implementation authority for the clean Modbit build. It intentionally uses **native Modbit terminology** and removes external-product names from feature requirements so agents do not imitate or superficially pattern-match against another product.

## Read order for every agent

1. `AGENTS.md`
2. `02_AUTHORITY_AND_DECISIONS.md`
3. `11_SYSTEM_ARCHITECTURE.md`
4. `12_REPOSITORY_AND_MODULE_LAYOUT.md`
5. the subsystem file for the task
6. `40_EVIDENCE_DERIVED_REQUIREMENT_LEDGER.md` rows referenced by the task
7. `48_FEATURE_DEPTH_CONTRACTS.md`
8. the linked `IMP-EV-*` tasks in `41_EVIDENCE_DERIVED_IMPLEMENTATION_TASKS.md`, `QUAL-EV-*` rows in `42_EVIDENCE_DERIVED_QUALIFICATION_TEST_MATRIX.md` and E2E scenarios in `51_E2E_ACCEPTANCE_TEST_CATALOG.md`
9. current `98_BUILD_MANIFEST.md`, the project graph (`../graph/project-graph.json`) and the latest handoff

Do **not** preload the entire dossier into every coding-agent turn. Use `89_BUILD_AGENT_CONTEXT_LOADING_POLICY.md` to keep context task-specific and avoid agents smoothing over details from too much text.

## Product baseline

- one Modbit product;
- agent-first Work + Code workspace, not an IDE fork;
- one canonical Core owning state, orchestration, policy, context, tool execution contracts, evidence and recovery;
- memory is separate from transcript, compaction and recovery;
- live observable browser with structural control first and visual fallback;
- local and isolated-cloud execution share canonical tool/event semantics;
- protected side effects require capability/effect governance and evidence;
- real end-to-end proof is required before completion.

## Build invariant

**Names do not satisfy behavior.** If the codebase already contains `BrowserRuntime`, `MemoryStore`, `AgentGraph`, `CheckpointService`, `SkillRegistry`, `SandboxGateway`, or any other expected name, the assigned agent must still trace the real production path and prove every required behavior. Existing scaffolding is an audit target, not a completion signal.
