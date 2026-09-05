#!/usr/bin/env python3
"""Typed evidence classification for the Modbit project graph.

Governing source: Future-tasks.md section 4 item 1 (audit of 2026-09-05) and
docs/82_NO_PLACEHOLDER_PRODUCTION_EVIDENCE_GATE.md. Before this module existed,
`check_dossier.py` accepted any non-empty evidence list; 709 of the entries were
bare CI run URLs or commit hashes, which is how M3-M10 were closed without
product code.

Rule: a `COMPLETE` or `E2E_PROVEN` node closes only with at least one *typed*
evidence reference. Untyped refs (bare `run:` CI URLs, `commit:` hashes) may
stay on the node as supporting history but never close it.

Typed forms
  log:docs/evidence/<file>      committed real-command output log; the file
                                must exist in the repository
  scenario:E2E-nnn              a release-gate scenario from docs/51 that was
                                proven; the id must exist as a scenario node
  receipt:<64 lowercase hex>    sha256 receipt over a real artifact/log
  run:<run-id>/<test-name>      a CI or local run bound to a named integration
                                or live test (e.g. `run:33823102873/
                                sigkill_crash_restart_recovers_fleet` or
                                `run:2026-09-05T10:22Z/qual-ev-0038`). A bare
                                `run:` URL (contains `//`) is never typed.

Standard library only, Python 3.9+.
"""
import os
import re

_LOG_RE = re.compile(r"^log:(docs/evidence/[A-Za-z0-9._/-]+)$")
_SCENARIO_RE = re.compile(r"^scenario:E2E-\d{3,4}$")
_RECEIPT_RE = re.compile(r"^receipt:[0-9a-f]{64}$")
_RUN_RE = re.compile(r"^run:([^ /]+)[ /]([A-Za-z0-9_:.+-]{4,})$")


def typed_refs(evidence, root=None, scenario_ids=None):
    """Return the subset of `evidence` that carries a typed proof.

    root            repository root; when given, `log:` targets must exist on
                    disk. None skips the existence check (self-tests).
    scenario_ids    iterable of known scenario ids; None skips the check.
    """
    typed = []
    for ref in evidence or []:
        if not isinstance(ref, str):
            continue
        r = ref.strip()
        m = _LOG_RE.match(r)
        if m:
            if root is None or os.path.isfile(os.path.join(root, m.group(1))):
                typed.append(r)
            continue
        if _SCENARIO_RE.match(r):
            if scenario_ids is None or r.split(":", 1)[1] in scenario_ids:
                typed.append(r)
            continue
        if _RECEIPT_RE.match(r):
            typed.append(r)
            continue
        if r.startswith("run:") and "//" not in r and _RUN_RE.match(r):
            typed.append(r)
            continue
    return typed


def self_test():
    """Prove each typed form passes and every untyped loophole fails."""
    ok = True

    def check(name, evidence, expect_typed, root=None, scenario_ids=None):
        nonlocal ok
        got = typed_refs(evidence, root=root, scenario_ids=scenario_ids)
        good = got == expect_typed
        ok = ok and good
        print("  %-52s %s" % (name, "OK" if good else "SELF-TEST FAIL: got %r" % (got,)))

    receipt = "receipt:" + "a" * 64
    check("log: form accepted", ["log:docs/evidence/x.log"], ["log:docs/evidence/x.log"])
    check("log: missing file rejected", ["log:docs/evidence/nope.log"], [],
          root=os.path.dirname(os.path.abspath(__file__)))
    check("scenario accepted", ["scenario:E2E-001"], ["scenario:E2E-001"], scenario_ids={"E2E-001"})
    check("scenario unknown id rejected", ["scenario:E2E-099"], [], scenario_ids={"E2E-001"})
    check("receipt accepted", [receipt], [receipt])
    check("receipt wrong length rejected", ["receipt:abc123"], [])
    check("run with named test accepted",
          ["run:33823102873/sigkill_crash_restart_recovers_fleet"],
          ["run:33823102873/sigkill_crash_restart_recovers_fleet"])
    check("run with qual id accepted", ["run:2026-09-05T10:22Z/qual-ev-0038"],
          ["run:2026-09-05T10:22Z/qual-ev-0038"])
    check("bare CI run URL rejected",
          ["run:https://github.com/moss101/zmodbit/actions/runs/33815987658"], [])
    check("bare run id rejected", ["run:33815987658"], [])
    check("commit hash never typed", ["commit:d210f31ad213aae81090472ce037d2586e2fe4e4"], [])
    check("empty evidence yields nothing", [], [])
    return ok
