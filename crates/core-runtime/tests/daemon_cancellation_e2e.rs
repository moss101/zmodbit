//! Daemon-driven cancellation E2E (Future-tasks.md Phase 2 item 3): the
//! REAL `modbit-core` daemon, real scheduler, real git worktree, real
//! execd broker running a REAL long-lived process (`sleep 30`), with a
//! body-capturing scripted model fixture. Proves the three lifecycle
//! semantics through production routing:
//!   StopTask   — aborts the in-flight run: the broker run is killed and
//!                the task reaches Cancelled in seconds, not after the
//!                tool timeout; the model is never invoked again.
//!   PauseTask  — parks the run at the next turn boundary (Waiting).
//!   SteerTask  — the note rides as a user message on the next request.

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use prost::Message;
use reqwest::blocking::Client;

use modbit_git::GitRepo;
use modbit_protocol::modbit::protocol::v1 as pb;

static E2E_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn tempdir(tag: &str) -> PathBuf {
    let suffix: String = uuid::Uuid::now_v7().simple().to_string().chars().rev().take(8).collect::<String>().chars().rev().collect();
    let dir = std::env::temp_dir().join(format!("dxe{tag}{suffix}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn readme_fixture(tag: &str) -> PathBuf {
    let root = tempdir(tag);
    let repo = GitRepo::init(&root).expect("init");
    repo.set_config("user.email", "e2e@modbit.test").unwrap();
    repo.set_config("user.name", "Modbit E2E").unwrap();
    repo.set_config("core.autocrlf", "false").unwrap();
    std::fs::write(root.join("notes.txt"), "cancellation fixture\n").unwrap();
    repo.commit_all("fixture baseline").expect("baseline");
    root
}

fn sse(frames: &[serde_json::Value], done: bool) -> String {
    let mut out = String::new();
    for f in frames {
        out.push_str(&format!("data: {f}\n\n"));
    }
    if done {
        out.push_str("data: [DONE]\n\n");
    }
    out
}

fn tool_call_turn(call_id: &str, name: &str, args: &str) -> String {
    let arguments: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
    sse(
        &[
            serde_json::json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": call_id,
                            "function": { "name": name, "arguments": arguments.to_string() },
                        }]
                    }
                }]
            }),
            serde_json::json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
            serde_json::json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}),
        ],
        true,
    )
}

fn text_turn(text: &str) -> String {
    sse(
        &[
            serde_json::json!({"choices": [{ "delta": { "content": text } }]}),
            serde_json::json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
            serde_json::json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}),
        ],
        true,
    )
}

fn spawn_model_fixture(script: Vec<String>) -> (SocketAddr, Arc<Mutex<Vec<serde_json::Value>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let bodies: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = bodies.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let script = script.clone();
            let captured = captured.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                use std::io::Read;
                loop {
                    let n = reader.read(&mut chunk).unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf);
                    if let Some(end) = text.find("\r\n\r\n") {
                        let head = &text[..end];
                        let clen = head
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + clen {
                            break;
                        }
                    }
                }
                let raw = String::from_utf8_lossy(&buf).to_string();
                let body_start = raw.find("\r\n\r\n").map(|i| i + 4).unwrap_or(raw.len());
                captured
                    .lock()
                    .unwrap()
                    .push(serde_json::from_str(&raw[body_start..]).expect("fixture body is JSON"));
                let index = captured.lock().unwrap().len() - 1;
                let body = script
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| text_turn("done"));
                use std::io::Write;
                let mut stream = stream;
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                );
                let _ = stream.write_all(body.as_bytes());
            });
        }
    });
    (addr, bodies)
}

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

