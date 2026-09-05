//! Worktree toolset integration (Phase 1 item 4): every new tool against a
//! REAL git worktree and, for shell.run, a REAL modbit-execd broker process
//! — proving effectors, not handlers-in-name.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use modbit_core_runtime::scheduler::build_worktree_registry;
use modbit_git::GitRepo;
use modbit_policy::{CapabilityGrant, EffectClass, PolicyDecision, PolicyKernel};
use modbit_terminal::client::ExecdClient;
use modbit_tools::ToolRegistry;
use modbit_workspace::WorkspaceFileService;

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("modbit-tools-{tag}-{}", uuid::Uuid::now_v7().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Real repo + committed files + a worktree checked out beside it.
fn worktree_fixture(tag: &str) -> PathBuf {
    let root = tempdir(tag);
    let repo = GitRepo::init(&root).expect("init");
    repo.set_config("user.email", "t@modbit.test").unwrap();
    repo.set_config("user.name", "T").unwrap();
    std::fs::write(root.join("app.py"), "def qty(x):\n    return x  # positive only\n").unwrap();
    std::fs::write(root.join("lib.txt"), "needle here\nnothing\n").unwrap();
    repo.commit_all("base").unwrap();
    let wt = root.parent().unwrap().join(format!("{}-wt", root.file_name().unwrap().to_string_lossy()));
    repo.worktree_add(&wt, &format!("task-{tag}")).expect("worktree");
    wt
}

fn registry_for(worktree: &std::path::Path, execd: Option<&ExecdClient>) -> ToolRegistry {
    let ws = Arc::new(WorkspaceFileService::open(worktree).unwrap());
    build_worktree_registry(&ws, worktree, execd, std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)), None)
}

fn grants() -> Vec<CapabilityGrant> {
    [
        ("fs.read", EffectClass::ReadOnly),
        ("fs.list", EffectClass::ReadOnly),
        ("search.grep", EffectClass::ReadOnly),
        ("git.status", EffectClass::ReadOnly),
        ("git.diff", EffectClass::ReadOnly),
        ("change.propose", EffectClass::ReadOnly),
        ("change.apply", EffectClass::Write),
        ("shell.run", EffectClass::External),
        ("test.run", EffectClass::External),
    ]
    .into_iter()
    .enumerate()
    .map(|(i, (tool, effect_class))| CapabilityGrant {
        grant_id: format!("g{i}"),
        tool: tool.into(),
        effect_class,
    })
    .collect()
}

/// Executes through the fail-closed registry exactly like the runtime does.
fn exec(registry: &ToolRegistry, tool: &str, args: serde_json::Value) -> serde_json::Value {
    let effect_class = match tool {
        "change.apply" => EffectClass::Write,
        "fs.read" | "fs.list" | "search.grep" | "git.status" | "git.diff" | "change.propose" => {
            EffectClass::ReadOnly
        }
        _ => EffectClass::External,
    };
    let decision = PolicyKernel::new(vec![]).check(
        &modbit_policy::ToolCallRequest { tool: tool.into(), effect_class, arguments: args.clone() },
        &grants(),
    );
    assert!(decision.is_allow(), "{tool} must be granted: {decision}");
    registry.execute(tool, &args, &decision).expect("tool executes").result
}

#[test]
fn change_gate_propose_preview_and_apply_with_revision_guard() {
    let wt = worktree_fixture("change");
    let registry = registry_for(&wt, None);

    // propose does NOT write: ambiguous old_text is refused.
    let out = exec(&registry, "change.propose", serde_json::json!({
        "path": "app.py", "old_text": "x", "new_text": "y"
    }));
    assert_eq!(out["ok"], false, "ambiguous match must be refused: {out}");
    assert_eq!(out["occurrences"], 2);

    // unique match previews; the file on disk is untouched.
    let out = exec(&registry, "change.propose", serde_json::json!({
        "path": "app.py", "old_text": "return x  # positive only",
        "new_text": "if x < 0:\n        raise ValueError('negative')\n    return x"
    }));
    assert_eq!(out["ok"], true, "{out}");
    assert!(
        std::fs::read_to_string(wt.join("app.py")).unwrap().contains("# positive only"),
        "propose must not write"
    );

    // apply with a stale revision is refused (optimistic concurrency).
    let out = exec(&registry, "change.apply", serde_json::json!({
        "path": "app.py",
        "old_text": "return x  # positive only",
        "new_text": "if x < 0:\n        raise ValueError('negative')\n    return x",
        "expected_revision": 99
    }));
    assert_eq!(out["ok"], false, "stale revision refused: {out}");

    // apply with the real revision writes through the change engine.
    let (_, rev) = WorkspaceFileService::open(&wt).unwrap().read("app.py").unwrap();
    let out = exec(&registry, "change.apply", serde_json::json!({
        "path": "app.py",
        "old_text": "return x  # positive only",
        "new_text": "if x < 0:\n        raise ValueError('negative')\n    return x",
        "expected_revision": rev
    }));
    assert_eq!(out["ok"], true, "{out}");
    let after = std::fs::read_to_string(wt.join("app.py")).unwrap();
    assert!(after.contains("raise ValueError('negative')"), "file changed on disk");
}

