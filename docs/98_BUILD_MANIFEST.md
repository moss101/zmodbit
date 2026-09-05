# Build Manifest

> The Status column below is **derived from `graph/project-graph.json`** by `tools/build_manifest.py` (Future-tasks.md section 4 item 5). Hand edits fail `tools/check_dossier.py` (G5). Scope and required-proof text is maintained by implementation agents.

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

## Task manifest rule

Individual tasks are tracked using `86_TASK_CARD_TEMPLATE.md`. A task row must carry requirement IDs and evidence before COMPLETE. Never bulk-mark an entire milestone complete because its directory/modules exist.

## Task-level status source of truth

Task-level (`IMP-EV-*`, `Mx.y`) status is stored on the corresponding node in `../graph/project-graph.json` and must use the lifecycle strings in `93_STATUS_VOCABULARY_AND_LIFECYCLE.md`. This table is the milestone roll-up of that graph. Regenerate the roll-up and validate consistency with:

```bash
python3 tools/graph.py status
python3 tools/check_dossier.py
```

A milestone row here may not read `COMPLETE` while any task node in that milestone is not `COMPLETE`; `tools/check_dossier.py` enforces this.
