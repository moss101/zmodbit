//! Daemon-driven conversation-roles E2E (Future-tasks.md Phase 2 item 1,
//! defect §2.1): the REAL `modbit-core` daemon (socket + HTTP), real
//! scheduler, real git worktrees, real execd broker, real `fs.read` effector
//! — with the model endpoint pointed at a scripted local HTTP fixture that
//! CAPTURES every request body. Proves on the wire, for BOTH providers,
//! that the repair-turn request carries proper roles: the compiled prompt
//! as the user turn, the assistant turn WITH the tool calls it issued, and
//! the tool result keyed by the SAME call id — no flattened
//! "tool <name> → …" user strings.

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

// ---- shared harness (mirrors daemon_scripted_e2e, plus body capture) ----

static E2E_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn tempdir(tag: &str) -> PathBuf {
    // Short prefix: unix socket paths must fit sun_path (104 bytes).
    let suffix: String = uuid::Uuid::now_v7().simple().to_string().chars().rev().take(8).collect::<String>().chars().rev().collect();
    let dir = std::env::temp_dir().join(format!("dre{tag}{suffix}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const NOTES: &str = "meeting notes: ship proper message roles\n";

fn notes_fixture(tag: &str) -> PathBuf {
    let root = tempdir(tag);
    let repo = GitRepo::init(&root).expect("init");
    repo.set_config("user.email", "e2e@modbit.test").unwrap();
    repo.set_config("user.name", "Modbit E2E").unwrap();
    repo.set_config("core.autocrlf", "false").unwrap();
    std::fs::write(root.join("notes.txt"), NOTES).unwrap();
    repo.commit_all("fixture baseline").expect("baseline");
    root
}

/// Phase 2.4 (Future-tasks §2.3): the repo's rules files ride the
/// compiled prompt with per-file sha256 provenance — asserted on the
/// FIRST request body the daemon sends to the provider.
#[test]
fn rules_files_ride_the_compiled_prompt_with_provenance() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let repo = notes_fixture("rf");
    std::fs::write(repo.join("AGENTS.md"), "Always run the tests before claiming done.").unwrap();
    std::fs::create_dir_all(repo.join(".cursor/rules")).unwrap();
    std::fs::write(repo.join(".cursor/rules/testing.mdc"), "Prefer node.test for new tests.").unwrap();
    // Rules must be COMMITTED: the run works in a linked worktree checked
    // out at the base revision — untracked files never reach it.
    GitRepo::open(&repo).unwrap().commit_all("rules files").expect("rules commit");
    let worktrees = tempdir("rw");
    let (model, bodies) = spawn_model_fixture(vec![text_turn("noted")]);
    let (mut core, daemon) = spawn_core(&repo, &worktrees, model, "openai");

    let task_id = run_read_task(&daemon);
    wait_ready_for_review(&daemon, &task_id);

    let bodies = bodies.lock().unwrap();
    assert!(!bodies.is_empty(), "fixture captured the first request");
    let messages = bodies[0]["messages"].as_array().expect("messages");
    // The compiled prompt is the user turn right after the system message.
    let user = messages
        .iter()
        .find(|m| m["role"] == "user")
        .expect("user turn carries the compiled prompt");
    let content = user["content"].as_str().unwrap();
    assert!(
        content.contains("Always run the tests before claiming done."),
        "AGENTS.md rides the prompt"
    );
    assert!(
        content.contains("Prefer node.test for new tests."),
        ".cursor/rules/*.mdc rides the prompt"
    );
    assert!(content.contains("sha256:"), "provenance hashes ride the prompt");
    assert!(
        content.contains("# Workspace rules"),
        "rules ride as the workspace_rules segment"
    );

    core.kill().ok();
    core.wait().ok();
}

/// Serves a scripted agent script (one SSE body per connection) and records
/// every incoming request body for wire-shape assertions.
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
                // drain request head + body
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

// ---- OpenAI-compatible scripted turns (chat-completions SSE) -----------

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

fn openai_tool_turn(call_id: &str, name: &str, args: &str) -> String {
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

// ---- Anthropic scripted turns (messages SSE) -----------------------------

fn anthropic_tool_turn(call_id: &str, name: &str, args: &str) -> String {
    let input: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
    sse(
        &[
            serde_json::json!({"type":"message_start","message":{"usage":{"input_tokens":10}}}),
            serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": { "type": "tool_use", "id": call_id, "name": name, "input": input }
            }),
            serde_json::json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}),
        ],
        false,
    )
}