fn spawn_core(repo_root: &PathBuf, worktree_root: &PathBuf, model_addr: SocketAddr) -> (Child, String, PathBuf) {
    let execd_bin = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/modbit-execd");
    let mut execd = Command::new(&execd_bin)
        .env("MODBIT_EXECD_ADDR", "127.0.0.1:0")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn execd");
    let execd_addr = read_boot_line(&mut execd).expect("execd boot");

    let db_dir = tempdir("db");
    let db_path = db_dir.join("core.db");
    let exe = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/modbit-core");
    let mut command = Command::new(exe);
    let mut command = command.env("MODBIT_CORE_DB", &db_path);
    if !cfg!(windows) {
        command = command.env("MODBIT_SOCKET", db_dir.join("s.sock"));
    }
    let mut child = command
        .env("MODBIT_HTTP_ADDR", "127.0.0.1:0")
        .env("MODBIT_REPO_ROOT", repo_root)
        .env("MODBIT_WORKTREE_ROOT", worktree_root)
        .env("MODBIT_EXECD_ADDR", &execd_addr)
        .env("MODBIT_BASE_URL", format!("http://{model_addr}"))
        .env("MODBIT_MODEL", "fixture-model")
        .env("MODBIT_PROVIDER", "openai")
        .env("OPENAI_API_KEY", "fixture-key")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn modbit-core");

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("core boot line");
    std::thread::spawn(move || {
        for _ in reader.lines() {}
    });

    let stderr = child.stderr.take().unwrap();
    let mut err_reader = BufReader::new(stderr);
    let mut daemon = None;
    while daemon.is_none() {
        let mut l = String::new();
        match err_reader.read_line(&mut l) {
            Ok(0) => break,
            Ok(_) => {
                if let Some(addr) = l.strip_prefix("modbit-core: http daemon on ").map(str::trim) {
                    daemon = Some(addr.to_string());
                }
            }
            Err(_) => break,
        }
    }
    std::thread::spawn(move || {
        for _line in err_reader.lines() {}
    });
    std::mem::forget(execd);
    (child, daemon.expect("daemon addr"), db_path)
}

fn request(daemon: &str, req: pb::surface_request::Request) -> pb::SurfaceResponse {
    let client = Client::builder().timeout(Duration::from_secs(30)).build().unwrap();
    let body = pb::SurfaceRequest { request: Some(req) }.encode_to_vec();
    let response = client
        .post(format!("http://{daemon}/commands"))
        .header("Content-Type", "application/x-protobuf")
        .body(body)
        .send()
        .expect("post");
    assert!(response.status().is_success());
    pb::SurfaceResponse::decode(response.bytes().unwrap().as_ref()).unwrap()
}

fn start_task(daemon: &str, title: &str, prompt: &str) -> String {
    let created = request(
        daemon,
        pb::surface_request::Request::CreateTask(pb::CreateTaskCommand {
            session_id: String::new(),
            title: title.into(),
            prompt: prompt.into(),
        }),
    );
    assert!(created.ok, "{}", created.error);
    let task_id = created.task.unwrap().task_id;
    for payload in [
        pb::surface_request::Request::QueueTask(pb::QueueTaskCommand { task_id: task_id.clone() }),
        pb::surface_request::Request::StartTask(pb::StartTaskCommand { task_id: task_id.clone() }),
    ] {
        let r = request(daemon, payload);
        assert!(r.ok, "{}", r.error);
    }
    task_id
}

fn task_state(daemon: &str, task_id: &str) -> i32 {
    request(daemon, pb::surface_request::Request::GetFleet(pb::GetFleetRequest {}))
        .fleet
        .unwrap()
        .tasks
        .iter()
        .find(|t| t.task_id == task_id)
        .map(|t| t.state)
        .unwrap_or(-1)
}

