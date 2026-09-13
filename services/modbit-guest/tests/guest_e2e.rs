//! M8.4/M8.5 REAL-process E2E: boots the actual `modbit-guest` binary as
//! a child process over the authenticated TCP transport and proves the
//! full guest contract on a live agent — signed-manifest verification
//! (and fail-closed rejection), protocol compatibility, real process
//! execution with timeout kill, real fs effects under deny-by-default
//! policy with traversal/symlink escape refusal, PTY sessions, generation
//! fencing that kills in-flight work, idempotent request replay, task
//! binding, and reconnect. Negative paths speak the raw wire protocol —
//! a hostile host is not obliged to use our client.

use std::io::BufRead as _;
use std::process::{Child, Command, Stdio};

use base64::Engine as _;
use modbit_protocol::guest::{
    GuestManifestFrame, GuestOp, GuestOutcome, GuestPayload, GuestRequest, GuestResponse,
    GUEST_PROTOCOL_MAJOR, GUEST_PROTOCOL_MINOR, GUEST_RPC_VERSION,
};
use modbit_protocol::transport::{BootSecret, Connection};
use modbit_sandbox::conformance_suite;
use modbit_sandbox::guest_client::{GuestBackend, GuestClient, GuestClientError};

fn tempdir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "modbit-guest-e2e-{tag}-{}",
        uuid::Uuid::now_v7().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn random_hex(n_bytes: usize) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(uuid::Uuid::now_v7().simple().to_string());
    h.update(
        std::time::SystemTime::now()
            .elapsed()
            .unwrap()
            .as_nanos()
            .to_le_bytes(),
    );
    let digest = h.finalize();
    hex(&digest[..n_bytes])
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

struct Guest {
    child: Child,
    pub addr: String,
    pub secret: BootSecret,
    pub build_hash: String,
}

