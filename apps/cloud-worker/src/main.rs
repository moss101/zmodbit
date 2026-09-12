//! modbit-cloud-worker — remote Core host (docs/24): THE SAME Core
//! runtime (CoreServices over the durable event store) executes here as
//! locally; the worker leases itself to the sandbox gateway under its
//! TENANT and serves guest request envelopes by running the real surface
//! dispatch. The worker never sees tenant-unscoped traffic — the gateway
//! only forwards envelopes addressed to its tenant.
//!
//! Boot env: MODBIT_GATEWAY_ADDR + MODBIT_GATEWAY_SECRET (from the
//! gateway boot line), MODBIT_WORKER_TENANT, MODBIT_CORE_DB (durable
//! store path — remote-continuation rides the same durable semantics as
//! local: restart resumes exact state).

use std::net::TcpStream;
use std::sync::Arc;

use modbit_core_runtime::CoreServices;
use modbit_event_store::EventStore;
use modbit_protocol::transport::{BootSecret, Connection};
use modbit_protocol::cloud::{Envelope, Registration};

fn env_or_die(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        eprintln!("cloud-worker: {name} is required");
        std::process::exit(1);
    })
}

fn main() {
    let addr = env_or_die("MODBIT_GATEWAY_ADDR");
    let secret_hex = env_or_die("MODBIT_GATEWAY_SECRET");
    let tenant = env_or_die("MODBIT_WORKER_TENANT");
    let db = env_or_die("MODBIT_CORE_DB");

    let secret = BootSecret::from_hex(&secret_hex).unwrap_or_else(|| {
        eprintln!("cloud-worker: MODBIT_GATEWAY_SECRET is not 64 hex chars");
        std::process::exit(1);
    });

    // THE canonical Core: real store, real surface dispatch — the cloud
    // worker hosts runtime, never a second one (docs/81).
    let store = match EventStore::open(std::path::Path::new(&db)) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("cloud-worker: cannot open store {db}: {e}");
            std::process::exit(1);
        }
    };
    let services = CoreServices::new(store);

    let stream = TcpStream::connect(&addr).unwrap_or_else(|e| {
        eprintln!("cloud-worker: cannot connect to gateway {addr}: {e}");
        std::process::exit(1);
    });
    let mut conn = Connection::over_stream(stream, &secret).unwrap_or_else(|e| {
        eprintln!("cloud-worker: gateway handshake failed: {e}");
        std::process::exit(1);
    });
    conn.send(
        &serde_json::to_vec(&Registration {
            role: "worker".into(),
            tenant: tenant.clone(),
            task: String::new(),
        })
        .expect("json"),
    )
    .expect("register");
    eprintln!("cloud-worker: leased to gateway for tenant {tenant}");

    // Relay loop: envelope → REAL Core dispatch → envelope.
    loop {
        let frame = match conn.receive() {
            Ok(f) => f,
            Err(e) => {
                eprintln!("cloud-worker: gateway disconnected ({e}); leased work continues from the durable store on next attach");
                return;
            }
        };
        let envelope: Envelope = match serde_json::from_slice(&frame) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("cloud-worker: bad envelope from gateway: {e}");
                continue;
            }
        };
        let raw = match envelope.decode_payload() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("cloud-worker: {e}");
                continue;
            }
        };
        let response = services.handle(&raw);
        let reply = Envelope {
            tenant: envelope.tenant,
            task: envelope.task,
            payload: Envelope::encode_payload(&response),
        };
        if let Err(e) = conn.send(&serde_json::to_vec(&reply).expect("json")) {
            eprintln!("cloud-worker: reply write failed: {e}");
            return;
        }
    }
}
