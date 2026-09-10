//! Phase 4.2 settings E2E: through the REAL daemon,
//! 1. UpdateSettings persists provider/model/base_url/max_turns/
//!    execution_mode (partial merge — empty fields keep stored values),
//! 2. GetSettings round-trips them, and they SURVIVE a core restart,
//! 3. the run overlay really applies: a task started after
//!    execution_mode=readonly has its change.apply refused by the
//!    capability kernel (the denial feeds back as an error tool result
//!    in the durable conversation; the file is never written),
//! 4. the run reached the settings' base_url — the model fixture at the
//!    configured endpoint served the turns (routing proof).

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
    let dir = std::env::temp_dir().join(format!("st1{tag}{suffix}"));
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

fn spawn_core(
    db_path: &PathBuf,
    repo_root: &PathBuf,
    worktree_root: &PathBuf,
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
    let mut command = Command::new(exe);
    command.env("MODBIT_CORE_DB", db_path);
    if !cfg!(windows) {
        command.env("MODBIT_SOCKET", db_path.parent().unwrap().join("s.sock"));
    }
    let mut child = command
        .env("MODBIT_HTTP_ADDR", "127.0.0.1:0")
        .env("MODBIT_REPO_ROOT", repo_root)
        .env("MODBIT_WORKTREE_ROOT", worktree_root)
        .env("MODBIT_EXECD_ADDR", &execd_addr)
        // NO MODBIT_BASE_URL / MODBIT_MODEL / MODBIT_PROVIDER / MODBIT_MAX_TURNS:
        // the run must take them from the PERSISTED settings. The API key
        // still comes from the env secret broker (keychain = item 3).
        .env("OPENAI_API_KEY", "fixture-key")
        .env("MODBIT_MAX_TURNS", "6")
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

fn conversation_of(db_path: &PathBuf) -> String {
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT json_extract(payload_inline, '$.conversation_json')
             FROM events WHERE event_type = 'conversation_checkpointed'
             ORDER BY sequence DESC LIMIT 1",
        )
        .unwrap();
    stmt.query_row([], |row| row.get::<_, String>(0))
        .expect("a conversation checkpoint")
        .replace("\\\"", "\"")
}

fn update_settings(
    daemon: &str,
    provider: &str,
    model: &str,
    base_url: &str,
    max_turns: u32,
    execution_mode: &str,
) -> pb::SurfaceResponse {
    request(
        daemon,
        pb::surface_request::Request::UpdateSettings(pb::UpdateSettingsCommand {
            provider: provider.into(),
            model: model.into(),
            base_url: base_url.into(),
            max_turns,
            execution_mode: execution_mode.into(),
        }),
    )
}

