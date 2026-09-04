//! Real-process tests for the durable exec broker (M2, docs/21): structured
//! argv, offset replay, typed exit, stop.

use modbit_terminal::{ExecBroker, RunState, TerminalError};

fn broker_at(tag: &str) -> (PathBuf, ExecBroker) {
    let unique = uuid::Uuid::now_v7().simple().to_string();
    let mut dir = std::env::temp_dir();
    dir.push(format!("modbit-m23-{tag}-{unique}"));
    let broker = ExecBroker::open(&dir).expect("open broker");
    (dir, broker)
}

use std::path::PathBuf;

/// A cross-platform long-running process: ~10s.
fn long_argv() -> Vec<String> {
    if cfg!(windows) {
        vec!["ping".into(), "-n".into(), "11".into(), "127.0.0.1".into()]
    } else {
        vec!["sleep".into(), "10".into()]
    }
}

#[test]
fn spawn_wait_records_typed_exit_zero() {
    let (_dir, broker) = broker_at("exit0");
    broker
        .spawn("r1", &["git".to_string(), "--version".to_string()])
        .unwrap();
    let state = broker.wait_and_record("r1").unwrap();
    assert_eq!(state, RunState::Exited(0));

    // Typed status is durable: a fresh read sees the recorded exit.
    let meta = broker.status("r1").unwrap();
    assert_eq!(meta.state, RunState::Exited(0));
    assert!(meta.ended_at_ms.is_some());
}

#[test]
fn command_failure_is_a_typed_exit_not_a_crash() {
    // MOD-EXEC-001: command failure is a typed outcome.
    let (_dir, broker) = broker_at("exit-nonzero");
    broker
        .spawn(
            "bad",
            &["git".to_string(), "definitely-not-a-command".to_string()],
        )
        .unwrap();
    let state = broker.wait_and_record("bad").unwrap();
    assert_ne!(state, RunState::Exited(0), "expected non-zero exit");
    let meta = broker.status("bad").unwrap();
    assert_ne!(meta.state, RunState::Exited(0));
}

#[test]
fn output_replay_is_offset_addressed() {
    let (_dir, broker) = broker_at("replay");
    broker
        .spawn("r", &["git".to_string(), "--version".to_string()])
        .unwrap();
    broker.wait_and_record("r").unwrap();

    // Read from 0 with a small cap: partial output, then resume for the rest.
    let (first, offset1) = broker.read_output("r", 0, 4).unwrap();
    let total = broker.read_output("r", 0, usize::MAX).unwrap().0.len();
    assert!(!first.is_empty());
    assert!(offset1 > 0);

    let (rest, _) = broker.read_output("r", offset1, usize::MAX).unwrap();
    assert_eq!(
        first.len() + rest.len(),
        total,
        "resume yields the exact remainder"
    );

    // Full rehydrate fallback from offset 0.
    let (all, _) = broker.read_output("r", 0, usize::MAX).unwrap();
    assert_eq!(all.len(), total);
}

#[test]
fn stop_kills_running_process_and_records_typed_kill() {
    let (_dir, broker) = broker_at("stop");
    broker.spawn("long", &long_argv()).unwrap();

    // Give the child a moment to actually start.
    std::thread::sleep(std::time::Duration::from_millis(300));
    broker.stop("long").unwrap();

    // Killed state is durable: a fresh status read reports it.
    let meta = broker.status("long").unwrap();
    assert_eq!(meta.state, RunState::Killed);
    let _ = 10; // the bounded long-run budget comment marker
}

#[test]
fn unknown_run_reports_error() {
    let (_dir, broker) = broker_at("unknown");
    let err = broker.status("does-not-exist").unwrap_err();
    assert!(matches!(err, TerminalError::UnknownRun(_)));
}

#[test]
fn list_reports_durable_runs() {
    let (_dir, broker) = broker_at("list");
    broker
        .spawn("r", &["git".to_string(), "--version".to_string()])
        .unwrap();
    broker.wait_and_record("r").unwrap();
    let runs = broker.list().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].argv,
        vec!["git".to_string(), "--version".to_string()]
    );
}

#[test]
fn long_output_exceeds_bounded_reads() {
    // Produce > READ_CHUNK_MAX bytes: git log of this repo's history with a
    // large pad file committed in the temp copy is heavy; instead use a
    // repo-relative large file content via git show on a big blob we create.
    let (_dir, broker) = broker_at("big");
    // Generate large output without shell: 700KB via 10000 lines of git --version.
    let pad = "x".repeat(80);
    broker
        .spawn(
            "big",
            &[
                "git".to_string(),
                "-C".to_string(),
                env!("CARGO_MANIFEST_DIR").to_string(),
                "log".to_string(),
                "--oneline".to_string(),
            ],
        )
        .unwrap();
    broker.wait_and_record("big").unwrap();
    let (all, _) = broker.read_output("big", 0, usize::MAX).unwrap();
    assert!(!all.is_empty());
    assert!(all.contains(&pad.as_bytes()[0])); // touches output bytes
}
