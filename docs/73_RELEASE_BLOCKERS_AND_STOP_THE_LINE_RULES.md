# Release Blockers and Stop-the-Line Rules

The following block release and must not be waived by a coding agent:

- any production requirement without qualification evidence;
- fake/demo provider, filesystem, browser, sandbox or policy path reachable in release build;
- protected effect that can execute without authoritative decision/receipt path;
- known duplicate effect after retry/crash;
- stale checkpoint/compaction writer can overwrite newer state;
- cross-tenant access or raw secret leak;
- browser/computer agent can fight human control;
- model/page/tool content can elevate its own capability;
- memory used to reconstruct protocol state after crash;
- unreconciled schema/event version incompatibility;
- hidden test skip or weakened acceptance assertion;
- broad feature closed on mock-only proof;
- Release Zero cannot be reproduced from clean environment.

If one occurs, mark affected work BLOCKED/FAILED and fix root cause. Do not downgrade the test or describe it as follow-up polish.