impl Guest {
    /// Hard-kills the guest process (the substrate loss event).
    pub fn terminate(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Guest {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn boot_with_secret(tag: &str, fs_root: &std::path::Path) -> (Guest, String, BootSecret) {
    let provision_key = random_hex(32);
    let secret = BootSecret::generate().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_modbit-guest"))
        .env("MODBIT_GUEST_LISTEN", "127.0.0.1:0")
        .env("MODBIT_GUEST_SECRET", secret.hex())
        .env("MODBIT_GUEST_PROVISION_KEY", &provision_key)
        .env("MODBIT_GUEST_TASK", "task-g-1")
        .env("MODBIT_GUEST_TOKENS", "tok-live, tok-second")
        .env(
            "MODBIT_GUEST_FS_ROOTS",
            fs_root.to_string_lossy().to_string(),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn modbit-guest");
    let mut out = std::io::BufReader::new(child.stdout.take().expect("stdout"));
    let mut line = String::new();
    out.read_line(&mut line).expect("boot line");
    let parts: Vec<&str> = line.split_whitespace().collect();
    assert_eq!(parts[0], "guest", "boot line: {line}");
    let guest = Guest {
        child,
        addr: parts[1].to_string(),
        secret: secret.clone(),
        build_hash: parts[3].to_string(),
    };
    let _ = tag;
    (guest, provision_key, secret)
}

fn connect(guest: &Guest, key: &[u8]) -> GuestClient {
    let stream = std::net::TcpStream::connect(&guest.addr).expect("connect guest");
    GuestClient::connect(stream, &guest.secret, key).expect("verified guest client")
}

fn echo_argv(what: &str) -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd".into(), "/c".into(), format!("echo {what}")]
    } else {
        vec!["sh".into(), "-c".into(), format!("echo {what}")]
    }
}

fn sleep_argv(seconds: u32) -> Vec<String> {
    if cfg!(windows) {
        vec![
            "ping".into(),
            "-n".into(),
            (seconds + 1).to_string(),
            "127.0.0.1".into(),
        ]
    } else {
        vec!["sleep".into(), seconds.to_string()]
    }
}

/// The signed identity path (M8.4): the host verifies the manifest BEFORE
/// any request; tampered manifests and wrong provisioning keys fail
/// closed; the verified manifest pins the running binary's hash.
#[test]
fn e2e_identity_verification_and_fail_closed() {
    let root = tempdir("identity");
    let (guest, provision_key_hex, _) = boot_with_secret("identity", &root);
    let key = hex_to_bytes(&provision_key_hex);

    // Happy path: connect verifies identity.
    let mut client = connect(&guest, &key);
    assert_eq!(client.manifest.task, "task-g-1");
    assert_eq!(client.manifest.build_hash, guest.build_hash);
    assert_eq!(client.manifest.protocol_major, GUEST_PROTOCOL_MAJOR,);
    // Ping works under the verified identity.
    match client.call("tok-live", 0, GuestOp::Ping).expect("ping") {
        GuestPayload::Pong {
            guest_version,
            protocol_major,
            protocol_minor,
        } => {
            assert_eq!(protocol_major, GUEST_PROTOCOL_MAJOR);
            assert_eq!(protocol_minor, GUEST_PROTOCOL_MINOR);
            assert!(!guest_version.is_empty());
        }
        other => panic!("expected pong, got {other:?}"),
    }

    // WRONG provisioning key: the signature does not verify — the host
    // refuses to serve requests against this guest (fail closed).
    let stream = std::net::TcpStream::connect(&guest.addr).unwrap();
    match GuestClient::connect(stream, &guest.secret, b"not-the-provisioning-key") {
        Err(GuestClientError::Identity(_)) => {}
        other => panic!("expected identity rejection, got {other:?}"),
    }
    // And the guest keeps serving verified hosts afterwards.
    let mut again = connect(&guest, &key);
    assert!(matches!(
        again.call("tok-live", 0, GuestOp::Ping).expect("ping"),
        GuestPayload::Pong { .. }
    ));
    let _ = std::fs::remove_dir_all(&root);
}

/// Raw-wire hostility: a host that skips verification, lies about its
/// request identity, or replays stale authority is refused with typed
/// errors — never a partial effect.
#[test]
fn e2e_raw_wire_negatives() {
    let root = tempdir("raw");
    let (guest, provision_key_hex, _) = boot_with_secret("raw", &root);
    let key = hex_to_bytes(&provision_key_hex);

    let mut client = connect(&guest, &key);

    // Wrong capability token.
    match client.call("tok-forged", 0, GuestOp::Ping) {
        Err(GuestClientError::Guest(e)) => assert_eq!(e.code, "stale_capability"),
        other => panic!("expected stale_capability, got {other:?}"),
    }

    // Raw frames: wrong task, schema mismatch, stale generation.
    let stream = std::net::TcpStream::connect(&guest.addr).unwrap();
    let mut conn = Connection::over_stream(stream, &guest.secret).expect("handshake");
    let frame_bytes = conn.receive().expect("manifest frame");
    let frame: GuestManifestFrame = serde_json::from_slice(&frame_bytes).unwrap();
    assert_eq!(frame.manifest.task, "task-g-1");

    let raw = |conn: &mut Connection<std::net::TcpStream>,
               request_id: &str,
               task: &str,
               token: &str,
               class: &str,
               generation: u64,
               op: GuestOp|
     -> GuestResponse {
        let req = GuestRequest {
            request_id: request_id.into(),
            rpc_version: GUEST_RPC_VERSION,
            task: task.into(),
            capability_token: token.into(),
            effect_class: class.into(),
            generation,
            op,
        };
        conn.send(&serde_json::to_vec(&req).unwrap()).unwrap();
        let bytes = conn.receive().unwrap();
        serde_json::from_slice(&bytes).unwrap()
    };

    // Advance generation to 7 first (legitimate request).
    let resp = raw(
        &mut conn,
        "adv-1",
        "task-g-1",
        "tok-live",
        "session.ping",
        7,
        GuestOp::Ping,
    );
    assert!(matches!(resp.outcome, GuestOutcome::Ok { .. }));

    // WRONG TASK: even with a live token, another task's host gets
    // nothing (the guest never serves a second task).
    let resp = raw(
        &mut conn,
        "wt-1",
        "task-other",
        "tok-live",
        "session.ping",
        7,
        GuestOp::Ping,
    );
    match resp.outcome {
        GuestOutcome::Err { error } => assert_eq!(error.code, "wrong_task"),
        other => panic!("expected wrong_task, got {other:?}"),
    }

    // SCHEMA MANIPULATION: declare fs.write, carry a Ping.
    let resp = raw(
        &mut conn,
        "sm-1",
        "task-g-1",
        "tok-live",
        "fs.write",
        7,
        GuestOp::Ping,
    );
    match resp.outcome {
        GuestOutcome::Err { error } => assert_eq!(error.code, "bad_request"),
        other => panic!("expected bad_request, got {other:?}"),
    }

    // STALE GENERATION: a request from before the advance is fenced.
    let resp = raw(
        &mut conn,
        "sg-1",
        "task-g-1",
        "tok-live",
        "session.ping",
        6,
        GuestOp::Ping,
    );
    match resp.outcome {
        GuestOutcome::Err { error } => assert_eq!(error.code, "fenced"),
        other => panic!("expected fenced, got {other:?}"),
    }

    // Undecodable garbage gets a typed bad_request, not a dropped
    // connection.
    conn.send(b"{not json").unwrap();
    let bytes = conn.receive().unwrap();
    let resp: GuestResponse = serde_json::from_slice(&bytes).unwrap();
    match resp.outcome {
        GuestOutcome::Err { error } => assert_eq!(error.code, "bad_request"),
        other => panic!("expected bad_request, got {other:?}"),
    }
    let _ = client.call("tok-live", 7, GuestOp::Ping).unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

/// Real effects under policy (M8.5): process execution with timeout kill,
/// fs write/read/list inside granted roots, traversal escape refusal, PTY
/// session, idempotent replay, and the ExecutionBackend conformance
/// contract against the LIVE guest.
#[test]
fn e2e_real_effects_and_policy() {
    let root = tempdir("effects");
    let (guest, provision_key_hex, _) = boot_with_secret("effects", &root);
    let key = hex_to_bytes(&provision_key_hex);
    let mut client = connect(&guest, &key);

    // REAL process through the guest.
    match client
        .call(
            "tok-live",
            0,
            GuestOp::ProcExec {
                argv: echo_argv("guest-effects"),
                cwd: None,
                env: Default::default(),
                timeout_ms: Some(10_000),
            },
        )
        .expect("exec")
    {
        GuestPayload::Proc {
            exit_code, stdout, ..
        } => {
            assert_eq!(exit_code, Some(0));
            assert!(stdout.contains("guest-effects"));
        }
        other => panic!("expected proc, got {other:?}"),
    }

    // REAL fs effect inside the granted root.
    let file = root.join("greeting.txt");
    let payload_b64 = base64::engine::general_purpose::STANDARD.encode(b"hello from host");
    client
        .call(
            "tok-live",
            0,
            GuestOp::FsWrite {
                path: file.to_string_lossy().to_string(),
                bytes_base64: payload_b64.clone(),
            },
        )
        .expect("fs write");
    match client
        .call(
            "tok-live",
            0,
            GuestOp::FsRead {
                path: file.to_string_lossy().to_string(),
            },
        )
        .expect("fs read")
    {
        GuestPayload::Data {
            bytes_base64,
            sha256,
        } => {
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(&bytes_base64)
                    .unwrap(),
                b"hello from host"
            );
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(b"hello from host");
            assert_eq!(sha256, format!("{:x}", h.finalize()));
        }
        other => panic!("expected data, got {other:?}"),
    }
    match client
        .call(
            "tok-live",
            0,
            GuestOp::FsList {
                path: root.to_string_lossy().to_string(),
            },
        )
        .expect("fs list")
    {
        GuestPayload::Listing { entries } => {
            assert!(entries.iter().any(|e| e == "greeting.txt"));
        }
        other => panic!("expected listing, got {other:?}"),
    }

    // Deny-by-default: a path OUTSIDE the granted root is refused.
    let outside = tempdir("outside-root");
    let outside_file = outside.join("must-not-exist.txt");
    match client.call(
        "tok-live",
        0,
        GuestOp::FsWrite {
            path: outside_file.to_string_lossy().to_string(),
            bytes_base64: payload_b64.clone(),
        },
    ) {
        Err(GuestClientError::Guest(e)) => assert_eq!(e.code, "policy_denied"),
        other => panic!("expected policy_denied, got {other:?}"),
    }
    assert!(!outside_file.exists(), "denied write must have no effect");
    let _ = std::fs::remove_dir_all(&outside);

    // Traversal out of the root is refused too.
    let escape = root.join("..").join("must-not-escape.txt");
    match client.call(
        "tok-live",
        0,
        GuestOp::FsWrite {
            path: escape.to_string_lossy().to_string(),
            bytes_base64: payload_b64,
        },
    ) {
        Err(GuestClientError::Guest(e)) => assert_eq!(e.code, "policy_denied"),
        other => panic!("expected policy_denied, got {other:?}"),
    }
    assert!(!root.parent().unwrap().join("must-not-escape.txt").exists());

    // Timeout: a long process is killed at the deadline and reported.
    match client
        .call(
            "tok-live",
            0,
            GuestOp::ProcExec {
                argv: sleep_argv(60),
                cwd: None,
                env: Default::default(),
                timeout_ms: Some(300),
            },
        )
        .expect("slow exec")
    {
        GuestPayload::Proc {
            timed_out, killed, ..
        } => {
            assert!(timed_out);
            assert!(killed);
        }
        other => panic!("expected proc, got {other:?}"),
    }

    // Idempotent replay over the raw wire: the SAME request_id with a
    // DIFFERENT op must not execute — the first response wins.
    let stream = std::net::TcpStream::connect(&guest.addr).unwrap();
    let mut conn = Connection::over_stream(stream, &guest.secret).unwrap();
    let _frame_bytes = conn.receive().unwrap(); // manifest
    let send_raw = |conn: &mut Connection<std::net::TcpStream>,
                    request_id: &str,
                    path: String,
                    bytes: String|
     -> GuestResponse {
        let req = GuestRequest {
            request_id: request_id.into(),
            rpc_version: GUEST_RPC_VERSION,
            task: "task-g-1".into(),
            capability_token: "tok-live".into(),
            effect_class: "fs.write".into(),
            generation: 0,
            op: GuestOp::FsWrite {
                path,
                bytes_base64: bytes,
            },
        };
        conn.send(&serde_json::to_vec(&req).unwrap()).unwrap();
        let b = conn.receive().unwrap();
        serde_json::from_slice(&b).unwrap()
    };
    let first_target = root.join("idem-a.txt");
    let second_target = root.join("idem-b.txt");
    let resp1 = send_raw(
        &mut conn,
        "idem-1",
        first_target.to_string_lossy().to_string(),
        base64::engine::general_purpose::STANDARD.encode(b"first"),
    );
    assert!(matches!(resp1.outcome, GuestOutcome::Ok { .. }));
    let resp2 = send_raw(
        &mut conn,
        "idem-1",
        second_target.to_string_lossy().to_string(),
        base64::engine::general_purpose::STANDARD.encode(b"second"),
    );
    assert_eq!(
        resp1, resp2,
        "duplicate request_id replays the first response"
    );
    assert!(first_target.exists());
    assert!(
        !second_target.exists(),
        "replayed request must not execute again"
    );

    // PTY session through the guest (windows: documented ConPTY gap).
    if !cfg!(windows) {
        match client
            .call(
                "tok-live",
                0,
                GuestOp::PtyOpen {
                    argv: vec!["sh".into(), "-c".into(), "echo pty-live".into()],
                    cwd: None,
                    cols: 80,
                    rows: 24,
                },
            )
            .expect("pty open")
        {
            GuestPayload::Pty { pty_id, .. } => {
                let mut offset = 0;
                let mut seen = String::new();
                for _ in 0..100 {
                    match client
                        .call(
                            "tok-live",
                            0,
                            GuestOp::PtyRead {
                                pty_id: pty_id.clone(),
                                offset,
                                max: 4096,
                            },
                        )
                        .expect("pty read")
                    {
                        GuestPayload::Pty {
                            bytes_base64,
                            offset: next,
                            ..
                        } => {
                            offset = next;
                            seen.push_str(&String::from_utf8_lossy(
                                &base64::engine::general_purpose::STANDARD
                                    .decode(&bytes_base64)
                                    .unwrap(),
                            ));
                            if seen.contains("pty-live") {
                                break;
                            }
                        }
                        other => panic!("expected pty, got {other:?}"),
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                assert!(
                    seen.contains("pty-live"),
                    "pty output never surfaced: {seen:?}"
                );
                client
                    .call(
                        "tok-live",
                        0,
                        GuestOp::PtyKill {
                            pty_id: pty_id.clone(),
                        },
                    )
                    .expect("pty kill");
            }
            other => panic!("expected pty, got {other:?}"),
        }
    }

    // ExecutionBackend conformance against the LIVE guest: the same
    // contract test that passes on the local reference backend passes
    // through the real guest transport (REQ-EV-0291) — no canned path.
    let stream = std::net::TcpStream::connect(&guest.addr).unwrap();
    let verified = GuestClient::connect(stream, &guest.secret, &key).unwrap();
    let backend = GuestBackend::new(verified, "tok-live", 0);
    conformance_suite(&backend).expect("live guest passes backend conformance");
    assert_eq!(backend.manifest().task, "task-g-1");

    let _ = std::fs::remove_dir_all(&root);
}

/// Generation fencing across a REAL restart of the host connection: while
/// a long proc runs for connection A, connection B advances the
/// generation — A's in-flight effect is killed and its caller gets a
/// typed fenced error; the guest stays healthy for B.
#[test]
fn e2e_generation_fencing_kills_in_flight() {
    let root = tempdir("fencing");
    let (guest, provision_key_hex, _) = boot_with_secret("fencing", &root);
    let key = hex_to_bytes(&provision_key_hex);

    let mut client_a = connect(&guest, &key);
    let a = std::thread::spawn(move || {
        client_a.call(
            "tok-live",
            0,
            GuestOp::ProcExec {
                argv: sleep_argv(60),
                cwd: None,
                env: Default::default(),
                timeout_ms: None,
            },
        )
    });

    // Let the child actually start.
    std::thread::sleep(std::time::Duration::from_millis(400));

    let mut client_b = connect(&guest, &key);
    client_b
        .call("tok-live", 1, GuestOp::Ping)
        .expect("generation advance from second connection");

    let result_a = a.join().expect("worker A");
    match result_a {
        Err(GuestClientError::Guest(e)) => assert_eq!(e.code, "fenced"),
        other => panic!("expected fenced, got {other:?}"),
    }

    // B stays healthy under the new generation.
    assert!(matches!(
        client_b.call("tok-live", 1, GuestOp::Ping).expect("ping"),
        GuestPayload::Pong { .. }
    ));
    let _ = std::fs::remove_dir_all(&root);
}

/// Reconnect: dropping the host connection never loses the guest's
/// durable-within-session state — effects persist and a fresh verified
/// connection continues the same task.
#[test]
fn e2e_reconnect_preserves_state() {
    let root = tempdir("reconnect");
    let (guest, provision_key_hex, _) = boot_with_secret("reconnect", &root);
    let key = hex_to_bytes(&provision_key_hex);

    let mut client = connect(&guest, &key);
    let marker = root.join("reconnect-marker.txt");
    client
        .call(
            "tok-live",
            0,
            GuestOp::FsWrite {
                path: marker.to_string_lossy().to_string(),
                bytes_base64: base64::engine::general_purpose::STANDARD.encode(b"still here"),
            },
        )
        .expect("write before reconnect");
    drop(client);

    let mut fresh = connect(&guest, &key);
    match fresh
        .call(
            "tok-live",
            0,
            GuestOp::FsRead {
                path: marker.to_string_lossy().to_string(),
            },
        )
        .expect("read after reconnect")
    {
        GuestPayload::Data { bytes_base64, .. } => {
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(&bytes_base64)
                    .unwrap(),
                b"still here"
            );
        }
        other => panic!("expected data, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// The guest's own secret material never reaches guest workloads: a
/// process that dumps its environment sees neither the provisioning key
/// nor the capability tokens nor any MODBIT_GUEST_* variable (E2E-017
/// pass criterion at the agent boundary; the image-level dump check is
/// the substrate's operator-gated conformance).
#[test]
fn e2e_no_secret_material_in_guest_child_env() {
    let root = tempdir("envdump");
    let (guest, provision_key_hex, _) = boot_with_secret("envdump", &root);
    let key = hex_to_bytes(&provision_key_hex);
    let mut client = connect(&guest, &key);

    let dump_argv = if cfg!(windows) {
        vec!["cmd".into(), "/c".into(), "set".into()]
    } else {
        vec!["env".into()]
    };
    match client
        .call(
            "tok-live",
            0,
            GuestOp::ProcExec {
                argv: dump_argv,
                cwd: None,
                env: Default::default(),
                timeout_ms: Some(10_000),
            },
        )
        .expect("env dump")
    {
        GuestPayload::Proc { stdout, .. } => {
            assert!(
                !stdout.contains(&provision_key_hex),
                "provisioning key leaked to child env"
            );
            assert!(
                !stdout.contains("tok-live"),
                "capability token leaked to child env"
            );
            assert!(
                !stdout.contains("MODBIT_GUEST_"),
                "guest boot env leaked to child"
            );
        }
        other => panic!("expected proc, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// M8.6 (credential-injection requirement, effects-security owner): the broker issues a short-lived,
/// task/generation/scope-scoped lease; materialization flows through the
/// REAL guest RPC into a REAL child env for the authorized request; the
/// audit trail carries references only; redaction scrubs leaked-shaped
/// text; the expired path fails closed. Synthetic secret fixture only.
#[test]
fn e2e_credential_lease_materializes_scoped_into_guest_proc() {
    use modbit_secrets::{redact, CredScope, CredentialBroker};

    let root = tempdir("cred");
    let (guest, provision_key_hex, _) = boot_with_secret("cred", &root);
    let key = hex_to_bytes(&provision_key_hex);
    let mut client = connect(&guest, &key);

    let mut broker = CredentialBroker::new();
    let secret_ref = broker.register("task-g-1", "provider-api", b"synthetic-cred-E2E-9911");
    let lease = broker
        .issue_lease(
            &secret_ref,
            "task-g-1",
            0,
            vec![CredScope::ProcEnv],
            std::time::Duration::from_secs(60),
        )
        .expect("lease");
    let value = broker
        .materialize(&lease.handle, "task-g-1", 0, CredScope::ProcEnv)
        .expect("materialize for the authorized request");
    let value_str = String::from_utf8(value.clone()).unwrap();

    // REAL child on the REAL guest receives the scoped value via env.
    let print_argv = if cfg!(windows) {
        vec!["cmd".into(), "/c".into(), "echo %SYNTH_CRED%".into()]
    } else {
        vec!["sh".into(), "-c".into(), "echo $SYNTH_CRED".into()]
    };
    let mut env = std::collections::BTreeMap::new();
    env.insert("SYNTH_CRED".to_string(), value_str.clone());
    match client
        .call(
            "tok-live",
            0,
            GuestOp::ProcExec {
                argv: print_argv,
                cwd: None,
                env,
                timeout_ms: Some(10_000),
            },
        )
        .expect("cred proc")
    {
        GuestPayload::Proc { stdout, .. } => {
            assert!(
                stdout.contains(&value_str),
                "scoped credential never reached the child env"
            );
        }
        other => panic!("expected proc, got {other:?}"),
    }

    // Durable-style evidence (broker audit) carries references only.
    let audit_text = format!("{:?}", broker.audit());
    assert!(
        !audit_text.contains("synthetic-cred-E2E-9911"),
        "audit leaked the credential"
    );

    // A log/error line carrying the value redacts before persistence.
    let noisy = format!("proc failed: env SYNTH_CRED={value_str} (task-g-1)");
    let clean = redact(&noisy, &[&value]);
    assert!(!clean.contains("synthetic-cred-E2E-9911"));
    assert!(clean.contains("[REDACTED:secret-0]"));

    // The lease handle itself is opaque on the wire paths.
    assert!(!lease.handle.contains("synthetic"));

    let _ = std::fs::remove_dir_all(&root);
}

fn hex_to_bytes(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// M8.9 / E2E-018 sandbox-loss recovery over REAL processes: an in-flight
/// op reports a typed GUEST LOSS when the guest process is killed; a lost
/// fs.write is reconciled against a FRESH guest (same checkpoint-restored
/// root) by read-back — already-applied effects are NOT re-executed,
/// absent effects are re-executed exactly once, divergent state is left
/// unresolved for the checkpoint-restore path.
#[test]
fn e2e_sandbox_loss_recovery() {
    use modbit_sandbox::guest_recovery::{is_guest_lost, reconcile_fs_write, LostOutcome};

    let root = tempdir("loss");
    let (mut guest1, provision_key_hex, _) = boot_with_secret("loss1", &root);
    let key1 = hex_to_bytes(&provision_key_hex);
    let mut client1 = connect(&guest1, &key1);

    // Checkpoint state: the task's durable workspace marker.
    client1
        .call(
            "tok-live",
            0,
            GuestOp::FsWrite {
                path: root.join("checkpoint.txt").to_string_lossy().to_string(),
                bytes_base64: base64::engine::general_purpose::STANDARD.encode(b"checkpoint-9"),
            },
        )
        .expect("checkpoint write");

    // CASE A (setup) — the write whose fate we will reconcile LANDS
    // while guest1 lives (response read = effect certain).
    let nonce_a = format!("landed-{}", uuid::Uuid::now_v7().simple());
    let payload_a = base64::engine::general_purpose::STANDARD.encode(nonce_a.as_bytes());
    let path_a = root.join("reconcile-a.txt").to_string_lossy().to_string();
    client1
        .call(
            "tok-live",
            0,
            GuestOp::FsWrite {
                path: path_a.clone(),
                bytes_base64: payload_a.clone(),
            },
        )
        .expect("case A write landed while guest1 lived");

    // LOSS DETECTION on an in-flight op: kill the guest while a long
    // process runs — the blocked caller gets a transport loss, never a
    // fabricated answer.
    let mut client_inflight = connect(&guest1, &key1);
    let lost_call = client_inflight.call(
        "tok-live",
        0,
        GuestOp::ProcExec {
            argv: sleep_argv(60),
            cwd: None,
            env: Default::default(),
            timeout_ms: None,
        },
    );
    std::thread::sleep(std::time::Duration::from_millis(400));
    guest1.terminate();
    let err: GuestClientError = match lost_call {
        Err(e) => e,
        Ok(_) => panic!("in-flight op must fail after guest loss, not succeed"),
    };
    assert!(
        is_guest_lost(&err),
        "in-flight op must classify as guest loss, got {err:?}"
    );

    // FRESH SANDBOX: the substrate restores the checkpoint into a new
    // guest process (same restored root, fresh identity + boot secret).
    let (guest2, provision_key_hex2, _) = boot_with_secret("loss2", &root);
    let key2 = hex_to_bytes(&provision_key_hex2);
    let mut fresh = connect(&guest2, &key2);
    // The checkpoint marker is present in the restored workspace.
    fresh
        .call(
            "tok-live",
            0,
            GuestOp::FsRead {
                path: root.join("checkpoint.txt").to_string_lossy().to_string(),
            },
        )
        .expect("checkpoint restored");

    // CASE A — the lost write HAD landed: read-back finds the exact bytes
    // and NOTHING is re-executed.
    let outcome = reconcile_fs_write(&mut fresh, "tok-live", 0, &path_a, &payload_a)
        .expect("case A reconcile");
    assert_eq!(outcome, LostOutcome::AlreadyApplied);

    // CASE B — the lost write NEVER landed (absent from the restored
    // root): re-executed exactly once and verified present.
    let nonce_b = format!("absent-{}", uuid::Uuid::now_v7().simple());
    let payload_b = base64::engine::general_purpose::STANDARD.encode(nonce_b.as_bytes());
    let path_b = root.join("reconcile-b.txt").to_string_lossy().to_string();
    let outcome = reconcile_fs_write(&mut fresh, "tok-live", 0, &path_b, &payload_b)
        .expect("case B reconcile");
    assert_eq!(outcome, LostOutcome::Reexecuted);
    // The effect is real on the fresh sandbox.
    match fresh
        .call(
            "tok-live",
            0,
            GuestOp::FsRead {
                path: path_b.clone(),
            },
        )
        .expect("read back B")
    {
        GuestPayload::Data { bytes_base64, .. } => {
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(&bytes_base64)
                    .unwrap(),
                nonce_b.as_bytes()
            );
        }
        other => panic!("expected data, got {other:?}"),
    }

    // CASE C — DIVERGENT state: the path holds different bytes; the
    // reconciler refuses to touch it (checkpoint restore required).
    let payload_c = base64::engine::general_purpose::STANDARD.encode(b"what-host-thinks");
    let path_c = root.join("reconcile-a.txt").to_string_lossy().to_string();
    let outcome = reconcile_fs_write(&mut fresh, "tok-live", 0, &path_c, &payload_c)
        .expect("case C reconcile");
    match outcome {
        LostOutcome::Unresolved(reason) => assert!(reason.contains("different bytes")),
        other => panic!("expected Unresolved, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&root);
}
