//! Real-filesystem tests for the Workspace File Service (M2.1, docs/20):
//! safe paths/revisions over an actual temp directory.

use std::path::PathBuf;

use modbit_workspace::{PatchHunk, WorkspaceError, WorkspaceFileService};

fn workspace(tag: &str) -> (PathBuf, WorkspaceFileService) {
    // Full uuid: v7's leading chars are timestamp-derived and collide when
    // tests start in the same millisecond.
    let unique = uuid::Uuid::now_v7().simple().to_string();
    let mut root = std::env::temp_dir();
    root.push(format!("modbit-m21-{tag}-{unique}"));
    let ws = WorkspaceFileService::open(&root).expect("open workspace");
    (root, ws)
}

#[test]
fn create_read_round_trip_and_revision_bump() {
    let (_root, ws) = workspace("roundtrip");
    let rev1 = ws.create("src/main.rs", b"fn main() {}").unwrap();
    assert_eq!(rev1, 1);
    let (bytes, rev2) = ws.read("src/main.rs").unwrap();
    assert_eq!(bytes, b"fn main() {}");
    assert_eq!(rev2, 1);

    let rev3 = ws
        .replace("src/main.rs", b"fn main() { println!(\"hi\"); }", rev1)
        .unwrap();
    assert_eq!(rev3, 2, "mutation bumps the file revision");
    let (bytes, _) = ws.read("src/main.rs").unwrap();
    assert_eq!(bytes, b"fn main() { println!(\"hi\"); }");
}

#[test]
fn create_fails_when_file_exists() {
    let (_root, ws) = workspace("exists");
    ws.create("a.txt", b"one").unwrap();
    let err = ws.create("a.txt", b"two").unwrap_err();
    assert!(matches!(err, WorkspaceError::AlreadyExists(_)));
}

#[test]
fn path_traversal_is_rejected() {
    let (_root, ws) = workspace("traversal");
    for evil in ["../escape.txt", "a/../../escape.txt", "..", "/etc/passwd"] {
        let err = ws.create(evil, b"evil").unwrap_err();
        assert!(
            matches!(err, WorkspaceError::OutsideRoot { .. }),
            "{evil} must be rejected, got {err}"
        );
    }
}

#[cfg(unix)]
#[test]
fn symlink_escape_is_rejected() {
    let (root, ws) = workspace("symlink");
    // An innocent-looking directory that is actually a symlink outside.
    let outside = tempfile_dir("modbit-m21-outside");
    std::fs::create_dir_all(&outside).unwrap();
    #[allow(deprecated)]
    std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

    let err = ws.create("link/escape.txt", b"evil").unwrap_err();
    assert!(
        matches!(err, WorkspaceError::OutsideRoot { .. }),
        "symlink traversal must be rejected, got {err}"
    );
    std::fs::remove_dir_all(outside).ok();
}

fn tempfile_dir(tag: &str) -> PathBuf {
    let unique = uuid::Uuid::now_v7().simple().to_string()[..8].to_string();
    let mut path = std::env::temp_dir();
    path.push(format!("modbit-m21-{tag}-{unique}"));
    path
}

#[test]
fn optimistic_revision_precondition_blocks_blind_overwrite() {
    let (_root, ws) = workspace("precondition");
    let rev1 = ws.create("cfg.toml", b"key = 1").unwrap();

    // Another actor writes using the same (now stale) revision.
    let stale = ws.replace("cfg.toml", b"key = 2", rev1).unwrap();
    assert_eq!(stale, 2);

    // The first actor replays its write with the old revision: rejected.
    let err = ws.replace("cfg.toml", b"key = 3", rev1).unwrap_err();
    match err {
        WorkspaceError::StaleRevision {
            expected, actual, ..
        } => {
            assert_eq!(expected, rev1);
            assert_eq!(actual, 2);
        }
        other => panic!("expected StaleRevision, got {other:?}"),
    }

    // The correct current revision applies.
    ws.replace("cfg.toml", b"key = 4", 2).unwrap();
    let (bytes, _) = ws.read("cfg.toml").unwrap();
    assert_eq!(bytes, b"key = 4");
}

#[test]
fn stat_lists_fingerprint_and_delete_moves_work() {
    let (_root, ws) = workspace("stat");
    ws.create("src/lib.txt", b"hello").unwrap();
    let (rev, sha, len) = ws.stat("src/lib.txt").unwrap().unwrap();
    assert_eq!((rev, len), (1, 5));
    assert_eq!(
        sha,
        modbit_workspace::WorkspaceFileService::sha256_hex(b"hello")
    );

    ws.move_file("src/lib.txt", "docs/lib.txt").unwrap();
    let (bytes, _) = ws.read("docs/lib.txt").unwrap();
    assert_eq!(bytes, b"hello");

    ws.delete("docs/lib.txt").unwrap();
    let entries = ws.list("docs").unwrap();
    assert!(!entries.contains(&"lib.txt".to_string()));
}

#[test]
fn apply_patch_matches_context_under_revision_precondition() {
    let (_root, ws) = workspace("patch");
    ws.mkdir("src").unwrap();
    ws.create("src/app.rs", b"fn one() {}\nfn two() {}\nfn three() {}\n")
        .unwrap();
    let (content, rev) = ws.read("src/app.rs").unwrap();
    let _ = content;

    let new_rev = ws
        .apply_patch(
            "src/app.rs",
            rev,
            &[PatchHunk {
                anchor_line: 2,
                old_lines: vec!["fn two() {}".into()],
                new_lines: vec!["fn two_renamed() {}".into()],
            }],
        )
        .unwrap();
    let (content, _) = ws.read("src/app.rs").unwrap();
    let text = String::from_utf8(content).unwrap();
    assert!(text.contains("fn two_renamed() {}"));
    assert!(!text.contains("fn two() {}"));
    assert_eq!(new_rev, 2);

    // A hunk whose context no longer matches is rejected.
    let err = ws
        .apply_patch(
            "src/app.rs",
            new_rev,
            &[PatchHunk {
                anchor_line: 1,
                old_lines: vec!["fn two() {}".into()],
                new_lines: vec!["x".into()],
            }],
        )
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::PatchMismatch { .. }));
}

#[test]
fn workspace_revision_is_monotonic_across_operations() {
    let (_root, ws) = workspace("monotonic");
    let r0 = ws.workspace_revision();
    ws.create("a.txt", b"a").unwrap();
    let r1 = ws.workspace_revision();
    ws.replace("a.txt", b"b", r1).unwrap();
    let r2 = ws.workspace_revision();
    ws.delete("a.txt").unwrap();
    let r3 = ws.workspace_revision();
    assert!(r0 < r1 && r1 < r2 && r2 < r3);
}

#[test]
fn revision_ledger_survives_service_restart() {
    let (root, ws) = workspace("restart");
    ws.create("keep.txt", b"durable").unwrap();
    let rev_before = ws.file_revision("keep.txt").unwrap();

    // Reopen: the revision ledger was persisted.
    let ws2 = WorkspaceFileService::open(&root).unwrap();
    assert_eq!(ws2.file_revision("keep.txt"), Some(rev_before));
    let (bytes, _) = ws2.read("keep.txt").unwrap();
    assert_eq!(bytes, b"durable");
}
