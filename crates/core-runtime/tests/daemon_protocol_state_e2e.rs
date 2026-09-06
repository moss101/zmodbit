//! Daemon-driven protocol-state E2E (M4.1): the SSE
//! client-cursor journal (crates/protocol-state — previously zero
//! production callers) is wired into the daemon. A reconnecting client
//! resumes from its last PERSISTED cursor — including across a Core
//! SIGKILL and restart: the first event after reconnect continues the
//! offset sequence EXACTLY, with no replay of already-delivered events
//! and no skipped ones.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
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
    let dir = std::env::temp_dir().join(format!("dpe{tag}{suffix}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn readme_fixture(tag: &str) -> PathBuf {
    let root = tempdir(tag);
    let repo = GitRepo::init(&root).expect("init");
    repo.set_config("user.email", "e2e@modbit.test").unwrap();
    repo.set_config("user.name", "Modbit E2E").unwrap();
    repo.set_config("core.autocrlf", "false").unwrap();
    std::fs::write(root.join("notes.txt"), "protocol-state fixture\n").unwrap();
    repo.commit_all("fixture baseline").expect("baseline");
    root
}

fn spawn_core_on_db(db_path: &PathBuf) -> (Child, String) {
    let exe = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/modbit-core");
    let mut command = Command::new(exe);
    let mut command = command.env("MODBIT_CORE_DB", db_path);
    if !cfg!(windows) {
        let sock = db_path
            .parent()
            .unwrap()
            .join(format!("s{}.sock", &uuid::Uuid::now_v7().simple().to_string()[..8]));
        command = command.env("MODBIT_SOCKET", sock);
    }
    // No repository/worktree/execd needed: this test never runs a task —
    // it exercises the event stream and the cursor journal only.
    let mut child = command
        .env("MODBIT_HTTP_ADDR", "127.0.0.1:0")
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

/// Connects an SSE client; when `since` is None the request omits the
/// offset — the daemon must resume the client from its persisted cursor.
fn sse_connect(daemon: &str, client_id: &str, since: Option<u64>) -> BufReader<TcpStream> {
    let mut stream = TcpStream::connect(daemon).expect("sse connect");
    // Bounded reads: a quiet stream must not block the reader forever.
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("read timeout");
    let query = match since {
        Some(offset) => format!("client={client_id}&since={offset}"),
        None => format!("client={client_id}"),
    };
    stream
        .write_all(format!("GET /events?{query} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
        .expect("sse request");
    let mut reader = BufReader::new(stream);
    // Drain the HTTP head up to the blank line.
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).expect("head");
        if line.trim().is_empty() {
            break;
        }
    }
    reader
}

/// Reads SSE data lines until `want` events have arrived; returns their
/// (offset, event_type) pairs.
fn read_events(reader: &mut BufReader<TcpStream>, want: usize, timeout: Duration) -> Vec<(u64, String)> {
    let deadline = Instant::now() + timeout;
    let mut events = Vec::new();
    while events.len() < want {
        if Instant::now() > deadline {
            panic!("only {}/{} SSE events within {timeout:?}", events.len(), want);
        }
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => panic!("sse stream closed"),
            Ok(_) => {
                let Some(payload) = line.strip_prefix("data: ") else {
                    continue;
                };
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(payload.trim()) {
                    if let (Some(offset), Some(kind)) = (
                        value["offset"].as_u64(),
                        value["event_type"].as_str(),
                    ) {
                        events.push((offset, kind.to_string()));
                    }
                }
            }
            // Read timeout on a quiet stream: no data yet, retry until
            // the deadline.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => panic!("sse read error"),
        }
    }
    events
}

/// The exact-resume proof: a client's cursor survives a Core SIGKILL in
/// the durable journal; the reconnected client continues the offset
/// sequence exactly where the pre-restart stream stopped.
#[test]
fn sse_cursor_journal_resumes_exactly_across_restart() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _ = readme_fixture("ps"); // fixture unused: no task runs here
    let db_dir = tempdir("psdb");
    let db_path = db_dir.join("core.db");
    let journal = db_dir.join("protocol-state.jsonl");
    let (mut core, daemon) = spawn_core_on_db(&db_path);

    // Client desk-1 connects with NO offset: first subscription starts at 0.
    let mut reader = sse_connect(&daemon, "desk-1", None);
    let created = request(
        &daemon,
        pb::surface_request::Request::CreateTask(pb::CreateTaskCommand {
            session_id: String::new(),
            title: "journal me".into(),
            prompt: "nothing".into(),
        }),
    );
    assert!(created.ok, "{}", created.error);

    // Read through the task_created event; remember the highest offset.
    // (CreateTask with no session emits session_created + task_created.)
    let events = read_events(&mut reader, 2, Duration::from_secs(15));
    assert!(events.iter().any(|(_, kind)| kind == "task_created"));
    let last_offset = events.last().unwrap().0;
    assert!(last_offset >= 2, "offsets progress: {events:?}");
    // Let the daemon persist the advanced cursor (append+flush per batch).
    std::thread::sleep(Duration::from_millis(500));

    // The journal exists with terminal_cursor records (durable, on disk).
    let journal_text = std::fs::read_to_string(&journal).expect("journal exists");
    assert!(
        journal_text.contains("\"kind\":\"terminal_cursor\""),
        "journal carries cursor records: {journal_text}"
    );
    assert!(
        journal_text.contains("\"run_id\":\"desk-1\""),
        "journal keys cursors by client id"
    );

    // SIGKILL the core; drop the client connection.
    drop(reader);
    core.kill().expect("kill core");
    core.wait().ok();

    // Restart on the same store + journal; create a second task.
    let (mut core2, daemon2) = spawn_core_on_db(&db_path);
    let created2 = request(
        &daemon2,
        pb::surface_request::Request::CreateTask(pb::CreateTaskCommand {
            session_id: String::new(),
            title: "after restart".into(),
            prompt: "nothing".into(),
        }),
    );
    assert!(created2.ok, "{}", created2.error);

    // desk-1 reconnects with NO offset: it must resume from the persisted
    // cursor — the first event continues the sequence EXACTLY (no replay,
    // no skip). A fresh client (desk-2) starts at 0 instead.
    let mut resumed = sse_connect(&daemon2, "desk-1", None);
    let first = read_events(&mut resumed, 1, Duration::from_secs(15));
    assert_eq!(
        first[0].0,
        last_offset + 1,
        "exact resume: expected offset {}, got {} ({:?})",
        last_offset + 1,
        first[0].0,
        first
    );

    let mut fresh = sse_connect(&daemon2, "desk-2", None);
    let fresh_first = read_events(&mut fresh, 1, Duration::from_secs(15));
    assert_eq!(fresh_first[0].0, 1, "a fresh client replays from zero");

    core2.kill().ok();
    core2.wait().ok();
}
