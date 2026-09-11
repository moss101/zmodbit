//! Headless CLI (Phase 4 item 5): `modbit run <repo> "<task>" --json` —
//! the CI entry over the SAME daemon the desktop uses. Boots (or reuses)
//! a core daemon for the repository, creates + queues + starts the task,
//! waits for a terminal state, and prints the outcome as JSON.
//!
//! `modbit settings` reads/updates the persisted daemon configuration
//! (Phase 4.2); an `--api-key` goes to the OS keychain (Phase 4 item 3)
//! and never touches the environment, files, or the event store.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command as StdCommand, Stdio};
use std::time::{Duration, Instant};

use prost::Message;
use reqwest::blocking::Client;
use serde_json::json;

use modbit_protocol::modbit::protocol::v1 as pb;

fn usage() -> ! {
    eprintln!(
        "usage:\n  modbit run <repo> \"<task>\" [--json] [--branch <name>] [--db <path>] [--timeout <secs>]\n  modbit settings [--provider <p>] [--model <m>] [--base-url <u>] [--max-turns <n>] [--execution-mode <m>] [--api-key <k>] [--json] [--db <path>]"
    );
    std::process::exit(1);
}

struct RunArgs {
    repo: PathBuf,
    prompt: String,
    json: bool,
    branch: Option<String>,
    db: Option<PathBuf>,
    timeout: Duration,
}

struct SettingsPatch {
    provider: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
    max_turns: Option<u32>,
    execution_mode: Option<String>,
    api_key: Option<String>,
    json: bool,
}

enum CliCommand {
    Run(RunArgs),
    Settings(SettingsPatch),
}

fn parse_args() -> CliCommand {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("run") => {
            let mut repo = None;
            let mut prompt = None;
            let mut json = false;
            let mut branch = None;
            let mut db = None;
            let mut timeout_secs = 600u64;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--json" => json = true,
                    "--branch" => branch = Some(args.next().unwrap_or_else(|| usage())),
                    "--db" => db = Some(PathBuf::from(args.next().unwrap_or_else(|| usage()))),
                    "--timeout" => {
                        timeout_secs = args
                            .next()
                            .and_then(|v| v.parse().ok())
                            .unwrap_or_else(|| usage());
                    }
                    other if other.starts_with('-') => usage(),
                    other if repo.is_none() => repo = Some(PathBuf::from(other)),
                    other if prompt.is_none() => prompt = Some(other.to_string()),
                    _ => usage(),
                }
            }
            CliCommand::Run(RunArgs {
                repo: repo.unwrap_or_else(|| usage()),
                prompt: prompt.unwrap_or_else(|| usage()),
                json,
                branch,
                db,
                timeout: Duration::from_secs(timeout_secs),
            })
        }
        Some("settings") => {
            let mut patch = SettingsPatch {
                provider: None,
                model: None,
                base_url: None,
                max_turns: None,
                execution_mode: None,
                api_key: None,
                json: false,
            };
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--provider" => patch.provider = Some(args.next().unwrap_or_else(|| usage())),
                    "--model" => patch.model = Some(args.next().unwrap_or_else(|| usage())),
                    "--base-url" => patch.base_url = Some(args.next().unwrap_or_else(|| usage())),
                    "--max-turns" => {
                        patch.max_turns = Some(
                            args.next()
                                .and_then(|v| v.parse().ok())
                                .unwrap_or_else(|| usage()),
                        )
                    }
                    "--execution-mode" => {
                        patch.execution_mode = Some(args.next().unwrap_or_else(|| usage()))
                    }
                    "--api-key" => patch.api_key = Some(args.next().unwrap_or_else(|| usage())),
                    "--json" => patch.json = true,
                    _ => usage(),
                }
            }
            CliCommand::Settings(patch)
        }
        _ => usage(),
    }
}

/// Boots a core daemon bound to `repo` (drain threads for its stdout/
/// stderr live in the returned Child + the spawned drainer thread; the
/// pipes must never close while the core lives).
fn spawn_core(repo: &PathBuf, db: &PathBuf) -> Option<Child> {
    let core_bin = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join("modbit-core")))
        .unwrap_or_else(|| PathBuf::from("modbit-core"));
    StdCommand::new(&core_bin)
        .env("MODBIT_CORE_DB", db)
        .env("MODBIT_HTTP_ADDR", "127.0.0.1:0")
        .env("MODBIT_REPO_ROOT", repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()
}

