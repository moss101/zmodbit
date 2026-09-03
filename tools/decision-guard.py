#!/usr/bin/env python3
"""Modbit decision guard (M0.2, docs/02 § Change control).

Fails when a locked architecture file is changed without an ACCEPTED Decision
Record (ADR) covering it in the same changeset. Also validates that every
changed ADR in docs/decisions/ is complete (all required sections, accepted
status, concrete human approval) so an empty ADR cannot rubber-stamp a change.

Exit codes: 0 = clean, 1 = violations, 2 = could not run.

Locked file set (docs/decisions/README.md, derived from AGENTS.md + docs/46):
requirement rows, dispositions and canonical owners are LOCKED; the decision
register, supersession ledger and architecture guardrails are their authority.

Self-test (run in CI) proves the engine rejects: a locked change with no ADR,
a locked change with an invalid ADR, and accepts valid changesets.
"""

import argparse
import re
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
DECISIONS_DIR = REPO_ROOT / "docs" / "decisions"

LOCKED_FILES = [
    "docs/02_AUTHORITY_AND_DECISIONS.md",
    "docs/03_ARCHITECTURAL_CONFLICTS_AND_SUPERSESSIONS.md",
    "docs/40_EVIDENCE_DERIVED_REQUIREMENT_LEDGER.md",
    "docs/41_EVIDENCE_DERIVED_IMPLEMENTATION_TASKS.md",
    "docs/42_EVIDENCE_DERIVED_QUALIFICATION_TEST_MATRIX.md",
    "docs/46_REQUIREMENT_COVERAGE_FREEZE_GATE.md",
    "docs/81_ARCHITECTURE_GUARDRAILS_AND_FORBIDDEN_DUPLICATION.md",
]

REQUIRED_SECTIONS = [
    "Trigger / Evidence",
    "Current Behavior",
    "Proposed Replacement",
    "Migration",
    "Compatibility",
    "Security Impact",
    "Test Impact",
    "Rollback",
    "Explicit User Approval",
]
APPROVAL_PLACEHOLDER = "Who approved, when, and via what channel."
TEMPLATE_NAME = "TEMPLATE.md"
LEDGER_NAME = "README.md"


def parse_adr_text(text):
    """Return (errors, affects) for one ADR file's content."""
    errors = []
    affects = []
    m = re.search(r"^\s*-\s*\*\*Status:\*\*\s*(.+)$", text, re.M)
    status = m.group(1).strip() if m else ""
    if not status:
        errors.append("missing Status field")
    elif status.upper() != "ACCEPTED":
        errors.append("status is %s, not ACCEPTED" % status)
    m = re.search(r"^\s*-\s*\*\*Affects:\*\*\s*(.*)$", text, re.M)
    if m:
        affects = [p.strip().strip("`") for p in m.group(1).split(",") if p.strip()]
    else:
        errors.append("missing Affects field")
    for section in REQUIRED_SECTIONS:
        m = re.search(
            r"^##\s+%s\s*\n(.*?)(?=^##\s|\Z)" % re.escape(section), text, re.M | re.S
        )
        if not m:
            errors.append("missing section '%s'" % section)
            continue
        body = m.group(1).strip()
        if not body:
            errors.append("section '%s' is empty" % section)
        elif section == "Explicit User Approval" and body.startswith(APPROVAL_PLACEHOLDER):
            errors.append("Explicit User Approval still contains the template placeholder")
    return errors, affects


def check(changed_files, decisions_dir):
    """Return violation strings for a changeset (list of repo-relative paths)."""
    violations = []
    changed = set(changed_files)
    changed_adrs = {
        f
        for f in changed
        if f.startswith("docs/decisions/")
        and f.endswith(".md")
        and Path(f).name not in (TEMPLATE_NAME, LEDGER_NAME)
    }

    accepted_affects = set()
    for adr in sorted(changed_adrs):
        path = Path(decisions_dir) / Path(adr).name
        try:
            text = path.read_text(encoding="utf-8")
        except OSError as exc:
            violations.append("ADR %s unreadable: %s" % (adr, exc))
            continue
        errors, affects = parse_adr_text(text)
        if errors:
            violations.append(
                "ADR %s invalid: %s" % (adr, "; ".join(errors))
            )
        elif affects:
            accepted_affects.update(affects)

    for locked in LOCKED_FILES:
        if locked in changed and locked not in accepted_affects:
            violations.append(
                "locked file %s changed without an ACCEPTED ADR covering it in this "
                "changeset (docs/02 change control; see docs/decisions/README.md)"
                % locked
            )
    return violations


