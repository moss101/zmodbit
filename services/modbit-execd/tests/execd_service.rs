//! modbit-execd service integration tests (M2.3): a real broker process
//! serving spawn/status/read/stop/list over TCP JSON-lines.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};

use base64::Engine as _;

struct Execd {
    child: Child,
    // The stdout reader must stay alive: dropping it closes the pipe and the
    // broker panics (and dies) on its next stdout write.
    _stdout: BufReader<std::process::ChildStdout>,
    addr: String,
}

impl Drop for Execd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_execd(tag: &str) -> Execd {
    let bin = env!("CARGO_BIN_EXE_modbit-execd");
    let unique = uuid::Uuid::now_v7().simple().to_string();
    let mut runs = std::env::temp_dir();
    runs.push(format!("modbit-execdt-{tag}-{unique}"));

    let mut child = Command::new(bin)
        .env("MODBIT_EXECD_RUNS", &runs)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn modbit-execd");

    let stdout = child.stdout.take().expect("stdout piped");
    let mut reader = BufReader::new(stdout);
    let mut boot_line = String::new();
    reader.read_line(&mut boot_line).expect("read boot line");
    let boot: serde_json::Value = serde_json::from_str(boot_line.trim()).expect("boot json");
    let addr = boot["addr"].as_str().expect("addr").to_string();
    Execd {
        child,
        _stdout: reader,
        addr,
    }
}

fn rpc(stream: &mut TcpStream, request: serde_json::Value) -> serde_json::Value {
    writeln!(stream, "{request}").expect("write rpc");
    let mut line = String::new();
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    reader.read_line(&mut line).expect("read rpc response");
    serde_json::from_str(line.trim()).expect("valid json response")
}

#[test]
fn execd_spawn_status_read_over_tcp() {
    let execd = spawn_execd("main");
    let mut stream = TcpStream::connect(&execd.addr).expect("connect execd");

    // Spawn a real process through the broker.
    let spawn = rpc(
        &mut stream,
        serde_json::json!({
            "op": "spawn",
            "id": "r1",
            "argv": ["git", "--version"],
        }),
    );
    assert_eq!(spawn["ok"], true, "{spawn}");

    // Typed status: durable and queryable.
    std::thread::sleep(std::time::Duration::from_millis(300));
    let status = rpc(
        &mut stream,
        serde_json::json!({ "op": "status", "id": "r1" }),
    );
    assert_eq!(status["ok"], true);
    assert!(
        status["state"].as_str().unwrap().starts_with("exited"),
        "expected exited, got {status}"
    );

    // Offset-addressed output read: base64 payload decodes to git version.
    let read = rpc(
        &mut stream,
        serde_json::json!({ "op": "read", "id": "r1", "offset": 0, "max": 1024 }),
    );
    assert_eq!(read["ok"], true);
    let data = read["data"].as_str().expect("base64 data");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(data)
        .unwrap();
    let text = String::from_utf8(decoded).unwrap();
    assert!(text.contains("git version"), "output: {text}");
}

#[test]
fn execd_stop_and_list_are_durable() {
    let execd = spawn_execd("stop");
    let mut stream = TcpStream::connect(&execd.addr).expect("connect execd");

    let long_argv: Vec<String> = if cfg!(windows) {
        vec!["ping".into(), "-n".into(), "11".into(), "127.0.0.1".into()]
    } else {
        vec!["sleep".into(), "10".into()]
    };
    let spawn = rpc(
        &mut stream,
        serde_json::json!({ "op": "spawn", "id": "long", "argv": long_argv }),
    );
    assert_eq!(spawn["ok"], true, "{spawn}");

    let stop = rpc(
        &mut stream,
        serde_json::json!({ "op": "stop", "id": "long" }),
    );
    assert_eq!(stop["ok"], true, "{stop}");

    let list = rpc(&mut stream, serde_json::json!({ "op": "list" }));
    assert_eq!(list["ok"], true);
    let runs = list["runs"].as_array().expect("runs array");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["state"], "Killed");
}
