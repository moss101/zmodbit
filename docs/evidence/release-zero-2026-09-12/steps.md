| docs/60 step | status | evidence (this run unless noted) |
|---|---|---|
| 2 (open/trust the real Git repository) | PASS | repo_picker_e2e;policy_attack_suite (see bins.txt) |
| 3 (start a coding task) | PASS | surface_e2e;daemon_scripted_e2e (see bins.txt) |
| 4 (context retrieval + provenance) | PASS | daemon_context_query_e2e;daemon_retrieve_gate_e2e (see bins.txt) |
| 5 (task-scoped tool projection) | PASS | scheduler_run (see bins.txt) |
| 6 (real tool effectors) | PASS | daemon_scripted_e2e (see bins.txt) |
| 7 (bounded child agent spawn (admission)) | PASS | agent_fleet_e2e (see bins.txt) |
| 8 (child result envelope) | PASS | agent_fleet_e2e (see bins.txt) |
| 9 (multi-file change into isolated worktree) | PASS | daemon_edit_gate_e2e (see bins.txt) |
| 10 (failing tests repaired, run survives) | PASS | daemon_scripted_e2e (see bins.txt) |
| 11 (media/pdf artifact via Media Pipeline) | PASS | daemon_media_e2e (see bins.txt) |
| 12 (real Chromium attach) | PASS | cdp_e2e (see bins.txt) |
| 13 (UI behavior validated structurally) | PASS | cdp_e2e (see bins.txt) |
| 14 (protected effect → pending approval) | PASS | approvals_e2e (see bins.txt) |
| 15 (renderer/Core termination with pending approval) | PASS | approvals_e2e;crash_restart (see bins.txt) |
| 16 (hard-kill Core, recover pending state) | PASS | approvals_e2e (see bins.txt) |
| 17 (approve → receipt, no duplicate on replay) | PASS | approvals_e2e (see bins.txt) |
| 18 (lossless event replay from offset) | PASS | daemon_sse (see bins.txt) |
| 19 (verification + evidence bound to revision) | PASS | daemon_scripted_e2e;diagnostics_regression (see bins.txt) |
| fault:stale compaction rejected (—) | PASS | daemon_compaction_e2e (see bins.txt) |
| fault:stale checkpoint epoch rejected (—) | PASS | daemon_checkpoint_journal_e2e (see bins.txt) |
| fault:duplicate spawn reattaches (—) | PASS | agent_fleet_e2e (see bins.txt) |
| fault:optimistic revert refuses overwrite (—) | PASS | daemon_edit_gate_e2e (see bins.txt) |
| fault:output spill retains full log (—) | PASS | daemon_output_e2e;daemon_scrollback_e2e (see bins.txt) |
| fault:prompt injection cannot expand capability (—) | PASS | policy_attack_suite (see bins.txt) |
| fault:browser absence stays explicit (—) | PASS | cdp_e2e (see bins.txt) |
| 1 (authenticate real staging gateway) | OPERATOR-GATED | standing proof: nightly-live five green nights (Future-tasks §1); production sign-in needs operator credentials (BLOCKED_EXTERNAL_CREDENTIAL precedent) |
| resume/replay exactness (pass criteria) | PASS | daemon_resume_e2e;daemon_protocol_state_e2e |
