//! Phase 6 item 1 — PTY sessions over the REAL modbit-execd broker
//! (docs/21 § Durable terminal): shell.attach semantics via a REAL
//! portable-pty session in the execd process — the client writes to the
//! session's stdin and reads the accumulated output back; cancel kills
//! the session. Uses the platform's REAL pty (ConPTY/libc), no fakes.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use modbit_terminal::client::ExecdClient;

fn read_boot_line(child: &mut Child) -> Option<String> {
    let stdout = child.stdout.take()?;
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    serde_json::from_str::<serde_json::Value>(&line)
        .ok()?
        .get("addr")
        .and_then(|v| v.as_str())
        .map(String::from)
}

fn wait_killed(client: &ExecdClient, id: &str) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        match client.status(id) {
            Ok(_) => {}
            Err(_) => return true, // session gone: killed and dropped
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    false
}

/// A REAL pty session served by the REAL broker: spawn → write to stdin →
/// read the echoed command + output → cancel.
#[test]
fn pty_session_over_the_broker_round_trips() {
    let execd_bin = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/modbit-execd");
    let mut child = Command::new(&execd_bin)
        .env("MODBIT_EXECD_ADDR", "127.0.0.1:0")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn execd");
    let addr = read_boot_line(&mut child).expect("execd boot");
    std::thread::spawn(move || {
        let r = BufReader::new(child.stdout.take().unwrap());
        for _ in r.lines() {}
    });

    let client = ExecdClient::connect(&addr).expect("connect execd");
    let shell = if cfg!(windows) { "cmd" } else { "sh" };

    // shell.attach: spawn the interactive session.
    client
        .pty_spawn("e2e-pty", &[shell.to_string()], None, 24, 80)
        .expect("pty_spawn over the broker");

    // shell.input: write a command into the session's stdin. No shell
    // arithmetic (cmd.exe has none) — the typed line itself is echoed by
    // the pty, so the literal marker is the cross-platform signal.
    let (command, marker) = if cfg!(windows) {
        (b"echo pty-broker-marker\r\n".as_slice(), "pty-broker-marker")
    } else {
        (
            b"echo pty-broker-marker-$((6*7))\n".as_slice(),
            "pty-broker-marker-42",
        )
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut wrote = false;
    while std::time::Instant::now() < deadline {
        if client.pty_write("e2e-pty", command).is_ok() {
            wrote = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(wrote, "stdin write failed");

    // Terminal-panel streaming: poll the accumulated output.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut seen = String::new();
    while std::time::Instant::now() < deadline {
        let (bytes, _) = client.pty_read("e2e-pty", 0, 64 * 1024).expect("pty_read");
        seen = String::from_utf8_lossy(&bytes).to_string();
        if seen.contains(marker) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        seen.contains(marker),
        "output must stream from the broker's pty session: {seen}"
    );

    // shell.cancel: kill the session.
    client.pty_cancel("e2e-pty").expect("cancel");
    assert!(
        wait_killed(&client, "e2e-pty") || client.pty_read("e2e-pty", 0, 10).is_err(),
        "the cancelled session is gone or killed"
    );
}
