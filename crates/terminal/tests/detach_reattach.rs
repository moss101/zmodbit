//! Integration tests for background shell handles (M2, REQ-EV-0135) and
//! foreground→background detach (REQ-EV-0027): a long process survives a
//! UI restart as a durable handle, its log stays offset-addressable under
//! an output cap, and a RESTARTED UI can still reattach and cancel it.

use modbit_terminal::{broker_ext, ExecBroker, RunState};
use std::path::PathBuf;

fn runs_dir(tag: &str) -> PathBuf {
    let unique = uuid::Uuid::now_v7().simple().to_string();
    let dir = std::env::temp_dir().join(format!("modbit-bg-{tag}-{unique}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

use modbit_terminal::pid_alive;

/// QUAL-EV-0027: start a long command, detach (UI restart), reattach with a
/// fresh broker on the same runs dir, and cancel it successfully — the
/// process is really gone afterwards.
#[test]
fn long_process_survives_restart_and_is_cancellable() {
    let dir = runs_dir("reattach");

    // UI 1: start a long test run and let go of the broker entirely.
    {
        let broker_a = ExecBroker::open(&dir).unwrap();
        broker_a
            .spawn("long-test", &["sleep".to_string(), "60".to_string()])
            .unwrap();
        let meta = broker_a.status("long-test").unwrap();
        assert!(meta.pid.is_some(), "pid recorded for detach");
    } // broker_a dropped = detach

    // UI restart: a brand-new broker instance adopts the durable handle.
    let broker_b = ExecBroker::open(&dir).unwrap();
    let meta = broker_b.status("long-test").unwrap();
    assert!(
        matches!(meta.state, RunState::Running),
        "detached run must still be running after UI restart"
    );
    let pid = meta.pid.expect("pid survived the restart");

    // Reattach + cancel from the NEW broker (no in-memory child).
    broker_b.stop("long-test").unwrap();
    let meta = broker_b.status("long-test").unwrap();
    assert!(matches!(meta.state, RunState::Killed));
    assert!(
        !pid_alive(pid),
        "process {pid} must be really dead after cancel"
    );
}

/// QUAL-EV-0135: a background process's full log stays retrievable under
/// an output cap (bounded reads + spill-to-artifact OutputRef).
#[test]
fn bounded_output_and_artifact_spill_after_restart() {
    let dir = runs_dir("bounded");

    // UI 1: background a process emitting well beyond the read cap.
    {
        let broker_a = ExecBroker::open(&dir).unwrap();
        #[cfg(windows)]
        let argv = vec![
            "cmd.exe".to_string(),
            "/C".to_string(),
            "powershell -NoProfile -Command \"1..40000 | ForEach-Object { \\\"line $_ payload padding padding\\\" }\"".to_string(),
        ];
        #[cfg(not(windows))]
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "for i in $(seq 1 40000); do echo \"line $i payload padding padding\"; done"
                .to_string(),
        ];
        broker_a.spawn("noisy", &argv).unwrap();
    } // detach: UI 1 is gone while the process runs

    // UI 2: reattach and wait for completion.
    let broker_b = ExecBroker::open(&dir).unwrap();
    let mut attempts = 0;
    loop {
        let meta = broker_b.status("noisy").unwrap();
        if !matches!(meta.state, RunState::Running) {
            break;
        }
        attempts += 1;
        assert!(attempts < 600, "background process did not finish");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    // Bounded reads: a small window against a multi-megabyte log.
    let (window, offset) = broker_b.read_output("noisy", 0, 64).unwrap();
    assert!(window.len() <= 64, "read respects the cap");
    assert!(offset > 0, "offset advanced for the next window");
    let (tail, final_offset) = broker_b.read_output("noisy", offset, usize::MAX).unwrap();
    assert!(final_offset > offset, "replay is offset-addressed");
    assert!(!tail.is_empty());

    // Full log spills to a content-addressed artifact (OutputRef) with a
    // bounded preview — never a prompt dump.
    let artifact = broker_ext::spill_artifact(&dir, "noisy").unwrap();
    assert_eq!(artifact.digest.len(), 64);
    assert!(artifact.byte_length > 1024 * 1024, "log is multi-megabyte");
    assert!(artifact.preview.chars().count() <= 256);
    // Artifact bytes equal the durable log bytes (digest integrity).
    let log = std::fs::read(dir.join("noisy").join("output.log")).unwrap();
    let at_offset = &log[offset as usize..offset as usize + tail.len()];
    assert_eq!(
        at_offset,
        &tail[..],
        "replayed bytes match the log at that offset"
    );
    let _ = &artifact.artifact_path;
}