/// Waits for the HTTP daemon address on the core's stderr
/// ("modbit-core: http daemon on <addr>") and readiness on stdout. The
/// stderr drainer thread stays alive draining the pipe (closing it early
/// would break the core).
fn wait_for_daemon(child: &mut Child) -> Option<String> {
    let stdout = child.stdout.take()?;
    let stderr = child.stderr.take()?;
    let (addr_tx, addr_rx) = std::sync::mpsc::channel();

    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    if let Some(addr) = line
                        .strip_prefix("modbit-core: http daemon on ")
                        .map(str::trim)
                    {
                        let _ = addr_tx.send(addr.to_string());
                    }
                }
            }
        }
    });

    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if line.trim() == "ready" {
                    break;
                }
            }
        }
    }
    addr_rx.recv_timeout(Duration::from_secs(30)).ok()
}

fn main() {
    let command = parse_args();
    let mut spawned_core: Option<Child> = None;
    let daemon_addr;

    // Daemon lifecycle: reuse MODBIT_DAEMON when set, else spawn a core
    // for the repo (run) / the ambient repo (settings).
    if let Ok(addr) = std::env::var("MODBIT_DAEMON") {
        daemon_addr = addr;
    } else {
        let (repo, db) = match &command {
            CliCommand::Run(run) => {
                if !run.repo.exists() {
                    eprintln!(
                        "modbit run: repository {} does not exist",
                        run.repo.display()
                    );
                    std::process::exit(1);
                }
                (
                    run.repo.clone(),
                    run.db
                        .clone()
                        .unwrap_or_else(|| run.repo.join(".modbit").join("cli.db")),
                )
            }
            CliCommand::Settings(_) => (
                std::env::var("MODBIT_REPO_ROOT")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default()),
                std::env::current_dir()
                    .unwrap_or_default()
                    .join(".modbit")
                    .join("cli.db"),
            ),
        };
        let Some(mut child) = spawn_core(&repo, &db) else {
            eprintln!("modbit: cannot start the core daemon");
            std::process::exit(1);
        };
        daemon_addr = wait_for_daemon(&mut child).unwrap_or_else(|| {
            eprintln!("modbit: daemon boot line missing");
            std::process::exit(1);
        });
        spawned_core = Some(child);
    }

    let client = Client::builder().timeout(Duration::from_secs(30)).build().unwrap();
    let request = |req: pb::surface_request::Request| -> Option<pb::SurfaceResponse> {
        let body = pb::SurfaceRequest { request: Some(req) }.encode_to_vec();
        match client
            .post(format!("http://{daemon_addr}/commands"))
            .header("Content-Type", "application/x-protobuf")
            .body(body)
            .send()
        {
            Ok(r) => match r.error_for_status() {
                Ok(r) => match r.bytes() {
                    Ok(bytes) => pb::SurfaceResponse::decode(bytes.as_ref()).ok(),
                    Err(e) => {
                        eprintln!("modbit: response read failed: {e}");
                        None
                    }
                },
                Err(e) => {
                    eprintln!("modbit: daemon returned {}: {}", e.status().unwrap_or_default(), e);
                    None
                }
            },
            Err(e) => {
                eprintln!("modbit: request failed: {e}");
                None
            }
        }
    };
    match &command {
        CliCommand::Run(run) => run_task(&request, run, &mut spawned_core),
        CliCommand::Settings(patch) => run_settings(&request, patch),
    }
}

