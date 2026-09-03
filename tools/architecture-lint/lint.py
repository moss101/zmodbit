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
  - IMP-EV-0208 (REQ/QUAL-EV-0208): one Engineering Memory interface — the name
    "memory" is reserved to modbit-memory, and memory is severed from the
    durability spine (event-store/checkpoint/compaction) in both directions.
  - IMP-EV-0242 (REQ/QUAL-EV-0242): durable state remains in stores; hook-layer
    members are ephemeral control — severed from durable stores both ways.

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
    # IMP-EV-0208 / REQ-EV-0208 (MOD-STATE-001): Engineering Memory is separate
    # from the durability spine — no edge in either direction, so the memory
    # system cannot become a recovery mechanism and recovery cannot require it.
    ("modbit-memory", "modbit-event-store"),
    ("modbit-event-store", "modbit-memory"),
    ("modbit-memory", "modbit-checkpoint"),
    ("modbit-checkpoint", "modbit-memory"),
    ("modbit-memory", "modbit-compaction"),
    ("modbit-compaction", "modbit-memory"),
]

# IMP-EV-0242 / REQ-EV-0242: durable state remains in stores; live hooks are
# ephemeral control. Hook-layer members (name contains "hook") may hold no
# durable truth: no edge in either direction between hook members and the
# durable stores, so a hook process reset cannot lose durable state.
HOOK_PATTERN = "hook"
DURABLE_STORES = [
    "modbit-event-store",
    "modbit-protocol-state",
    "modbit-checkpoint",
    "modbit-memory",
]

# IMP-EV-0208: name-reserved single ownership. Any member whose name contains
# a reserved pattern must BE the canonical owner, and at most one member may
# match — structurally one Engineering Memory interface, one hook bus.
RESERVED_NAME_PATTERNS = [
    ("memory", "modbit-memory"),  # (pattern, canonical owner; None = M9 future owner)
    ("hook", None),
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

    # IMP-EV-0242: hook members (ephemeral control) hold no durable truth —
    # severed from durable stores in both directions.
    for src, deps in sorted(graph.items()):
        for dst in sorted(deps):
            src_hook = HOOK_PATTERN in src
            dst_hook = HOOK_PATTERN in dst
            dst_durable = dst in DURABLE_STORES
            src_durable = src in DURABLE_STORES
            if (src_hook and dst_durable) or (src_durable and dst_hook):
                violations.append(
                    "durable/live separation violated: %s -> %s (REQ-EV-0242: durable state "
                    "remains in stores; live hooks are ephemeral control)" % (src, dst)
                )

    # IMP-EV-0208: reserved name patterns — single canonical owner each.
    for pattern, owner in RESERVED_NAME_PATTERNS:
        matches = [m for m in graph if pattern in m]
        for m in matches:
            if owner is not None and m != owner:
                violations.append(
                    "second %s system: %s (reserved owner: %s; REQ-EV-0208/QUAL-EV-0208)"
                    % (pattern, m, owner)
                )
        if len(matches) > 1:
            violations.append(
                "multiple %r-layer members: %s (at most one allowed)" % (pattern, sorted(matches))
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

    # IMP-EV-0208: second Engineering Memory owner must be rejected.
    two_mem = dict(clean)
    two_mem["modbit-agent-memory"] = set()
    f3 = check(two_mem)
    if not any("modbit-agent-memory" in v for v in f3):
        print("SELF-TEST FAIL: second memory owner not detected: %r" % f3)
        return False

    # IMP-EV-0208: memory entangled with the recovery spine must be rejected.
    mem_recovery = dict(clean)
    mem_recovery["modbit-memory"] = {"modbit-event-store"}
    f4 = check(mem_recovery)
    if not any("modbit-memory -> modbit-event-store" in v for v in f4):
        print("SELF-TEST FAIL: memory/recovery entanglement not detected: %r" % f4)
        return False

    # IMP-EV-0242: hooks holding durable truth must be rejected, both directions.
    hooks_out = dict(clean)
    hooks_out["modbit-hooks"] = {"modbit-event-store"}
    f5 = check(hooks_out)
    if not any("modbit-hooks -> modbit-event-store" in v for v in f5):
        print("SELF-TEST FAIL: hook->durable-store edge not detected: %r" % f5)
        return False
    hooks_in = dict(clean)
    hooks_in["modbit-event-store"] = {"modbit-domain", "modbit-hooks"}
    f6 = check(hooks_in)
    if not any("modbit-event-store -> modbit-hooks" in v for v in f6):
        print("SELF-TEST FAIL: durable-store->hook edge not detected: %r" % f6)
        return False

    print(
        "SELF-TEST OK: forbidden-edge detection works "
        "(6/6 injections caught, clean graph passes)"
    )
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
