//! Daemon-driven tool-output streaming E2E (Future-tasks.md Phase 2 item
//! 6, second half): the REAL `modbit-core` daemon, real scheduler, real
//! git worktree, real execd broker running a REAL command whose output
//! arrives in bursts. Proves through production routing that shell.run
//! emits output chunks as durable run events DURING execution and stores
//! the full output behind a paginated OutputRef (the previously-unwired
//! runtime table), readable range-by-range over the surface protocol.

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
    let dir = std::env::temp_dir().join(format!("doe{tag}{suffix}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn notes_fixture(tag: &str) -> PathBuf {
    let root = tempdir(tag);
    let repo = GitRepo::init(&root).expect("init");
    repo.set_config("user.email", "e2e@modbit.test").unwrap();
    repo.set_config("user.name", "Modbit E2E").unwrap();
    repo.set_config("core.autocrlf", "false").unwrap();
    std::fs::write(root.join("notes.txt"), "streaming fixture\n").unwrap();
    // A script file (not `sh -c "..."`): multi-word -c arguments do not
    // survive every broker argv encoding (observed on windows CI); the
    // script keeps argv exact on every platform.
    std::fs::write(
        root.join("stream.sh"),
        "#!/bin/sh\necho first-burst\nsleep 1\necho second-burst\n",
    )
    .unwrap();
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
            panic!("output e2e: task did not complete (state {state})");
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// The full proof: chunk events stream during a REAL bursty command, the
/// full output lands behind a paginated OutputRef in the runtime table,
/// the tool result carries the reference, and the surface RPC pages
/// through the exact bytes.
#[test]
fn shell_output_streams_and_pages_through_output_refs() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let repo = notes_fixture("so");
    let worktrees = tempdir("sw");
    // Two bursts separated by a sleep: the 25ms drain loop emits a chunk
    // per burst (>= 2 chunk events) instead of one lump at completion.
    let (model, bodies) = spawn_model_fixture(vec![
        // Windows: sh scripts exit 1 with no output on the runners; the
        // platform-native bursty command is ping (one line per second —
        // real multi-chunk output through the same production path).
        tool_call_turn(
            "c1",
            "shell.run",
            if cfg!(windows) {
                r#"{"argv":["ping","-n","3","127.0.0.1"]}"#
            } else {
                r#"{"argv":["sh","stream.sh"]}"#
            },
        ),
        text_turn("done"),
    ]);
    let (mut core, daemon, db_path) = spawn_core(&repo, &worktrees, model);

    let created = request(
        &daemon,
        pb::surface_request::Request::CreateTask(pb::CreateTaskCommand {
            session_id: String::new(),
            title: "streaming shell output".into(),
            prompt: "Run the bursty command.".into(),
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

    // 1. The tool result that rode the model conversation carries the
    //    paginated OutputRef with the exact total byte length.
    let bodies = bodies.lock().unwrap();
    assert!(bodies.len() >= 2, "fixture captured both turns");
    let messages = bodies[1]["messages"].as_array().expect("messages");
    let tool = messages
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("tool result rides the conversation");
    let content: serde_json::Value =
        serde_json::from_str(tool["content"].as_str().unwrap()).expect("tool result JSON");
    let output_ref = content["output_ref"].as_object().expect("output_ref rides the result");
    let ref_id = output_ref["output_ref_id"].as_str().expect("ref id").to_string();
    assert!(ref_id.starts_with("outref-"), "content-addressed id: {ref_id}");
    let total = output_ref["byte_length"].as_u64().expect("byte length");
    if !cfg!(windows) {
        assert_eq!(
            total, 25,
            "exact full-output length; tool result was: {content}"
        );
    } else {
        assert!(total > 0, "ping produced output; tool result was: {content}");
    }
    drop(bodies);

    // 2. Chunk events streamed DURING execution (two bursts -> >= 2 events
    //    with bounded previews carrying the burst text).
    let conn = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("open core db");
    let (chunk_count, previews): (i64, String) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(GROUP_CONCAT(json_extract(payload_inline, '$.preview')), '')
             FROM events WHERE event_type = 'tool_output_chunk'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(chunk_count >= 2, "chunks streamed during execution: {chunk_count}");
    assert!(previews.contains("first-burst"), "previews: {previews}");
    assert!(previews.contains("second-burst"), "previews: {previews}");

    // 3. The runtime table (previously unwired) holds the FULL payload.
    let (payload_len, payload): (i64, Vec<u8>) = conn
        .query_row(
            "SELECT byte_length, payload FROM output_refs WHERE output_ref_id = ?1",
            [&ref_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("output_refs row exists");
    assert_eq!(payload_len as u64, total, "stored length matches the ref");
    if cfg!(windows) {
        let text = String::from_utf8_lossy(&payload);
        assert!(text.contains("127.0.0.1"), "real ping output: {text}");
    } else {
        assert_eq!(payload, b"first-burst\nsecond-burst\n");
    }
    assert_eq!(payload, b"first-burst\nsecond-burst\n");
    drop(conn);

    // 4. The surface RPC pages through the exact bytes: two ranges
    //    concatenate to the full output.
    let page1 = request(
        &daemon,
        pb::surface_request::Request::ReadOutputRef(pb::ReadOutputRefRequest {
            output_ref_id: ref_id.clone(),
            offset: 0,
            max_bytes: 11,
        }),
    );
    assert!(page1.ok, "{}", page1.error);
    let page1 = page1.output_chunk.expect("chunk view");
    assert_eq!(page1.offset, 0);
    assert_eq!(page1.total_length as u64, total);
    assert_eq!(page1.data, payload[..11].to_vec());
    let page2 = request(
        &daemon,
        pb::surface_request::Request::ReadOutputRef(pb::ReadOutputRefRequest {
            output_ref_id: ref_id,
            offset: 11,
            max_bytes: 1024,
        }),
    );
    assert!(page2.ok, "{}", page2.error);
    let page2 = page2.output_chunk.expect("chunk view");
    assert_eq!(page2.data, payload[11..].to_vec(), "range resumes at the offset");

    core.kill().ok();
    core.wait().ok();
}
