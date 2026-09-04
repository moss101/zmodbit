//! Kill-point recovery suite (M4, M4.6): the process is "killed" at EVERY
//! stage of a tool/approval/protocol round — after the tool call record,
//! after the approval request, mid-journal (torn write) — and each kill
//! point must recover the exact state from the durable journal.

use modbit_protocol_state::{ProtocolRecord, ProtocolStateStore, ToolCallStatus};
use std::path::PathBuf;
use uuid::Uuid;

fn journal_path(tag: &str) -> PathBuf {
    let unique = Uuid::now_v7().simple().to_string();
    std::env::temp_dir().join(format!("modbit-kill-{tag}-{unique}.jsonl"))
}

fn tool_call() -> ProtocolRecord {
    ProtocolRecord::ToolCall {
        call_id: "call-9".into(),
        name: "fs.write".into(),
        arguments: r#"{"path":"src/lib.rs"}"#.into(),
        status: ToolCallStatus::AwaitingApproval,
    }
}

/// Kill point 1: crash after the tool call was journaled but BEFORE the
/// approval request was written.
#[test]
fn kill_after_tool_call_recovers_pending_call() {
    let path = journal_path("kp1");
    {
        let mut store = ProtocolStateStore::open(&path).unwrap();
        store.append(tool_call()).unwrap();
        // KILL here.
    }
    let store = ProtocolStateStore::open(&path).unwrap();
    assert_eq!(
        store.tool_calls(),
        vec![("fs.write", ToolCallStatus::AwaitingApproval)]
    );
    assert!(
        store.pending_approval().is_none(),
        "approval never journaled"
    );
    let _ = std::fs::remove_file(&path);
}

/// Kill point 2: crash after the approval request — the pending approval
/// is the recovered state.
#[test]
fn kill_after_approval_request_recovers_pending_approval() {
    let path = journal_path("kp2");
    {
        let mut store = ProtocolStateStore::open(&path).unwrap();
        store.append(tool_call()).unwrap();
        store
            .append(ProtocolRecord::ApprovalRequest {
                approval_id: "apr-9".into(),
                tool: "fs.write".into(),
                scope: "src/lib.rs".into(),
                status: modbit_protocol_state::ApprovalStatus::Pending,
            })
            .unwrap();
        // KILL here.
    }
    let store = ProtocolStateStore::open(&path).unwrap();
    let (id, tool, status) = store.pending_approval().expect("approval recovered");
    assert_eq!((id, tool), ("apr-9", "fs.write"));
    assert_eq!(
        status,
        modbit_protocol_state::ApprovalStatus::Pending,
        "still pending after recovery"
    );
    let _ = std::fs::remove_file(&path);
}

/// Kill point 3: a TORN write (the process died mid-line). The loader
/// must skip the torn tail and recover every intact record — the journal
/// is prefix-consistent.
#[test]
fn torn_write_recovers_intact_prefix() {
    let path = journal_path("kp3");
    // Write two intact records, then simulate a torn append.
    {
        let mut store = ProtocolStateStore::open(&path).unwrap();
        store.append(tool_call()).unwrap();
        store
            .append(ProtocolRecord::TerminalCursor {
                run_id: "run-7".into(),
                offset: 128,
            })
            .unwrap();
    }
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        // Half a record: the process died mid-serialization.
        f.write_all(b"{\"kind\":\"tool_call\",\"call_id\":\"call-1")
            .unwrap();
    }

    // Recovery: intact records load; the torn tail is skipped.
    let store = ProtocolStateStore::open(&path).unwrap();
    assert_eq!(
        store.tool_calls(),
        vec![("fs.write", ToolCallStatus::AwaitingApproval)]
    );
    assert!(store
        .records
        .iter()
        .any(|r| matches!(r, ProtocolRecord::TerminalCursor { offset: 128, .. })));
    let _ = std::fs::remove_file(&path);
}

/// Kill point 4: the worktree checkpoint journal survives a crash
/// between deltas — baseline + the deltas that landed restore exactly.
#[test]
fn worktree_journal_survives_crash_between_deltas() {
    use modbit_checkpoint::delta::{Cursors, DeltaJournal, WorktreeDelta};
    let path = journal_path("kp4");
    let mut baseline = std::collections::BTreeMap::new();
    baseline.insert("src/a.rs".to_string(), b"a\n".to_vec());
    {
        let mut journal = DeltaJournal::from_baseline(&baseline, Cursors::default());
        journal.push_delta(WorktreeDelta {
            path: "src/a.rs".into(),
            content: Some(b"a edited\n".to_vec()),
        });
        std::fs::write(&path, serde_json::to_vec(&journal).unwrap()).unwrap();
        // KILL before the second delta lands.
    }
    // Recovery loads what landed, then the new process continues the task.
    let bytes = std::fs::read(&path).unwrap();
    let mut journal: DeltaJournal = serde_json::from_slice(&bytes).unwrap();
    journal.push_delta(WorktreeDelta {
        path: "src/b.rs".into(),
        content: Some(b"b\n".to_vec()),
    });
    journal.set_cursors(31, 5);
    let restored = journal.restore().unwrap();
    assert_eq!(restored.files.get("src/a.rs").unwrap(), b"a edited\n");
    assert!(restored.files.contains_key("src/b.rs"));
    assert_eq!(restored.cursors.runtime_seq, 31);
    let _ = std::fs::remove_file(&path);
}
