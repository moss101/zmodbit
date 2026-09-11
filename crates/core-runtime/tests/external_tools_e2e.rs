//! Phase 6 item 3 — external MCP tools E2E: through the REAL daemon with
//! a REAL MCP stdio fixture server (MODBIT_MCP_FIXTURE), the model's
//! external.list call discovers the fixture's tools and external.call
//! round-trips a tool invocation; both ride the durable conversation.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use prost::Message;
use reqwest::blocking::Client;

use modbit_git::GitRepo;
use modbit_protocol::modbit::protocol::v1 as pb;

static E2E_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn tempdir(tag: &str) -> PathBuf {
    let suffix: String = uuid::Uuid::now_v7().simple().to_string().chars().rev().take(8).collect::<String>().chars().rev().collect();
    let dir = std::env::temp_dir().join(format!("mx1{tag}{suffix}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn text_turn(text: &str) -> String {
    let mut out = String::new();
    for f in [
        serde_json::json!({"choices": [{ "delta": { "content": text } }]}),
        serde_json::json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
        serde_json::json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}),
    ] {
        out.push_str(&format!("data: {f}\n\n"));
    }
    out.push_str("data: [DONE]\n\n");
    out
}

fn tool_call_turn(call_id: &str, name: &str, args: &str) -> String {
    let arguments: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
    let mut out = String::new();
    for f in [
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
    ] {
        out.push_str(&format!("data: {f}\n\n"));
    }
    out.push_str("data: [DONE]\n\n");
    out
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

fn wait_state(daemon: &str, task_id: &str, want: i32) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let client = Client::builder().timeout(Duration::from_secs(30)).build().unwrap();
        let body = pb::SurfaceRequest {
            request: Some(pb::surface_request::Request::GetFleet(pb::GetFleetRequest {})),
        }
        .encode_to_vec();
        let response = client
            .post(format!("http://{daemon}/commands"))
            .header("Content-Type", "application/x-protobuf")
            .body(body)
            .send()
            .expect("post");
        let fleet = pb::SurfaceResponse::decode(response.bytes().unwrap().as_ref())
            .unwrap()
            .fleet
            .unwrap();
        if let Some(t) = fleet.tasks.iter().find(|t| t.task_id == task_id) {
            if t.state == want {
                return;
            }
            if Instant::now() > deadline {
                panic!("task {task_id} did not reach {want} (state {})", t.state);
            }
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Through the REAL daemon + REAL MCP stdio fixture server: the model's
/// external.list and external.call results ride the durable conversation.
#[test]
fn external_tools_round_trip_over_the_real_daemon() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let repo = tempdir("repo");
    {
        let r = GitRepo::init(&repo).expect("init");
        r.set_config("user.email", "e2e@modbit.test").unwrap();
        r.set_config("user.name", "Modbit E2E").unwrap();
        r.set_config("core.autocrlf", "false").unwrap();
        std::fs::write(repo.join("note.txt"), "hello\n").unwrap();
        r.commit_all("base").expect("base");
    }

    // Model fixture: the script drives the task.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let model_addr = listener.local_addr().unwrap();
    let script = [
        tool_call_turn("c1", "external.list", "{}"),
        tool_call_turn("c2", "external.call", r#"{"server":"fixture","tool":"echo","arguments":{"text":"modbit"}}"#),
        text_turn("finished"),
    ];
    std::thread::spawn(move || {
        for (turn, stream) in listener.incoming().flatten().enumerate() {
            let body = script.get(turn).cloned().unwrap_or_else(|| text_turn("done"));
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
            use std::io::Write;
            let mut stream = stream;
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            );
            let _ = stream.write_all(body.as_bytes());
        }
    });

    let fixture_bin = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/mcp-fixture-server");
    let _worktrees = tempdir("mxw");
    let db_dir = tempdir("db");
    let db_path = db_dir.join("core.db");

    // Spawn core (shell parts per OS in other e2es; execd too).
    let execd_bin = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/modbit-execd");
    let mut execd = Command::new(&execd_bin)
        .env("MODBIT_EXECD_ADDR", "127.0.0.1:0")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn execd");
    let execd_addr = read_boot_line(&mut execd).expect("execd boot");

    let exe = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/modbit-core");
    let mut child = Command::new(exe)
        .env("MODBIT_CORE_DB", &db_path)
        .env("MODBIT_HTTP_ADDR", "127.0.0.1:0")
        .env("MODBIT_REPO_ROOT", &repo)
        .env("MODBIT_WORKTREE_ROOT", tempdir("wt"))
        .env("MODBIT_EXECD_ADDR", &execd_addr)
        .env("MODBIT_MCP_FIXTURE", &fixture_bin)
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
    let daemon = daemon.expect("daemon addr");

    let client = Client::builder().timeout(Duration::from_secs(30)).build().unwrap();
    let send = |req: pb::surface_request::Request| -> pb::SurfaceResponse {
        let body = pb::SurfaceRequest { request: Some(req) }.encode_to_vec();
        let response = client
            .post(format!("http://{daemon}/commands"))
            .header("Content-Type", "application/x-protobuf")
            .body(body)
            .send()
            .expect("post");
        pb::SurfaceResponse::decode(response.bytes().unwrap().as_ref()).unwrap()
    };

    let created = send(pb::surface_request::Request::CreateTask(pb::CreateTaskCommand {
        session_id: String::new(),
        title: "external tools".into(),
        prompt: "Do it.".into(),
        ..Default::default()
    }));
    assert!(created.ok, "{}", created.error);
    let task_id = created.task.unwrap().task_id;
    for payload in [
        pb::surface_request::Request::QueueTask(pb::QueueTaskCommand { task_id: task_id.clone() }),
        pb::surface_request::Request::StartTask(pb::StartTaskCommand { task_id: task_id.clone() }),
    ] {
        let r = send(payload);
        assert!(r.ok, "{}", r.error);
    }
    wait_state(&daemon, &task_id, pb::TaskStatus::ReadyForReview as i32);

    // Durable conversation carries both external tool results.
    let conn = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let conversation: String = {
        let mut stmt = conn
            .prepare(
                "SELECT json_extract(payload_inline, '$.conversation_json')
                 FROM events WHERE event_type = 'conversation_checkpointed'
                 ORDER BY sequence DESC LIMIT 1",
            )
            .unwrap();
        stmt.query_row([], |row| row.get::<_, String>(0))
            .expect("conversation checkpoint")
    };
    let visible = conversation.replace("\\\"", "\"");
    assert!(
        visible.contains("\"name\":\"echo\"") || visible.contains("echo:"),
        "external.list must surface the fixture tool: {visible}"
    );
    assert!(
        visible.contains("echo: modbit"),
        "external.call result must ride the conversation: {visible}"
    );

    // The core is killed to end the run.
    let _ = child.kill();
    let _ = child.wait();
}
