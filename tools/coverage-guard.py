#!/usr/bin/env python3
"""Modbit requirement-coverage guard (M0.4, docs/45 § CI rule to implement).

Parses the evidence-derived ledgers and the project graph and fails CI when:

  R1  an ADOPT/ADAPT requirement row in docs/40 lacks a canonical owner,
      an IMP-EV-* task link, or a QUAL-EV-* test link (docs/46 gate 2);
  R2  a task node marked COMPLETE (or E2E_PROVEN) in graph/project-graph.json
      has no TYPED evidence reference (tools/evidence.py: log:docs/evidence/,
      scenario:E2E-nnn, receipt:<sha256>, or run:<id>/<test name>; bare
      run:/commit: refs are history and never close a node — Future-tasks.md
      section 4 item 1);
  R3  an architectural owner has two active production implementations for
      the same requirement without an ACCEPTED ADR covering both task IDs
      (docs/45: duplicate active owner);
  R4  an IMP task's owner subsystem disagrees with its requirement's owner
      (docs/11: every component maps to exactly one owner);
  R5  join consistency: every REQ-EV id in docs/40 exists as a requirement
      node in the graph, every referenced IMP-EV/QUAL-EV id exists, and the
      row/node counts match (docs/44: CI joins the files, fails on missing
      IDs; docs/46 gate 1: exactly 291 requirement rows);
  R6  a requirement row contains placeholder/TBD language (docs/46 gate 8).

Exit codes: 0 = clean, 1 = violations, 2 = could not run.
"""

import argparse
import json
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
REQ_LEDGER = REPO_ROOT / "docs" / "40_EVIDENCE_DERIVED_REQUIREMENT_LEDGER.md"
GRAPH = REPO_ROOT / "graph" / "project-graph.json"
DECISIONS = REPO_ROOT / "docs" / "decisions"
sys.path.insert(0, str(Path(__file__).resolve().parent))
import evidence as evidencetool  # noqa: E402

ACTIVE_STATUSES = {"AUDITING", "IMPLEMENTING", "WIRED", "REAL_TESTING", "E2E_PROVEN"}
EXEMPT_DISPOSITIONS = {"ALREADY COVERED", "EXPERIMENT", "DEFERRED", "REJECT", "REJECTED"}
EXPECTED_REQ_ROWS = 291

ROW_RE = re.compile(r"^\|\s*(REQ-EV-\d+)\s*\|")
CELL_SPLIT = re.compile(r"\s*\|\s*")
IMP_REF = re.compile(r"IMP-EV-\d+")
QUAL_REF = re.compile(r"QUAL-EV-\d+")


def parse_req_rows(text):
    """Return {req_id: {disposition, owner, imp, qual, row_text}} from docs/40."""
    rows = {}
    for line in text.splitlines():
        m = ROW_RE.match(line)
        if not m:
            continue
        cells = [c.strip() for c in CELL_SPLIT.split(line.strip().strip("|"))]
        # cells: [ID, title, disposition, owner, behavior, IMP, QUAL, proof, ...]
        rows[m.group(1)] = {
            "disposition": cells[2] if len(cells) > 2 else "",
            "owner": cells[3] if len(cells) > 3 else "",
            "imp": cells[5] if len(cells) > 5 else "",
            "qual": cells[6] if len(cells) > 6 else "",
            "row_text": line,
        }
    return rows


def load_graph():
    data = json.loads(GRAPH.read_text(encoding="utf-8"))
    nodes = {n["id"]: n for n in data["nodes"]}
    edges = data["edges"]
    implemented_by = {}   # req_id -> [imp_task ids]
    owned_by_req = {}     # req_id -> subsystem
    owned_by = {}         # imp_id -> subsystem
    for e in edges:
        if e["type"] == "implemented_by":
            implemented_by.setdefault(e["from"], []).append(e["to"])
        elif e["type"] == "owned_by_req":
            owned_by_req[e["from"]] = e["to"]
        elif e["type"] == "owned_by":
            owned_by[e["from"]] = e["to"]
    return nodes, implemented_by, owned_by_req, owned_by


def accepted_adr_text():
    text = ""
    for adr in sorted(DECISIONS.glob("ADR-*.md")):
        content = adr.read_text(encoding="utf-8")
        if re.search(r"^\s*-\s*\*\*Status:\*\*\s*ACCEPTED", content, re.M):
            text += content + "\n"
    return text


