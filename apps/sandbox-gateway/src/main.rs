//! modbit-sandbox-gateway — tenant-bound substrate boundary (docs/24):
//! authenticated TCP peers (workers and guests) register under a TENANT;
//! guest request envelopes relay ONLY to that tenant's leased worker, and
//! cross-tenant traffic is refused with a typed error plus an audit line.
//! The gateway never parses the relayed payload — it is the SurfaceProtocol
//! boundary, not a second runtime.
//!
//! Boot: MODBIT_GATEWAY_ADDR (default 127.0.0.1:0) + MODBIT_GATEWAY_SECRET
//! (hex; generated ephemerally when absent). Boot line on stdout:
//! `gateway <addr> <secret-hex>` — the deployment channel shares it with
//! workers and guests exactly like the execd boot channel.

use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use modbit_protocol::transport::{BootSecret, Connection};
use modbit_sandbox_gateway::{Envelope, GatewayError, Registration};

type WorkerConn = Arc<Mutex<Connection<std::net::TcpStream>>>;

/// (tenant → leased worker connection). Task-scoped leases refine this
/// table in a later slice; tenant scoping is the isolation boundary that
/// must hold regardless.
type Leases = Arc<Mutex<HashMap<String, WorkerConn>>>;

fn main() {
    let addr = std::env::var("MODBIT_GATEWAY_ADDR").unwrap_or_else(|_| "127.0.0.1:0".into());
    let secret = match std::env::var("MODBIT_GATEWAY_SECRET") {
        Ok(hex) => BootSecret::from_hex(&hex).unwrap_or_else(|| {
            eprintln!("gateway: MODBIT_GATEWAY_SECRET is not 64 hex chars");
            std::process::exit(1);
        }),
        Err(_) => BootSecret::generate().expect("boot secret"),
    };

    let listener = TcpListener::bind(&addr).unwrap_or_else(|e| {
        eprintln!("gateway: cannot bind {addr}: {e}");
        std::process::exit(1);
    });
    let bound = listener.local_addr().expect("bound addr");
    println!("gateway {bound} {}", secret.hex());

    let leases: Leases = Arc::new(Mutex::new(HashMap::new()));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let secret = secret.clone();
        let leases = leases.clone();
        std::thread::spawn(move || {
            // The GATEWAY is the server side of the handshake: it
            // speaks the Challenge first (mirror of the worker/guest
            // client path).
            let mut conn = match Connection::accept_stream(stream, &secret) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("gateway: handshake failed: {e}");
                    return;
                }
            };
            // Registration frame: role + tenant (+ optional task scope).
            let reg_bytes = match conn.receive() {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("gateway: no registration: {e}");
                    return;
                }
            };
            let reg: Registration = match serde_json::from_slice(&reg_bytes) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("gateway: bad registration: {e}");
                    return;
                }
            };
            if reg.tenant.is_empty() {
                let _ = conn.send(
                    &serde_json::to_vec(&GatewayError {
                        error: "registration requires a tenant".into(),
                    })
                    .expect("json"),
                );
                return;
            }
            match reg.role.as_str() {
                "worker" => serve_worker(conn, &reg, leases),
                "guest" => serve_guest(conn, &reg, leases),
                other => {
                    eprintln!("gateway: unknown role {other:?} for tenant {}", reg.tenant);
                }
            }
        });
    }
}

fn serve_worker(conn: Connection<std::net::TcpStream>, reg: &Registration, leases: Leases) {
    let leased: WorkerConn = Arc::new(Mutex::new(conn));
    leases
        .lock()
        .expect("leases")
        .insert(reg.tenant.clone(), leased);
    eprintln!("gateway: worker leased for tenant {}", reg.tenant);
    // The worker's connection is driven by guest relays on other threads;
    // this thread parks until the process ends.
    loop {
        std::thread::park();
    }
}

fn serve_guest(
    mut conn: Connection<std::net::TcpStream>,
    reg: &Registration,
    leases: Leases,
) {
    loop {
        let frame = match conn.receive() {
            Ok(f) => f,
            // Clean EOF or reset ends the guest session.
            Err(_) => return,
        };
        let envelope: Envelope = match serde_json::from_slice(&frame) {
            Ok(e) => e,
            Err(e) => {
                let _ = conn.send(
                    &serde_json::to_vec(&GatewayError {
                        error: format!("bad envelope: {e}"),
                    })
                    .expect("json"),
                );
                continue;
            }
        };
        // TENANT ISOLATION (docs/24 § Multi-tenancy): the envelope tenant
        // must match the REGISTRATION tenant — a guest cannot borrow
        // another tenant's worker by writing a different tenant in the
        // envelope. The registration is the only trusted source.
        if envelope.tenant != reg.tenant {
            eprintln!(
                "gateway: CROSS-TENANT REFUSED guest tenant {} requested {}",
                reg.tenant, envelope.tenant
            );
            let _ = conn.send(
                &serde_json::to_vec(&GatewayError {
                    error: "cross-tenant denied".into(),
                })
                .expect("json"),
            );
            continue;
        }
        let worker = leases.lock().expect("leases").get(&reg.tenant).cloned();
        let Some(worker) = worker else {
            // Explicit failure — Core stays sound, the guest retries or
            // fails visibly (docs/60 fault variant: worker disconnects).
            let _ = conn.send(
                &serde_json::to_vec(&GatewayError {
                    error: format!("no worker leased for tenant {}", reg.tenant),
                })
                .expect("json"),
            );
            continue;
        };
        // Relay: opaque payload in, opaque payload out.
        let relayed = {
            let mut w = worker.lock().expect("worker conn");
            w.send(&frame)
                .and_then(|_| w.receive())
                .map_err(|e| format!("worker unreachable: {e}"))
        };
        match relayed {
            Ok(bytes) => {
                if let Err(e) = conn.send(&bytes) {
                    eprintln!("gateway: guest write failed: {e}");
                    return;
                }
            }
            Err(e) => {
                eprintln!("gateway: relay failed for tenant {}: {e}", reg.tenant);
                // The worker lease is dead — release it so the next
                // request reports "no worker" instead of hanging.
                leases.lock().expect("leases").remove(&reg.tenant);
                let _ = conn
                    .send(&serde_json::to_vec(&GatewayError { error: e }).expect("json"));
            }
        }
    }
}
