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
    let mut services = CoreServices::new(store.clone());
    if let Some(source) = modbit_core_runtime::scheduler::EnvWorktreeSource::from_env() {
        services = services.with_task_worktrees(std::sync::Arc::new(source));
    }
    let services = Arc::new(services);

    // The single scheduler (docs/14): tails the store for task_started and
    // owns every run. Started in every host mode so runs begin whichever
    // surface executed the command (socket or HTTP daemon).
    let _scheduler = modbit_core_runtime::scheduler::Scheduler::spawn(
        store.clone(),
        modbit_core_runtime::scheduler::SchedulerConfig::from_env(),
    );

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
