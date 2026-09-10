//! Phase 4.1 repository-picker E2E: through the REAL daemon —
//! 1. register a local repo by path (default branch captured),
//! 2. register a second repo BY CLONE URL (a local path URL proves the
//!    real git-clone transport),
//! 3. list the recent repos (the picker's data),
//! 4. create + start a task pinned to repo A with base branch
//!    `feature/edge` — the worktree is allocated FROM that branch (its
//!    branch file is checked out) on a `modbit/<task>` branch,
//! 5. the registry survives a core restart (durable, docs/31),
//! 6. a task naming an UNKNOWN repo fails gracefully (parks/failed),
//!    never silently falling back to another repository.

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
    let dir = std::env::temp_dir().join(format!("rp1{tag}{suffix}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn make_repo(tag: &str, with_feature_branch: bool) -> PathBuf {
    let root = tempdir(tag);
    let repo = GitRepo::init(&root).expect("init");
    repo.set_config("user.email", "e2e@modbit.test").unwrap();
    repo.set_config("user.name", "Modbit E2E").unwrap();
    repo.set_config("core.autocrlf", "false").unwrap();
    std::fs::write(root.join("base.txt"), "base content\n").unwrap();
    repo.commit_all("base").expect("base commit");
    if with_feature_branch {
        // The edge file exists ONLY on feature/edge: main never sees it.
        repo.create_branch("feature/edge", None).expect("feature branch");
        repo.checkout("feature/edge").expect("checkout feature");
        std::fs::write(root.join("edge.txt"), "edge content\n").unwrap();
        repo.stage_path("edge.txt").unwrap();
        repo.commit_all("edge commit").expect("edge commit");
        repo.checkout("main").expect("back to main");
    }
    root
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

/// Boots a core daemon against `db_path` (restarts share the DB) with NO
/// MODBIT_REPO_ROOT — the picker path must not need it.
fn spawn_core(db_path: &PathBuf, worktree_root: &PathBuf, model_addr: SocketAddr) -> (Child, String) {
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
        // Deliberately NO MODBIT_REPO_ROOT: registered repos replace it.
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
    let which = match &req {
        pb::surface_request::Request::RegisterRepo(_) => "register",
        pb::surface_request::Request::ListRecentRepos(_) => "list",
        pb::surface_request::Request::CreateTask(_) => "create",
        pb::surface_request::Request::QueueTask(_) => "queue",
        pb::surface_request::Request::StartTask(_) => "start",
        pb::surface_request::Request::GetFleet(_) => "fleet",
        _ => "other",
    };
    eprintln!("REQ> {which}");
    let client = Client::builder().timeout(Duration::from_secs(60)).build().unwrap();
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

fn spawn_model_fixture(model_addr: SocketAddr) {
    let listener = std::net::TcpListener::bind(model_addr).unwrap();
    std::thread::spawn(move || {
        for _stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut reader = BufReader::new(_stream.try_clone().unwrap());
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                use std::io::Read;
                loop {
                    let n = reader.read(&mut chunk).unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf);
                    if text.contains("\r\n\r\n") && text.contains("}") && text.rfind('}').is_some() {
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
                }
                use std::io::Write;
                let mut stream = _stream;
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                );
                let _ = stream.write_all(text_turn("done").as_bytes());
            });
        }
    });
}

