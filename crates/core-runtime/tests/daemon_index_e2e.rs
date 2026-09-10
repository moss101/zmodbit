//! Daemon-driven repository-index E2E (Future-tasks Phase 3 item 1,
//! IMP-EV-0004 / QUAL-EV-0004): through the REAL daemon + worktree + model
//! transport, the task's repository index is
//! 1. built at task start by walking the worktree (index_updated
//!    `task_start` evidence: file count + Merkle root digest),
//! 2. refreshed INCREMENTALLY on a real change.apply — the recomputed
//!    evidence names only the edited leaf and its ancestor dir chain,
//! 3. equal to a cold rebuild of the same final tree (incremental ==
//!    full), and
//! 4. fail-closed: a change.apply rejected by the edit gate writes no
//!    journal event and leaves the index (and its evidence) untouched.

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use prost::Message;
use reqwest::blocking::Client;

use modbit_git::GitRepo;
use modbit_protocol::modbit::protocol::v1 as pb;
use modbit_retrieval::merkle::MerkleIndex;
use modbit_retrieval::task_index::TaskIndex;
use modbit_retrieval::walker::walk_worktree;

static E2E_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn tempdir(tag: &str) -> PathBuf {
    let suffix: String = uuid::Uuid::now_v7().simple().to_string().chars().rev().take(8).collect::<String>().chars().rev().collect();
    let dir = std::env::temp_dir().join(format!("ixe{tag}{suffix}"));
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
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(root.join("src/greet.js"), ORIGINAL).unwrap();
    std::fs::write(root.join("docs/readme.md"), "# docs\n").unwrap();
    std::fs::write(root.join(".gitignore"), "secret.txt\n").unwrap();
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
            panic!("index e2e: task did not complete (state {state})");
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

fn create_queue_start(daemon: &str, title: &str) -> String {
    let created = request(
        daemon,
        pb::surface_request::Request::CreateTask(pb::CreateTaskCommand {
            session_id: String::new(),
            title: title.into(),
            prompt: "Do it.".into(),
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

#[allow(clippy::type_complexity)]
fn index_events(db_path: &PathBuf) -> Vec<(String, i64, i64, String, String)> {
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("open core db");
    let mut stmt = conn
        .prepare(
            "SELECT json_extract(payload_inline, '$.reason'),
                    json_extract(payload_inline, '$.workspace_revision'),
                    json_extract(payload_inline, '$.file_count'),
                    json_extract(payload_inline, '$.root_digest'),
                    json_extract(payload_inline, '$.recomputed')
             FROM events WHERE event_type = 'index_updated'
             ORDER BY sequence",
        )
        .unwrap();
    stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
        ))
    })
    .unwrap()
    .flatten()
    .collect()
}

/// The task-start index, the incremental refresh on a real change.apply,
/// and the equality of the refreshed root with a cold rebuild.
#[test]
fn daemon_builds_and_incrementally_refreshes_the_repository_index() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let repo = code_fixture("ix");
    let worktrees = tempdir("ixw");
    let model = spawn_model_fixture(vec![
        tool_call_turn(
            "c1",
            "change.apply",
            r#"{"path":"src/greet.js","old_text":"return 'hi';","new_text":"return 'hello';"}"#,
        ),
        text_turn("edited"),
    ]);
    let (mut core, daemon, db_path) = spawn_core(&repo, &worktrees, model);
    let task_id = create_queue_start(&daemon, "edit greet through the index");
    wait_ready_for_review(&daemon, &task_id);

    let events = index_events(&db_path);
    assert_eq!(events.len(), 2, "task_start + change_apply evidence: {events:?}");

    let (reason0, rev0, files0, digest0, _) = &events[0];
    assert_eq!(reason0, "task_start");
    assert_eq!(*rev0, 0, "bound to the pre-run workspace revision");
    assert_eq!(*files0, 2, "greet.js + readme.md; hidden/.gitignore skipped");
    assert_eq!(digest0.len(), 64, "sha256 hex root digest");

    let (reason1, rev1, files1, digest1, recomputed1) = &events[1];
    assert_eq!(reason1, "change_apply", "refresh happened in the apply, not lazily");
    // change.apply bumps the counter twice: adopt (track the checked-out
    // file) + replace (the edit). The index binds to the post-edit value.
    assert_eq!(*rev1, 2, "one edit: adopt + replace");
    assert_eq!(*files1, 2, "no file added or removed");
    assert_ne!(digest0, digest1, "the root moved with the edit");
    assert!(
        recomputed1.contains("leaf:src/greet.js") && recomputed1.contains("dir:src"),
        "only-affected-segments evidence: {recomputed1}"
    );
    assert!(
        !recomputed1.contains("docs"),
        "the untouched docs subtree was not recomputed: {recomputed1}"
    );

    // Incremental == full: rebuild over the REAL final worktree and the
    // refreshed digest must match exactly.
    let worktree = worktrees.join(&task_id);
    let (files, stats) = walk_worktree(&worktree);
    assert_eq!(stats.indexed, 2);
    let owned: std::collections::BTreeMap<String, Vec<u8>> = files
        .into_iter()
        .map(|f| (f.path, f.bytes))
        .collect();
    let rebuilt = MerkleIndex::build(&owned, 1);
    assert_eq!(
        rebuilt.root_digest(),
        *digest1,
        "incrementally refreshed index equals a cold rebuild of the same tree"
    );

    // The on-disk edit itself is real (the worktree moved).
    let on_disk = std::fs::read_to_string(worktree.join("src/greet.js")).unwrap();
    assert_eq!(on_disk, "function greet() {\n  return 'hello';\n}\n");

    core.kill().ok();
    core.wait().ok();
}

/// Failure behavior: a change.apply refused by the edit gate performs NO
/// write, advances nothing, and leaves the index exactly as built — the
/// only evidence is the task_start build.
#[test]
fn refused_edit_leaves_the_index_untouched() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let repo = code_fixture("in");
    let worktrees = tempdir("inw");
    let model = spawn_model_fixture(vec![
        tool_call_turn(
            "c1",
            "change.apply",
            r#"{"path":"src/greet.js","old_text":"THIS TEXT IS NOWHERE","new_text":"x"}"#,
        ),
        text_turn("gave up"),
    ]);
    let (mut core, daemon, db_path) = spawn_core(&repo, &worktrees, model);
    let task_id = create_queue_start(&daemon, "attempt a refused edit");
    wait_ready_for_review(&daemon, &task_id);

    let events = index_events(&db_path);
    assert_eq!(events.len(), 1, "no refresh evidence without a write: {events:?}");
    let (reason, rev, files, digest, recomputed) = &events[0];
    assert_eq!(reason, "task_start");
    assert_eq!(*rev, 0);
    assert_eq!(*files, 2);
    assert!(recomputed == "[]" || recomputed.is_empty());

    // The index still describes the tree exactly (nothing drifted).
    let worktree = worktrees.join(&task_id);
    let index = TaskIndex::build_at(&worktree, 0);
    assert_eq!(index.root_digest(), *digest);

    core.kill().ok();
    core.wait().ok();
}
