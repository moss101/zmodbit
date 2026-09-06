//! `modbit-core` — the canonical Core process host (M1.4).
//!
//! Boot sequence (docs/30 § Local SurfaceProtocol):
//! 1. opens the durable store (`MODBIT_CORE_DB`, default `core.db`);
//! 2. obtains the boot-scoped secret: `MODBIT_BOOT_SECRET` (64 hex chars) if
//!    provided, otherwise generates one;
//! 3. binds the SurfaceProtocol endpoint (`MODBIT_SOCKET` fs path on unix,
//!    namespace name on windows, else an ephemeral per-boot endpoint);
//! 4. prints ONE json line `{"socket": ..., "secret": ...}` followed by a
//!    `ready` line — stdout is the inherited secure channel the desktop main
//!    process owns (the secret is never exposed to the renderer);
//! 5. serves authenticated SurfaceProtocol connections until shutdown.

use std::sync::Arc;

use modbit_core_runtime::CoreServices;
use modbit_event_store::EventStore;
use modbit_protocol::transport::{self, BootSecret, EndpointName};

fn main() {
    let db = std::env::var("MODBIT_CORE_DB").unwrap_or_else(|_| "core.db".into());
    let store = match EventStore::open(std::path::Path::new(&db)) {
        Ok(store) => Arc::new(store),
        Err(e) => {
            eprintln!("modbit-core: cannot open store {db}: {e}");
            std::process::exit(1);
        }
    };

    // Phase 2.6 (Future-tasks §2.4): every host path gets a process
    // broker — when no execd address is provided, the CORE spawns a
    // modbit-execd child next to its own binary (the desktop never has
    // to export MODBIT_EXECD_ADDR by hand). The child is a daemon
    // lifetime companion; shell.run fails closed if it cannot start.
    if std::env::var("MODBIT_EXECD_ADDR").map(|v| v.is_empty()).unwrap_or(true) {
        match spawn_embedded_execd() {
            Ok(addr) => {
                eprintln!("modbit-core: spawned modbit-execd on {addr}");
                std::env::set_var("MODBIT_EXECD_ADDR", addr);
            }
            Err(e) => {
                eprintln!(
                    "modbit-core: embedded modbit-execd unavailable ({e}); shell.run will fail closed"
                );
            }
        }
    }

    let mut services = CoreServices::new(store.clone());
    if let Some(source) = modbit_core_runtime::scheduler::EnvWorktreeSource::from_env() {
        services = services.with_task_worktrees(std::sync::Arc::new(source));
    }

    // The single scheduler (docs/14): tails the store for task_started and
    // owns every run. Started in every host mode so runs begin whichever
    // surface executed the command (socket or HTTP daemon).
    let scheduler = modbit_core_runtime::scheduler::Scheduler::spawn(
        store.clone(),
        modbit_core_runtime::scheduler::SchedulerConfig::from_env(),
    );
    // Phase 2.3: the surface signals in-flight runs (Stop/Pause/Steer)
    // through the scheduler's live control registry.
    services = services.with_run_controls(scheduler.controls());
    let services = Arc::new(services);

    // Optional multi-client HTTP+SSE daemon (headless mode):
    // MODBIT_HTTP_ADDR=127.0.0.1:0 binds it alongside the socket transport.
    if let Ok(addr) = std::env::var("MODBIT_HTTP_ADDR") {
        match modbit_core_runtime::daemon::Daemon::bind(&addr, store.clone(), services.clone()) {
            Ok(daemon) => {
                let bound = daemon.local_addr().unwrap_or_default();
                eprintln!("modbit-core: http daemon on {bound}");
                std::thread::spawn(move || daemon.serve());
            }
            Err(e) => {
                eprintln!("modbit-core: cannot bind http daemon on {addr}: {e}");
                std::process::exit(1);
            }
        }
    }

    let (secret, secret_hex) = match std::env::var("MODBIT_BOOT_SECRET") {
        Ok(hex) => match BootSecret::from_hex(&hex) {
            Some(secret) => (secret, hex),
            None => {
                eprintln!("modbit-core: MODBIT_BOOT_SECRET must be 64 hex chars");
                std::process::exit(1);
            }
        },
        Err(_) => {
            let secret = BootSecret::generate().expect("boot secret entropy");
            let hex = secret.hex();
            (secret, hex)
        }
    };

    let endpoint = match std::env::var("MODBIT_SOCKET") {
        Ok(path) => EndpointName::fs_path(std::path::PathBuf::from(path)),
        Err(_) => EndpointName::ephemeral("core"),
    }
    .expect("endpoint name");
    let socket_display = endpoint.display_name();

    let listener = match transport::bind(&endpoint) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("modbit-core: cannot bind {socket_display}: {e}");
            std::process::exit(1);
        }
    };

    // stdout IS the inherited secure channel (docs/30 § Local
    // SurfaceProtocol). Writes tolerate a parent that stops reading
    // (println! panics on a closed pipe and would kill the core).
    use std::io::Write;
    let mut stdout = std::io::stdout();
    let _ = writeln!(
        stdout,
        "{}",
        serde_json::json!({ "socket": socket_display, "secret": secret_hex })
    );
    let _ = writeln!(stdout, "ready");
    let _ = stdout.flush();

    eprintln!("modbit-core: serving on {socket_display}");
    transport::serve(
        listener,
        secret,
        Arc::new(move |request| services.handle(request)),
    );
}

/// Spawns a `modbit-execd` child next to the running core binary and
/// returns its boot address (Phase 2.6). The child's stdout boot line is
/// `{"addr": "..."}`; its lifetime is the core's (the scheduler's runs
/// die with the process; resume is the M4 spine's job).
fn spawn_embedded_execd() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // Windows binaries carry the .exe suffix; both live beside the core.
    let execd_name = if cfg!(windows) { "modbit-execd.exe" } else { "modbit-execd" };
    let execd_path = exe
        .parent()
        .ok_or("no parent dir for core binary")?
        .join(execd_name);
    if !execd_path.is_file() {
        return Err(format!("no modbit-execd beside the core ({})", execd_path.display()));
    }
    let mut child = std::process::Command::new(&execd_path)
        .env("MODBIT_EXECD_ADDR", "127.0.0.1:0")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", execd_path.display()))?;
    let stdout = child.stdout.take().ok_or("execd stdout not piped")?;
    let mut line = String::new();
    use std::io::BufRead;
    std::io::BufReader::new(stdout)
        .read_line(&mut line)
        .map_err(|e| format!("read execd boot line: {e}"))?;
    let addr = serde_json::from_str::<serde_json::Value>(&line)
        .ok()
        .and_then(|v| v.get("addr").and_then(|a| a.as_str()).map(String::from))
        .ok_or_else(|| format!("malformed execd boot line: {line:?}"))?;
    // Daemon companion: leak the handle (the child outlives this frame by
    // design; it dies with the session or is reused across resume boots).
    std::mem::forget(child);
    Ok(addr)
}