fn anthropic_text_turn(text: &str) -> String {
    sse(
        &[
            serde_json::json!({"type":"message_start","message":{"usage":{"input_tokens":10}}}),
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
            serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}),
        ],
        false,
    )
}

// ---- daemon plumbing -----------------------------------------------------

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

fn spawn_core(
    repo_root: &PathBuf,
    worktree_root: &PathBuf,
    model_addr: SocketAddr,
    provider: &str,
) -> (Child, String) {
    // Real execd broker.
    let execd_bin = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/modbit-execd");
    let mut execd = Command::new(&execd_bin)
        .env("MODBIT_EXECD_ADDR", "127.0.0.1:0")
        .stdout(Stdio::piped())
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
    let key_env = format!("{}_API_KEY", provider.to_uppercase());
    let mut child = command
        .env("MODBIT_HTTP_ADDR", "127.0.0.1:0")
        .env("MODBIT_REPO_ROOT", repo_root)
        .env("MODBIT_WORKTREE_ROOT", worktree_root)
        .env("MODBIT_EXECD_ADDR", &execd_addr)
        .env("MODBIT_BASE_URL", format!("http://{model_addr}"))
        .env("MODBIT_MODEL", "fixture-model")
        .env("MODBIT_PROVIDER", provider)
        .env(key_env, "fixture-key")
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
    std::mem::forget(execd);
    (child, daemon.expect("daemon addr"))
}