fn run_task(
    request: &dyn Fn(pb::surface_request::Request) -> Option<pb::SurfaceResponse>,
    run: &RunArgs,
    spawned_core: &mut Option<Child>,
) -> ! {
    let created = request(pb::surface_request::Request::CreateTask(pb::CreateTaskCommand {
        session_id: String::new(),
        title: format!("CLI: {}", run.prompt.chars().take(60).collect::<String>()),
        prompt: run.prompt.clone(),
        repo_id: String::new(),
        base_branch: run.branch.clone().unwrap_or_default(),
        parent_task_id: String::new(),
    
        write_scope: String::new(),
    }))
    .unwrap_or_else(|| finish_unreachable(spawned_core));
    if !created.ok {
        eprintln!("modbit run: {}", created.error);
        std::process::exit(1);
    }
    let task_id = created.task.as_ref().expect("task view").task_id.clone();
    for req in [
        pb::surface_request::Request::QueueTask(pb::QueueTaskCommand { task_id: task_id.clone() }),
        pb::surface_request::Request::StartTask(pb::StartTaskCommand { task_id: task_id.clone() }),
    ] {
        let r = request(req).unwrap_or_else(|| finish_unreachable(spawned_core));
        if !r.ok {
            eprintln!("modbit run: {}", r.error);
            std::process::exit(1);
        }
    }

    let deadline = Instant::now() + run.timeout;
    #[allow(unused_assignments)]
    let mut state = -1;
    loop {
        let Some(fleet) = request(pb::surface_request::Request::GetFleet(pb::GetFleetRequest {}))
        else {
            finish_unreachable(spawned_core);
        };
        if let Some(f) = fleet.fleet {
            if let Some(t) = f.tasks.iter().find(|t| t.task_id == task_id) {
                state = t.state;
                let terminal = state == pb::TaskStatus::ReadyForReview as i32
                    || state == pb::TaskStatus::Failed as i32
                    || state == pb::TaskStatus::Cancelled as i32
                    || state == pb::TaskStatus::Waiting as i32;
                if terminal || Instant::now() > deadline {
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(300));
    }

    let state_name = pb::TaskStatus::try_from(state)
        .map(|s| s.as_str_name().to_string())
        .unwrap_or_else(|_| format!("UNKNOWN({state})"));
    if run.json {
        println!(
            "{}",
            json!({
                "task_id": task_id,
                "state": state_name,
                "repo": run.repo.display().to_string(),
                "base_branch": run.branch,
            })
        );
    } else {
        println!("task {task_id}: {state_name}");
    }

    if let Some(mut child) = spawned_core.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    if state == pb::TaskStatus::ReadyForReview as i32 {
        std::process::exit(0);
    }
    std::process::exit(2);
}

fn run_settings(
    request: &dyn Fn(pb::surface_request::Request) -> Option<pb::SurfaceResponse>,
    patch: &SettingsPatch,
) {
    let Some(response) = request(pb::surface_request::Request::UpdateSettings(
        pb::UpdateSettingsCommand {
            provider: patch.provider.clone().unwrap_or_default(),
            model: patch.model.clone().unwrap_or_default(),
            base_url: patch.base_url.clone().unwrap_or_default(),
            max_turns: patch.max_turns.unwrap_or(0),
            execution_mode: patch.execution_mode.clone().unwrap_or_default(),
            api_key: patch.api_key.clone().unwrap_or_default(),
        },
    )) else {
        eprintln!("modbit settings: daemon unreachable (set MODBIT_DAEMON or MODBIT_REPO_ROOT)");
        std::process::exit(1);
    };
    if !response.ok {
        eprintln!("modbit settings: {}", response.error);
        std::process::exit(1);
    }
    if let Some(view) = response.settings {
        if patch.json {
            println!(
                "{}",
                json!({
                    "provider": view.provider,
                    "model": view.model,
                    "base_url": view.base_url,
                    "max_turns": view.max_turns,
                    "execution_mode": view.execution_mode,
                    "has_api_key": view.has_api_key,
                })
            );
        } else {
            println!(
                "provider={} model={} base_url={} max_turns={} execution_mode={} api_key_in_keychain={}",
                display_or_unset(&view.provider),
                display_or_unset(&view.model),
                display_or_unset(&view.base_url),
                view.max_turns,
                display_or_unset(&view.execution_mode),
                view.has_api_key,
            );
        }
    }
}

fn display_or_unset(v: &str) -> &str {
    if v.is_empty() {
        "(unset)"
    } else {
        v
    }
}

/// Unreachable daemon: fatal for every subcommand.
fn finish_unreachable(spawned: &mut Option<Child>) -> ! {
    eprintln!("modbit: daemon unreachable");
    if let Some(mut c) = spawned.take() {
        let _ = c.kill();
        let _ = c.wait();
    }
    std::process::exit(1);
}
