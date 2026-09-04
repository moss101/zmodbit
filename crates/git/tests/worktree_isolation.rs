//! Real-git E2E for worktree isolation (M2, REQ-EV-0125): parallel work in
//! linked worktrees never crosses write boundaries, and the reviewed merge
//! lands the isolated work on the integration branch.

use std::path::{Path, PathBuf};
use std::process::Command;

use modbit_git::{GitRepo, MergeOutcome};

fn repo_root(tag: &str) -> PathBuf {
    let unique = uuid::Uuid::now_v7().simple().to_string();
    let mut root = std::env::temp_dir();
    root.push(format!("modbit-wt-{tag}-{unique}"));
    root
}

fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn git(root: &Path, args: &[&str]) -> String {
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

/// QUAL-EV-0125: parallel builders in separate worktrees — a write in one
/// worktree is INVISIBLE to the other worktrees and to the main checkout,
/// and the reviewed merge brings the isolated work home cleanly.
#[test]
fn parallel_worktrees_never_cross_write() {
    let main_root = repo_root("main");
    let repo = GitRepo::init(&main_root).unwrap();
    write(&main_root, "shared.txt", "base\n");
    write(&main_root, "README.md", "docs\n");
    repo.commit_all("base").unwrap();
    let base = repo.head().unwrap();

    // Two parallel builder worktrees on their own branches.
    let wt_a_root = repo_root("wt-a");
    let wt_b_root = repo_root("wt-b");
    let wt_a = repo.worktree_add(&wt_a_root, "builder/a").unwrap();
    let wt_b = repo.worktree_add(&wt_b_root, "builder/b").unwrap();

    // Parallel writes: A edits shared.txt and adds a-new.txt; B edits
    // README.md and adds b-new.txt.
    write(&wt_a_root, "shared.txt", "base + A's isolated edit\n");
    write(&wt_a_root, "a-new.txt", "only A has this\n");
    wt_a.commit_all("A's isolated work").unwrap();

    write(&wt_b_root, "README.md", "docs + B's isolated edit\n");
    write(&wt_b_root, "b-new.txt", "only B has this\n");
    wt_b.commit_all("B's isolated work").unwrap();

    // ISOLATION: neither worktree sees the other's files; the main
    // checkout sees neither.
    assert!(
        !wt_a_root.join("b-new.txt").exists(),
        "B's file must not leak into A"
    );
    assert!(
        !wt_b_root.join("a-new.txt").exists(),
        "A's file must not leak into B"
    );
    assert!(
        !main_root.join("a-new.txt").exists() && !main_root.join("b-new.txt").exists(),
        "worktree writes must not leak into the main checkout"
    );
    assert_eq!(
        std::fs::read_to_string(main_root.join("shared.txt")).unwrap(),
        "base\n",
        "A's edit must not leak into the main checkout"
    );

    // Reviewed merge: A's work lands via the typed merge; B's stays put.
    let outcome = repo.merge("builder/a").unwrap();
    assert_eq!(outcome, MergeOutcome::Merged);
    assert_eq!(
        std::fs::read_to_string(main_root.join("shared.txt")).unwrap(),
        "base + A's isolated edit\n"
    );
    assert!(main_root.join("a-new.txt").exists());
    assert!(
        !main_root.join("b-new.txt").exists(),
        "unmerged worktree work must not appear on main"
    );

    // The base commit never moved for the other worktree's history.
    let b_base = git(&wt_b_root, &["merge-base", "builder/b", "main"]);
    assert_eq!(b_base, base, "merge-base unchanged by the parallel work");

    // Cleanup removes the worktrees safely.
    repo.worktree_remove(&wt_a_root).unwrap();
    repo.worktree_remove(&wt_b_root).unwrap();
    let list = git(&main_root, &["worktree", "list", "--porcelain"]);
    assert!(!list.contains(&wt_a_root.display().to_string()));
    assert!(!list.contains(&wt_b_root.display().to_string()));
}
