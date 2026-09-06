//! Scripted-model daemon E2E (M2 machinery proof; runs ALWAYS, no
//! credentials): the real `modbit-core` binary (socket + HTTP daemon), the
//! real scheduler, real git worktrees, real execd broker, real tool
//! effectors — with the model endpoint pointed at a scripted local HTTP
//! fixture. This proves the full M2 loop machinery on every push; the
//! live-model proof (`daemon_live_e2e`) runs the same scenarios against a
//! real provider when credentials exist.
//!
//! E2E-001: task → worktree → read → fix → run tests (real node) →
//! ReadyForReview with a real diff.
//! E2E-002: first test command fails for real; the agent repairs; the task
//! must NOT fail from the first exit code.

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use prost::Message;
use reqwest::blocking::Client;

use modbit_git::GitRepo;
use modbit_protocol::modbit::protocol::v1 as pb;

// ---- shared harness (mirrors daemon_live_e2e) --------------------------------

/// These tests mutate process env (MODBIT_EXECD_ADDR) and leak execd
/// brokers by design; run them serialized within the binary.
static E2E_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn tempdir(tag: &str) -> PathBuf {
    // Short prefix: unix socket paths must fit sun_path (104 bytes).
    // UUIDv7 leads with its TIMESTAMP: take the random tail instead, or
    // dirs created in the same millisecond window collide across runs.
    let suffix: String = uuid::Uuid::now_v7().simple().to_string().chars().rev().take(8).collect::<String>().chars().rev().collect();
    let dir = std::env::temp_dir().join(format!("mse{tag}{suffix}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const BROKEN: &str = "function validateQuantity(q) {\n  return true;\n}\nmodule.exports = { validateQuantity };\n";

fn ts_webapp_fixture(tag: &str) -> PathBuf {
    let root = tempdir(tag);
    let repo = GitRepo::init(&root).expect("init");
    repo.set_config("user.email", "e2e@modbit.test").unwrap();
    repo.set_config("user.name", "Modbit E2E").unwrap();
    // Byte-exact worktrees: the runner's global autocrlf must not rewrite
    // fixture files on checkout (edit-gate old_text is LF-exact).
    repo.set_config("core.autocrlf", "false").unwrap();
    std::fs::write(root.join("quantity.js"), BROKEN).unwrap();
    std::fs::write(
        root.join("quantity.test.js"),
        "const { validateQuantity } = require('./quantity');\n\
         const t = require('node:test');\n\
         const assert = require('node:assert');\n\
         t.test('rejects negative quantities', () => {\n\
         \x20 assert.throws(() => validateQuantity(-5), /negative/);\n\
         });\n",
    )
    .unwrap();
    std::fs::write(root.join("package.json"), "{\n  \"name\": \"ts-webapp-fixture\",\n  \"version\": \"1.0.0\"\n}\n").unwrap();
    std::fs::write(root.join("run_tests.sh"), "#!/bin/sh\nnode --test quantity.test.js\n").unwrap();
    repo.commit_all("fixture baseline").expect("baseline");
    root
}

/// Scripted model turns: OpenAI-compatible SSE with complete tool calls.
/// Bodies are assembled with serde_json to keep JSON valid by construction.
/// change.apply arguments for the fixture fix (real newlines via json!).
fn fix_args() -> String {
    serde_json::json!({
        "path": "quantity.js",
        "old_text": "function validateQuantity(q) {\n  return true;\n}",
        "new_text": "function validateQuantity(q) {\n  if (q < 0) throw new Error('negative');\n  return true;\n}"
    })
    .to_string()
}

fn tool_call_turn(call_id: &str, name: &str, args: &str) -> String {
    let arguments: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
    let frame = serde_json::json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": call_id,
                    "function": { "name": name, "arguments": arguments.to_string() },
                }]
            }
        }]
    });
    let finish = r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#;
    let usage = r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#;
    [
        frame.to_string(),
        finish.to_string(),
        usage.to_string(),
        "[DONE]".into(),
    ]
    .iter()
    .map(|f| format!("data: {f}\n\n"))
    .collect()
}

fn text_turn(text: &str) -> String {
    let frame = serde_json::json!({
        "choices": [{ "delta": { "content": text } }]
    });
    let finish = r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#;
    let usage = r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#;
    [
        frame.to_string(),
        finish.to_string(),
        usage.to_string(),
        "[DONE]".into(),
    ]
    .iter()
    .map(|f| format!("data: {f}\n\n"))
    .collect()
}

/// Serves the scripted agent script (one SSE body per connection).
fn spawn_model_fixture(script: Vec<String>) -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let turn = Arc::new(AtomicUsize::new(0));
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let script = script.clone();
            let turn = turn.clone();
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
                let index = turn.fetch_add(1, Ordering::SeqCst);
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

