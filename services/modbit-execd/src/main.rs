//! modbit-execd — the durable PTY/process broker service (M2, docs/21 §
//! Durable `modbit-execd`).
//!
//! TCP JSON-line service over the `modbit-terminal` exec broker:
//!   {"op":"spawn","id":"r1","argv":["git","--version"]}
//!   {"op":"status","id":"r1"}      → {"ok":true,"state":"exited","exit":0}
//!   {"op":"read","id":"r1","offset":0,"max":1024}
//!                                  → {"ok":true,"data":"<base64>","offset":n}
//!   {"op":"stop","id":"r1"}        → {"ok":true}
//!   {"op":"list"}                  → {"ok":true,"runs":[...]}
//!
//! The broker owns local processes; UI/clients can detach and reconnect
//! without losing output — the durable run directories are the source of
//! truth. The broker is not authorized to create capabilities or decide
//! policy (docs/21).

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::Arc;

use modbit_terminal::{ExecBroker, TerminalError};

fn main() {
    let runs_dir = std::env::var("MODBIT_EXECD_RUNS").unwrap_or_else(|_| {
        let mut p = std::env::temp_dir();
        p.push(format!("modbit-execd-{}", std::process::id()));
        p.to_string_lossy().to_string()
    });
    let _ = &runs_dir;
    let addr = std::env::var("MODBIT_EXECD_ADDR").unwrap_or_else(|_| "127.0.0.1:0".into());

    let broker = Arc::new(ExecBroker::open(Path::new(&runs_dir)).expect("open runs dir"));
    let listener = std::net::TcpListener::bind(&addr).expect("bind execd");
    let local = listener.local_addr().expect("local addr").to_string();
    // Boot channel: the bound address as one json line on stdout.
    println!("{}", serde_json::json!({ "addr": local }));
    println!("ready");
    use std::io::Write as _;
    std::io::stdout().flush().ok();

    eprintln!("modbit-execd: serving on {local} (runs: {runs_dir})");
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let broker = broker.clone();
        std::thread::spawn(move || {
            let reader = BufReader::new(match stream.try_clone() {
                Ok(s) => s,
                Err(_) => return,
            });
            for line in reader.lines() {
                let Ok(line) = line else { return };
                if line.trim().is_empty() {
                    continue;
                }
                let response = handle_line(&broker, &line);
                let mut stream = stream.try_clone().expect("stream clone");
                if writeln!(stream, "{response}").is_err() {
                    return;
                }
            }
        });
    }
}

fn handle_line(broker: &ExecBroker, line: &str) -> String {
    let parsed: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return serde_json::json!({ "ok": false, "error": format!("bad json: {e}") })
                .to_string()
        }
    };
    let op = parsed
        .get("op")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let get_str = |key: &str| parsed.get(key).and_then(|v| v.as_str()).map(String::from);
    let get_num = |key: &str| parsed.get(key).and_then(|v| v.as_u64());

    match op.as_str() {
        "spawn" => {
            let id = get_str("id").unwrap_or_default();
            let argv: Vec<String> = parsed
                .get("argv")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            // Optional cwd pins the run's working directory exactly
            // (REQ-EV-0100 contract; absent means the broker's own cwd).
            let cwd = get_str("cwd").map(std::path::PathBuf::from);
            let result = broker.spawn_full(&id, &argv, cwd.as_deref(), &[]);
            eprintln!(
                "modbit-execd: spawn id={id:?} argv={argv:?} cwd={cwd:?} -> {}",
                result.is_ok()
            );
            match result {
                Ok(()) => serde_json::json!({ "ok": true }).to_string(),
                Err(e) => error_response(&e),
            }
        }
        "status" => {
            let id = get_str("id").unwrap_or_default();
            let has_child = broker.has_child(&id);
            let meta = broker.status(&id);
            eprintln!(
                "modbit-execd: status id={id:?} has_child_before={has_child} -> {:?}",
                meta.as_ref().map(|m| (&m.state, m.pid))
            );
            match meta {
                Ok(meta) => {
                    let state = match meta.state {
                        modbit_terminal::RunState::Running => "running".to_string(),
                        modbit_terminal::RunState::Exited(code) => {
                            format!("exited({code})")
                        }
                        modbit_terminal::RunState::Killed => "killed".to_string(),
                        modbit_terminal::RunState::Interrupted => "interrupted".to_string(),
                    };
                    serde_json::json!({ "ok": true, "state": state, "argv": meta.argv }).to_string()
                }
                Err(e) => error_response(&e),
            }
        }
        "read" => {
            let id = get_str("id").unwrap_or_default();
            let offset = get_num("offset").unwrap_or(0);
            let max = get_num("max").unwrap_or(512 * 1024) as usize;
            match broker.read_output(&id, offset, max) {
                Ok((bytes, new_offset)) => {
                    use base64::Engine as _;
                    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    serde_json::json!({ "ok": true, "data": encoded, "offset": new_offset })
                        .to_string()
                }
                Err(e) => error_response(&e),
            }
        }
        "stop" => {
            let id = get_str("id").unwrap_or_default();
            match broker.stop(&id) {
                Ok(()) => serde_json::json!({ "ok": true }).to_string(),
                Err(e) => error_response(&e),
            }
        }
        "list" => match broker.list() {
            Ok(runs) => serde_json::json!({ "ok": true, "runs": runs }).to_string(),
            Err(e) => error_response(&e),
        },
        other => {
            serde_json::json!({ "ok": false, "error": format!("unknown op {other:?}") }).to_string()
        }
    }
}

fn error_response(e: &TerminalError) -> String {
    serde_json::json!({ "ok": false, "error": e.to_string() }).to_string()
}
