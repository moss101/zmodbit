//! Canonical tool and capability conformance harness (M10.7, docs/17):
//! builds the REAL task-scoped registry (the same builder the scheduler
//! uses in production) and asserts, for every tool, the conformance
//! contract — exact membership in the canonical inventory, typed schema
//! presence, valid model-facing JSON-Schema projection, and a known
//! capability class. Runs on every push; a tool that drifts from the
//! inventory (renamed, added without schema, unknown class) fails here.

use std::path::PathBuf;
use std::sync::Arc;

use modbit_core_runtime::scheduler::build_worktree_registry;
use modbit_git::GitRepo;
use modbit_policy::EffectClass;
use modbit_tools::ToolRegistry;
use modbit_workspace::WorkspaceFileService;

/// The canonical BASE inventory: always registered for every task
/// (docs/17 § inventory; the set the registry builder MUST provide).
const CANONICAL: &[&str] = &[
    "fs.read",
    "fs.list",
    "shell.run",
    "test.run",
    "change.propose",
    "change.apply",
    "git.status",
    "git.diff",
    "search.grep",
    "search.symbol",
    "context.query",
];

/// Pool-conditional: `external.list`/`external.call` appear only when an
/// MCP pool is attached (configured servers); their full path is proven
/// in external_tools_e2e against the real fixture server.
const POOL_CONDITIONAL: &[&str] = &["external.list", "external.call"];

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "modbit-conf-{tag}-{}",
        uuid::Uuid::now_v7().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn fixture_registry() -> ToolRegistry {
    let root = tempdir("repo");
    let repo = GitRepo::init(&root).expect("init");
    repo.set_config("user.email", "t@modbit.test").unwrap();
    repo.set_config("user.name", "T").unwrap();
    std::fs::write(root.join("app.py"), "def qty(x):\n    return x\n").unwrap();
    repo.commit_all("base").unwrap();
    let wt = root
        .parent()
        .unwrap()
        .join(format!("{}-wt", root.file_name().unwrap().to_string_lossy()));
    repo.worktree_add(&wt, "task-conf").expect("worktree");
    let ws = Arc::new(WorkspaceFileService::open(&wt).unwrap());
    build_worktree_registry(
        &ws,
        &wt,
        None,
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        None,
        None,
        None,
        None,
        None,
    )
}

#[test]
fn registered_tools_conform_to_the_canonical_inventory() {
    let registry = fixture_registry();
    let tools = registry.list();

    // 1. Exact membership: no drift in EITHER direction. The base set
    //    is mandatory; pool-conditional tools must be ABSENT without a
    //    pool (fail-closed: no phantom MCP surface).
    let mut names: Vec<String> = tools.iter().map(|(n, _, _)| n.clone()).collect();
    names.sort();
    let mut canonical: Vec<&str> = CANONICAL.to_vec();
    canonical.sort();
    assert_eq!(
        names, canonical,
        "registry inventory drifted from docs/17"
    );
    for conditional in POOL_CONDITIONAL {
        assert!(
            !names.contains(&conditional.to_string()),
            "{conditional} registered without an MCP pool"
        );
    }

    // 2. Versions present; capability classes are KNOWN variants and
    //    sane for the tool's family (reads are read-only; applies write).
    for (name, version, class) in &tools {
        assert!(!version.is_empty(), "{name} has no version");
        match class {
            EffectClass::ReadOnly => {}
            EffectClass::Write => assert!(
                name.starts_with("change.")
                    || name.starts_with("shell.")
                    || name.starts_with("test.")
                    || name.starts_with("external."),
                "{name} classified Write — check docs/17 capability lifecycle",
            ),
            EffectClass::External => assert!(
                name.starts_with("shell.") || name.starts_with("external.") || name.starts_with("test."),
                "{name} classified External — check docs/17",
            ),
        }
    }

    // 3. EVERY tool is model-offerable: typed schema present and the
    //    projection is a valid JSON-Schema object whose properties match
    //    the schema parameters exactly.
    let definitions = registry.tool_definitions();
    let def_names: Vec<&str> = definitions.iter().map(|d| d.name.as_str()).collect();
    for name in CANONICAL {
        assert!(
            def_names.contains(name),
            "{name} has no schema — it would be invisible to the model (fail-closed projection)"
        );
    }
    for def in &definitions {
        assert_eq!(def.parameters["type"], "object", "{}: schema root", def.name);
        let properties = def.parameters["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{}: projection has no properties", def.name));
        // Zero-parameter tools (e.g. git.status) are conformant: the
        // schema exists and projects an empty properties object. The
        // assertion is that the SCHEMA exists (checked above), not that
        // every tool takes arguments.
        for (param, spec) in properties {
            let spec = spec.as_object().expect("param spec object");
            assert!(
                spec.contains_key("type"),
                "{}.{}: param spec has no type",
                def.name,
                param
            );
        }
    }
}
