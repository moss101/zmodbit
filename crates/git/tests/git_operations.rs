//! Real-git integration tests for M2.2: branch, worktree, diff, and typed
//! merge with conflict evidence — over actual git repositories.

use std::path::PathBuf;
use std::process::Command;

use modbit_git::{GitRepo, MergeOutcome};

fn repo_root(tag: &str) -> PathBuf {
    let unique = uuid::Uuid::now_v7().simple().to_string();
    let mut root = std::env::temp_dir();
    root.push(format!("modbit-m22-{tag}-{unique}"));
    root
}

fn write(root: &std::path::Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn git(root: &PathBuf, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_AUTHOR_NAME", "modbit-core")
        .env("GIT_AUTHOR_EMAIL", "core@modbit.local")
        .env("GIT_COMMITTER_NAME", "modbit-core")
        .env("GIT_COMMITTER_EMAIL", "core@modbit.local")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn branch_checkout_and_diff_numstat() {
    let root = repo_root("branch");
    let repo = GitRepo::init(&root).unwrap();
    write(&root, "src/main.rs", "fn main() {}\n");
    repo.commit_all("initial").unwrap();
    assert_eq!(repo.current_branch().unwrap(), "main");

    repo.create_branch("feature/rename", Some("main")).unwrap();
    repo.checkout("feature/rename").unwrap();
    assert_eq!(repo.current_branch().unwrap(), "feature/rename");

    write(
        &root,
        "src/main.rs",
        "fn main_renamed() {}\nfn extra() {}\n",
    );
    repo.commit_all("rename entrypoint").unwrap();

    let diffs = repo.diff_numstat("main", "feature/rename").unwrap();
    assert_eq!(diffs.len(), 1);
    assert_eq!(diffs[0].path, "src/main.rs");
    assert_eq!(diffs[0].additions, 2);
    assert_eq!(diffs[0].deletions, 1);
}

#[test]
fn worktrees_are_isolated_between_builders() {
    let root = repo_root("worktree");
    let repo = GitRepo::init(&root).unwrap();
    write(&root, "shared.txt", "base\n");
    repo.commit_all("initial").unwrap();

    let mut builder_b = root.clone();
    builder_b.push("builder-b");
    let _worktree = repo.worktree_add(&builder_b, "builder-b-branch").unwrap();

    // Builder B's worktree starts at the same commit but is a separate dir.
    assert!(builder_b.join("shared.txt").exists());

    std::fs::write(builder_b.join("shared.txt"), "builder b was here\n").unwrap();
    let worktree_repo = GitRepo::open(&builder_b).unwrap();
    worktree_repo.commit_all("builder b edit").unwrap();

    // The main worktree is untouched by B's edit (isolation).
    let main_shared = std::fs::read_to_string(root.join("shared.txt")).unwrap();
    assert_eq!(main_shared, "base\n");

    // B's branch has the edit.
    let diffs = worktree_repo
        .diff_numstat("main", "builder-b-branch")
        .unwrap();
    assert_eq!(diffs.len(), 1);
    assert_eq!(diffs[0].path, "shared.txt");

    repo.worktree_remove(&builder_b).unwrap();
    assert!(!builder_b.exists());
}

#[test]
fn clean_merge_reports_merged() {
    let root = repo_root("merge-clean");
    let repo = GitRepo::init(&root).unwrap();
    write(&root, "base.txt", "base\n");
    repo.commit_all("initial").unwrap();

    repo.create_branch("feature/add", Some("main")).unwrap();
    repo.checkout("feature/add").unwrap();
    write(&root, "feature.txt", "feature\n");
    repo.commit_all("add feature").unwrap();

    repo.checkout("main").unwrap();
    let outcome = repo.merge("feature/add").unwrap();
    assert_eq!(outcome, MergeOutcome::Merged);
    assert!(root.join("feature.txt").exists());
}

#[test]
fn conflicting_merge_provides_typed_conflict_evidence() {
    let root = repo_root("merge-conflict");
    let repo = GitRepo::init(&root).unwrap();
    write(&root, "spec.txt", "line: original\n");
    repo.commit_all("initial").unwrap();

    repo.create_branch("side-a", Some("main")).unwrap();
    repo.checkout("side-a").unwrap();
    write(&root, "spec.txt", "line: from side a\n");
    repo.commit_all("side a edit").unwrap();

    repo.checkout("main").unwrap();
    write(&root, "spec.txt", "line: from main\n");
    repo.commit_all("main edit").unwrap();

    let outcome = repo.merge("side-a").unwrap();
    match outcome {
        MergeOutcome::Conflict { conflicted_files } => {
            assert_eq!(conflicted_files, vec!["spec.txt".to_string()]);
        }
        other => panic!("expected conflict evidence, got {other:?}"),
    }

    // The worktree was left clean by the abort — the operator decides next.
    let status = git(&root, &["status", "--porcelain"]);
    assert!(
        status.is_empty(),
        "worktree must be clean after abort: {status:?}"
    );
}
