//! ExecdClient against the REAL modbit-execd binary (docs/21: durable
//! process broker). `CARGO_BIN_EXE_modbit-execd` is the binary cargo just
//! built for this test target — a genuine process boundary, not a stub.

use std::process::{Command, Stdio};
use std::time::Duration;

use modbit_terminal::client::ExecdClient;
use modbit_terminal::RunState;

fn spawn_execd() -> (ExecdClient, std::process::Child) {
    let exe = env!("CARGO_BIN_EXE_modbit-execd");
    let mut child = Command::new(exe)
        .env("MODBIT_EXECD_ADDR", "127.0.0.1:0")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn modbit-execd");
    let boot_line = {
        use std::io::BufRead;
        let stdout = child.stdout.take().expect("stdout");
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line).expect("boot line");
        line
    };
    let addr = serde_json::from_str::<serde_json::Value>(&boot_line)
        .expect("boot json")
        .get("addr")
        .and_then(|v| v.as_str())
        .expect("addr in boot line")
        .to_string();
    // Wait for the "ready" line before connecting.
    std::thread::sleep(Duration::from_millis(150));
    let client = ExecdClient::connect(&addr).expect("connect execd");
    (client, child)
}

#[test]
fn client_runs_command_through_real_broker_with_cwd() {
    let (client, mut child) = spawn_execd();
    let cwd = std::env::temp_dir();
    let run = format!("it-{}", uuid::Uuid::now_v7().simple());

    let (status, output) = client
        .run_capture(
            &run,
            &[
                "git".to_string(),
                "--version".to_string(),
            ],
            Some(&cwd),
            Duration::from_secs(30),
            64 * 1024,
        )
        .expect("run through broker");

    match status.state {
        RunState::Exited(0) => {}
        other => panic!("expected clean exit, got {other:?}"),
    }
    let text = String::from_utf8_lossy(&output);
    assert!(text.contains("git version"), "output captured: {text}");

    // The run survives as durable state in the broker (list).
    let _ = client.read_output(&run, 0, 1024).expect("re-read output");
    child.kill().expect("stop execd");
}

#[test]
fn client_surfaces_nonzero_exit_and_missing_cwd_fails() {
    let (client, mut child) = spawn_execd();
    let run_ok = format!("fail-{}", uuid::Uuid::now_v7().simple());

    // Non-zero exit is a RESULT, not an error (E2E-002 semantics).
    let (status, _) = client
        .run_capture(
            &run_ok,
            &["git".to_string(), "--bogus-flag".to_string()],
            None,
            Duration::from_secs(30),
            64 * 1024,
        )
        .expect("capture failed command");
    match status.state {
        RunState::Exited(code) => assert_ne!(code, 0, "bogus flag must fail"),
        other => panic!("expected exit, got {other:?}"),
    }

    // A nonexistent cwd is rejected by spawn (honored exactly, never
    // silently falling back to another directory).
    let err = client.spawn(
        "bad-cwd",
        &["true".to_string()],
        Some(std::path::Path::new("/nonexistent/modbit/cwd")),
    );
    assert!(err.is_err(), "spawn must fail closed on a bad cwd");
    child.kill().expect("stop execd");
}
