//! Daemon-driven worktree-checkpoint E2E (M4.3, docs/22 § Checkpoint/
//! rollback): the checkpoint crate's DeltaJournal + epoch-fenced store
//! (previously zero production callers) is wired into the run plane.
//! Every successful change.apply through the REAL daemon persists a
//! durable WorktreeCheckpointed run event; the journal's baseline holds
//! the PRE-EDIT bytes and its replay reconstructs the edited state
//! exactly, with strictly increasing fenced epochs.

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use prost::Message;
use reqwest::blocking::Client;

use modbit_checkpoint::delta::DeltaJournal;
use modbit_git::GitRepo;
use modbit_protocol::modbit::protocol::v1 as pb;

static E2E_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn tempdir(tag: &str) -> PathBuf {
    let suffix: String = uuid::Uuid::now_v7().simple().to_string().chars().rev().take(8).collect::<String>().chars().rev().collect();
    let dir = std::env::temp_dir().join(format!("dje{tag}{suffix}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const ORIGINAL: &str = "function greet() {\n  return 'hi';\n}\n";

fn code_fixture(tag: &str) -> PathBuf {
    let root = tempdir(tag);
    let repo = GitRepo::init(&root).expect("init");
    repo.set_config("user.email", "e2e@modbit.test").unwrap();
    repo.set_config("user.name", "Modbit E2E").unwrap();
    repo.set_config("core.autocrlf", "false").unwrap();
    std::fs::write(root.join("greet.js"), ORIGINAL).unwrap();
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

fn spawn_model_fixture(script: Vec<String>) -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for (current_turn, stream) in listener.incoming().flatten().enumerate() {
            let script = script.clone();
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
                let body = script
                    .get(current_turn)
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
    addr
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
            panic!("journal e2e: task did not complete (state {state})");
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Two real change.apply edits through the daemon: the durable store
/// carries WorktreeCheckpointed events with strictly increasing epochs;
/// the latest journal's baseline is the ORIGINAL file and its replay
/// reconstructs the final edited state exactly.
#[test]
fn change_applies_persist_epoch_fenced_worktree_checkpoints() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let repo = code_fixture("ja");
    let worktrees = tempdir("jw");
    let model = spawn_model_fixture(vec![
        tool_call_turn(
            "c1",
            "change.apply",
            r#"{"path":"greet.js","old_text":"return 'hi';","new_text":"return 'hello';"}"#,
        ),
        tool_call_turn(
            "c2",
            "change.apply",
            r#"{"path":"greet.js","old_text":"function greet() {","new_text":"export function greet() {"}"#,
        ),
        text_turn("edited"),
    ]);
    let (mut core, daemon, db_path) = spawn_core(&repo, &worktrees, model);

    let created = request(
        &daemon,
        pb::surface_request::Request::CreateTask(pb::CreateTaskCommand {
            session_id: String::new(),
            title: "edit greet".into(),
            prompt: "Make the edits.".into(),
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

    // Durable proof: fenced checkpoint events, strictly increasing epochs.
    let conn = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("open core db");
    let mut stmt = conn
        .prepare(
            "SELECT json_extract(payload_inline, '$.epoch'),
                    json_extract(payload_inline, '$.journal_json')
             FROM events WHERE event_type = 'worktree_checkpointed'
             ORDER BY sequence",
        )
        .unwrap();
    let rows: Vec<(i64, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(rows.len(), 2, "one checkpoint per successful apply");
    assert_eq!(rows[0].0, 1, "epochs start at 1");
    assert_eq!(rows[1].0, 2, "epochs strictly increase");

    // The latest journal: baseline = ORIGINAL bytes; replay = final state.
    let journal: DeltaJournal = serde_json::from_str(&rows[1].1).expect("journal deserializes");
    assert_eq!(
        journal.baseline.get("greet.js").map(|b| b.as_slice()),
        Some(ORIGINAL.as_bytes()),
        "baseline holds the PRE-EDIT bytes"
    );
    assert_eq!(journal.deltas.len(), 2, "both edits journaled");
    let restored = journal.restore().expect("journal replays");
    let final_content = String::from_utf8(restored.files["greet.js"].clone()).unwrap();
    assert_eq!(
        final_content,
        "export function greet() {\n  return 'hello';\n}\n",
        "replay reconstructs the edited state exactly: {final_content}"
    );

    // The worktree itself carries the same final state (real boundary).
    let worktree = worktrees.join(&task_id);
    let on_disk = std::fs::read_to_string(worktree.join("greet.js")).unwrap();
    assert_eq!(on_disk, final_content);

    core.kill().ok();
    core.wait().ok();
}
