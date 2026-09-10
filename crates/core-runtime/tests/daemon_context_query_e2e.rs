//! Daemon-driven context.query / search.symbol E2E (Future-tasks Phase 3
//! item 2, M3.2 Tantivy BM25 + M3.3 tree-sitter symbols): through the REAL
//! daemon + worktree + model transport,
//! 1. context.query returns fused BM25 hits over the real worktree with
//!    provenance, riding the durable conversation,
//! 2. search.symbol resolves definitions AND references across files,
//! 3. after a real change.apply, the index answers queries about the NEW
//!    content (freshness through the change journal), with the lexical
//!    source proven — proving the Tantivy delta batch is committed,
//! 4. no index event is emitted for pure queries with nothing new (the
//!    evidence stream stays exactly task_start + change_apply).

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
    let dir = std::env::temp_dir().join(format!("cqe{tag}{suffix}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn code_fixture(tag: &str) -> PathBuf {
    let root = tempdir(tag);
    let repo = GitRepo::init(&root).expect("init");
    repo.set_config("user.email", "e2e@modbit.test").unwrap();
    repo.set_config("user.name", "Modbit E2E").unwrap();
    repo.set_config("core.autocrlf", "false").unwrap();
    std::fs::create_dir_all(root.join("src/ui")).unwrap();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(root.join("src/helpers.rs"), "pub fn retry_helpers() -> bool {\n    true\n}\n").unwrap();
    std::fs::write(root.join("src/caller.rs"), "fn run_task() {\n    let _ = retry_helpers();\n}\n").unwrap();
    std::fs::write(root.join("src/ui/button.rs"), "button click render focus style widget\n").unwrap();
    std::fs::write(root.join("docs/retry-notes.md"), "retry notes about the retry mechanism and its backoff\n").unwrap();
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
            panic!("context query e2e: task did not complete (state {state})");
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// The whole fused-index flow through the real daemon: BM25 query,
/// cross-file symbol resolution, post-edit freshness with lexical
/// provenance, and an evidence stream that pure queries do not inflate.
#[test]
fn context_query_and_search_symbol_answer_from_the_live_index() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let repo = code_fixture("cq");
    let worktrees = tempdir("cqw");
    let model = spawn_model_fixture(vec![
        tool_call_turn("c1", "context.query", r#"{"query":"retry backoff"}"#),
        tool_call_turn("c2", "search.symbol", r#"{"name":"retry_helpers"}"#),
        // Real edit through the change engine: a NEW definition appears.
        tool_call_turn(
            "c3",
            "change.apply",
            r#"{"path":"src/helpers.rs","old_text":"pub fn retry_helpers() -> bool {","new_text":"pub fn brand_new_marker() {}\n\npub fn retry_helpers() -> bool {"}"#,
        ),
        // Post-edit: the lexical AND symbol surfaces must know the new fn.
        tool_call_turn("c4", "context.query", r#"{"query":"brand_new_marker"}"#),
        text_turn("done"),
    ]);
    let (mut core, daemon, db_path) = spawn_core(&repo, &worktrees, model);

    let created = request(
        &daemon,
        pb::surface_request::Request::CreateTask(pb::CreateTaskCommand {
            session_id: String::new(),
            title: "query the index".into(),
            prompt: "Do it.".into(),
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

    // Evidence stream: exactly the build + the apply; pure queries with
    // nothing new emit nothing.
    let conn = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("open core db");
    let reasons: Vec<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT json_extract(payload_inline, '$.reason')
                 FROM events WHERE event_type = 'index_updated'
                 ORDER BY sequence",
            )
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .flatten()
            .collect()
    };
    assert_eq!(
        reasons,
        vec!["task_start".to_string(), "change_apply".to_string()],
        "index evidence stream: {reasons:?}"
    );

    // The durable conversation carries the tool results: the newest
    // checkpoint is the model-visible projection the next turn would see.
    let conversation: String = {
        let mut stmt = conn
            .prepare(
                "SELECT json_extract(payload_inline, '$.conversation_json')
                 FROM events WHERE event_type = 'conversation_checkpointed'
                 ORDER BY sequence DESC LIMIT 1",
            )
            .unwrap();
        stmt.query_row([], |row| row.get::<_, String>(0))
            .expect("at least one conversation checkpoint")
    };
    // The projection embeds tool-result JSON as an escaped string; unescape
    // the quote sequences so plain JSON substrings can be asserted.
    let visible = conversation.replace("\\\"", "\"");

    // 1) BM25 fused hit over the real worktree (turn 1).
    assert!(
        visible.contains("docs/retry-notes.md"),
        "context.query hit with path provenance must ride the conversation"
    );
    assert!(
        visible.contains("\"sources\":[\"bm25\""),
        "lexical provenance is exposed: {visible}"
    );

    // 2) Cross-file symbol resolution (turn 2): definition + reference.
    assert!(
        visible.contains("\"kind\":\"function\""),
        "symbol definition with kind rides the conversation"
    );
    assert!(
        visible.contains("src/caller.rs"),
        "references resolve across files: {visible}"
    );

    // 3) Freshness (turn 4): the brand-new definition is queryable with
    // BOTH sources — symbol (tree-sitter reparse) and bm25 (Tantivy delta
    // commit). Without a committed lexical batch the bm25 source is absent.
    assert!(
        visible.contains("brand_new_marker"),
        "post-edit symbol is queryable through the daemon"
    );
    assert!(
        visible.contains("\"sources\":[\"bm25\",\"symbol\"]"),
        "the new function is found by both index surfaces (lexical batch committed): {visible}"
    );

    // The edit is real on disk.
    let helpers = std::fs::read_to_string(worktrees.join(&task_id).join("src/helpers.rs")).unwrap();
    assert!(helpers.contains("pub fn brand_new_marker()"));

    core.kill().ok();
    core.wait().ok();
}