fn wait_for_state(daemon: &str, task_id: &str, want: i32, timeout: Duration) -> Duration {
    let start = Instant::now();
    loop {
        let state = task_state(daemon, task_id);
        if state == want {
            return start.elapsed();
        }
        if start.elapsed() > timeout {
            panic!("task did not reach state {want} within {timeout:?} (last {state})");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// StopTask during a REAL 30-second broker process: the run aborts (the
/// sleep is killed), the task is Cancelled within seconds, and the model
/// is never invoked for a second turn.
#[test]
fn stop_task_aborts_long_running_tool_and_cancels() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let repo = readme_fixture("sx");
    let worktrees = tempdir("sw");
    let (model, bodies) = spawn_model_fixture(vec![
        tool_call_turn("c1", "shell.run", r#"{"argv":"sleep 30"}"#),
        text_turn("never reached"),
    ]);
    let (mut core, daemon, db_path) = spawn_core(&repo, &worktrees, model);

    let task_id = start_task(&daemon, "sleep forever", "Run the sleep.");
    // Let the tool actually start, then stop mid-flight.
    std::thread::sleep(Duration::from_millis(1_500));
    let stopped = request(
        &daemon,
        pb::surface_request::Request::StopTask(pb::StopTaskCommand {
            task_id: task_id.clone(),
            reason: "test stop".into(),
        }),
    );
    assert!(stopped.ok, "{}", stopped.error);

    let elapsed = wait_for_state(
        &daemon,
        &task_id,
        pb::TaskStatus::Cancelled as i32,
        Duration::from_secs(15),
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "stop must not wait out the 30s tool (took {elapsed:?})"
    );
    // The RUN aborted too, not just the task projection: the durable run
    // plane carries run_failed(cancelled) while the 30s tool would still
    // be sleeping. This is the assertion an events-only implementation
    // (the old defect) cannot satisfy.
    let deadline = Instant::now() + Duration::from_secs(10);
    let aborted = loop {
        let conn = rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("open core db");
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE event_type = 'run_failed'
                 AND payload_inline LIKE '%\"cancelled\"%'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        drop(conn);
        if count >= 1 {
            break true;
        }
        if Instant::now() > deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(
        aborted,
        "run_failed(cancelled) must be durable within 10s of StopTask — the run itself must abort"
    );
    // The model was invoked exactly once; no repair turn after the stop.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(bodies.lock().unwrap().len(), 1, "no invoke after stop");

    core.kill().ok();
    core.wait().ok();
}

/// PauseTask parks the run at the next turn boundary: the task reaches
/// Waiting and no second model invoke happens.
#[test]
fn pause_task_parks_the_run_at_the_boundary() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let repo = readme_fixture("px");
    let worktrees = tempdir("pw");
    let (model, bodies) = spawn_model_fixture(vec![
        tool_call_turn("c1", "shell.run", r#"{"argv":"sleep 3"}"#),
        text_turn("never reached"),
    ]);
    let (mut core, daemon, db_path) = spawn_core(&repo, &worktrees, model);

    let task_id = start_task(&daemon, "pause mid tool", "Run the sleep.");
    std::thread::sleep(Duration::from_millis(1_000));
    let paused = request(
        &daemon,
        pb::surface_request::Request::PauseTask(pb::PauseTaskCommand {
            task_id: task_id.clone(),
        }),
    );
    assert!(paused.ok, "{}", paused.error);

    wait_for_state(
        &daemon,
        &task_id,
        pb::TaskStatus::Waiting as i32,
        Duration::from_secs(15),
    );
    // The 3s tool finishes; the paused run never invokes turn 2.
    std::thread::sleep(Duration::from_millis(3_500));
    assert_eq!(bodies.lock().unwrap().len(), 1, "parked run makes no invokes");

    core.kill().ok();
    core.wait().ok();
}

/// SteerTask during an in-flight run: the note rides as a user message on
/// the NEXT request the fixture receives.
#[test]
fn steer_task_note_rides_the_next_request() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let repo = readme_fixture("tx");
    let worktrees = tempdir("tw");
    let (model, bodies) = spawn_model_fixture(vec![
        tool_call_turn("c1", "fs.read", r#"{"path":"notes.txt"}"#),
        text_turn("steered"),
    ]);
    let (mut core, daemon, db_path) = spawn_core(&repo, &worktrees, model);

    let task_id = start_task(&daemon, "steer me", "Read the notes.");
    // Steer while turn 1 is in flight: the note must land on request 2.
    let steered = request(
        &daemon,
        pb::surface_request::Request::SteerTask(pb::SteerTaskCommand {
            task_id: task_id.clone(),
            note: "keep the summary under three lines".into(),
        }),
    );
    assert!(steered.ok, "{}", steered.error);

    wait_for_state(
        &daemon,
        &task_id,
        pb::TaskStatus::ReadyForReview as i32,
        Duration::from_secs(60),
    );

    let bodies = bodies.lock().unwrap();
    assert!(bodies.len() >= 2, "fixture captured both turns");
    let messages = bodies[1]["messages"].as_array().expect("messages");
    let steer = messages
        .iter()
        .find(|m| {
            m["role"] == "user"
                && m["content"]
                    .as_str()
                    .is_some_and(|c| c.starts_with("user steer: "))
        })
        .expect("steer note rides as a user message");
    assert!(steer["content"]
        .as_str()
        .unwrap()
        .contains("keep the summary under three lines"));

    core.kill().ok();
    core.wait().ok();
}
