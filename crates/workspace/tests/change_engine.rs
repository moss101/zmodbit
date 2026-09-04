//! Change Engine integration tests (M2, REQ-EV-0011/0014/0015/0016): blob
//! addressing with fail-closed digest verification, canonical edit
//! transactions with preconditions, deterministic match ladder, and
//! multi-edit rollback semantics.

use modbit_workspace::change_engine::{apply_transaction, EditOp, TransactionError};
use modbit_workspace::{PatchHunk, WorkspaceFileService};

fn ws_at(tag: &str) -> WorkspaceFileService {
    let unique = uuid::Uuid::now_v7().simple().to_string();
    let mut root = std::env::temp_dir();
    root.push(format!("modbit-ce-{tag}-{unique}"));
    WorkspaceFileService::open(&root).expect("open workspace")
}

/// REQ-EV-0015 ladder: exact match applies directly.
#[test]
fn ladder_exact_match_applies() {
    let ws = ws_at("exact");
    ws.create("f.txt", b"alpha\nbeta\ngamma\n").unwrap();
    let (bytes, rev) = ws.read("f.txt").unwrap();
    let _ = bytes;
    let new_rev = apply_transaction(
        &ws,
        &[EditOp {
            path: "f.txt".into(),
            expected_revision: rev,
            hunks: vec![PatchHunk {
                anchor_line: 2,
                old_lines: vec!["beta".into()],
                new_lines: vec!["beta-prime".into()],
            }],
        }],
        "exact edit",
    )
    .unwrap();
    let (content, _) = ws.read("f.txt").unwrap();
    assert!(String::from_utf8_lossy(&content).contains("beta-prime"));
    assert_eq!(new_rev[0].1, 2);
}

/// REQ-EV-0015: safe whitespace remap applies when unambiguous.
#[test]
fn ladder_whitespace_remap_applies_when_unique() {
    let ws = ws_at("remap");
    ws.create("f.txt", b"alpha\nbeta   with  spaces\ngamma\n")
        .unwrap();
    let (_, rev) = ws.read("f.txt").unwrap();
    let new_rev = apply_transaction(
        &ws,
        &[EditOp {
            path: "f.txt".into(),
            expected_revision: rev,
            hunks: vec![PatchHunk {
                anchor_line: 2,
                old_lines: vec!["beta with spaces".into()],
                new_lines: vec!["beta normalized".into()],
            }],
        }],
        "remap edit",
    )
    .unwrap();
    let (content, _) = ws.read("f.txt").unwrap();
    assert!(String::from_utf8_lossy(&content).contains("beta normalized"));
    assert_eq!(new_rev[0].1, 2);
}

/// REQ-EV-0015/0016: ambiguity fails and leaves the worktree unchanged.
#[test]
fn ambiguous_target_fails_without_data_loss() {
    let ws = ws_at("ambiguous");
    ws.create("dup.txt", b"same\nother\nsame\n").unwrap();
    let (_, rev) = ws.read("dup.txt").unwrap();
    let content_before = ws.read("dup.txt").unwrap().0.clone();

    let err = apply_transaction(
        &ws,
        &[EditOp {
            path: "dup.txt".into(),
            expected_revision: rev,
            hunks: vec![PatchHunk {
                anchor_line: 1,
                old_lines: vec!["same".into()],
                new_lines: vec!["replaced".into()],
            }],
        }],
        "ambiguous edit",
    );
    assert!(
        matches!(err, Err(TransactionError::Ambiguous { .. })),
        "duplicated target must be an ambiguity error"
    );
    assert_eq!(
        ws.read("dup.txt").unwrap().0,
        content_before,
        "worktree unchanged on failure"
    );
}

/// QUAL-EV-0014: concurrent user edit causes precondition failure without
/// data loss.
#[test]
fn stale_precondition_fails_without_data_loss() {
    let ws = ws_at("precondition");
    ws.create("doc.txt", b"v1\n").unwrap();
    let (_, rev1) = ws.read("doc.txt").unwrap();

    // Another actor writes using the same revision.
    ws.replace("doc.txt", b"v2 by other\n", rev1).unwrap();

    // First actor's transaction replays with the stale revision: rejected.
    let err = apply_transaction(
        &ws,
        &[EditOp {
            path: "doc.txt".into(),
            expected_revision: rev1,
            hunks: vec![PatchHunk {
                anchor_line: 1,
                old_lines: vec!["v1".into()],
                new_lines: vec!["v1 patched".into()],
            }],
        }],
        "stale transaction",
    );
    assert!(
        matches!(err, Err(TransactionError::Precondition { .. })),
        "stale transaction must fail on precondition"
    );
    // Data intact.
    let (bytes, _) = ws.read("doc.txt").unwrap();
    assert_eq!(bytes, b"v2 by other\n");
}

/// QUAL-EV-0016: multi-edit transaction rolls back atomically when a later
/// edit fails.
#[test]
fn multi_edit_rolls_back_on_late_failure() {
    let ws = ws_at("rollback");
    ws.create("a.txt", b"a\n").unwrap();
    ws.create("b.txt", b"b\n").unwrap();
    let (_, rev_a) = ws.read("a.txt").unwrap();
    let (_, rev_b) = ws.read("b.txt").unwrap();

    // Two edits: a.txt ok, b.txt with a WRONG expected revision.
    let err = apply_transaction(
        &ws,
        &[
            EditOp {
                path: "a.txt".into(),
                expected_revision: rev_a,
                hunks: vec![PatchHunk {
                    anchor_line: 1,
                    old_lines: vec!["a".into()],
                    new_lines: vec!["a2".into()],
                }],
            },
            EditOp {
                path: "b.txt".into(),
                expected_revision: rev_b + 5,
                hunks: vec![PatchHunk {
                    anchor_line: 1,
                    old_lines: vec!["b".into()],
                    new_lines: vec!["b2".into()],
                }],
            },
        ],
        "multi edit",
    );
    assert!(
        err.is_err(),
        "stale b.txt precondition must fail the transaction"
    );
    // Rollback: neither file changed.
    assert_eq!(ws.read("a.txt").unwrap().0, b"a\n");
    assert_eq!(ws.read("b.txt").unwrap().0, b"b\n");
}

/// Two-edit transaction where both apply cleanly.
#[test]
fn multi_edit_applies_both_when_valid() {
    let ws = ws_at("multi-ok");
    ws.create("x.txt", b"x\n").unwrap();
    ws.create("y.txt", b"y\n").unwrap();
    let (x_content, rev_x) = ws.read("x.txt").unwrap();
    let (y_content, rev_y) = ws.read("y.txt").unwrap();
    let _ = (x_content.clone(), y_content.clone());

    let results = apply_transaction(
        &ws,
        &[
            EditOp {
                path: "x.txt".into(),
                expected_revision: rev_x,
                hunks: vec![PatchHunk {
                    anchor_line: 1,
                    old_lines: vec!["x".into()],
                    new_lines: vec!["x2".into()],
                }],
            },
            EditOp {
                path: "y.txt".into(),
                expected_revision: rev_y,
                hunks: vec![PatchHunk {
                    anchor_line: 1,
                    old_lines: vec!["y".into()],
                    new_lines: vec!["y2".into()],
                }],
            },
        ],
        "both valid",
    )
    .unwrap();
    assert_eq!(results.len(), 2);
    assert!(String::from_utf8_lossy(&ws.read("x.txt").unwrap().0).contains("x2"));
    assert!(String::from_utf8_lossy(&ws.read("y.txt").unwrap().0).contains("y2"));
}
