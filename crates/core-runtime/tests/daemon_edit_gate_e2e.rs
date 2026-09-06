//! Daemon-driven edit-gate E2E (M2 IMP-EV-0015, QUAL-EV-0015): an
//! AMBIGUOUS edit target (old_text occurring more than once) must fail
//! through the production change.apply tool, leave the worktree
//! byte-unchanged, and produce no checkpoint journal event — the agent
//! then reads the file and sees the original content.

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
    let dir = std::env::temp_dir().join(format!("deg{tag}{suffix}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const ORIGINAL: &str = "function step() {\n  return 'next';\n}\nfunction step() {\n  return 'next';\n}\n";

fn code_fixture(tag: &str) -> PathBuf {
    let root = tempdir(tag);
    let repo = GitRepo::init(&root).expect("init");
    repo.set_config("user.email", "e2e@modbit.test").unwrap();
    repo.set_config("user.name", "Modbit E2E").unwrap();
    repo.set_config("core.autocrlf", "false").unwrap();
    // old_text "function step() {" appears TWICE: ambiguous by construction.
    std::fs::write(root.join("steps.js"), ORIGINAL).unwrap();
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

fn wait_ready_for_review(daemon: &str, task_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(120);
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
            panic!("edit-gate e2e: task did not complete (state {state})");
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// QUAL-EV-0015: the ambiguous edit FAILS (occurrences reported), the
/// worktree stays byte-unchanged, the agent reads the original content,
/// and NO worktree checkpoint was journaled (a failed edit is not an
/// edit).
#[test]
fn ambiguous_edit_fails_and_leaves_worktree_unchanged() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let repo = code_fixture("ga");
    let worktrees = tempdir("gw");
    let (model, bodies) = spawn_model_fixture(vec![
        // The ambiguous edit: old_text occurs twice -> edit gate rejects.
        tool_call_turn(
            "c1",
            "change.apply",
            r#"{"path":"steps.js","old_text":"function step() {","new_text":"export function step() {"}"#,
        ),
        // The agent verifies the file is untouched.
        tool_call_turn("c2", "fs.read", r#"{"path":"steps.js"}"#),
        text_turn("gate held"),
    ]);
    let (mut core, daemon, db_path) = spawn_core(&repo, &worktrees, model);

    let created = request(
        &daemon,
        pb::surface_request::Request::CreateTask(pb::CreateTaskCommand {
            session_id: String::new(),
            title: "ambiguous edit".into(),
            prompt: "Try the edit.".into(),
        }),
    );
    assert!(created.ok, "{}", created.error);
    let task_id = created.task.unwrap().task_id;
    for payload in [
        pb::surface_request::Request::QueueTask(pb::QueueTaskCommand { task_id: task_id.clone() }),
        pb::surface_request::Request::StartTask(pb::StartTaskCommand { task_id: task_id.clone() }),
    ] {
        let r = request(&daemon, payload);
        assert!(r.ok, "{}", r.error);
    }
    wait_ready_for_review(&daemon, &task_id);

    // 1. The tool result reported the gate rejection with the count.
    let bodies = bodies.lock().unwrap();
    assert!(bodies.len() >= 2);
    let messages = bodies[1]["messages"].as_array().expect("messages");
    let tool = messages
        .iter()
        .find(|m| m["role"] == "tool" && m["tool_call_id"] == "c1")
        .expect("the ambiguous edit's result");
    let result: serde_json::Value = serde_json::from_str(tool["content"].as_str().unwrap()).unwrap();
    assert_eq!(result["ok"], false, "the gate rejected the edit: {result}");
    assert_eq!(result["occurrences"], 2, "both occurrences counted");

    // 2. The agent's follow-up read (request 3 carries c2's result) saw
    //    the ORIGINAL bytes.
    let messages3 = bodies[2]["messages"].as_array().expect("messages");
    let read = messages3
        .iter()
        .find(|m| m["role"] == "tool" && m["tool_call_id"] == "c2")
        .expect("follow-up read");
    let read_result: serde_json::Value =
        serde_json::from_str(read["content"].as_str().unwrap()).unwrap();
    assert_eq!(
        read_result["content"], ORIGINAL,
        "worktree unchanged after the rejected edit"
    );

    // 3. The durable store holds NO checkpoint for a failed edit.
    drop(bodies);
    let conn = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("open core db");
    let checkpoints: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE event_type = 'worktree_checkpointed'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(checkpoints, 0, "a rejected edit is not journaled");
    drop(conn);

    // 4. The worktree file on disk is byte-identical to the baseline.
    let on_disk = std::fs::read_to_string(worktrees.join(&task_id).join("steps.js")).unwrap();
    assert_eq!(on_disk, ORIGINAL);

    core.kill().ok();
    core.wait().ok();
}
