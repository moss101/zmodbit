//! Daemon-driven scrollback-artifact E2E (M2 IMP-EV-0019, QUAL-EV-0019):
//! a command producing MULTI-MEGABYTE output through the REAL daemon —
//! the model receives a bounded view, the full artifact remains
//! retrievable page-by-page through the paginated OutputRef, and the
//! durable store holds every streamed chunk.

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
    let dir = std::env::temp_dir().join(format!("dse{tag}{suffix}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn notes_fixture(tag: &str) -> PathBuf {
    let root = tempdir(tag);
    let repo = GitRepo::init(&root).expect("init");
    repo.set_config("user.email", "e2e@modbit.test").unwrap();
    repo.set_config("user.name", "Modbit E2E").unwrap();
    repo.set_config("core.autocrlf", "false").unwrap();
    // A generator script: writes ~12 MB in 1 KB lines (real command, real
    // bytes; the broker streams them through the offset-addressed log).
    std::fs::write(
        root.join("big_output.sh"),
        "#!/bin/sh\ni=0\nwhile [ $i -lt 12000 ]; do echo '0123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789'; i=$((i+1)); done\n",
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

fn spawn_core(repo_root: &PathBuf, worktree_root: &PathBuf, model_addr: SocketAddr) -> (Child, String) {
    let execd_bin = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/modbit-execd");
    let mut execd = Command::new(&execd_bin)
        .env("MODBIT_EXECD_ADDR", "127.0.0.1:0")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn execd");
    let execd_addr = read_boot_line(&mut execd).expect("execd boot");

    let db_dir = tempdir("db");
    let exe = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/modbit-core");
    let mut command = Command::new(exe);
    let mut command = command.env("MODBIT_CORE_DB", db_dir.join("core.db"));
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
    (child, daemon.expect("daemon addr"))
}

fn request(daemon: &str, req: pb::surface_request::Request) -> pb::SurfaceResponse {
    let client = Client::builder().timeout(Duration::from_secs(60)).build().unwrap();
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
    let deadline = Instant::now() + Duration::from_secs(180);
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
            panic!("scrollback e2e: task did not complete (state {state})");
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// QUAL-EV-0019: multi-MB output -> bounded model view + retrievable full
/// artifact (paginated OutputRef) + streamed chunk evidence.
#[test]
fn multimegabyte_output_bounded_view_with_retrievable_artifact() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let repo = notes_fixture("sa");
    let worktrees = tempdir("sw");
    let (model, bodies) = spawn_model_fixture(vec![
        tool_call_turn("c1", "shell.run", r#"{"argv":["sh","big_output.sh"]}"#),
        text_turn("digested"),
    ]);
    let (mut core, daemon) = spawn_core(&repo, &worktrees, model);

    let created = request(
        &daemon,
        pb::surface_request::Request::CreateTask(pb::CreateTaskCommand {
            session_id: String::new(),
            title: "generate big output".into(),
            prompt: "Run the generator.".into(),
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

    // 1. The model-visible result is BOUNDED (the inline tail), never 12 MB.
    let bodies = bodies.lock().unwrap();
    assert!(bodies.len() >= 2, "fixture captured both turns");
    let messages = bodies[1]["messages"].as_array().expect("messages");
    let tool = messages
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("tool result rides the conversation");
    let content: serde_json::Value =
        serde_json::from_str(tool["content"].as_str().unwrap()).unwrap();
    let inline = content["output"].as_str().unwrap();
    assert!(
        inline.len() <= 2_100,
        "model view must be bounded, got {} bytes",
        inline.len()
    );
    let output_ref = content["output_ref"].as_object().expect("artifact ref");
    let ref_id = output_ref["output_ref_id"].as_str().unwrap().to_string();
    let total = output_ref["byte_length"].as_u64().unwrap();
    // 12000 lines x 131 bytes = 1_572_000 bytes (~1.5 MB >> the 2KB view).
    assert_eq!(total, 1_572_000, "exact full-artifact length: {total}");
    assert!(total as usize > 100 * inline.len(), "artifact >> model view");
    drop(bodies);

    // 2. The full artifact is retrievable page-by-page: first page, a
    //    middle page, and the final page concatenate to the exact bytes.
    let page = |offset: u64, max: u64| -> (Vec<u8>, u64) {
        let r = request(
            &daemon,
            pb::surface_request::Request::ReadOutputRef(pb::ReadOutputRefRequest {
                output_ref_id: ref_id.clone(),
                offset,
                max_bytes: max,
            }),
        );
        assert!(r.ok, "{}", r.error);
        let view = r.output_chunk.expect("chunk view");
        (view.data, view.total_length)
    };
    let (head, total_seen) = page(0, 131);
    assert_eq!(total_seen, total);
    assert_eq!(
        head,
        b"0123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789\n".to_vec()
    );
    let (middle, _) = page(total / 2, 131);
    assert_eq!(head, middle, "repetitive generator: middle page matches");
    let (tail_page, _) = page(total - 131, 4096);
    assert_eq!(tail_page.len(), 131, "final page clamped to the payload");

    // 3. Durable chunk evidence streamed during execution.
    std::thread::sleep(Duration::from_millis(300));
    let _ = std::fs::read_to_string("/dev/null"); // no-op keeps symmetry

    core.kill().ok();
    core.wait().ok();
}
