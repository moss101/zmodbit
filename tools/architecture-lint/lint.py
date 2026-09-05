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

Reachability rule (Future-tasks.md section 4 item 2): the tool computes the
cargo dependency closure of the product binary `modbit-core` (package
`modbit-core-runtime`) and fails any COMPLETE IMP-EV-* node whose owning
subsystem has no crate inside that closure. Shipped non-cargo surfaces
(`apps/desktop`, `services/*`, `tools/*`) count as product-reachable because
they ship and run with the binary and are exercised by CI. The cloud binary
(`cloud-worker`) becomes a second closure seed once it has real content.

Placement rule (Future-tasks.md section 4 item 3): every `REQ-EV-nnnn` tag in
a Rust source file must belong to the subsystem that owns the crate the file
lives in (docs/81 single-owner map). Known violations are allowlisted in
`placement_allowlist.json` (crates/checkpoint M9 modules pending relocation to
their owner crates) and reported but do not fail CI; any NEW violation fails.

`--self-test` proves the checker catches violations: it runs the same rule
engine over a synthetic graph containing a forbidden edge (must FAIL) and a
clean synthetic graph (must PASS), plus synthetic reachability/placement
cases, without touching real code.

`--placement-report` prints every placement violation (allowlisted or not)
without failing, for planning the M9 code moves.
"""

import json
import re
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
GRAPH_PATH = REPO_ROOT / "graph" / "project-graph.json"
ALLOWLIST_PATH = Path(__file__).resolve().parent / "placement_allowlist.json"

# Package that contains the `modbit-core` binary. The closure of this package
# is the product's Rust dependency closure (Future-tasks.md section 4 item 2).
PRODUCT_PACKAGES = ["modbit-core-runtime"]

# Directory prefixes that ship with the product even though they are not cargo
# dependencies of modbit-core: the Electron desktop shell, spawned service
# binaries, and the agent governance tooling CI executes on every run.
SHIPPED_NON_CARGO_PREFIXES = ("apps/desktop", "services/", "tools/")

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
    """Return ({member_name: set(direct workspace-member deps)}, {crate_dir: package name}).

    With `--no-deps` every listed package IS a workspace member, so member
    identity is the package name; dependency entries reference that name.
    The second map resolves doc-side crate paths (`crates/tools`) and shipped
    binaries (`services/modbit-execd`) to real package names via manifest paths.
    """
    meta = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--no-deps"],
            cwd=root,
            capture_output=True,
            text=True,
            check=True,
        ).stdout
    )
    members = {pkg["name"] for pkg in meta["packages"]}
    graph = {}
    dir_to_pkg = {}
    for pkg in meta["packages"]:
        graph[pkg["name"]] = {
            dep["name"]
            for dep in pkg.get("dependencies", [])
            if dep.get("kind") in (None, "build") and dep["name"] in members
        }
        manifest = Path(pkg["manifest_path"]).resolve()
        for base in ("crates", "services", "apps"):
            try:
                rel = manifest.relative_to(Path(root) / base)
            except ValueError:
                continue
            dir_to_pkg["%s/%s" % (base, rel.parts[0])] = pkg["name"]
            break
    return graph, dir_to_pkg


def dependency_closure(graph, seed):
    """Transitive workspace-member closure of `seed` (inclusive)."""
    seen = {seed}
    stack = [seed]
    while stack:
        for dep in graph.get(stack.pop(), ()):
            if dep not in seen:
                seen.add(dep)
                stack.append(dep)
    return seen


def crate_entry_reachable(entry, closure_set, dir_to_pkg):
    """True if a docs-side crate entry (`crates/tools (media)`) is product-reachable."""
    token = entry.split()[0]
    pkg = dir_to_pkg.get(token)
    if pkg and pkg in closure_set:
        return True
    return token.startswith(SHIPPED_NON_CARGO_PREFIXES)


def check_reachability(work_nodes, subsystems, closure_set, dir_to_pkg):
    """COMPLETE imp_task nodes must live in the product binary's closure.

    work_nodes: graph work-item nodes (need id, type, status, subsystem).
    subsystems: {subsystem_id: [crate entry, ...]} from tools/build_graph.py.
    """
    violations = []
    for n in sorted(work_nodes, key=lambda x: x.get("id", "")):
        if n.get("type") != "imp_task" or n.get("status") != "COMPLETE":
            continue
        crates = subsystems.get(n.get("subsystem"), [])
        if any(crate_entry_reachable(c, closure_set, dir_to_pkg) for c in crates):
            continue
        violations.append(
            "COMPLETE task not reachable from product binary: %s (subsystem %r owns %s; "
            "none of them is in the %s dependency closure — Future-tasks.md section 4 item 2)"
            % (n["id"], n.get("subsystem"), ", ".join(crates) or "no crates", "/".join(PRODUCT_PACKAGES))
        )
    return violations


# ---- placement rule (Future-tasks.md section 4 item 3) ----------------------

REQ_TAG_RE = re.compile(r"\bREQ-EV-\d{4}\b")


def crate_dir_of(source_path):
    """`crates/checkpoint/src/x.rs` -> `crates/checkpoint`; None outside crates/services/apps."""
    parts = Path(source_path).parts
    for base in ("crates", "services", "apps"):
        if base in parts:
            i = parts.index(base)
            if i + 1 < len(parts):
                return "%s/%s" % (base, parts[i + 1])
    return None


def scan_req_tags(root):
    """Return {source_path: set(REQ-EV ids)} for every .rs file under crates/services/apps."""
    tags = {}
    for base in ("crates", "services", "apps"):
        base_dir = Path(root) / base
        if not base_dir.is_dir():
            continue
        for path in sorted(base_dir.rglob("*.rs")):
            found = set(REQ_TAG_RE.findall(path.read_text(encoding="utf-8", errors="replace")))
            if found:
                tags[str(path.relative_to(root))] = found
    return tags


def check_placement(tags_by_file, dir_owners, req_owner):
    """Return [(path, req_id, req_owner, crate_owners)] where a REQ tag is misplaced.

    tags_by_file: {path: {REQ-EV ids}}; dir_owners: {crate dir: {subsystem ids}};
    req_owner: {REQ-EV id: owning subsystem id} from the graph.
    """
    violations = []
    for path in sorted(tags_by_file):
        crate_dir = crate_dir_of(path)
        owners = dir_owners.get(crate_dir) if crate_dir else None
        if not owners:
            continue  # location not claimed by any subsystem in docs/81
        for tag in sorted(tags_by_file[path]):
            owner = req_owner.get(tag)
            if owner and owner not in owners:
                violations.append((path, tag, owner, sorted(owners)))
    return violations


def load_allowlist():
    if not ALLOWLIST_PATH.exists():
        return set()
    data = json.loads(ALLOWLIST_PATH.read_text(encoding="utf-8"))
    return {(e["path"], e["req"]) for e in data.get("known_violations", [])}


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


def self_test_reachability():
    """Synthetic closures prove COMPLETE tasks outside the product fail (§4.2)."""
    graph = {
        "modbit-core-runtime": {"modbit-domain", "modbit-providers"},
        "modbit-providers": {"modbit-domain"},
        "modbit-domain": set(),
        "modbit-sandbox": {"modbit-domain"},
    }
    closure_set = dependency_closure(graph, "modbit-core-runtime")
    dir_to_pkg = {
        "crates/providers": "modbit-providers",
        "crates/sandbox": "modbit-sandbox",
        "crates/core-runtime": "modbit-core-runtime",
    }
    subsystems = {
        "model-gateway": ["crates/providers"],
        "sandbox-cloud": ["crates/sandbox", "apps/cloud-api"],
        "desktop": ["apps/desktop", "packages/ui"],
        "eval-bench": ["benchmarks/retrieval"],
    }
    work = [
        {"id": "IMP-EV-0001", "type": "imp_task", "status": "COMPLETE", "subsystem": "model-gateway"},
        {"id": "IMP-EV-0002", "type": "imp_task", "status": "COMPLETE", "subsystem": "desktop"},
        {"id": "IMP-EV-0003", "type": "imp_task", "status": "WIRED", "subsystem": "sandbox-cloud"},
        {"id": "IMP-EV-0004", "type": "imp_task", "status": "COMPLETE", "subsystem": "sandbox-cloud"},
        {"id": "IMP-EV-0005", "type": "imp_task", "status": "COMPLETE", "subsystem": "eval-bench"},
    ]
    found = check_reachability(work, subsystems, closure_set, dir_to_pkg)
    ok = (len(found) == 2 and
          any("IMP-EV-0004" in v for v in found) and
          any("IMP-EV-0005" in v for v in found))
    print("  %-52s %s" % ("reachability: out-of-closure COMPLETE rejected",
                          "OK" if ok else "SELF-TEST FAIL: %r" % found))
    # a second closure seed widens reachability (future cloud-worker): the
    # sandbox task becomes reachable, the benchmark task stays unreachable.
    closure2 = closure_set | dependency_closure(graph, "modbit-sandbox")
    found2 = check_reachability(work, subsystems, closure2, dict(dir_to_pkg, **{"apps/cloud-api": "modbit-cloud-api"}))
    ok2 = len(found2) == 1 and "IMP-EV-0005" in found2[0]
    print("  %-52s %s" % ("reachability: added seed unblocks (cloud-worker path)",
                          "OK" if ok2 else "SELF-TEST FAIL: %r" % found2))
    return ok and ok2


def self_test_placement():
    """Synthetic REQ tags prove misplaced requirements are detected (§4.3)."""
    dir_owners = {"crates/checkpoint": {"durability"}, "crates/policy": {"tool-runtime", "effects-security"}}
    req_owner = {"REQ-EV-0270": "effects-security", "REQ-EV-0012": "durability"}
    tags = {
        "crates/checkpoint/src/security_hardening.rs": {"REQ-EV-0270"},
        "crates/checkpoint/src/lib.rs": {"REQ-EV-0012"},
        "crates/policy/src/approvals.rs": {"REQ-EV-0270"},
    }
    found = check_placement(tags, dir_owners, req_owner)
    ok = found == [("crates/checkpoint/src/security_hardening.rs", "REQ-EV-0270", "effects-security", ["durability"])]
    print("  %-52s %s" % ("placement: cross-subsystem REQ tag rejected",
                          "OK" if ok else "SELF-TEST FAIL: %r" % (found,)))
    ok2 = crate_dir_of("crates/checkpoint/src/a.rs") == "crates/checkpoint" and \
        crate_dir_of("services/modbit-execd/src/main.rs") == "services/modbit-execd" and \
        crate_dir_of("README.md") is None
    print("  %-52s %s" % ("placement: crate dir extraction", "OK" if ok2 else "SELF-TEST FAIL"))
    return ok and ok2


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
    r1 = self_test_reachability()
    r2 = self_test_placement()
    return r1 and r2


def main(argv):
    if "--self-test" in argv:
        return 0 if self_test() else 1
    try:
        graph, dir_to_pkg = workspace_graph(REPO_ROOT)
    except (subprocess.CalledProcessError, FileNotFoundError, json.JSONDecodeError) as exc:
        print("architecture-lint: could not read cargo metadata: %s" % exc, file=sys.stderr)
        return 2

    violations = list(check(graph))

    if not GRAPH_PATH.exists():
        print("architecture-lint: graph missing: %s (run tools/build_graph.py)" % GRAPH_PATH, file=sys.stderr)
        return 2
    g = json.loads(GRAPH_PATH.read_text(encoding="utf-8"))
    nodes = g["nodes"]
    subsystems = {n["id"]: n.get("crates", []) for n in nodes if n["type"] == "subsystem"}
    req_owner = {n["id"]: n.get("subsystem") for n in nodes if n["type"] == "requirement"}

    closure_set = set()
    for seed in PRODUCT_PACKAGES:
        if seed in graph:
            closure_set |= dependency_closure(graph, seed)
        else:
            print("architecture-lint: WARNING product package %r not in workspace" % seed, file=sys.stderr)
    violations.extend(
        check_reachability([n for n in nodes if n["type"] == "imp_task"], subsystems, closure_set, dir_to_pkg)
    )

    dir_owners = {}
    for n in nodes:
        if n["type"] != "subsystem":
            continue
        for entry in n.get("crates", []):
            dir_owners.setdefault(entry.split()[0], set()).add(n["id"])
    placement_all = check_placement(scan_req_tags(REPO_ROOT), dir_owners, req_owner)
    allowlist = load_allowlist()
    placement_new = [v for v in placement_all if (v[0], v[1]) not in allowlist]
    stale = sorted(allowlist - {(p, t) for p, t, _, _ in placement_all})

    if "--placement-report" in argv:
        print("placement report: %d misplaced REQ tag(s), %d allowlisted" %
              (len(placement_all), len(placement_all) - len(placement_new)))
        for path, tag, owner, owners in placement_all:
            print("  %s tags %s (owner %r; crate %s owned by %s)"
                  % (path, tag, owner, crate_dir_of(path), ", ".join(owners)))

    for entry in stale:
        violations.append("stale placement allowlist entry: %s (violation no longer occurs; remove it)" % (entry,))

    if violations or placement_new:
        n_total = len(violations) + len(placement_new)
        print("architecture-lint: %d violation(s):" % n_total)
        for v in violations:
            print("  - %s" % v)
        for path, tag, owner, owners in placement_new:
            print("  - misplaced REQ tag: %s tags %s (owner %r) but %s is owned by %s (docs/81)"
                  % (path, tag, owner, crate_dir_of(path), ", ".join(owners)))
        if placement_all and not placement_new:
            print("  (%d known placement violation(s) allowlisted in placement_allowlist.json; "
                  "run with --placement-report)" % len(placement_all))
        return 1
    print("architecture-lint: OK (%d members checked, 0 forbidden edges, %d COMPLETE tasks reachable, "
          "%d misplaced REQ tag(s) allowlisted)" % (len(graph), sum(1 for n in nodes if n.get("type") == "imp_task" and n.get("status") == "COMPLETE"), len(placement_all)))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