/// Phase 2.6: spawn the core with NO MODBIT_EXECD_ADDR — the Core must
/// spawn its own modbit-execd child (every host path gets a broker).
fn spawn_core_without_execd(
    repo_root: &PathBuf,
    worktree_root: &PathBuf,
    model_addr: SocketAddr,
) -> (Child, String) {
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
        .env_remove("MODBIT_EXECD_ADDR")
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
    let mut spawned_execd = false;
    while daemon.is_none() {
        let mut l = String::new();
        match err_reader.read_line(&mut l) {
            Ok(0) => break,
            Ok(_) => {
                eprintln!("[core] {}", l.trim_end());
                if l.contains("spawned modbit-execd on ") {
                    spawned_execd = true;
                }
                if let Some(addr) = l.strip_prefix("modbit-core: http daemon on ").map(str::trim) {
                    daemon = Some(addr.to_string());
                }
            }
            Err(_) => break,
        }
    }
    assert!(
        spawned_execd,
        "the Core must spawn its own execd when MODBIT_EXECD_ADDR is unset"
    );
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

fn run_read_task(daemon: &str) -> String {
    let created = request(
        daemon,
        pb::surface_request::Request::CreateTask(pb::CreateTaskCommand {
            session_id: String::new(),
            title: "read the notes".into(),
            prompt: "Read notes.txt and summarize it.".into(),
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
            panic!("roles e2e: task did not reach ReadyForReview (state {state})");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Shared body-shape check over the SECOND (repair-turn) request: the first
/// provider-agnostic ChatMessage contract holds for both providers — one
/// user turn (the compiled prompt), then assistant+tool-result linkage.
fn repair_bodies(bodies: &Arc<Mutex<Vec<serde_json::Value>>>) -> Vec<serde_json::Value> {
    let bodies = bodies.lock().unwrap();
    assert!(bodies.len() >= 2, "fixture must capture tool turn + repair turn");
    bodies.clone()
}

// ---- OpenAI wire shape ----------------------------------------------------

#[test]
fn openai_wire_body_carries_typed_roles() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let repo = notes_fixture("oa");
    let worktrees = tempdir("wo");
    let (model, bodies) = spawn_model_fixture(vec![
        openai_tool_turn("c1", "fs.read", r#"{"path":"notes.txt"}"#),
        text_turn("noted"),
    ]);
    let (mut core, daemon) = spawn_core(&repo, &worktrees, model, "openai");

    let task_id = run_read_task(&daemon);
    wait_ready_for_review(&daemon, &task_id);

    let bodies = repair_bodies(&bodies);
    let messages = bodies[1]["messages"].as_array().expect("messages array");
    let roles: Vec<&str> = messages.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(
        roles,
        vec!["system", "user", "assistant", "tool"],
        "exact role sequence on the repair-turn request"
    );
    // The user turn is the compiled prompt, not a flattened tool log.
    let user = &messages[1];
    assert!(user["content"].as_str().unwrap().contains("objective:"));
    // The assistant turn carries the issued call with its id.
    let assistant = &messages[2];
    let calls = assistant["tool_calls"].as_array().expect("assistant tool_calls");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["id"], "c1");
    assert_eq!(calls[0]["type"], "function");
    assert_eq!(calls[0]["function"]["name"], "fs.read");
    let args: serde_json::Value =
        serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args, serde_json::json!({"path": "notes.txt"}));
    // The tool result answers the SAME call id and carries the real file.
    let tool = &messages[3];
    assert_eq!(tool["tool_call_id"], "c1");
    let content: serde_json::Value = serde_json::from_str(tool["content"].as_str().unwrap()).unwrap();
    assert!(content["content"].as_str().unwrap().contains("ship proper message roles"));
    // The old flattened format is gone from every message.
    assert!(messages
        .iter()
        .all(|m| !(m["role"] == "user" && m["content"].as_str().unwrap_or("").starts_with("tool "))));

    core.kill().ok();
    core.wait().ok();
}

// ---- Anthropic wire shape ---------------------------------------------------

#[test]
fn anthropic_wire_body_carries_typed_roles() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let repo = notes_fixture("an");
    let worktrees = tempdir("wn");
    let (model, bodies) = spawn_model_fixture(vec![
        anthropic_tool_turn("toolu-1", "fs.read", r#"{"path":"notes.txt"}"#),
        anthropic_text_turn("noted"),
    ]);
    let (mut core, daemon) = spawn_core(&repo, &worktrees, model, "anthropic");

    let task_id = run_read_task(&daemon);
    wait_ready_for_review(&daemon, &task_id);

    let bodies = repair_bodies(&bodies);
    // System prompt is top-level on the Anthropic wire.
    assert!(!bodies[1]["system"].as_str().unwrap_or("").is_empty());
    let messages = bodies[1]["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 3, "user + assistant(tool_use) + user(tool_result)");
    // Anthropic has no system/tool roles in messages.
    assert!(messages.iter().all(|m| m["role"] != "system" && m["role"] != "tool"));
    // The user turn is the compiled prompt.
    assert!(messages[0]["content"].as_str().unwrap().contains("objective:"));
    // The assistant turn serializes as a tool_use content block with the id.
    let assistant = &messages[1];
    assert_eq!(assistant["role"], "assistant");
    let blocks = assistant["content"].as_array().expect("assistant content blocks");
    assert_eq!(blocks.len(), 1, "no text before the tool_use in this turn");
    assert_eq!(blocks[0]["type"], "tool_use");
    assert_eq!(blocks[0]["id"], "toolu-1");
    assert_eq!(blocks[0]["name"], "fs.read");
    assert_eq!(blocks[0]["input"], serde_json::json!({"path": "notes.txt"}));
    // The tool result rides as a tool_result block in a user message,
    // keyed by tool_use_id, and carries the real file content.
    let result = &messages[2];
    assert_eq!(result["role"], "user");
    let rblocks = result["content"].as_array().expect("tool_result blocks");
    assert_eq!(rblocks[0]["type"], "tool_result");
    assert_eq!(rblocks[0]["tool_use_id"], "toolu-1");
    assert!(rblocks[0].get("is_error").is_none(), "successful read is not an error");
    let content: serde_json::Value =
        serde_json::from_str(rblocks[0]["content"].as_str().unwrap()).unwrap();
    assert!(content["content"].as_str().unwrap().contains("ship proper message roles"));

    core.kill().ok();
    core.wait().ok();
}

/// Phase 2.6 (Future-tasks §2.4): with no broker address provided, the
/// Core spawns modbit-execd itself — a real shell.run works end to end
/// (the string argv form with quotes rides the same turn).
#[test]
fn core_spawns_its_own_execd_when_none_is_configured() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let repo = notes_fixture("ee");
    let worktrees = tempdir("ew");
    let (model, _bodies) = spawn_model_fixture(vec![
        openai_tool_turn("c1", "shell.run", r#"{"argv":["sh","-c","echo broker-ok"]}"#),
        text_turn("done"),
    ]);
    let (mut core, daemon) = spawn_core_without_execd(&repo, &worktrees, model);

    let created = request(
        &daemon,
        pb::surface_request::Request::CreateTask(pb::CreateTaskCommand {
            session_id: String::new(),
            title: "shell without exported broker".into(),
            prompt: "Run the echo command.".into(),
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

    core.kill().ok();
    core.wait().ok();
}
