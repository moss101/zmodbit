//! Phase 5 approvals E2E (docs/13 Waiting(Approval), docs/23): through
//! the REAL daemon, in `execution_mode = approvals`,
//! 1. a model's change.apply attempt on an ungranted write effect is
//!    PERSISTED as a pending approval (durable across a Core restart),
//! 2. ApproveEffect resolves it and the effect then proceeds — the file
//!    is really edited in the worktree,
//! 3. DenyEffect on a fresh pending approval refuses the effect (the
//!    worktree stays untouched) and the task still completes (the model
//!    is told, never left hanging),
//! 4. an approval replay (second decision) is rejected: first decision
//!    wins.

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
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
    let dir = std::env::temp_dir().join(format!("ap1{tag}{suffix}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn code_fixture(tag: &str) -> PathBuf {
    let root = tempdir(tag);
    let repo = GitRepo::init(&root).expect("init");
    repo.set_config("user.email", "e2e@modbit.test").unwrap();
    repo.set_config("user.name", "Modbit E2E").unwrap();
    repo.set_config("core.autocrlf", "false").unwrap();
    std::fs::write(root.join("greet.js"), "function greet() {\n  return 'hi';\n}\n").unwrap();
    repo.commit_all("base").expect("baseline");
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

#[allow(clippy::too_many_arguments)]
fn spawn_core(
    db_path: &PathBuf,
    repo_root: &PathBuf,
    worktree_root: &PathBuf,
    model_addr: SocketAddr,
    _execution_mode: &str,
) -> (Child, String) {
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
        .env("MODBIT_CORE_DB", db_path)
        .env("MODBIT_HTTP_ADDR", "127.0.0.1:0")
        .env("MODBIT_REPO_ROOT", repo_root)
        .env("MODBIT_WORKTREE_ROOT", worktree_root)
        .env("MODBIT_EXECD_ADDR", &execd_addr)
        .env("MODBIT_BASE_URL", format!("http://{model_addr}"))
        .env("MODBIT_MODEL", "fixture-model")
        .env("MODBIT_PROVIDER", "openai")
        .env("OPENAI_API_KEY", "fixture-key")
        .env("MODBIT_MAX_TURNS", "8")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn modbit-core");

    // The execution mode is set via the persisted settings (Phase 4.2)
    // after boot — done by the caller through UpdateSettings.

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
        let mut line = String::new();
        loop {
            line.clear();
            match err_reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });
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

fn wait_state(daemon: &str, task_id: &str, want: i32) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let fleet = request(daemon, pb::surface_request::Request::GetFleet(pb::GetFleetRequest {}))
            .fleet
            .unwrap();
        if let Some(t) = fleet.tasks.iter().find(|t| t.task_id == task_id) {
            if t.state == want {
                return;
            }
            if Instant::now() > deadline {
                panic!("task {task_id} did not reach {want} (state {})", t.state);
            }
        } else if Instant::now() > deadline {
            panic!("task {task_id} vanished");
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

fn spawn_model(listener: std::net::TcpListener, script: Vec<String>) -> SocketAddr {
    let addr = listener.local_addr().unwrap();
    spawn_model_thread(listener, script);
    addr
}

fn spawn_model_thread(listener: std::net::TcpListener, script: Vec<String>) {
    // The listener is moved into the thread and leaked with it: each run
    // owns its own fixture so turn counters never leak across restarts.
    std::thread::spawn(move || {
        for (turn, stream) in listener.incoming().flatten().enumerate() {
            let body = script
                .get(turn)
                .cloned()
                .unwrap_or_else(|| text_turn("done"));
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
}

const EDIT_OLD: &str = "return 'hi';";
const EDIT_NEW: &str = "return 'howdy';";

struct ApprovalBench {
    worktrees: PathBuf,
    db_path: PathBuf,
    daemon: String,
    core: Option<Child>,
}

/// Spins a daemon in approvals mode with a model fixture that ALWAYS
/// requests the same protected edit first.
fn approvals_bench(tag: &str, second_edit: &str) -> ApprovalBench {
    let repo = code_fixture(tag);
    let worktrees = tempdir(&format!("{tag}w"));
    let db_dir = tempdir(&format!("{tag}db"));
    let db_path = db_dir.join("core.db");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let model_addr = listener.local_addr().unwrap();
    let script = vec![
        tool_call_turn(
            "c1",
            "change.apply",
            &format!(
                r#"{{"path":"greet.js","old_text":"return 'hi';","new_text":"{second_edit}"}}"#
            ),
        ),
        text_turn("finished."),
    ];
    spawn_model(listener, script);
    let (core, daemon) = spawn_core(
        &db_path,
        &repo,
        &worktrees,
        model_addr,
        "default",
    );
    // Switch into approvals mode (persists), then restart so it applies.
    let saved = request(
        &daemon,
        pb::surface_request::Request::UpdateSettings(pb::UpdateSettingsCommand {
            provider: String::new(),
            model: String::new(),
            base_url: String::new(),
            max_turns: 0,
            execution_mode: "approvals".into(),
            api_key: String::new(),
        }),
    );
    assert!(saved.ok, "set approvals mode: {}", saved.error);
    let mut core = core;
    core.kill().ok();
    core.wait().ok();
    let (core, daemon) = spawn_core(
        &db_path,
        &repo,
        &worktrees,
        model_addr,
        "default",
    );
    ApprovalBench {
        worktrees,
        db_path,
        daemon,
        core: Some(core),
    }
}

fn start_task(bench: &ApprovalBench, title: &str) -> String {
    let created = request(
        &bench.daemon,
        pb::surface_request::Request::CreateTask(pb::CreateTaskCommand {
            session_id: String::new(),
            title: title.into(),
            prompt: "Do it.".into(),
            ..Default::default()
        }),
    );
    assert!(created.ok, "{}", created.error);
    let task_id = created.task.unwrap().task_id;
    for payload in [
        pb::surface_request::Request::QueueTask(pb::QueueTaskCommand { task_id: task_id.clone() }),
        pb::surface_request::Request::StartTask(pb::StartTaskCommand { task_id: task_id.clone() }),
    ] {
        let r = request(&bench.daemon, payload);
        assert!(r.ok, "{}", r.error);
    }
    task_id
}

fn wait_pending(bench: &ApprovalBench, task_id: &str) -> String {
    let conn = rusqlite::Connection::open_with_flags(
        &bench.db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut stmt = conn
            .prepare(
                "SELECT approval_id FROM approvals WHERE task_id = ?1 AND state = 'pending'",
            )
            .unwrap();
        let found: Vec<String> = stmt
            .query_map([task_id], |row| row.get::<_, String>(0))
            .unwrap()
            .flatten()
            .collect();
        if let Some(id) = found.first() {
            return id.clone();
        }
        if Instant::now() > deadline {
            panic!("no pending approval persisted for {task_id}");
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// The core Phase 5 loop: pending approval PERSISTS, the operator
/// approves while the effect is blocked, the effect then proceeds.
#[test]
fn approve_unblocks_the_protected_effect() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut bench = approvals_bench("ap", EDIT_NEW);
    let task_id = start_task(&bench, "edit under approvals");
    let approval_id = wait_pending(&bench, &task_id);

    // The Needs-Attention source: ListPendingApprovals shows the pending
    // card with its tool + scope BEFORE the decision.
    let listed = request(
        &bench.daemon,
        pb::surface_request::Request::ListPendingApprovals(
            pb::ListPendingApprovalsRequest {},
        ),
    );
    assert!(listed.ok, "list: {}", listed.error);
    let pending = listed.pending_approvals.expect("pending list");
    let card = pending
        .approvals
        .iter()
        .find(|a| a.approval_id == approval_id)
        .unwrap_or_else(|| panic!("pending approval {} not listed", approval_id));
    assert!(!card.tool.is_empty(), "card names the tool: {card:?}");
    assert!(!card.scope.is_empty(), "card names the scope: {card:?}");

    let approved = request(
        &bench.daemon,
        pb::surface_request::Request::ApproveEffect(pb::ApproveEffectCommand {
            approval_id: approval_id.clone(),
            resolved_by: "operator".into(),
        }),
    );
    assert!(approved.ok, "approve: {}", approved.error);

    // After the decision the card is gone (state no longer pending).
    let listed = request(
        &bench.daemon,
        pb::surface_request::Request::ListPendingApprovals(
            pb::ListPendingApprovalsRequest {},
        ),
    );
    let pending = listed.pending_approvals.expect("pending list");
    assert!(
        !pending
            .approvals
            .iter()
            .any(|a| a.approval_id == approval_id),
        "approved card must leave the pending list"
    );

    wait_state(&bench.daemon, &task_id, pb::TaskStatus::ReadyForReview as i32);
    let on_disk =
        std::fs::read_to_string(bench.worktrees.join(&task_id).join("greet.js")).unwrap();
    assert!(on_disk.contains(EDIT_NEW), "the approved edit applied: {on_disk}");

    // Replay the SAME decision: first decision wins, replay refused.
    let replay = request(
        &bench.daemon,
        pb::surface_request::Request::ApproveEffect(pb::ApproveEffectCommand {
            approval_id,
            resolved_by: "operator".into(),
        }),
    );
    assert!(!replay.ok, "replayed approval must be refused");

    bench.core.take().unwrap().kill().ok();
}

/// Denial: the worktree stays untouched and the task still completes —
/// the model is told, never left hanging.
#[test]
fn deny_refuses_the_protected_effect_but_completes() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut bench = approvals_bench("dn", EDIT_OLD);
    let task_id = start_task(&bench, "edit to deny");
    let approval_id = wait_pending(&bench, &task_id);

    let denied = request(
        &bench.daemon,
        pb::surface_request::Request::DenyEffect(pb::DenyEffectCommand {
            approval_id: approval_id.clone(),
            reason: "operator refused".into(),
            resolved_by: "operator".into(),
        }),
    );
    assert!(denied.ok, "deny: {}", denied.error);

    wait_state(&bench.daemon, &task_id, pb::TaskStatus::ReadyForReview as i32);
    let on_disk =
        std::fs::read_to_string(bench.worktrees.join(&task_id).join("greet.js")).unwrap();
    assert_eq!(on_disk, "function greet() {
  return 'hi';
}
", "denied effect must not apply");

    // Replay a decision on the denied row: refused.
    let replay = request(
        &bench.daemon,
        pb::surface_request::Request::DenyEffect(pb::DenyEffectCommand {
            approval_id,
            reason: String::new(),
            resolved_by: "operator".into(),
        }),
    );
    assert!(!replay.ok, "replayed denial must be refused");

    bench.core.take().unwrap().kill().ok();
}

/// Durable across a Core kill: the pending approval row survives the
/// death of the core that created it (SQLite, not process memory).
#[test]
fn pending_approval_survives_a_core_kill() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut bench = approvals_bench("kl", EDIT_NEW);
    let task_id = start_task(&bench, "edit then die");
    let _approval_id = wait_pending(&bench, &task_id);

    bench.core.take().unwrap().kill().ok();
    bench.core.take();
    let conn = rusqlite::Connection::open_with_flags(
        &bench.db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let still_pending: i64 = {
        let mut stmt = conn
            .prepare("SELECT COUNT(*) FROM approvals WHERE state = 'pending'")
            .unwrap();
        stmt.query_row([], |row| row.get(0)).unwrap()
    };
    assert_eq!(
        still_pending, 1,
        "the pending approval is durable across the Core kill"
    );
}
