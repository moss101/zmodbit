//! Real-process tests for the durable exec broker (M2, docs/21): structured
//! argv, offset replay, typed exit, stop, and large-output bounded reads.

use std::path::PathBuf;

use modbit_git::GitRepo;
use modbit_terminal::{ExecBroker, RunState, TerminalError};

fn broker_at(tag: &str) -> (PathBuf, ExecBroker) {
    // Full uuid: v7's leading chars are timestamp-derived and collide when
    // tests start in the same millisecond.
    let unique = uuid::Uuid::now_v7().simple().to_string();
    let mut dir = std::env::temp_dir();
    dir.push(format!("modbit-m23-{tag}-{unique}"));
    let broker = ExecBroker::open(&dir).expect("open broker");
    (dir, broker)
}

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
    assert!(!first.is_empty());
    assert!(offset1 > 0);

    let (rest, _) = broker.read_output("r", offset1, usize::MAX).unwrap();
    let total = broker.read_output("r", 0, usize::MAX).unwrap().0.len();
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
    assert!(meta.ended_at_ms.is_some());
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
fn large_output_requires_bounded_reads_to_completion() {
    // Build a real git repo with a 700KB file, then `git show` it through
    // the broker: the output exceeds READ_CHUNK_MAX so the client must keep
    // resuming from the returned offset until the stream completes.
    let repo_dir = {
        let unique = uuid::Uuid::now_v7().simple().to_string();
        let mut root = std::env::temp_dir();
        root.push(format!("modbit-m23-big-{unique}"));
        root
    };
    let repo = GitRepo::init(&repo_dir).unwrap();
    let big = vec![b'A'; 700 * 1024];
    std::fs::create_dir_all(repo_dir.join("src")).unwrap();
    std::fs::write(repo_dir.join("big.txt"), &big).unwrap();
    repo.commit_all("big file").unwrap();

    let (_dir, broker) = broker_at("bigshow");
    broker
        .spawn(
            "show",
            &[
                "git".to_string(),
                "-C".to_string(),
                repo_dir.display().to_string(),
                "show".to_string(),
                "--no-color".to_string(),
                "HEAD:big.txt".to_string(),
            ],
        )
        .unwrap();
    broker.wait_and_record("show").unwrap();

    // Rehydrate from 0: each read is capped at READ_CHUNK_MAX.
    let mut offset = 0u64;
    let mut total: Vec<u8> = Vec::new();
    loop {
        let (chunk, next) = broker.read_output("show", offset, usize::MAX).unwrap();
        let done = chunk.is_empty();
        total.extend_from_slice(&chunk);
        offset = next;
        if done {
            break;
        }
    }
    assert!(
        total.len() >= 700 * 1024,
        "expected >= 700KB of output, got {}",
        total.len()
    );
    assert_eq!(total[0], b'A');
    assert_eq!(*total.last().unwrap(), b'A');
}
