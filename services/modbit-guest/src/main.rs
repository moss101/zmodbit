//! modbit-guest — the sandbox guest RPC agent (M8.4/M8.5, docs/24 §
//! substrate, layout docs/33): runs INSIDE the isolated substrate, proves
//! its SIGNED versioned identity to the host on every connection, then
//! serves the typed process/fs/PTY RPC — every request gated by task
//! binding, capability token, effect-class schema match, generation fence
//! and deny-by-default path policy (crates/sandbox::guest).
//!
//! Boot env:
//!   MODBIT_GUEST_LISTEN         dev/test transport: TCP listen addr
//!                               (default 127.0.0.1:0). The PRODUCTION
//!                               transport is vsock (substrate adapter);
//!                               the RPC contract is identical.
//!   MODBIT_GUEST_SECRET         optional hex boot secret (else generated
//!                               ephemerally — only the substrate that
//!                               spawned this guest can read stdout).
//!   MODBIT_GUEST_PROVISION_KEY  hex HMAC key that signed this guest's
//!                               manifest (dev/test: ephemeral per boot;
//!                               production: the operator key service).
//!   MODBIT_GUEST_TASK           the single task this guest is bound to.
//!   MODBIT_GUEST_TOKENS         comma-separated live capability tokens.
//!   MODBIT_GUEST_FS_ROOTS       colon-separated granted roots
//!                               (deny-by-default: empty = no fs access).
//!
//! Boot line on stdout: `guest <addr> <guest_version> <build_hash>` —
//! mirrors the gateway/execd boot channel. `build_hash` is the SHA-256 of
//! THIS running executable.

use std::net::TcpListener;

use modbit_protocol::guest::{
    sign_manifest, GuestError, GuestManifest, GuestManifestFrame, GuestRequest, GuestResponse,
};
use modbit_protocol::transport::{BootSecret, Connection};
use modbit_sandbox::guest::GuestExecutor;
use modbit_sandbox::SandboxPolicy;

fn env_or_die(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        eprintln!("modbit-guest: {name} is required");
        std::process::exit(1);
    })
}

