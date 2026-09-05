//! Daemon-driven live E2E (M2.11; docs/51 E2E-001/002/003): the real
//! `modbit-core` binary hosting BOTH the socket transport and the HTTP+SSE
//! daemon, the real scheduler, real git worktrees, the real execd broker
//! and — when credentials are present — a LIVE model. Driven exclusively
//! over the surface protocol (HTTP /commands + /events), never the runtime
//! in process.
//!
//! Gating:
//!   MODBIT_LIVE_E2E=1 + OPENAI_API_KEY (+ optional MODBIT_LIVE_MODEL,
//!   MODBIT_BASE_URL) → full live run against the provider.
//!   Without credentials these tests SKIP (nothing closes on a skip).
//!
//! The fixture is a frozen, low-cost ts-webapp-style repo: one JS module,
//! one test command. E2E-001 asks the model to add validation; E2E-002
//! seeds the fixture so the first natural test command fails; E2E-003
//! reconnects the event stream mid-run and requires lossless replay.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use prost::Message;
use reqwest::blocking::Client;

use modbit_git::GitRepo;
use modbit_protocol::modbit::protocol::v1 as pb;

// ---- fixture ---------------------------------------------------------------

fn tempdir(tag: &str) -> PathBuf {
    // Short prefix: unix socket paths must fit sun_path (104 bytes).
    // UUIDv7 leads with its TIMESTAMP: take the random tail instead, or
    // dirs created in the same millisecond window collide across runs.
    let suffix: String = uuid::Uuid::now_v7().simple().to_string().chars().rev().take(8).collect::<String>().chars().rev().collect();
    let dir = std::env::temp_dir().join(format!("mle{tag}{suffix}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Frozen ts-webapp-style fixture (E2E-001 wording: "Add validation so
/// negative quantities are rejected and add tests."). `seed_failure` makes
/// the first natural test command fail (E2E-002).
fn ts_webapp_fixture(tag: &str, seed_failure: bool) -> PathBuf {
    let root = tempdir(tag);
    let repo = GitRepo::init(&root).expect("init fixture");
    repo.set_config("user.email", "e2e@modbit.test").unwrap();
    repo.set_config("user.name", "Modbit E2E").unwrap();
    // Byte-exact worktrees: the runner's global autocrlf must not rewrite
    // fixture files on checkout (edit-gate old_text is LF-exact).
    repo.set_config("core.autocrlf", "false").unwrap();

    let quantity = if seed_failure {
        // Broken on purpose: the initial test run fails, the agent must
        // repair it (E2E-002: command failure ≠ task failure).
        "function validateQuantity(q) {\n  return true;\n}\nmodule.exports = { validateQuantity };\n"
    } else {
        "function validateQuantity(q) {\n  if (q < 0) throw new Error('negative');\n  return true;\n}\nmodule.exports = { validateQuantity };\n"
    };
    std::fs::write(root.join("quantity.js"), quantity).unwrap();
    std::fs::write(
        root.join("quantity.test.js"),
        "const { validateQuantity } = require('./quantity');\n\
         test('rejects negative quantities', () => {\n\
         \x20 expect(() => validateQuantity(-5)).toThrow('negative');\n\
         });\n\
         test('accepts positive', () => {\n\
         \x20 expect(validateQuantity(5)).toBe(true);\n\
         });\n",
    )
    .unwrap();
    std::fs::write(
        root.join("package.json"),
        r#"{ "name": "ts-webapp-fixture", "version": "1.0.0", "scripts": { "test": "node --test ." } }"#,
    )
    .unwrap();
    std::fs::write(root.join("run_tests.sh"), "#!/bin/sh\nnode --test .\n").unwrap();
    repo.commit_all("fixture baseline").expect("baseline");
    root
}

// ---- process harness --------------------------------------------------------

struct CoreProc {
    child: Child,
    daemon: String,
    _execd: Option<Child>,
}

fn spawn_execd() -> Option<Child> {
    let exe = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/modbit-execd");
    let mut child = Command::new(exe)
        .env("MODBIT_EXECD_ADDR", "127.0.0.1:0")
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    let addr = read_boot_line(&mut child)?;
    std::env::set_var("MODBIT_EXECD_ADDR", addr);
    Some(child)
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

fn spawn_core(repo_root: &Path, worktree_root: &Path) -> CoreProc {
    let execd = spawn_execd();
    let db_dir = tempdir("e2e-db");
    let exe = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/modbit-core");
    let mut command = Command::new(exe);
    let mut command = command.env("MODBIT_CORE_DB", db_dir.join("core.db"));
    if !cfg!(windows) {
        // Unix socket path must fit sun_path; Windows uses named pipes and
        // MODBIT_SOCKET must stay unset there (ephemeral endpoint).
        command = command.env("MODBIT_SOCKET", db_dir.join("e2e.sock"));
    }
    let mut child = command
        .env("MODBIT_HTTP_ADDR", "127.0.0.1:0")
        .env("MODBIT_REPO_ROOT", repo_root)
        .env("MODBIT_WORKTREE_ROOT", worktree_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn modbit-core");

    // Boot line from stdout; daemon address from stderr.
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("boot line");
    assert!(line.contains("socket"), "boot line: {line}");
    // Keep draining stdout so the pipe never breaks the core's writes.
    std::thread::spawn(move || {
        for _ in reader.lines() {}
    });

    let stderr = child.stderr.take().unwrap();
    let mut err_reader = BufReader::new(stderr);
    let mut daemon = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let mut l = String::new();
        match err_reader.read_line(&mut l) {
            Ok(0) => break,
            Ok(_) => {
                if let Some(addr) = l
                    .strip_prefix("modbit-core: http daemon on ")
                    .map(str::trim)
                {
                    daemon = Some(addr.to_string());
                    break;
                }
            }
            Err(_) => break,
        }
    }
    // Keep draining stderr on a background thread so core never blocks.
    std::thread::spawn(move || {
        for line in err_reader.lines().map_while(Result::ok) {
            eprintln!("[core] {line}");
        }
    });

    CoreProc {
        child,
        daemon: daemon.expect("daemon bound address"),
        _execd: execd,
    }
}

impl Drop for CoreProc {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
        if let Some(execd) = self._execd.as_mut() {
            execd.kill().ok();
            execd.wait().ok();
        }
    }
}

// ---- surface protocol over the daemon ---------------------------------------

fn request(core: &CoreProc, req: pb::surface_request::Request) -> pb::SurfaceResponse {
    let client = Client::builder().timeout(Duration::from_secs(30)).build().unwrap();
    let body = pb::SurfaceRequest { request: Some(req) }.encode_to_vec();
    let response = client
        .post(format!("http://{}/commands", core.daemon))
        .header("Content-Type", "application/x-protobuf")
        .body(body)
        .send()
        .expect("post command");
    assert!(response.status().is_success(), "daemon status");
    let bytes = response.bytes().unwrap();
    pb::SurfaceResponse::decode(bytes.as_ref()).expect("decode response")
}

/// Streams /events until `predicate` holds over the accumulated event ids
/// (blocking, bounded). Returns every event seen.
fn stream_events_until(
    core: &CoreProc,
    mut predicate: impl FnMut(&[serde_json::Value]) -> bool,
    timeout: Duration,
) -> Vec<serde_json::Value> {
    let client = Client::builder()
        .timeout(timeout + Duration::from_secs(5))
        .build()
        .unwrap();
    let mut response = client
        .get(format!("http://{}/events?since=0", core.daemon))
        .send()
        .expect("subscribe events");
    assert!(response.status().is_success());
    let deadline = Instant::now() + timeout;
    let mut events: Vec<serde_json::Value> = Vec::new();
    let mut buffer = String::new();
    let mut byte_buf = [0u8; 4096];
    use std::io::Read;
    loop {
        if Instant::now() > deadline {
            panic!("event predicate not met within {timeout:?}");
        }
        if predicate(&events) {
            return events;
        }
        match response.read(&mut byte_buf) {
            Ok(0) => std::thread::sleep(Duration::from_millis(50)),
            Ok(n) => {
                buffer.push_str(&String::from_utf8_lossy(&byte_buf[..n]));
                while let Some(idx) = buffer.find('\n') {
                    let line: String = buffer.drain(..=idx).collect();
                    if let Some(payload) = line.trim().strip_prefix("data: ") {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) {
                            events.push(v);
                        }
                    }
                }
            }
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

struct CoreProj<'a>(&'a CoreProc);

impl CoreProj<'_> {
    fn state(&self, task_id: &str) -> i32 {
        request(
            self.0,
            pb::surface_request::Request::GetFleet(pb::GetFleetRequest {}),
        )
        .fleet
        .unwrap()
        .tasks
        .iter()
        .find(|t| t.task_id == task_id)
        .map(|t| t.state)
        .unwrap_or(-1)
    }
}

/// Creates + queues + starts a task over the daemon; returns its id.
fn start_task(core: &CoreProc, title: &str, prompt: &str) -> String {
    let created = request(
        core,
        pb::surface_request::Request::CreateTask(pb::CreateTaskCommand {
            session_id: String::new(),
            title: title.into(),
            prompt: prompt.into(),
        }),
    );
    assert!(created.ok, "create: {}", created.error);
    let task_id = created.task.expect("task view").task_id;
    for payload in [
        pb::surface_request::Request::QueueTask(pb::QueueTaskCommand { task_id: task_id.clone() }),
        pb::surface_request::Request::StartTask(pb::StartTaskCommand { task_id: task_id.clone() }),
    ] {
        let response = request(core, payload);
        assert!(response.ok, "queue/start: {}", response.error);
    }
    task_id
}

fn live_enabled() -> bool {
    std::env::var("MODBIT_LIVE_E2E").is_ok() && std::env::var("OPENAI_API_KEY").is_ok()
}

const E2E001_PROMPT: &str =
    "Add validation so negative quantities are rejected and add tests. Use the tools: read quantity.js, fix it, and run the tests with shell.run argv 'sh run_tests.sh' until they pass.";

fn wait_for_terminal(core: &CoreProc, task_id: &str, timeout: Duration) -> i32 {
    let deadline = Instant::now() + timeout;
    loop {
        let state = CoreProj(core).state(task_id);
        if matches!(
            state,
            s if s == pb::TaskStatus::ReadyForReview as i32
                || s == pb::TaskStatus::Failed as i32
                || s == pb::TaskStatus::Cancelled as i32
        ) {
            return state;
        }
        if Instant::now() > deadline {
            panic!("task never reached a terminal state (last {state})");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

// ---- E2E-001 -----------------------------------------------------------------

#[test]
fn e2e_001_fresh_local_coding_task_end_to_end() {
    if !live_enabled() {
        eprintln!("skipped: live e2e credentials not present (docs/51 E2E-001 pending live proof)");
        return;
    }
    let repo = ts_webapp_fixture("e2e001", false);
    let worktrees = tempdir("e2e001-wt");
    let core = spawn_core(&repo, &worktrees);

    let task_id = start_task(&core, "reject negative quantities", E2E001_PROMPT);

    // Events stream over the daemon while the run proceeds (docs/30).
    let _events = stream_events_until(&core, |events| events.len() >= 2, Duration::from_secs(20));

    let state = wait_for_terminal(&core, &task_id, Duration::from_secs(600));
    assert_eq!(
        state,
        pb::TaskStatus::ReadyForReview as i32,
        "E2E-001: task must reach ReadyForReview from real outcomes"
    );

    // Dedicated worktree exists with real changes (implementation + test).
    let worktree = worktrees.join(&task_id);
    assert!(worktree.exists(), "worktree allocated");
    let diff = request(
        &core,
        pb::surface_request::Request::GetDiff(pb::GetDiffRequest { task_id: task_id.clone() }),
    );
    assert!(diff.ok, "diff: {}", diff.error);
    let files = diff.diff.expect("diff view").files;
    assert!(
        files.iter().any(|f| f.path.contains("quantity")),
        "diff contains implementation+test, got {files:?}"
    );
}

// ---- E2E-002 -----------------------------------------------------------------

#[test]
fn e2e_002_command_failure_repair_not_task_failure() {
    if !live_enabled() {
        eprintln!("skipped: live e2e credentials not present (docs/51 E2E-002 pending live proof)");
        return;
    }
    let repo = ts_webapp_fixture("e2e002", true);
    let worktrees = tempdir("e2e002-wt");
    let core = spawn_core(&repo, &worktrees);

    let task_id = start_task(
        &core,
        "reject negative quantities (broken fixture)",
        E2E001_PROMPT,
    );

    let state = wait_for_terminal(&core, &task_id, Duration::from_secs(600));
    assert_ne!(
        state,
        pb::TaskStatus::Failed as i32,
        "E2E-002: a first failing command must not fail the task"
    );
    assert_eq!(
        state,
        pb::TaskStatus::ReadyForReview as i32,
        "agent must repair the seeded failure"
    );
}

// ---- E2E-003 -----------------------------------------------------------------

#[test]
fn e2e_003_stream_reconnect_replays_losslessly() {
    if !live_enabled() {
        eprintln!("skipped: live e2e credentials not present (docs/51 E2E-003 pending live proof)");
        return;
    }
    let repo = ts_webapp_fixture("e2e003", false);
    let worktrees = tempdir("e2e003-wt");
    let core = spawn_core(&repo, &worktrees);

    let task_id = start_task(&core, "reject negative quantities", E2E001_PROMPT);

    // Watch until the run is underway, then DROP the connection mid-stream
    // (renderer restart analog) and reconnect from the last cursor.
    let first = stream_events_until(&core, |e| e.len() >= 3, Duration::from_secs(30));
    let cursor = first
        .iter()
        .filter_map(|e| e.get("sequence").and_then(|s| s.as_u64()))
        .max()
        .unwrap_or(0);
    std::thread::sleep(Duration::from_millis(300)); // events land while offline

    let client = Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap();
    let mut response = client
        .get(format!("http://{}/events?since={cursor}", core.daemon))
        .send()
        .expect("reconnect");
    use std::io::Read;
    let mut body = String::new();
    response.read_to_string(&mut body).ok();

    let replayed: Vec<&str> = body
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .collect();
    // Every replayed event id must be unique and disjoint from the first
    // batch's ids at the same sequences: no duplicate tool actions.
    let first_ids: Vec<&str> = first
        .iter()
        .filter_map(|e| e.get("event_id").and_then(|v| v.as_str()))
        .collect();
    let replay_ids: Vec<String> = replayed
        .iter()
        .filter_map(|p| serde_json::from_str::<serde_json::Value>(p).ok())
        .filter_map(|v| {
            v.get("event_id").and_then(|v| v.as_str()).map(String::from)
        })
        .collect();
    assert!(!replay_ids.is_empty(), "reconnect replays events after cursor");
    for id in &replay_ids {
        assert!(!first_ids.contains(&id.as_str()), "no duplicate events after replay");
    }
    assert_eq!(
        replay_ids.len(),
        replay_ids.iter().collect::<std::collections::HashSet<_>>().len(),
        "replay carries no duplicates"
    );

    // The task itself is untouched by the reconnect.
    let state = CoreProj(&core).state(&task_id);
    assert!(
        state == pb::TaskStatus::Started as i32
            || state == pb::TaskStatus::ReadyForReview as i32
            || state == pb::TaskStatus::Waiting as i32,
        "task continues across stream reconnect, got {state}"
    );
    wait_for_terminal(&core, &task_id, Duration::from_secs(600));
}