#[test]
fn grep_and_git_tools_read_the_real_worktree() {
    let wt = worktree_fixture("grep");
    let registry = registry_for(&wt, None);

    let out = exec(&registry, "search.grep", serde_json::json!({ "pattern": "needle" }));
    let matches = out["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1);
    assert!(matches[0].as_str().unwrap().ends_with("lib.txt:1"));

    // Untracked change → status shows it; diff counts committed-file edits.
    std::fs::write(wt.join("new.txt"), "fresh\n").unwrap();
    let out = exec(&registry, "git.status", serde_json::json!({}));
    let entries = out["entries"].as_array().unwrap();
    assert!(entries.iter().any(|e| e["path"] == "new.txt"), "{entries:?}");

    std::fs::write(wt.join("app.py"), "def qty(x):\n    return 0\n").unwrap();
    let out = exec(&registry, "git.diff", serde_json::json!({}));
    assert!(
        out["files"].as_array().unwrap().iter().any(|f| f["path"] == "app.py"),
        "{out}"
    );
}

/// Locates the freshly-built modbit-execd binary (same target dir cargo
/// built this test into).
fn execd_binary() -> PathBuf {
    let exe = if cfg!(windows) { "modbit-execd.exe" } else { "modbit-execd" };
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug")
        .join(exe)
}

#[test]
fn shell_run_routes_through_real_execd_and_test_run_runs_a_gate() {
    let wt = worktree_fixture("shell");

    let mut execd_child = Command::new(execd_binary())
        .env("MODBIT_EXECD_ADDR", "127.0.0.1:0")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn execd");
    let boot = {
        use std::io::BufRead;
        let mut line = String::new();
        std::io::BufReader::new(execd_child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        line
    };
    let addr = serde_json::from_str::<serde_json::Value>(&boot)
        .unwrap()
        .get("addr")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();
    std::thread::sleep(Duration::from_millis(150));
    let execd = ExecdClient::connect(&addr).expect("connect execd");

    let registry = registry_for(&wt, Some(&execd));

    // shell.run through the broker, cwd-pinned to the worktree.
    let out = exec(&registry, "shell.run", serde_json::json!({ "argv": "git status --porcelain" }));
    assert_eq!(out["exit_code"], 0, "{out}");
    assert!(!out["broker_run_id"].as_str().unwrap().is_empty(), "durable broker run id");

    // Without a broker, shell.run FAILS CLOSED (no direct-spawn fallback).
    let bare = registry_for(&wt, None);
    let err = bare
        .execute("shell.run", &serde_json::json!({"argv": "true"}), &PolicyDecision::Allow)
        .unwrap_err();
    assert!(err.to_string().contains("execd"), "fail closed: {err}");

    // test.run executes a REAL cargo test suite in the worktree through
    // the verification engine's runner adapter (minimal project: no deps,
    // one passing test, one file — fast compile).
    std::fs::create_dir_all(wt.join("src")).unwrap();
    std::fs::write(
        wt.join("Cargo.toml"),
        "[package]\nname = \"fixture-task\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
    )
    .unwrap();
    std::fs::write(
        wt.join("src/lib.rs"),
        "#[test]\nfn passes() { assert_eq!(2 + 2, 4); }\n",
    )
    .unwrap();
    let out = exec(&registry, "test.run", serde_json::json!({ "runner": "cargo" }));
    assert_eq!(out["passed"], true, "gate passed: {out}");

    execd_child.kill().ok();
    let _ = execd_child.wait();
}
