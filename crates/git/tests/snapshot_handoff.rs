//! Real-git integration tests for provenance-bound dirty-state snapshots
//! (M2, REQ-EV-0022): local capture, exact remote reconstruction, and safe
//! temporary-ref cleanup — over actual git repositories.

use std::path::{Path, PathBuf};
use std::process::Command;

use modbit_git::{GitRepo, SnapshotProvenance, SNAPSHOT_NAMESPACE};

fn repo_root(tag: &str) -> PathBuf {
    let unique = uuid::Uuid::now_v7().simple().to_string();
    let mut root = std::env::temp_dir();
    root.push(format!("modbit-snap-{tag}-{unique}"));
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

/// Snapshot captures dirty state (staged + unstaged + untracked) without
/// moving the user's branch, touching their index, or altering the worktree.
#[test]
fn snapshot_is_nondestructive_and_provenance_bound() {
    let root = repo_root("nondest");
    let repo = GitRepo::init(&root).unwrap();
    write(&root, "tracked.txt", "original\n");
    repo.commit_all("base").unwrap();
    let base = repo.head().unwrap();

    // The user has staged work AND further unstaged edits AND an untracked file.
    write(&root, "tracked.txt", "staged version\n");
    git(&root, &["add", "tracked.txt"]);
    write(&root, "tracked.txt", "staged + unstaged edits\n");
    write(&root, "untracked.txt", "brand new\n");

    let prov = SnapshotProvenance::new("task-42", "workstation-7", "local-workspace");
    let handle = repo.create_snapshot(&prov).unwrap();

    // Provenance is bound into the ref and the handle.
    assert!(handle.ref_name.starts_with(SNAPSHOT_NAMESPACE));
    assert!(handle.ref_name.contains("task-42"));
    assert_eq!(handle.provenance, prov);
    assert_eq!(handle.base_commit, base);

    // The user's branch did not move and their staged state survives.
    assert_eq!(repo.head().unwrap(), base);
    let staged = git(&root, &["diff", "--cached", "--name-only"]);
    assert_eq!(staged, "tracked.txt", "user's staged entry preserved");

    let msg = git(&root, &["log", "-1", "--format=%B", &handle.commit]);
    assert!(msg.contains("task-42") && msg.contains("workstation-7"));

    // Cleanup removes exactly the temporary ref.
    repo.cleanup_snapshot(&handle).unwrap();
    let refs = git(
        &root,
        &["for-each-ref", "--format=%(refname)", SNAPSHOT_NAMESPACE],
    );
    assert!(refs.is_empty());
}

/// QUAL-EV-0022: a remote ("cloud") run reconstructs the dirty state
/// exactly — the reconstructed tree equals the snapshot tree — and cleanup
/// removes the temporary refs safely.
#[test]
fn cloud_reconstruction_is_exact_and_cleanup_is_safe() {
    let local_root = repo_root("local");
    let local = GitRepo::init(&local_root).unwrap();
    // Exactness test: no line-ending translation in either repo (Windows
    // runners default to autocrlf=true).
    git(&local_root, &["config", "core.autocrlf", "false"]);
    write(&local_root, "keep.txt", "unchanged\n");
    write(&local_root, "gone.txt", "will be deleted by the task\n");
    write(&local_root, "edit.txt", "v1\n");
    local.commit_all("base").unwrap();

    // Cloud is a true clone, so both repos share the same base commit.
    let cloud_root = repo_root("cloud");
    let out = Command::new("git")
        .arg("-c")
        .arg("core.autocrlf=false")
        .arg("clone")
        .arg("--quiet")
        .arg(&local_root)
        .arg(&cloud_root)
        .env("GIT_AUTHOR_NAME", "modbit-core")
        .env("GIT_COMMITTER_NAME", "modbit-core")
        .output()
        .unwrap();
    assert!(out.status.success(), "clone failed");
    let cloud = GitRepo::open(&cloud_root).unwrap();
    git(&cloud_root, &["config", "core.autocrlf", "false"]);

    // Local task dirties the worktree: modify, add, delete.
    write(&local_root, "edit.txt", "v2 — task output\n");
    write(&local_root, "fresh.txt", "created by the task\n");
    std::fs::remove_file(local_root.join("gone.txt")).unwrap();

    let prov = SnapshotProvenance::new("task-9", "m1", "local");
    let handle = local.create_snapshot(&prov).unwrap();

    // Transfer + reconstruct on the cloud side.
    cloud.fetch_snapshot(&local_root, &handle).unwrap();
    cloud.restore_snapshot(&handle).unwrap();

    // Exactness: the reconstructed tree equals the snapshot tree, and the
    // worktree content matches byte-for-byte.
    assert!(
        cloud.verify_snapshot(&handle).unwrap(),
        "reconstructed tree must equal the snapshot tree"
    );
    assert_eq!(
        std::fs::read_to_string(cloud_root.join("edit.txt")).unwrap(),
        "v2 — task output\n"
    );
    assert_eq!(
        std::fs::read_to_string(cloud_root.join("fresh.txt")).unwrap(),
        "created by the task\n"
    );
    assert!(
        !cloud_root.join("gone.txt").exists(),
        "snapshot deletion must be reproduced on the remote"
    );
    assert_eq!(
        std::fs::read_to_string(cloud_root.join("keep.txt")).unwrap(),
        "unchanged\n"
    );

    // Cleanup on both sides; leftover refs are swept by cleanup_all.
    cloud.cleanup_snapshot(&handle).unwrap();
    local.cleanup_snapshot(&handle).unwrap();
    assert_eq!(local.cleanup_all_snapshots().unwrap(), 0);

    // Cleanup NEVER touches refs outside the modbit namespace.
    assert!(
        git(&cloud_root, &["rev-parse", "--verify", "refs/heads/main"]) == cloud.head().unwrap()
    );
}

/// Cleanup refuses refs outside the modbit snapshot namespace — including
/// traversal attempts smuggled through provenance-provided ids.
#[test]
fn cleanup_and_fetch_refuse_foreign_refs() {
    let root = repo_root("foreign");
    let repo = GitRepo::init(&root).unwrap();
    write(&root, "f.txt", "x\n");
    repo.commit_all("c").unwrap();

    let mut handle = modbit_git::SnapshotHandle {
        ref_name: "refs/heads/main".into(),
        commit: repo.head().unwrap(),
        tree: "deadbeef".into(),
        base_commit: repo.head().unwrap(),
        provenance: SnapshotProvenance::new("t", "m", "o"),
    };
    assert!(repo.cleanup_snapshot(&handle).is_err());
    assert!(repo.restore_snapshot(&handle).is_err());

    handle.ref_name = format!("{SNAPSHOT_NAMESPACE}../secrets");
    assert!(
        repo.cleanup_snapshot(&handle).is_err(),
        "traversal rejected"
    );
    assert!(repo.fetch_snapshot(&root, &handle).is_err());

    // main survived all attempts.
    assert!(git(&root, &["rev-parse", "--verify", "main"]).len() == 40);
}

/// A clean worktree fails clean: no ref is created.
#[test]
fn snapshot_of_clean_worktree_fails_clean() {
    let root = repo_root("clean");
    let repo = GitRepo::init(&root).unwrap();
    write(&root, "a.txt", "content\n");
    repo.commit_all("base").unwrap();

    let prov = SnapshotProvenance::new("t", "m", "o");
    assert!(repo.create_snapshot(&prov).is_err());
    assert_eq!(repo.cleanup_all_snapshots().unwrap(), 0);
}