def changed_files_from_git():
    base = os.environ.get("GITHUB_BASE_REF")
    if base:
        out = subprocess.run(
            ["git", "diff", "--name-only", "origin/%s...HEAD" % base],
            capture_output=True, text=True,
        )
        return out.stdout.split()
    out = subprocess.run(
        ["git", "diff", "--name-only", "HEAD~1"], capture_output=True, text=True
    )
    return out.stdout.split() if out.returncode == 0 else []


def self_test():
    """Prove the guard detects locked changes without/with invalid ADRs."""
    import tempfile

    valid_adr = (
        "# ADR-9000: test\n\n"
        "- **ID:** ADR-9000\n- **Status:** ACCEPTED\n- **Date:** 2026-09-04\n"
        "- **Affects:** docs/02_AUTHORITY_AND_DECISIONS.md\n- **Decides:** test\n\n"
        + "".join("## %s\n\nreal content\n\n" % s for s in REQUIRED_SECTIONS[:-1])
        + "## Explicit User Approval\n\nApproved by mohsin, 2026-09-04, in session.\n"
    )
    invalid_adr = valid_adr.replace(
        "Approved by mohsin, 2026-09-04, in session.", APPROVAL_PLACEHOLDER
    )

    scenarios = [
        ("locked change, no ADR",
         [LOCKED_FILES[0]], {}, True),
        ("locked change, valid ADR in changeset",
         [LOCKED_FILES[0], "docs/decisions/ADR-9000-test.md"],
         {"docs/decisions/ADR-9000-test.md": valid_adr}, False),
        ("unlocked change only",
         ["README.md", "crates/domain/src/lib.rs"], {}, False),
        ("locked change, ADR missing approval",
         [LOCKED_FILES[0], "docs/decisions/ADR-9000-test.md"],
         {"docs/decisions/ADR-9000-test.md": invalid_adr}, True),
        ("locked change, ADR not ACCEPTED",
         [LOCKED_FILES[0], "docs/decisions/ADR-9000-test.md"],
         {"docs/decisions/ADR-9000-test.md": valid_adr.replace("ACCEPTED", "PROPOSED")}, True),
    ]
    failures = 0
    with tempfile.TemporaryDirectory() as tmp:
        d = Path(tmp) / "decisions"
        d.mkdir()
        for name, changed, files, should_fail in scenarios:
            for rel, content in files.items():
                p = d / Path(rel).name
                p.write_text(content, encoding="utf-8")
            violations = check(changed, d)
            for rel in files:
                (d / Path(rel).name).unlink()
            ok = bool(violations) == should_fail
            print("  %-45s %s" % (name, "OK" if ok else "SELF-TEST FAIL: %r" % violations))
            failures += 0 if ok else 1
    return failures == 0


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--changed", nargs="*", default=None,
                        help="explicit changeset (repo-relative paths); default: derive from git")
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--decisions-dir", default=str(DECISIONS_DIR))
    args = parser.parse_args(argv)

    d = Path(args.decisions_dir)
    if args.self_test:
        print("decision-guard self-test:")
        return 0 if self_test() else 1

    if not d.is_dir():
        print("decision-guard: decisions dir missing: %s" % d, file=sys.stderr)
        return 2
    changed = args.changed if args.changed is not None else changed_files_from_git()
    violations = check(sorted(changed), d)
    if violations:
        print("decision-guard: %d violation(s):" % len(violations))
        for v in violations:
            print("  - %s" % v)
        return 1
    print("decision-guard: OK (%d changed files checked, %d locked files watched)"
          % (len(changed), len(LOCKED_FILES)))
    return 0


if __name__ == "__main__":
    import os
    sys.exit(main(sys.argv[1:]))
