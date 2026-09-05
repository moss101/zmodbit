# Modbit — Build Dossier and Project Driver

Modbit is an **agent-first engineering workspace**: a desktop Work + Code application where a user delegates software tasks to AI agents, supervises a fleet of them, and reviews real diffs, test output, browser actions and effect receipts, without living inside an IDE. One canonical Rust Core owns state, orchestration, policy, context, tool execution, evidence and recovery, and the same Core runs locally and in isolated cloud MicroVMs.

This repository currently contains the **complete implementation specification** (the "dossier") and the **project graph that drives the build**. It contains no product code yet; every milestone starts at `NOT_STARTED`.

## Layout

```text
README.md                       this file
AGENTS.md                       operating contract for AI build agents (highest authority)
SKILLS.md                       governed procedures agents follow, step by step
MANIFEST.md / manifest.json     every file with SHA-256, size, section and old name
graph/
  project-graph.json            machine-readable driver: milestones → tasks → subsystems →
                                requirements → tests → docs, with live status
  PROJECT_GRAPH.md              human view (mermaid) and query cookbook
tools/
  build_manifest.py             regenerate MANIFEST.md + manifest.json
  build_graph.py                regenerate graph structure from docs (keeps statuses)
  graph.py                      query/update the graph: ready, show, set, status, render
  check_dossier.py              integrity gate for docs + graph + manifest
docs/                           70 specification files, uniquely numbered by section
```

## Where to start

| You are… | Read |
|---|---|
| an AI build agent | `AGENTS.md`, then `SKILLS.md`, then `docs/01_START_HERE_FOR_BUILD_AGENTS.md` |
| a human architect | `docs/00_MASTER_INDEX.md`, `docs/02_AUTHORITY_AND_DECISIONS.md`, `docs/11_SYSTEM_ARCHITECTURE.md` |
| a reviewer checking progress | `graph/PROJECT_GRAPH.md`, `docs/98_BUILD_MANIFEST.md`, `python3 tools/graph.py status` |
| someone adding or editing a spec | `SKILLS.md` → `dossier-maintenance` |

## The numbering scheme

| Range | Section |
|---|---|
| 00–09 | Authority and orientation |
| 10–29 | Architecture and canonical subsystems |
| 30–39 | Implementation specifications |
| 40–49 | Requirements (291 `REQ-EV-*`), tasks (265 `IMP-EV-*`), qualifications (291 `QUAL-EV-*`), traceability |
| 50–69 | Verification and testing, including 25 E2E release-gate scenarios and Release Zero |
| 70–79 | Delivery and operations |
| 80–97 | Agent process and governance |
| 98 | Live build manifest |

Numbers are stable identifiers. Never renumber; add new files at the next free number in their section.

## The project graph

`graph/project-graph.json` is the single place where "what to do next" is answered. It links:

- 11 milestones (`M0`–`M10`) with dependency edges and the critical path `M0 → M1 → M2 → M4`;
- milestone tasks (`M1.3`, `M7.6`, …) in execution order;
- canonical subsystems (the single-owner systems that may never be duplicated) with their crates and spec files;
- every `REQ-EV-*` row, its `IMP-EV-*` task and `QUAL-EV-*` test, attached to a subsystem and therefore a milestone;
- E2E, skill-evolution, media and fault-injection scenarios attached to the milestone they prove;
- `MOD-*` decisions attached to the subsystems they constrain;
- every document, its section and the documents it references.

Every work-item node carries a `status` from the lifecycle `NOT_STARTED → AUDITING → IMPLEMENTING → WIRED → REAL_TESTING → E2E_PROVEN → COMPLETE` (+ `BLOCKED`) and an `evidence` list. `COMPLETE` without evidence is rejected by the tooling.

```bash
python3 tools/graph.py ready              # what can be started now
python3 tools/graph.py show M1.5          # one node with all its links
python3 tools/graph.py set IMP-EV-0013 WIRED
python3 tools/graph.py status             # milestone roll-up
python3 tools/graph.py render > graph/PROJECT_GRAPH.md   # refresh the mermaid view
python3 tools/check_dossier.py            # integrity gate
```

The tools need only Python 3.9+ and the standard library.

## Non-negotiables in one paragraph

Names do not satisfy behavior. A feature is complete only when its domain contract, canonical owner, production wiring, policy, persistence, real effector, failure/recovery, evidence, projection and a real test all exist. No mock closes a production feature. Every protected effect has a receipt. Memory is not recovery. There is exactly one scheduler, one policy kernel, one event store, one context engine, one tool runtime. Release Zero must pass on a packaged build before anyone says "Modbit works end to end".

## Status

Specification: frozen at 291 requirement rows (V3.1, 2026-09-03).  
Milestone status is **derived from the project graph** by `tools/build_manifest.py`; hand edits fail `tools/check_dossier.py` (G5).

| Milestone | Scope | Status | Required proof |
|---|---|---|---|
| M0 | repository/CI/protocol generation | COMPLETE | clean clone build + architecture lint (CI runs 3-OS matrix + architecture/decision/coverage guards; runs 33820576307, 33821105668) |
| M1 | durable local shell/Core | COMPLETE | create task, kill/restart app+Core, exact recovery (SIGKILL crash/restart test green; 5/5 milestone tasks + 26/26 IMP tasks COMPLETE; CI 33841864831) |
| M2 | real local coding loop | IN_PROGRESS | live provider + real repo edit/test/review |
| M3 | context intelligence | IN_PROGRESS | fixed-revision retrieval benchmarks + freshness proof |
| M4 | durable recovery spine | IN_PROGRESS | kill-point suite, compaction/checkpoint fencing |
| M5 | procedural runtime/skills | IN_PROGRESS | real tools through isolated composition + skill provenance |
| M6 | subagents/fleet | IN_PROGRESS | durable isolated child execution/conflict proof |
| M7 | live browser/computer use | IN_PROGRESS | actual Chromium, same-session takeover, hostile-page test |
| M8 | isolated cloud execution | IN_PROGRESS | real guest, tenant isolation, loss/recovery |
| M9 | memory/effects/security | IN_PROGRESS | promotion policy + receipt chain + attack suite |
| M10 | release hardening | IN_PROGRESS | full Release Zero proof + package evidence |
