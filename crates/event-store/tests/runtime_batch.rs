//! Backend batch integration tests (M1): background operations with durable
//! handles/OutputRef/stop (REQ-EV-0221), typed tool-call/result pairing that
//! survives restarts (REQ-EV-0098), and OutputRef spill for multi-MB
//! payloads instead of giant IPC bodies (REQ-EV-0108).

use modbit_event_store::runtime::RuntimeStore;
use std::path::PathBuf;

fn runtime_paths(tag: &str) -> (PathBuf, PathBuf) {
    let unique = uuid::Uuid::now_v7().simple().to_string()[..8].to_string();
    let mut core = std::env::temp_dir();
    core.push(format!("modbit-b-{tag}-{unique}-core.db"));
    let mut index = std::env::temp_dir();
    index.push(format!("modbit-b-{tag}-{unique}-index.db"));
    (core, index)
}

/// REQ-EV-0221: run a "long" background operation, list/status/read full
/// output/stop — all durable across a store restart.
#[test]
fn background_list_output_stop_survive_restart() {
    let (core_db, _index_db) = runtime_paths("bg");

    // First life: register the long operation with its OutputRef.
    {
        let runtime = RuntimeStore::open(&core_db).unwrap();
        let big_output = vec![b'x'; 3 * 1024 * 1024]; // multi-MB output
        let output_ref = runtime
            .write_output_ref("out-1", "text/plain", &big_output)
            .unwrap();
        assert_eq!(output_ref.byte_length, big_output.len() as u64);
        assert!(
            output_ref.preview_text.len() <= modbit_event_store::runtime::PREVIEW_BYTES,
            "preview must be bounded"
        );
        runtime
            .register_background(
                "bg-1",
                "terminal-run",
                Some("out-1"),
                "long output preview…",
            )
            .unwrap();
    }

    // Restart: the handle, status and full output all survive (no transcript
    // inference — they come from the durable store).
    let runtime = RuntimeStore::open(&core_db).unwrap();
    let list = runtime.list_background().unwrap();
    assert_eq!(list.len(), 1);
    let (handle_id, kind, status, preview) = &list[0];
    assert_eq!(handle_id, "bg-1");
    assert_eq!(kind, "terminal-run");
    assert_eq!(status, "running");
    assert_eq!(preview, "long output preview…");

    let output = runtime.read_output("out-1").unwrap();
    assert_eq!(output.len(), 3 * 1024 * 1024);

    // Stop after the UI restart: durable cancellation.
    runtime.stop_background("bg-1").unwrap();
    let (_, _, status, _) = runtime.list_background().unwrap().remove(0);
    assert_eq!(status, "stopped");
}

/// REQ-EV-0098: tool call/result pairs survive a restart — provider replay
/// preserves the typed pairing.
#[test]
fn tool_result_pairs_survive_restart() {
    let (core_db, _) = runtime_paths("pairs");
    {
        let runtime = RuntimeStore::open(&core_db).unwrap();
        runtime
            .record_tool_call("call-1", "step-7", "fs.read", "read", "args-hash-1")
            .unwrap();
        runtime
            .record_tool_call("call-2", "step-7", "git.diff", "read", "args-hash-2")
            .unwrap();
        runtime
            .record_tool_result("call-1", br#"{"ok":true,"bytes":1024}"#)
            .unwrap();
    }

    // Restart: pairing intact, results re-enter as typed payloads.
    let runtime = RuntimeStore::open(&core_db).unwrap();
    let pairs = runtime.tool_pairs("step-7").unwrap();
    assert_eq!(pairs.len(), 2);
    assert_eq!(pairs[0].0, "call-1");
    assert_eq!(
        pairs[0].2.as_deref(),
        Some(br#"{"ok":true,"bytes":1024}"#.as_slice())
    );
    assert_eq!(pairs[1].0, "call-2");
    assert!(pairs[1].2.is_none(), "uncompleted call has no result yet");
}

/// REQ-EV-0108: a multi-MB result is spilled to an OutputRef (bounded
/// preview) instead of crossing IPC as a giant body.
#[test]
fn multi_mb_result_uses_output_ref_spill() {
    let (core_db, index_db) = runtime_paths("spill");
    let runtime = RuntimeStore::open(&core_db).unwrap();

    let huge = vec![0u8; 12 * 1024 * 1024];
    let output_ref = runtime
        .write_output_ref("out-huge", "application/octet-stream", &huge)
        .unwrap();

    // Bounded preview, exact length, content-addressed hash.
    assert!(output_ref.preview_text.len() <= modbit_event_store::runtime::PREVIEW_BYTES);
    assert_eq!(output_ref.byte_length, huge.len() as u64);
    assert_eq!(output_ref.object_hash, output_ref.checksum);

    // Full reassembly is exact.
    assert_eq!(runtime.read_output("out-huge").unwrap(), huge);

    // The index database is a separate durable store (REQ-EV-0101).
    let index = modbit_event_store::index_store::IndexStore::open(&index_db).unwrap();
    index
        .record_generation(1, "2026-09-04T00:00:00Z", "hash-gen-1")
        .unwrap();
    assert_eq!(index.latest_generation().unwrap(), Some(1));
    assert!(
        index_db.exists() && core_db.exists(),
        "core.db and index.db are separate durable files"
    );
}
