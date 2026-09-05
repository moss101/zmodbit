//! Daemon-driven crash/resume E2E (Future-tasks.md Phase 2 item 5, M4
//! recovery spine): SIGKILL the REAL `modbit-core` daemon mid-run, restart
//! it on the same durable store, and prove the run RESUMES from the last
//! committed conversation checkpoint — the restored conversation (with
//! its typed roles) and the interruption note travel on the first request
//! after restart, and the task completes without re-running completed
//! turns from scratch.
//!
//! Kill points (docs/54): mid-tool in turn 2 (checkpoint from turn 1
//! exists) and mid-tool in turn 1 (no checkpoint — fresh attempt with the
//! interruption note surfacing possibly-partial effects).

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
    let dir = std::env::temp_dir().join(format!("dre{tag}{suffix}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn notes_fixture(tag: &str) -> PathBuf {
    let root = tempdir(tag);
    let repo = GitRepo::init(&root).expect("init");
    repo.set_config("user.email", "e2e@modbit.test").unwrap();
    repo.set_config("user.name", "Modbit E2E").unwrap();
    repo.set_config("core.autocrlf", "false").unwrap();
    std::fs::write(root.join("notes.txt"), "resume fixture content\n").unwrap();
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

fn spawn_execd() -> String {
    let execd_bin = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/modbit-execd");
    let mut execd = Command::new(&execd_bin)
        .env("MODBIT_EXECD_ADDR", "127.0.0.1:0")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn execd");
    read_boot_line(&mut execd).expect("execd boot")
}

/// Spawns the core; `db_path` is stable across the kill/restart so the
/// durable store (and the checkpoints in it) survive.
fn spawn_core_on_db(
    db_path: &PathBuf,
    repo_root: &PathBuf,
    worktree_root: &PathBuf,
    model_addr: SocketAddr,
    execd_addr: &str,
) -> (Child, String) {
    let exe = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/modbit-core");
    let mut command = Command::new(exe);
    let mut command = command.env("MODBIT_CORE_DB", db_path);
    if !cfg!(windows) {
        command = command.env("MODBIT_SOCKET", db_path.with_extension("sock"));
    }
    let mut child = command
        .env("MODBIT_HTTP_ADDR", "127.0.0.1:0")
        .env("MODBIT_REPO_ROOT", repo_root)
        .env("MODBIT_WORKTREE_ROOT", worktree_root)
        .env("MODBIT_EXECD_ADDR", execd_addr)
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
                eprintln!("[core] {}", l.trim_end());
                if let Some(addr) = l.strip_prefix("modbit-core: http daemon on ").map(str::trim) {
                    daemon = Some(addr.to_string());
                }
            }
            Err(_) => break,
        }
    }
    std::thread::spawn(move || {
        for line in err_reader.lines().map_while(Result::ok) {
            eprintln!("[core] {line}");
        }
    });
    (child, daemon.expect("daemon addr"))
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