fn wait_state(daemon: &str, task_id: &str, want: i32, deadline_secs: u64) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(deadline_secs);
    loop {
        let fleet = request(daemon, pb::surface_request::Request::GetFleet(pb::GetFleetRequest {}))
            .fleet
            .unwrap();
        if let Some(t) = fleet.tasks.iter().find(|t| t.task_id == task_id) {
            if t.state == want {
                return t.state;
            }
            if Instant::now() > deadline {
                panic!("task {task_id} did not reach {want:?} (state {})", t.state);
            }
        } else if Instant::now() > deadline {
            panic!("task {task_id} vanished from the fleet");
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

#[test]
fn registered_repos_drive_task_worktrees_with_base_branches() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let repo_a = make_repo("a", true); // has feature/edge with edge.txt
    let repo_b = make_repo("b", false);
    let worktrees = tempdir("w");
    let db_dir = tempdir("db");
    let db_path = db_dir.join("core.db");
    // Bind the model fixture on a real port first.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let model_addr = listener.local_addr().unwrap();
    drop(listener);
    spawn_model_fixture(model_addr);

    let (mut core, daemon) = spawn_core(&db_path, &worktrees, model_addr);
    let reg_a = request(
        &daemon,
        pb::surface_request::Request::RegisterRepo(pb::RegisterRepoCommand {
            path: repo_a.display().to_string(),
            clone_url: String::new(),
        }),
    );
    assert!(reg_a.ok, "register A: {}", reg_a.error);
    let repo_a_view = reg_a.repo.expect("repo view");
    assert_eq!(repo_a_view.default_branch, "main");
    let repo_a_id = repo_a_view.repo_id.clone();

    // 2) Register repo B BY CLONE URL (a local path URL — real git clone).
    let reg_b = request(
        &daemon,
        pb::surface_request::Request::RegisterRepo(pb::RegisterRepoCommand {
            path: String::new(),
            clone_url: repo_b.display().to_string(),
        }),
    );
    assert!(reg_b.ok, "register B: {}", reg_b.error);
    let repo_b_view = reg_b.repo.expect("repo view b");
    assert!(
        repo_b_view.path.contains("registered"),
        "the clone lands under the worktree root's registered/ dir: {}",
        repo_b_view.path
    );
    assert!(worktrees.join("registered").exists(), "clone directory exists");

    // 3) The picker's list: both repos, with ids.
    let listed = request(
        &daemon,
        pb::surface_request::Request::ListRecentRepos(pb::ListRecentReposRequest {}),
    );
    assert!(listed.ok, "list: {}", listed.error);
    let repos = listed.recent_repos.expect("repo list").repos;
    assert_eq!(repos.len(), 2, "{repos:?}");
    let ids: Vec<String> = repos.iter().map(|r| r.repo_id.clone()).collect();
    assert!(ids.contains(&repo_a_id));
    assert!(ids.contains(&repo_b_view.repo_id));

    // 4) A task pinned to repo A with base branch feature/edge.
    let created = request(
        &daemon,
        pb::surface_request::Request::CreateTask(pb::CreateTaskCommand {
            session_id: String::new(),
            title: "work the edge branch".into(),
            prompt: "Do it.".into(),
            repo_id: repo_a_id.clone(),
            base_branch: "feature/edge".into(),
        }),
    );
    assert!(created.ok, "create: {}", created.error);
    let task_id = created.task.unwrap().task_id;
    for payload in [
        pb::surface_request::Request::QueueTask(pb::QueueTaskCommand { task_id: task_id.clone() }),
        pb::surface_request::Request::StartTask(pb::StartTaskCommand { task_id: task_id.clone() }),
    ] {
        let r = request(&daemon, payload);
        assert!(r.ok, "{}", r.error);
    }
    // Watch the core while the run executes: a silent death here (e.g. a
    // stack overflow in the scheduler poller) shows up as a signal exit.
    let mut state_now = -1;
    for _ in 0..600 {
        if let Ok(Some(status)) = core.try_wait() {
            panic!("CORE DIED during the run: {status}");
        }
        let fleet = request(&daemon, pb::surface_request::Request::GetFleet(pb::GetFleetRequest {}))
            .fleet
            .unwrap();
        if let Some(t) = fleet.tasks.iter().find(|t| t.task_id == task_id) {
            state_now = t.state;
            if state_now == pb::TaskStatus::ReadyForReview as i32 {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(state_now, pb::TaskStatus::ReadyForReview as i32, "task must complete");

    // The worktree was allocated FROM feature/edge: the edge file is
    // checked out and the branch is the task's own modbit/<id> branch.
    let worktree = worktrees.join(&task_id);
    assert!(
        worktree.join("edge.txt").exists(),
        "the base branch's file is checked out (per-task base branch selection)"
    );
    let branch_out = Command::new("git")
        .args(["-C", &worktree.display().to_string(), "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .expect("branch probe");
    let branch = String::from_utf8_lossy(&branch_out.stdout).trim().to_string();
    assert_eq!(branch, format!("modbit/{}", &task_id[..12.min(task_id.len())]));

    // 5) Durability: the registry survives a core restart on the same DB.
    core.kill().ok();
    core.wait().ok();
    let (mut core2, daemon2) = spawn_core(&db_path, &worktrees, model_addr);
    let listed2 = request(
        &daemon2,
        pb::surface_request::Request::ListRecentRepos(pb::ListRecentReposRequest {}),
    );
    assert!(listed2.ok, "list after restart: {}", listed2.error);
    let repos2 = listed2.recent_repos.expect("repo list after restart").repos;
    assert_eq!(repos2.len(), 2, "the registry is durable across restarts");

    // 6) An UNKNOWN repo id fails the task gracefully — no silent
    // fallback to another repository (and with no MODBIT_REPO_ROOT there
    // is nothing to fall back TO, so the run cannot allocate a worktree
    // and the task must FAIL rather than run somewhere else).
    let created_bad = request(
        &daemon2,
        pb::surface_request::Request::CreateTask(pb::CreateTaskCommand {
            session_id: String::new(),
            title: "unknown repo".into(),
            prompt: "Do it.".into(),
            repo_id: "repo-does-not-exist".into(),
            base_branch: String::new(),
        }),
    );
    assert!(
        created_bad.ok,
        "create with unknown repo still creates the task: {}",
        created_bad.error
    );
    let bad_id = created_bad.task.expect("task view").task_id;
    for payload in [
        pb::surface_request::Request::QueueTask(pb::QueueTaskCommand { task_id: bad_id.clone() }),
        pb::surface_request::Request::StartTask(pb::StartTaskCommand { task_id: bad_id.clone() }),
    ] {
        let r = request(&daemon2, payload);
        assert!(r.ok, "{}", r.error);
    }
    wait_state(&daemon2, &bad_id, pb::TaskStatus::Failed as i32, 60);

    core2.kill().ok();
    core2.wait().ok();
}
