//! Per-hunk selective review and optimistic-concurrency revert tests
//! (M2, IMP-EV-0036 + IMP-EV-0065).

use modbit_workspace::{PatchHunk, ReviewedHunk, WorkspaceFileService};

fn ws_at(tag: &str) -> WorkspaceFileService {
    let unique = uuid::Uuid::now_v7().simple().to_string();
    let mut root = std::env::temp_dir();
    root.push(format!("modbit-imp-{tag}-{unique}"));
    WorkspaceFileService::open(&root).unwrap()
}

#[test]
fn selective_hunk_review_applies_only_accepted() {
    let ws = ws_at("selective");
    ws.create("src/main.rs", b"fn one() {}\nfn two() {}\nfn three() {}\n")
        .unwrap();
    let (_, rev) = ws.read("src/main.rs").unwrap();

    let reviewed = vec![
        ReviewedHunk {
            accepted: true,
            hunk: PatchHunk {
                anchor_line: 1,
                old_lines: vec!["fn one() {}".into()],
                new_lines: vec!["fn one_renamed() {}".into()],
            },
        },
        ReviewedHunk {
            accepted: false,
            hunk: PatchHunk {
                anchor_line: 2,
                old_lines: vec!["fn two() {}".into()],
                new_lines: vec!["fn two_dropped() {}".into()],
            },
        },
    ];
    let results =
        modbit_workspace::review::apply_review(&ws, "src/main.rs", rev, &reviewed).unwrap();
    assert_eq!(results.len(), 1, "only accepted hunks produced changes");

    let (content, _) = ws.read("src/main.rs").unwrap();
    let text = String::from_utf8_lossy(&content);
    assert!(text.contains("fn one_renamed()"), "accepted hunk applied");
    assert!(text.contains("fn two()"), "rejected hunk NOT applied");
}

#[test]
fn stale_precondition_fails_without_data_loss() {
    let ws = ws_at("stale-precondition");
    ws.create("doc.txt", b"v1\n").unwrap();
    let (_, rev1) = ws.read("doc.txt").unwrap();

    // Another actor writes using the same revision.
    ws.replace("doc.txt", b"v2 by other\n", rev1).unwrap();

    // First actor replays with the stale revision: rejected.
    let err = ws.replace("doc.txt", b"v3 by first actor\n", rev1);
    assert!(err.is_err(), "stale revision must fail");

    // Data intact from the second actor.
    let (bytes, _) = ws.read("doc.txt").unwrap();
    assert_eq!(bytes, b"v2 by other\n");
}

#[test]
fn concurrent_user_edit_blocks_destructive_revert() {
    let ws = ws_at("revert");
    ws.create("src/lib.txt", b"agent output v1\n").unwrap();
    let (_, rev) = ws.read("src/lib.txt").unwrap();

    // Agent "reverts" to original — this succeeds because the hash matches.
    ws.replace("src/lib.txt", b"agent output v1\n", rev)
        .unwrap();

    // Now a concurrent user edit lands.
    let (_, user_rev) = ws.read("src/lib.txt").unwrap();
    ws.replace("src/lib.txt", b"user manual edit\n", user_rev)
        .unwrap();

    // Agent tries to revert to its own output but the hash has changed —
    // the revert is rejected because the concurrent edit would be lost.
    let (current, _) = ws.read("src/lib.txt").unwrap();
    let current_hash = modbit_workspace::WorkspaceFileService::sha256_hex(&current);
    let agent_hash = modbit_workspace::WorkspaceFileService::sha256_hex(b"agent output v1\n");
    assert_ne!(
        current_hash, agent_hash,
        "concurrent edit must change the hash"
    );

    // The agent's revert via replace with a stale revision fails.
    let err = ws.replace("src/lib.txt", b"agent output v1\n", rev);
    assert!(err.is_err(), "stale revision replace must fail");
}
