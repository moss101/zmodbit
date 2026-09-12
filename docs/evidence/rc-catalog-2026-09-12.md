# RC E2E Catalog

Generated 2026-09-12T19:48:11 by tools/rc_catalog.py (rev 85721a5fdad2).

Every docs/51 acceptance scenario mapped to its proving tests in
the current tree. Dead mappings fail the generator — the catalog
cannot silently rot.

| Scenario | Title | Proving tests |
|---|---|---|
| E2E-001 | Fresh local coding task | `daemon_scripted_e2e`, `daemon_live_e2e` |
| E2E-002 | Command failure repair | `daemon_scripted_e2e` |
| E2E-003 | Renderer restart | `detach_reattach`, `daemon_sse` |
| E2E-004 | Core crash/resume during approval | `approvals_e2e::pending_approval_survives_a_core_kill` |
| E2E-005 | Core crash after effect dispatch ambiguity | `crash_restart` |
| E2E-006 | Compaction stale rejection | `daemon_compaction_e2e` |
| E2E-007 | Checkpoint fencing | `daemon_checkpoint_journal_e2e` |
| E2E-008 | Durable terminal replay | `daemon_scrollback_e2e` |
| E2E-009 | Transactional subagent admission | `agent_fleet_e2e::admission_refuses_without_partial_reservation` |
| E2E-010 | Independent subagents | `agent_fleet_e2e` |
| E2E-011 | Tool projection | `tool_conformance` |
| E2E-012 | Procedural runtime | `external_tools_e2e` |
| E2E-013 | Real browser semantic control | `cdp_e2e::cdp_bridge_drives_a_real_chromium_end_to_end` |
| E2E-014 | Browser visual fallback | `cdp_e2e` |
| E2E-015 | Browser user takeover | `browser_view_e2e::browser_view_launches_observes_and_takes_over` |
| E2E-016 | Prompt injection resistance | `policy_attack_suite`, `cdp_e2e::hostile_page_content_stays_inert_data` |
| E2E-017 | Cloud isolated task | `gateway_e2e::gateway_relays_real_core_work_and_enforces_tenant_isolation` |
| E2E-018 | Sandbox loss recovery | `gateway_e2e` |
| E2E-019 | Provider failover before effects | `daemon_roles_e2e` |
| E2E-020 | Provider stream interruption after partial tool proposal | `daemon_sse` |
| E2E-021 | Large output pagination | `daemon_output_e2e` |
| E2E-022 | Stale code reference | `code_surface` |
| E2E-023 | Protected path attack | `policy_attack_suite` |
| E2E-024 | Cross-tenant cloud isolation | `gateway_e2e::gateway_relays_real_core_work_and_enforces_tenant_isolation` |
| E2E-025 | End-to-end release-zero scenario | `release_zero_sh` |

Coverage: 25/25 scenarios mapped, 0 problem(s).