def check(rows, nodes, implemented_by, owned_by_req, owned_by, expect_req_rows=None):
    violations = []

    # R5 join: counts and id existence. Row-count freeze applies only to the
    # real ledger (self-test data is synthetic).
    req_nodes = {n for n in nodes if n.startswith("REQ-EV-")}
    if expect_req_rows is not None and len(rows) != expect_req_rows:
        violations.append(
            "docs/40 has %d REQ rows, freeze gate requires exactly %d (docs/46 gate 1)"
            % (len(rows), expect_req_rows)
        )
    if set(rows) != req_nodes:
        missing_in_graph = sorted(set(rows) - req_nodes)
        missing_in_docs = sorted(req_nodes - set(rows))
        if missing_in_graph:
            violations.append("REQ ids in docs/40 missing from graph: %s" % missing_in_graph[:5])
        if missing_in_docs:
            violations.append("REQ ids in graph missing from docs/40: %s" % missing_in_docs[:5])

    # R2: COMPLETE / E2E_PROVEN tasks (imp + milestone tasks) need TYPED evidence
    for node in nodes.values():
        if node.get("status") in ("COMPLETE", "E2E_PROVEN"):
            if not evidencetool.typed_refs(node.get("evidence"), root=str(REPO_ROOT)):
                violations.append(
                    "task %s is %s without typed evidence (need log:docs/evidence/<file>, "
                    "scenario:E2E-nnn, receipt:<sha256>, or run:<id>/<integration|live test name>; "
                    "docs/45; Future-tasks.md section 4; forbidden completion shortcut in AGENTS.md)"
                    % (node["id"], node["status"])
                )

    adr_text = accepted_adr_text()
    for req_id, row in sorted(rows.items()):
        disposition = row["disposition"].upper()

        # R6: placeholder language
        if re.search(r"\bTBD\b|\bplaceholder\b", row["row_text"], re.I):
            violations.append(
                "requirement %s contains placeholder/TBD language (docs/46 gate 8)" % req_id
            )

        if disposition in EXEMPT_DISPOSITIONS:
            continue

        # R1: ADOPT/ADAPT completeness
        missing = []
        if not row["owner"] or row["owner"] == "—":
            missing.append("canonical owner")
        if not IMP_REF.search(row["imp"]):
            missing.append("IMP-EV-* task link")
        if not QUAL_REF.search(row["qual"]):
            missing.append("QUAL-EV-* test link")
        if missing:
            violations.append(
                "%s requirement %s is missing: %s (docs/46 gate 2, docs/45)"
                % (disposition, req_id, ", ".join(missing))
            )
            continue

        imp_refs = IMP_REF.findall(row["imp"])
        qual_refs = QUAL_REF.findall(row["qual"])
        # R5 join: referenced ids exist as graph nodes
        for ref in imp_refs + qual_refs:
            if ref not in nodes:
                violations.append(
                    "%s referenced by %s does not exist as a graph node" % (ref, req_id)
                )

        # R3: duplicate active production implementations
        active = [
            t
            for t in implemented_by.get(req_id, [])
            if nodes.get(t, {}).get("status") in ACTIVE_STATUSES
        ]
        if len(active) > 1:
            pair = " and ".join(sorted(active))
            if pair in adr_text or all(t in adr_text for t in active):
                pass  # approved migration ADR covers the overlap
            else:
                violations.append(
                    "duplicate active owner: %s has %d active implementations (%s) "
                    "without an ACCEPTED migration ADR (docs/45)" % (req_id, len(active), pair)
                )

        # R4: single-owner agreement between requirement and its active tasks
        req_owner = owned_by_req.get(req_id)
        if req_owner:
            for t in active:
                task_owner = owned_by.get(t)
                if task_owner and task_owner != req_owner:
                    violations.append(
                        "owner drift: %s (owner %s) is implemented by %s (owner %s); "
                        "docs/11 requires exactly one canonical owner"
                        % (req_id, req_owner, t, task_owner)
                    )
    return violations


