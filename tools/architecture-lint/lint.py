#!/usr/bin/env python3
"""Modbit architecture dependency lint (M0.1, docs/12 + docs/81).

Runs `cargo metadata`, builds the workspace crate dependency graph, and rejects
forbidden dependency edges. Exit code 0 = clean, 1 = violations, 2 = could not run.

Rules are derived from docs/12_REPOSITORY_AND_MODULE_LAYOUT.md ("Dependency
direction") and AGENTS.md ("Architecture ownership"):
  - `domain` depends on no infrastructure crate (no workspace-internal deps at all).
  - No crate may depend on an `apps/*` or `services/*` member (top-level consumers only).
  - Explicit forbidden pairs from docs/12, translated to real member names:
      event-store -> providers          ("event-store -> provider implementation")
      workspace  -> browser             ("workspace -> browser")
      sandbox    -> core-runtime        ("Sandbox/browser do not import scheduler internals")
      browser    -> core-runtime        (same)
      modbit-guest -> cloud-api / cloud-worker ("guest -> cloud-api business logic")
      retrieval  -> any app/UI member   ("retrieval -> desktop", "policy -> electron")

`--self-test` proves the checker catches violations: it runs the same rule
engine over a synthetic graph containing a forbidden edge (must FAIL) and a
clean synthetic graph (must PASS), without touching real code.
"""

import json
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]

# (from, to) pairs that must never appear in the workspace dependency graph.
# `from`/`to` are workspace member names as declared in Cargo.toml
# (modbit-* crates keep their full package name here; bins use their dir name).
FORBIDDEN_EDGES = [
    ("modbit-event-store", "modbit-providers"),
    ("modbit-workspace", "modbit-browser"),
    ("modbit-sandbox", "modbit-core-runtime"),
    ("modbit-browser", "modbit-core-runtime"),
    ("modbit-guest", "modbit-cloud-api"),
    ("modbit-guest", "modbit-cloud-worker"),
]

# `from` may not depend on any member whose name is in the list.
FORBIDDEN_EDGES_BY_SOURCE = [
    ("modbit-retrieval", ["modbit-cloud-api", "modbit-cloud-worker", "modbit-sandbox-gateway"]),
    ("modbit-policy", ["modbit-cloud-api", "modbit-cloud-worker", "modbit-sandbox-gateway"]),
]

# Members that are top-level consumers: nothing inside the workspace may depend on them.
LEAF_MEMBERS = {
    "modbit-cloud-api",
    "modbit-cloud-worker",
    "modbit-sandbox-gateway",
    "modbit-execd",
    "modbit-guest",
}

# Foundation member that must not depend on any workspace member.
NO_DEPS_MEMBERS = ["modbit-domain"]


def workspace_graph(root):
    """Return {member_name: set(direct workspace-member deps)} via cargo metadata."""
    meta = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--no-deps"],
            cwd=root,
            capture_output=True,
            text=True,
            check=True,
        ).stdout
    )
    # With --no-deps every listed package IS a workspace member, so member
    # identity is the package name; dependency entries reference that name.
    members = {pkg["name"] for pkg in meta["packages"]}
    graph = {}
    for pkg in meta["packages"]:
        graph[pkg["name"]] = {
            dep["name"]
            for dep in pkg.get("dependencies", [])
            if dep.get("kind") in (None, "build") and dep["name"] in members
        }
    return graph


def check(graph):
    """Return a list of violation strings for the given {name: set(deps)} graph."""
    violations = []
    for src, dst in FORBIDDEN_EDGES:
        if dst in graph.get(src, set()):
            violations.append("forbidden edge: %s -> %s (docs/12)" % (src, dst))
    for src, dsts in FORBIDDEN_EDGES_BY_SOURCE:
        for dst in dsts:
            if dst in graph.get(src, set()):
                violations.append("forbidden edge: %s -> %s (docs/12)" % (src, dst))
    for src, deps in sorted(graph.items()):
        for dst in sorted(deps):
            if dst in LEAF_MEMBERS:
                violations.append(
                    "workspace member depends on top-level consumer: %s -> %s (docs/12)" % (src, dst)
                )
    for src in NO_DEPS_MEMBERS:
        for dst in sorted(graph.get(src, set())):
            violations.append(
                "foundation member depends on infrastructure: %s -> %s (docs/12: domain depends "
                "on no infrastructure crate)" % (src, dst)
            )
    return violations


def self_test():
    """Prove the rule engine detects forbidden edges (docs/43 M0.1 acceptance)."""
    clean = {
        "modbit-domain": set(),
        "modbit-core-runtime": {"modbit-domain"},
        "modbit-workspace": {"modbit-domain"},
        "modbit-browser": {"modbit-domain"},
        "modbit-event-store": {"modbit-domain"},
        "modbit-providers": {"modbit-domain"},
    }
    if check(clean):
        print("SELF-TEST FAIL: clean graph reported violations: %r" % check(clean))
        return False

    tainted = dict(clean)
    tainted["modbit-workspace"] = {"modbit-domain", "modbit-browser"}  # docs/12 forbidden pair
    found = check(tainted)
    if not any("modbit-workspace -> modbit-browser" in v for v in found):
        print("SELF-TEST FAIL: injected forbidden edge was not detected: %r" % found)
        return False

    tainted2 = dict(clean)
    tainted2["modbit-domain"] = {"modbit-event-store"}  # foundation depends on infrastructure
    found2 = check(tainted2)
    if not any("modbit-domain -> modbit-event-store" in v for v in found2):
        print("SELF-TEST FAIL: foundation dependency rule not enforced: %r" % found2)
        return False

    print("SELF-TEST OK: forbidden-edge detection works (2/2 injections caught, clean graph passes)")
    return True


def main(argv):
    if "--self-test" in argv:
        return 0 if self_test() else 1
    try:
        graph = workspace_graph(REPO_ROOT)
    except (subprocess.CalledProcessError, FileNotFoundError, json.JSONDecodeError) as exc:
        print("architecture-lint: could not read cargo metadata: %s" % exc, file=sys.stderr)
        return 2
    violations = check(graph)
    if violations:
        print("architecture-lint: %d forbidden dependency violation(s):" % len(violations))
        for v in violations:
            print("  - %s" % v)
        return 1
    print("architecture-lint: OK (%d members checked, 0 violations)" % len(graph))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