fn start_task(daemon: &str) -> String {
    let created = request(
        daemon,
        pb::surface_request::Request::CreateTask(pb::CreateTaskCommand {
            session_id: String::new(),
            title: "resume me".into(),
            prompt: "Read notes.txt and then run the slow step.".into(),
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

fn wait_ready_for_review(daemon: &str, task_id: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let fleet = request(daemon, pb::surface_request::Request::GetFleet(pb::GetFleetRequest {}))
            .fleet
            .unwrap();
        let state = fleet
            .tasks
            .iter()
            .find(|t| t.task_id == task_id)
            .map(|t| t.state)
            .unwrap_or(-1);
        if state == pb::TaskStatus::ReadyForReview as i32 {
            return;
        }
        if state == pb::TaskStatus::Failed as i32 || Instant::now() > deadline {
            panic!("resume e2e: task did not complete after restart (state {state})");
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

fn wait_for_requests(bodies: &Arc<Mutex<Vec<serde_json::Value>>>, count: usize, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if bodies.lock().unwrap().len() >= count {
            return;
        }
        if Instant::now() > deadline {
            panic!("fixture did not see {count} requests");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Kill point 1 (docs/54): Core killed while turn 2's tool runs, with a
/// committed checkpoint from turn 1. Restart resumes from the checkpoint:
/// the first post-restart request carries the restored typed conversation
/// AND the interruption note; the task then completes.
#[test]
fn core_kill_mid_run_resumes_from_checkpoint() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let repo = notes_fixture("ka");
    let worktrees = tempdir("kw");
    let (model, bodies) = spawn_model_fixture(vec![
        tool_call_turn("c1", "fs.read", r#"{"path":"notes.txt"}"#),
        tool_call_turn("c2", "shell.run", r#"{"argv":"sleep 10"}"#),
        text_turn("resumed and done"),
    ]);
    let db_dir = tempdir("kdb");
    let db_path = db_dir.join("core.db");
    let execd_addr = spawn_execd();
    let (mut core, daemon) = spawn_core_on_db(&db_path, &repo, &worktrees, model, &execd_addr);

    let task_id = start_task(&daemon);
    // Turn 1 (fs.read) completes and checkpoints; turn 2's sleep runs.
    wait_for_requests(&bodies, 2, Duration::from_secs(30));
    std::thread::sleep(Duration::from_millis(500));

    // KILL (docs/54: crash while a real tool call is in flight).
    core.kill().expect("SIGKILL core");
    core.wait().ok();

    // Restart on the SAME durable store; the boot scan resumes the task.
    let execd_addr2 = spawn_execd();
    let (mut core2, daemon2) = spawn_core_on_db(&db_path, &repo, &worktrees, model, &execd_addr2);
    wait_ready_for_review(&daemon2, &task_id, Duration::from_secs(60));

    // The post-restart request carried the CHECKPOINTED conversation:
    // the turn-1 assistant tool call and its tool result (typed roles),
    // plus the interruption note — no re-run from a bare prompt.
    {
    let bodies = bodies.lock().unwrap();
    assert!(bodies.len() >= 3, "fixture saw the resume request");
    let resume_body = &bodies[bodies.len() - 1];
    let messages = resume_body["messages"].as_array().expect("messages");
    let roles: Vec<&str> = messages.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert!(
        roles.contains(&"assistant") && roles.contains(&"tool"),
        "checkpointed conversation restored: {roles:?}"
    );
    let tool = messages
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("checkpointed tool result rides the resume request");
    assert_eq!(tool["tool_call_id"], "c1", "call-id linkage restored");
    assert!(
        tool["content"].as_str().unwrap().contains("resume fixture content"),
        "the checkpointed RESULT (not a re-read stub) rides the request"
    );
    assert!(
        messages
            .iter()
            .any(|m| m["role"] == "user"
                && m["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("previous run attempt was interrupted"))),
        "interruption note surfaces possibly-partial effects"
    );

    } // checkpoint proof block
    // The durable store carries the checkpoint events (>= 1).
    let conn = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("open core db");
    let checkpoints: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE event_type = 'conversation_checkpointed'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(checkpoints >= 1, "checkpoints are durable");

    core2.kill().ok();
    core2.wait().ok();
}

/// Kill point 2 (docs/54): Core killed during the FIRST turn's tool — no
/// checkpoint exists. Restart starts a fresh attempt whose prompt carries
/// the interruption note (unknown-outcome guidance), and the task
/// completes.
#[test]
fn core_kill_first_tool_resumes_with_note() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let repo = notes_fixture("fb");
    let worktrees = tempdir("fw");
    let (model, bodies) = spawn_model_fixture(vec![
        tool_call_turn("c1", "shell.run", r#"{"argv":"sleep 10"}"#),
        text_turn("fresh attempt done"),
    ]);
    let db_dir = tempdir("fdb");
    let db_path = db_dir.join("core.db");
    let execd_addr = spawn_execd();
    let (mut core, daemon) = spawn_core_on_db(&db_path, &repo, &worktrees, model, &execd_addr);

    let task_id = start_task(&daemon);
    wait_for_requests(&bodies, 1, Duration::from_secs(30));
    std::thread::sleep(Duration::from_millis(500));

    core.kill().expect("SIGKILL core");
    core.wait().ok();

    let execd_addr2 = spawn_execd();
    let (mut core2, daemon2) = spawn_core_on_db(&db_path, &repo, &worktrees, model, &execd_addr2);
    wait_ready_for_review(&daemon2, &task_id, Duration::from_secs(60));

    // Fresh attempt: the post-restart request has no checkpointed history
    // but DOES carry the interruption note.
    let bodies = bodies.lock().unwrap();
    let resume_body = &bodies[bodies.len() - 1];
    let messages = resume_body["messages"].as_array().expect("messages");
    assert!(
        messages
            .iter()
            .any(|m| m["role"] == "user"
                && m["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("previous run attempt was interrupted"))),
        "interruption note rides the fresh attempt"
    );

    core2.kill().ok();
    core2.wait().ok();
}