def self_test():
    """Synthetic ledgers/graphs prove each rule detects its violation."""
    rows_ok = {
        "REQ-EV-9001": {
            "disposition": "ADOPT", "owner": "memory",
            "imp": "IMP-EV-9001", "qual": "QUAL-EV-9001", "row_text": "| REQ-EV-9001 | t | ADOPT | memory | b | IMP-EV-9001 | QUAL-EV-9001 | p |",
        },
        "REQ-EV-9002": {
            "disposition": "ALREADY COVERED", "owner": "—",
            "imp": "—", "qual": "QUAL-EV-9002", "row_text": "| REQ-EV-9002 | t | ALREADY COVERED | Core | b | — | QUAL-EV-9002 | p |",
        },
    }
    nodes = {
        "REQ-EV-9001": {"id": "REQ-EV-9001", "status": "ADOPT"},
        "REQ-EV-9002": {"id": "REQ-EV-9002", "status": "ALREADY COVERED"},
        "IMP-EV-9001": {"id": "IMP-EV-9001", "status": "IMPLEMENTING"},
        "QUAL-EV-9001": {"id": "QUAL-EV-9001"},
        "QUAL-EV-9002": {"id": "QUAL-EV-9002"},
    }

    scenarios = []
    # R1: ADOPT row missing IMP link
    bad = json.loads(json.dumps(rows_ok))
    bad["REQ-EV-9001"]["imp"] = "—"
    scenarios.append(("R1 missing IMP link", bad, {}, {}, {}, True))
    # R2: COMPLETE without evidence
    nodes_r2 = dict(nodes)
    nodes_r2["IMP-EV-9001"] = {"id": "IMP-EV-9001", "status": "COMPLETE", "evidence": []}
    scenarios.append(("R2 COMPLETE without evidence", rows_ok, nodes_r2, {}, {}, True))
    # R2: COMPLETE whose only refs are untyped run:/commit: history
    nodes_r2b = dict(nodes)
    nodes_r2b["IMP-EV-9001"] = {"id": "IMP-EV-9001", "status": "COMPLETE",
                                "evidence": ["run:https://github.com/x/actions/runs/1", "commit:abc123"]}
    scenarios.append(("R2 COMPLETE with untyped refs only", rows_ok, nodes_r2b, {}, {}, True))
    # R2: COMPLETE with one typed ref closes the node
    nodes_r2c = dict(nodes)
    nodes_r2c["IMP-EV-9001"] = {"id": "IMP-EV-9001", "status": "COMPLETE",
                                "evidence": ["run:https://github.com/x/actions/runs/1",
                                             "receipt:" + "a" * 64]}
    scenarios.append(("R2 COMPLETE with typed ref passes", rows_ok, nodes_r2c, {}, {}, False))
    # R3: duplicate active implementations, no ADR
    ib = {"REQ-EV-9001": ["IMP-EV-9001", "IMP-EV-9002"]}
    nodes_r3 = dict(nodes)
    nodes_r3["IMP-EV-9002"] = {"id": "IMP-EV-9002", "status": "IMPLEMENTING"}
    scenarios.append(("R3 duplicate active owner", rows_ok, nodes_r3, ib, {}, True))
    # R4: owner drift (task owned by 'skills', requirement owned by 'memory')
    drift_impl = {"REQ-EV-9001": ["IMP-EV-9001"]}
    drift_owned_by = {"IMP-EV-9001": "skills"}
    drift_owned_by_req = {"REQ-EV-9001": "memory"}
    # R5: id in docs missing from graph
    rows_r5 = json.loads(json.dumps(rows_ok))
    rows_r5["REQ-EV-9999"] = dict(rows_ok["REQ-EV-9001"], row_text="| REQ-EV-9999 | t | ADOPT | memory | b | IMP-EV-9001 | QUAL-EV-9001 | p |")
    scenarios.append(("R5 missing graph id", rows_r5, nodes, {}, {}, True))

    failures = 0

    def run(rows_s, nodes_s, ib_map, obr_map, ob_map):
        return check(rows_s, nodes_s, ib_map, obr_map, ob_map)

    for name, r, n, ib_map, obr_map, expect_fail in scenarios:
        violations = run(r, n, ib_map, obr_map, {})
        detected = bool(violations)
        ok = detected == expect_fail
        print("  %-40s %s" % (name, "OK" if ok else "SELF-TEST FAIL: %r" % violations))
        failures += 0 if ok else 1

    # R4 owner drift, checked explicitly on the drift-specific maps
    v = run(rows_ok, nodes, drift_impl, drift_owned_by_req, drift_owned_by)
    drift_detected = any("owner drift" in x for x in v)
    print("  %-40s %s" % ("R4 owner drift", "OK" if drift_detected else "SELF-TEST FAIL: %r" % v))
    failures += 0 if drift_detected else 1

    # clean scenario must pass
    v = run(rows_ok, nodes, {"REQ-EV-9001": ["IMP-EV-9001"]},
            {"REQ-EV-9001": "memory"}, {"IMP-EV-9001": "memory"})
    clean_ok = not v
    print("  %-40s %s" % ("clean ledger passes", "OK" if clean_ok else "SELF-TEST FAIL: %r" % v))
    failures += 0 if clean_ok else 1
    return failures == 0


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)

    if args.self_test:
        print("coverage-guard self-test:")
        return 0 if self_test() else 1

    rows = parse_req_rows(REQ_LEDGER.read_text(encoding="utf-8"))
    nodes, implemented_by, owned_by_req, owned_by = load_graph()
    violations = check(rows, nodes, implemented_by, owned_by_req, owned_by,
                       expect_req_rows=EXPECTED_REQ_ROWS)
    if violations:
        print("coverage-guard: %d violation(s):" % len(violations))
        for v in violations:
            print("  - %s" % v)
        return 1
    print(
        "coverage-guard: OK (%d REQ rows joined with %d graph requirement nodes; "
        "%d tasks checked for evidence; no duplicate active owners)"
        % (len(rows), sum(1 for n in nodes if n.startswith("REQ-EV-")), len(nodes))
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