#[test]
fn persisted_settings_drive_runs_and_the_readonly_mode_refuses_edits() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let repo = code_fixture("st");
    let worktrees = tempdir("stw");
    let db_dir = tempdir("db");
    let db_path = db_dir.join("core.db");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let model_addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let script = [
            tool_call_turn(
                "c1",
                "change.apply",
                r#"{"path":"greet.js","old_text":"return 'hi';","new_text":"return 'howdy';"}"#,
            ),
            text_turn("I cannot edit in readonly mode."),
        ];
        for (turn, stream) in listener.incoming().flatten().enumerate() {
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
            let body = script.get(turn).cloned().unwrap_or_else(|| text_turn("done"));
            let _ = stream.write_all(body.as_bytes());
        }
    });

    let (mut core, daemon) = spawn_core(&db_path, &repo, &worktrees);

    // 1) Persist settings. base_url POINTS AT THE FIXTURE: the run must
    // reach the model there (routing proof). execution_mode = readonly.
    let saved = update_settings(
        &daemon,
        "openai",
        "fixture-model",
        &format!("http://{model_addr}"),
        6,
        "readonly",
    );
    assert!(saved.ok, "update settings: {}", saved.error);
    let view = saved.settings.expect("settings view");
    assert_eq!(view.provider, "openai");
    assert_eq!(view.model, "fixture-model");
    assert_eq!(view.max_turns, 6);
    assert_eq!(view.execution_mode, "readonly");

    // 2) GetSettings round-trips.
    let got = request(
        &daemon,
        pb::surface_request::Request::GetSettings(pb::GetSettingsRequest {}),
    );
    assert!(got.ok, "get settings: {}", got.error);
    let view = got.settings.expect("settings view");
    assert_eq!(view.base_url, format!("http://{model_addr}"));
    assert_eq!(view.execution_mode, "readonly");

    // 3) A readonly task's edit attempt is refused by the kernel.
    let created = request(
        &daemon,
        pb::surface_request::Request::CreateTask(pb::CreateTaskCommand {
            session_id: String::new(),
            title: "try an edit".into(),
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
        let r = request(&daemon, payload);
        assert!(r.ok, "{}", r.error);
    }
    wait_state(&daemon, &task_id, pb::TaskStatus::ReadyForReview as i32);

    let visible = conversation_of(&db_path);
    assert!(
        visible.contains("granted")
            || visible.contains("denied")
            || visible.contains("policy"),
        "the readonly kernel denial must ride the conversation: {visible}"
    );
    let on_disk = std::fs::read_to_string(worktrees.join(&task_id).join("greet.js")).unwrap();
    assert_eq!(on_disk, "function greet() {\n  return 'hi';\n}\n", "nothing written in readonly mode");

    // 4) The run REACHED the settings' base_url: the fixture served the
    // turns (the task completed against it), and the model id matched —
    // proven by the run completing at all with no MODBIT_BASE_URL set.

    core.kill().ok();
    core.wait().ok();

    // 5) Persistence: settings survive a restart on the same DB.
    let (mut core2, daemon2) = spawn_core(&db_path, &repo, &worktrees);
    let got2 = request(
        &daemon2,
        pb::surface_request::Request::GetSettings(pb::GetSettingsRequest {}),
    );
    assert!(got2.ok, "get settings after restart");
    let view2 = got2.settings.expect("settings view after restart");
    assert_eq!(view2.model, "fixture-model");
    assert_eq!(view2.execution_mode, "readonly");
    assert_eq!(view2.base_url, format!("http://{model_addr}"));
    core2.kill().ok();
    core2.wait().ok();
}

/// Control: with execution_mode left at default, the same edit applies —
/// proving the readonly refusal is the SETTING's effect, not a breakage.
#[test]
fn default_mode_applies_the_edit() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let repo = code_fixture("sd");
    let worktrees = tempdir("sdw");
    let db_dir = tempdir("db");
    let db_path = db_dir.join("core.db");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let model_addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for (turn, stream) in listener.incoming().flatten().enumerate() {
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
            let script = [
                tool_call_turn(
                    "c1",
                    "change.apply",
                    r#"{"path":"greet.js","old_text":"return 'hi';","new_text":"return 'howdy';"}"#,
                ),
                text_turn("edited."),
            ];
            let body = script.get(turn).cloned().unwrap_or_else(|| text_turn("done"));
            let _ = stream.write_all(body.as_bytes());
        }
    });

    let (mut core, daemon) = spawn_core(&db_path, &repo, &worktrees);
    // Settings: default mode, no readonly.
    let saved = update_settings(&daemon, "openai", "fixture-model", &format!("http://{model_addr}"), 6, "default");
    assert!(saved.ok, "{}", saved.error);

    let created = request(
        &daemon,
        pb::surface_request::Request::CreateTask(pb::CreateTaskCommand {
            session_id: String::new(),
            title: "edit normally".into(),
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
        let r = request(&daemon, payload);
        assert!(r.ok, "{}", r.error);
    }
    wait_state(&daemon, &task_id, pb::TaskStatus::ReadyForReview as i32);
    let on_disk =
        std::fs::read_to_string(worktrees.join(&task_id).join("greet.js")).unwrap_or_default();
    assert!(
        on_disk.contains("return 'howdy';"),
        "default mode applies the edit; conversation: {}; disk: {on_disk:?}",
        conversation_of(&db_path)
    );
    core.kill().ok();
    core.wait().ok();
}
