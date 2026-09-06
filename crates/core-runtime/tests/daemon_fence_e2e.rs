//! Daemon-driven lease-fencing E2E (M4.4, Phase 2 exit work): TWO real
//! `modbit-core` daemons open the SAME durable store. The second core's
//! run for the same session acquires the session lease — the first
//! core's in-flight run is fenced out: its next run-plane append loses
//! with a typed StaleLease, its cancellation path aborts the run, and it
//! writes NOTHING further (the events table proves exactly one writer
//! reached a terminal state).

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
    let dir = std::env::temp_dir().join(format!("dfe{tag}{suffix}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn notes_fixture(tag: &str) -> PathBuf {
    let root = tempdir(tag);
    let repo = GitRepo::init(&root).expect("init");
    repo.set_config("user.email", "e2e@modbit.test").unwrap();
    repo.set_config("user.name", "Modbit E2E").unwrap();
    repo.set_config("core.autocrlf", "false").unwrap();
    std::fs::write(root.join("notes.txt"), "fence fixture\n").unwrap();
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

/// Spawns a core on the GIVEN db path (shared across the two cores).
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
        command = command.env("MODBIT_SOCKET", db_path.parent().unwrap().join(format!("s{}.sock", &uuid::Uuid::now_v7().simple().to_string()[..8])));
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
            panic!("fence e2e: task did not complete (state {state})");
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Split-brain fencing: core A's in-flight run for a session is fenced
/// out the moment core B starts a run for the same session on the same
/// store. Exactly one writer completes the task.
#[test]
fn second_core_fences_the_first_mid_run() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let repo = notes_fixture("fa");
    let worktrees = tempdir("fw");
    // A's turn 1 runs a LONG tool; B resumes with the text turn.
    let (model, bodies) = spawn_model_fixture(vec![
        tool_call_turn("c1", "shell.run", r#"{"argv":"sleep 8"}"#),
        text_turn("fenced takeover complete"),
    ]);
    let db_dir = tempdir("fdb");
    let db_path = db_dir.join("core.db");
    let execd_a = spawn_execd();
    let (mut core_a, daemon_a) = spawn_core_on_db(&db_path, &repo, &worktrees, model, &execd_a);

    let created = request(
        &daemon_a,
        pb::surface_request::Request::CreateTask(pb::CreateTaskCommand {
            session_id: String::new(),
            title: "split brain".into(),
            prompt: "Run the slow command.".into(),
        }),
    );
    assert!(created.ok, "{}", created.error);
    let task_id = created.task.unwrap().task_id;
    for payload in [
        pb::surface_request::Request::QueueTask(pb::QueueTaskCommand { task_id: task_id.clone() }),
        pb::surface_request::Request::StartTask(pb::StartTaskCommand { task_id: task_id.clone() }),
    ] {
        let r = request(&daemon_a, payload);
        assert!(r.ok, "{}", r.error);
    }
    // Let A reach the long tool (turn-1 model call done, tool running).
    std::thread::sleep(Duration::from_millis(1_500));

    // Core B opens the SAME store; its boot scan sees the task running
    // and resumes it — acquiring the session lease and fencing A out.
    let execd_b = spawn_execd();
    let (mut core_b, daemon_b) = spawn_core_on_db(&db_path, &repo, &worktrees, model, &execd_b);
    wait_ready_for_review(&daemon_b, &task_id, Duration::from_secs(60));

    // Exactly one model request per core: A's turn 1, B's resume turn;
    // then outlive A's 8s tool — a fenced A appends NOTHING after the
    // takeover (no further invokes, no terminal run event). An unfenced A
    // would complete its own turn here; this window is what the mutation
    // check catches.
    {
        std::thread::sleep(Duration::from_secs(9));
        let bodies = bodies.lock().unwrap();
        assert_eq!(
            bodies.len(),
            2,
            "A invoked once, B invoked once, and the fenced A never invokes again: {}",
            bodies.len()
        );
    }

    // The shared store proves single-writer semantics: exactly ONE
    // terminal task transition, and A's run aggregate never reached a
    // terminal run event (the fenced writer stops writing — its run
    // dangles while B's completes).
    drop(bodies);
    let conn = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("open shared db");
    let ready: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE event_type = 'task_ready_for_review'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(ready, 1, "exactly one terminal task transition");
    let run_completed: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE event_type = 'run_completed'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(run_completed, 1, "exactly one run completed (B's)");
    // The lease table records the takeover (generation >= 2).
    let generation: i64 = conn
        .query_row("SELECT MAX(generation) FROM session_leases", [], |r| r.get(0))
        .unwrap();
    assert!(generation >= 2, "lease generation bumped by B's takeover: {generation}");

    core_a.kill().ok();
    core_a.wait().ok();
    core_b.kill().ok();
    core_b.wait().ok();
}
