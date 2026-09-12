#!/usr/bin/env python3
"""RC E2E catalog generator (M10.3, docs/51 + docs/70).

Maps every docs/51 acceptance scenario (E2E-001..025) to the TEST
BINARIES/TESTS that prove it in the current tree, verifies every mapping
target EXISTS (a dead mapping fails, exactly like a missing one), and
emits the RC catalog markdown. The mapping is explicit and maintained —
never keyword-guessed.

Usage: tools/rc_catalog.py            # verify + emit to stdout
       tools/rc_catalog.py --write    # also write docs/evidence/rc-catalog-<date>.md
Exit 1 on any unmapped scenario or dead target.
"""

import re
import subprocess
import sys
from datetime import date, datetime
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# docs/51 scenario → proving tests (test binary name → optional test fn).
# Each entry must exist in the tree (verified below).
MAPPING = {
    "E2E-001": [("daemon_scripted_e2e", None), ("daemon_live_e2e", None)],
    "E2E-002": [("daemon_scripted_e2e", None)],
    "E2E-003": [("detach_reattach", None), ("daemon_sse", None)],
    "E2E-004": [("approvals_e2e", "pending_approval_survives_a_core_kill")],
    "E2E-005": [("crash_restart", None)],
    "E2E-006": [("daemon_compaction_e2e", None)],
    "E2E-007": [("daemon_checkpoint_journal_e2e", None)],
    "E2E-008": [("daemon_scrollback_e2e", None)],
    "E2E-009": [("agent_fleet_e2e", "admission_refuses_without_partial_reservation")],
    "E2E-010": [("agent_fleet_e2e", None)],
    "E2E-011": [("tool_conformance", None)],
    "E2E-012": [("external_tools_e2e", None)],
    "E2E-013": [("cdp_e2e", "cdp_bridge_drives_a_real_chromium_end_to_end")],
    "E2E-014": [("cdp_e2e", None)],
    "E2E-015": [("browser_view_e2e", "browser_view_launches_observes_and_takes_over")],
    "E2E-016": [("policy_attack_suite", None), ("cdp_e2e", "hostile_page_content_stays_inert_data")],
    "E2E-017": [("gateway_e2e", "gateway_relays_real_core_work_and_enforces_tenant_isolation")],
    "E2E-018": [("gateway_e2e", None)],
    "E2E-019": [("daemon_roles_e2e", None)],
    "E2E-020": [("daemon_sse", None)],
    "E2E-021": [("daemon_output_e2e", None)],
    "E2E-022": [("code_surface", None)],
    "E2E-023": [("policy_attack_suite", None)],
    "E2E-024": [("gateway_e2e", "gateway_relays_real_core_work_and_enforces_tenant_isolation")],
    "E2E-025": [("release_zero_sh", None)],
}

# Files (relative to ROOT) whose existence satisfies a mapping target.
SPECIAL_FILES = {
    "release_zero_sh": "tools/release/release-zero.sh",
}


def test_file_exists(binary: str) -> bool:
    special = SPECIAL_FILES.get(binary)
    if special:
        return (ROOT / special).exists()
    hits = list((ROOT / "crates").rglob(f"tests/{binary}.rs")) + list(
        (ROOT / "apps").rglob(f"tests/{binary}.rs")
    )
    return bool(hits)


def test_fn_exists(binary: str, fn: str) -> bool:
    hits = list((ROOT / "crates").rglob(f"tests/{binary}.rs")) + list(
        (ROOT / "apps").rglob(f"tests/{binary}.rs")
    )
    for path in hits:
        if re.search(rf"fn {fn}\b", path.read_text(errors="ignore")):
            return True
    return False


def scenario_titles() -> dict:
    titles = {}
    text = (ROOT / "docs/51_E2E_ACCEPTANCE_TEST_CATALOG.md").read_text()
    for match in re.finditer(r"^## (E2E-\d+) — (.+)$", text, re.M):
        titles[match.group(1)] = match.group(2).strip()
    return titles


def main() -> int:
    titles = scenario_titles()
    failures = []
    lines = [
        "# RC E2E Catalog",
        "",
        f"Generated {datetime.now().isoformat(timespec='seconds')} by tools/rc_catalog.py "
        f"(rev {subprocess.run(['git', 'rev-parse', 'HEAD'], capture_output=True, text=True, cwd=ROOT).stdout.strip()[:12]}).",
        "",
        "Every docs/51 acceptance scenario mapped to its proving tests in",
        "the current tree. Dead mappings fail the generator — the catalog",
        "cannot silently rot.",
        "",
        "| Scenario | Title | Proving tests |",
        "|---|---|---|",
    ]
    for scenario in sorted(MAPPING):
        title = titles.get(scenario, "MISSING FROM docs/51")
        targets = []
        for binary, fn in MAPPING[scenario]:
            if not test_file_exists(binary):
                failures.append(f"{scenario}: test binary/source '{binary}' not found")
                targets.append(f"`{binary}` (MISSING)")
            elif fn and not test_fn_exists(binary, fn):
                failures.append(f"{scenario}: test fn '{fn}' not found in {binary}")
                targets.append(f"`{binary}::{fn}` (MISSING)")
            else:
                targets.append(f"`{binary}::{fn}`" if fn else f"`{binary}`")
        lines.append(f"| {scenario} | {title} | {', '.join(targets)} |")

    unmapped = [s for s in titles if s not in MAPPING]
    for scenario in unmapped:
        failures.append(f"{scenario} ('{titles[scenario]}') has no mapping in tools/rc_catalog.py")

    lines.append("")
    lines.append(
        f"Coverage: {len(MAPPING)}/{len(titles)} scenarios mapped, "
        f"{len(failures)} problem(s)."
    )
    output = "\n".join(lines) + "\n"
    print(output)

    if "--write" in sys.argv and not failures:
        out = ROOT / f"docs/evidence/rc-catalog-{date.today().isoformat()}.md"
        out.write_text(output)
        print(f"written: {out}")

    if failures:
        print("RC CATALOG: FAIL", file=sys.stderr)
        for f in failures:
            print(f"  - {f}", file=sys.stderr)
        return 1
    print("RC CATALOG: OK (all scenarios mapped to live proofs)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