fn spawn_core(repo_root: &PathBuf, worktree_root: &PathBuf, model_addr: SocketAddr) -> (Child, String) {
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
        // Unix socket path must fit sun_path; Windows uses named pipes and
        // MODBIT_SOCKET must stay unset there (ephemeral endpoint).
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
    // Keep draining stdout so the pipe never breaks the core's writes.
    std::thread::spawn(move || {
        for _ in reader.lines() {}
    });

    let stderr = child.stderr.take().unwrap();
    let mut err_reader = BufReader::new(stderr);
    let mut daemon = None;
    // Blocking reads: the daemon line is core's first stderr output; EOF
    // means the core died (diagnostics follow below).
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
    // execd must outlive core; leak it into a detached guard
    std::mem::forget(execd);
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

fn wait_for_terminal(daemon: &str, task_id: &str, timeout: Duration) -> i32 {
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
        if matches!(
            state,
            s if s == pb::TaskStatus::ReadyForReview as i32
                || s == pb::TaskStatus::Failed as i32
                || s == pb::TaskStatus::Cancelled as i32
        ) {
            return state;
        }
        if Instant::now() > deadline {
            panic!("no terminal state within {timeout:?} (last {state})");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn start_task(daemon: &str, prompt: &str) -> String {
    let created = request(
        daemon,
        pb::surface_request::Request::CreateTask(pb::CreateTaskCommand {
            session_id: String::new(),
            title: "reject negative quantities".into(),
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

// ---- E2E-001 (scripted model) -------------------------------------------------

#[test]
fn e2e_001_full_loop_read_fix_test_review() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let repo = ts_webapp_fixture("a");
    let worktrees = tempdir("wa");
    let model = spawn_model_fixture(vec![
        tool_call_turn("c1", "fs.read", r#"{"path":"quantity.js"}"#),
        tool_call_turn("c2", "change.apply", &fix_args()),
        tool_call_turn("c3", "shell.run", r#"{"argv":"sh run_tests.sh"}"#),
        text_turn("validation added and tests pass"),
    ]);
    let (mut core, daemon) = spawn_core(&repo, &worktrees, model);

    let task_id = start_task(&daemon, "Add validation so negative quantities are rejected and add tests.");
    let state = wait_for_terminal(&daemon, &task_id, Duration::from_secs(120));
    assert_eq!(state, pb::TaskStatus::ReadyForReview as i32, "E2E-001 scripted");

    // Worktree exists; the diff shows the real fix; node really ran.
    let worktree = worktrees.join(&task_id);
    assert!(worktree.join("quantity.js").exists());
    let fixed = std::fs::read_to_string(worktree.join("quantity.js")).unwrap();
    assert!(fixed.contains("negative"), "fix applied in the worktree: {fixed}");
    let diff = request(
        &daemon,
        pb::surface_request::Request::GetDiff(pb::GetDiffRequest { task_id: task_id.clone() }),
    );
    assert!(diff.ok, "{}", diff.error);
    assert!(
        diff.diff.unwrap().files.iter().any(|f| f.path == "quantity.js"),
        "diff bound to the revision"
    );

    // The durable run plane carries model+tool steps with clean integrity.
    let detail = request(
        &daemon,
        pb::surface_request::Request::GetRunDetail(pb::GetRunDetailRequest { task_id }),
    );
    assert!(detail.ok, "{}", detail.error);
    let detail = detail.run_detail.unwrap();
    assert_eq!(detail.run_state, "completed");
    assert!(detail.turns.iter().flat_map(|t| t.steps.iter()).count() >= 4,
        "model-invoke + fs.read + change.apply + shell.run steps recorded");

    core.kill().ok();
    core.wait().ok();
}

// ---- E2E-002 (scripted model) -------------------------------------------------

#[test]
fn e2e_002_first_command_failure_repairs_without_task_failure() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let repo = ts_webapp_fixture("b");
    let worktrees = tempdir("wb");
    // Script: run tests FIRST (fails on the broken fixture — a real
    // non-zero exit through execd), then fix, then re-run (passes).
    let model = spawn_model_fixture(vec![
        // Phase 2.6: the JSON-array argv form (exact argv, no splitting).
        tool_call_turn("c1", "shell.run", r#"{"argv":["sh","run_tests.sh"]}"#),
        tool_call_turn("c2", "change.apply", &fix_args()),
        tool_call_turn("c3", "shell.run", r#"{"argv":"sh run_tests.sh"}"#),
        text_turn("repaired"),
    ]);
    let (mut core, daemon) = spawn_core(&repo, &worktrees, model);

    let task_id = start_task(&daemon, "Add validation so negative quantities are rejected and add tests.");
    let state = wait_for_terminal(&daemon, &task_id, Duration::from_secs(120));
    assert_ne!(state, pb::TaskStatus::Failed as i32, "E2E-002: no TaskFailed from a first failing command");
    assert_eq!(state, pb::TaskStatus::ReadyForReview as i32);

    // The failing first command is durable evidence, and the repair turn ran.
    let detail = request(
        &daemon,
        pb::surface_request::Request::GetRunDetail(pb::GetRunDetailRequest { task_id }),
    );
    let detail = detail.run_detail.unwrap();
    let shell_steps: Vec<_> = detail
        .turns
        .iter()
        .flat_map(|t| t.steps.iter())
        .filter(|s| s.step_type == "tool_call")
        .collect();
    assert!(shell_steps.len() >= 2, "failing command + repair rerun recorded");

    core.kill().ok();
    core.wait().ok();
}