fn main() {
    let listen = std::env::var("MODBIT_GUEST_LISTEN").unwrap_or_else(|_| "127.0.0.1:0".into());
    let secret = match std::env::var("MODBIT_GUEST_SECRET") {
        Ok(hex) => BootSecret::from_hex(&hex).unwrap_or_else(|| {
            eprintln!("modbit-guest: MODBIT_GUEST_SECRET is not 64 hex chars");
            std::process::exit(1);
        }),
        Err(_) => BootSecret::generate().expect("boot secret"),
    };
    let key_hex = env_or_die("MODBIT_GUEST_PROVISION_KEY");
    let key = match hex_to_bytes(&key_hex) {
        Ok(k) if k.len() >= 16 => k,
        _ => {
            eprintln!("modbit-guest: MODBIT_GUEST_PROVISION_KEY must be >=16 bytes of hex");
            std::process::exit(1);
        }
    };
    let task = env_or_die("MODBIT_GUEST_TASK");
    let tokens: Vec<String> = env_or_die("MODBIT_GUEST_TOKENS")
        .split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    // Windows paths contain ':' (drive letters) — the list separator is
    // ';' there and ':' on unix.
    let roots_sep = if cfg!(windows) { ';' } else { ':' };
    let fs_roots: Vec<String> = std::env::var("MODBIT_GUEST_FS_ROOTS")
        .unwrap_or_default()
        .split(roots_sep)
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .collect();
    let policy = SandboxPolicy {
        fs_roots,
        network_allow: vec![],
        max_cpu_seconds: 0,
        max_memory_mb: 0,
    };

    // Identity: version + SHA-256 of THIS running executable, signed under
    // the provisioning key (M8.4).
    let build_hash = sha256_of_self();
    let manifest = GuestManifest {
        guest_version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_major: modbit_protocol::guest::GUEST_PROTOCOL_MAJOR,
        protocol_minor: modbit_protocol::guest::GUEST_PROTOCOL_MINOR,
        build_hash: build_hash.clone(),
        task: task.clone(),
    };
    let frame = GuestManifestFrame {
        signature_hex: sign_manifest(&manifest, &key),
        manifest,
    };

    let executor = std::sync::Arc::new(GuestExecutor::new(&task, tokens, policy));

    let vsock_port: Option<u32> = std::env::var("MODBIT_GUEST_VSOCK_PORT")
        .ok()
        .and_then(|p| p.parse().ok());

    // Both transports share ONE connection body (handshake, signed
    // manifest, request loop) — see serve_connection below. The vsock
    // transport is PRODUCTION inside the Linux guest VM; TCP is the
    // dev/test transport (the host reaches vsock through the Firecracker
    // UDS link — crates/sandbox::substrate::connect_guest_vsock).
    #[cfg(not(target_os = "linux"))]
    if let Some(port) = vsock_port {
        eprintln!(
            "modbit-guest: MODBIT_GUEST_VSOCK_PORT={port} requires the Linux guest kernel (fail closed)"
        );
        std::process::exit(1);
    }

    #[cfg(target_os = "linux")]
    if let Some(port) = vsock_port {
        let listener = match modbit_sandbox::substrate::guest_vsock::VsockListener::bind(port) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("modbit-guest: vsock bind port {port}: {e}");
                std::process::exit(1);
            }
        };
        println!(
            "guest vsock:{port} {} {build_hash}",
            env!("CARGO_PKG_VERSION")
        );
        loop {
            let stream = match listener.accept() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("modbit-guest: vsock accept: {e}");
                    continue;
                }
            };
            let secret = secret.clone();
            let frame = frame.clone();
            let executor = executor.clone();
            std::thread::spawn(move || {
                serve_connection(stream, &secret, &frame, &executor);
            });
        }
    }

    // Dev/test TCP transport.
    let listener = TcpListener::bind(&listen).unwrap_or_else(|e| {
        eprintln!("modbit-guest: cannot bind {listen}: {e}");
        std::process::exit(1);
    });
    let bound = listener.local_addr().expect("bound addr");
    println!("guest {bound} {} {build_hash}", env!("CARGO_PKG_VERSION"));

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let secret = secret.clone();
        let frame = frame.clone();
        let executor = executor.clone();
        std::thread::spawn(move || {
            serve_connection(stream, &secret, &frame, &executor);
        });
    }
}

/// One accepted host connection: server-side boot-secret handshake, the
/// SIGNED manifest (the host verifies before serving anything), then the
/// gated request loop. Shared by the TCP (dev) and vsock (production)
/// transports — the RPC contract is transport-independent.
fn serve_connection<S: std::io::Read + std::io::Write + Send + 'static>(
    stream: S,
    secret: &BootSecret,
    frame: &GuestManifestFrame,
    executor: &GuestExecutor,
) {
    let mut conn = match Connection::accept_stream(stream, secret) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("modbit-guest: handshake failed: {e}");
            return;
        }
    };
    let manifest_bytes = match serde_json::to_vec(frame) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("modbit-guest: manifest encode: {e}");
            return;
        }
    };
    if conn.send(&manifest_bytes).is_err() {
        return;
    }
    loop {
        let bytes = match conn.receive() {
            Ok(b) => b,
            Err(_) => return,
        };
        let resp = match serde_json::from_slice::<GuestRequest>(&bytes) {
            Ok(req) => executor.execute(&req),
            Err(e) => GuestResponse::err(
                "",
                GuestError::new(
                    modbit_protocol::guest::guest_error_codes::BAD_REQUEST,
                    format!("undecodable request: {e}"),
                ),
            ),
        };
        let out = match serde_json::to_vec(&resp) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("modbit-guest: response encode: {e}");
                return;
            }
        };
        if conn.send(&out).is_err() {
            return;
        }
    }
}

fn sha256_of_self() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::read(p).ok())
        .map(|bytes| {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(&bytes);
            format!("{:x}", h.finalize())
        })
        .unwrap_or_else(|| "unavailable".to_string())
}

fn hex_to_bytes(s: &str) -> Result<Vec<u8>, ()> {
    if !s.len().is_multiple_of(2) {
        return Err(());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| ()))
        .collect()
}
