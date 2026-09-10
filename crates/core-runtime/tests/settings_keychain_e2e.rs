//! Phase 4.3 keychain SecretBroker E2E: through the REAL daemon + REAL
//! OS keychain,
//! 1. an API key stored via UpdateSettings goes into the OS KEYCHAIN —
//!    never the settings document, the environment, or the event store;
//! 2. GetSettings reports has_api_key (the key itself never rides a
//!    message);
//! 3. the run's model request carries the key in the Authorization
//!    header — proven by a fixture that REJECTS any other credential;
//! 4. the daemon runs with NO OPENAI_API_KEY in its environment: the
//!    keychain broker supplies the credential.

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

/// The credential the test stores via the settings surface.
const SECRET_KEY: &str = "sk-keychain-e2e-98765";

fn tempdir(tag: &str) -> PathBuf {
    let suffix: String = uuid::Uuid::now_v7().simple().to_string().chars().rev().take(8).collect::<String>().chars().rev().collect();
    let dir = std::env::temp_dir().join(format!("kc1{tag}{suffix}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn code_fixture(tag: &str) -> PathBuf {
    let root = tempdir(tag);
    let repo = GitRepo::init(&root).expect("init");
    repo.set_config("user.email", "e2e@modbit.test").unwrap();
    repo.set_config("user.name", "Modbit E2E").unwrap();
    repo.set_config("core.autocrlf", "false").unwrap();
    std::fs::write(root.join("note.txt"), "hello\n").unwrap();
    repo.commit_all("base").expect("baseline");
    root
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

fn spawn_core(db_path: &PathBuf, repo_root: &PathBuf, model_addr: SocketAddr) -> (Child, String) {
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
        .env("MODBIT_WORKTREE_ROOT", worktree_root())
        .env("MODBIT_EXECD_ADDR", &execd_addr)
        // The model endpoint + credentials come from the PERSISTED
        // settings + the OS keychain. Deliberately NO OPENAI_API_KEY here:
        // if the keychain broker fails, the fixture's credential check
        // fails the task. The keychain service is run-scoped so the test
        // never touches (or prompts for) a real user key.
        .env("MODBIT_KEYCHAIN_SERVICE", keychain_service())
        .env("MODBIT_BASE_URL", format!("http://{model_addr}"))
        .env("MODBIT_MODEL", "fixture-model")
        .env("MODBIT_PROVIDER", "openai")
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
        for line in err_reader.lines().flatten() {
            eprintln!("CORE: {line}");
        }
    });
    std::mem::forget(execd);
    (child, daemon.expect("daemon addr"))
}

fn worktree_root() -> PathBuf {
    let d = tempdir("wt");
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn request(daemon: &str, req: pb::surface_request::Request) -> pb::SurfaceResponse {
    let which = match &req {
        pb::surface_request::Request::RegisterRepo(_) => "register",
        pb::surface_request::Request::CreateTask(_) => "create",
        pb::surface_request::Request::QueueTask(_) => "queue",
        pb::surface_request::Request::StartTask(_) => "start",
        pb::surface_request::Request::GetFleet(_) => "fleet",
        pb::surface_request::Request::UpdateSettings(_) => "update-settings",
        pb::surface_request::Request::GetSettings(_) => "get-settings",
        _ => "other",
    };
    eprintln!("REQ> {which}");
    let client = Client::builder().timeout(Duration::from_secs(30)).build().unwrap();
    let body = pb::SurfaceRequest { request: Some(req) }.encode_to_vec();
    let response = client
        .post(format!("http://{daemon}/commands"))
        .header("Content-Type", "application/x-protobuf")
        .body(body)
        .send()
        .map_err(|e| {
            eprintln!("REQ> {which} FAILED: {e}");
            e
        })
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

/// The keychain is REAL and per-user: the round-trip proof needs a
/// working platform store. Where none exists (headless Linux CI without
/// a Secret Service), the test records the skip — the keychain unit tests
/// and the broker fallback tests remain the always-on coverage there.
fn keychain_service() -> String {
    format!("modbit-core-e2e-{}", uuid::Uuid::now_v7().simple())
}

fn keychain_available() -> bool {
    let probe = format!(
        "{}:PROBE",
        keychain_service()
    );
    match modbit_providers::keychain::store_secret(&probe, "probe") {
        Ok(()) => {
            let _ = modbit_providers::keychain::delete_secret(&probe);
            true
        }
        Err(e) => {
            println!("keychain unavailable ({e}); keychain-dependent assertions skipped");
            false
        }
    }
}

#[test]
fn api_key_lives_in_the_keychain_and_flows_to_the_transport() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let has_keychain = keychain_available();

    let repo = code_fixture("kc");
    let db_dir = tempdir("db");
    let db_path = db_dir.join("core.db");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let model_addr = listener.local_addr().unwrap();

    // The model fixture REJECTS any credential other than the key the
    // test stored in the keychain — proving the broker supplied it.
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
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
            let head = String::from_utf8_lossy(&buf).to_lowercase();
            let authorized = head.contains(&format!(
                "authorization: bearer {}",
                SECRET_KEY.to_lowercase()
            ));
            use std::io::Write;
            let mut stream = stream;
            if !authorized {
                let _ = stream.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 14\r\n\r\nbad credential");
                continue;
            }
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            );
            let _ = stream.write_all(text_turn("the keychain key worked.").as_bytes());
        }
    });

    let (mut core, daemon) = spawn_core(&db_path, &repo, model_addr);

    if has_keychain {
        // 1) Store the key THROUGH the settings surface.
        let saved = request(
            &daemon,
            pb::surface_request::Request::UpdateSettings(pb::UpdateSettingsCommand {
                provider: "openai".into(),
                model: "fixture-model".into(),
                base_url: format!("http://{model_addr}"),
                max_turns: 6,
                execution_mode: "default".into(),
                api_key: SECRET_KEY.into(),
            }),
        );
        assert!(saved.ok, "store key: {}", saved.error);
        let view = saved.settings.expect("settings view");
        assert!(view.has_api_key, "the keychain indicator must be on");

        // 2) GetSettings reports the keychain flag and never the key.
        let got = request(
            &daemon,
            pb::surface_request::Request::GetSettings(pb::GetSettingsRequest {}),
        );
        let view = got.settings.expect("settings view");
        assert!(view.has_api_key, "the keychain indicator must be on");
        assert_eq!(view.model, "fixture-model");
        assert!(
            !format!("{view:?}").contains(SECRET_KEY),
            "the key must never ride a settings read"
        );
    }

    // 3) Start a task. The fixture rejects wrong credentials, so a
    // ReadyForReview task PROVES the keychain broker supplied the key to
    // the transport header — with NO OPENAI_API_KEY in the environment.
    let created = request(
        &daemon,
        pb::surface_request::Request::CreateTask(pb::CreateTaskCommand {
            session_id: String::new(),
            title: "run with keychain creds".into(),
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
    if has_keychain {
        wait_state(&daemon, &task_id, pb::TaskStatus::ReadyForReview as i32);
    } else {
        // No keychain: the run fails on missing credentials — also fine.
        wait_state(&daemon, &task_id, pb::TaskStatus::Failed as i32);
        core.kill().ok();
        core.wait().ok();
        return;
    }

    // 4) The key NEVER reached the durable event store.
    core.kill().ok();
    core.wait().ok();
    let conn = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let leaks: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE payload_inline LIKE '%' || ?1 || '%'",
            rusqlite::params![SECRET_KEY],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(leaks, 0, "the API key must never appear in the event store");
}
