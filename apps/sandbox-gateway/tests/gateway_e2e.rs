//! Sandbox gateway + cloud worker E2E (Phase 8a/8b, docs/24): REAL
//! processes over REAL TCP — the gateway authenticates peers (boot-secret
//! handshake), a cloud worker leases a tenant and serves requests through
//! THE Core runtime against a real durable store, and a guest drives a
//! task through the relay. Proven: task execution through the gateway,
//! cross-tenant refusal, explicit worker-disconnect behavior, and
//! loss/recovery (worker killed and re-attached; durable state exact).

use std::io::BufRead as _;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

use prost::Message;

use modbit_event_store::EventStore;
use modbit_protocol::transport::{BootSecret, Connection};
use modbit_sandbox_gateway::{Envelope, Registration};

fn tempdir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "modbit-gw-{tag}-{}",
        uuid::Uuid::now_v7().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Boot {
    addr: String,
    secret: String,
    child: Child,
}

/// Test binaries live next to this test's own executable in
/// `target/<profile>/` — CARGO_BIN_EXE only covers same-package bins.
fn bin(name: &str) -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let dir = exe.parent().expect("deps dir").parent().expect("profile dir");
    dir.join(name)
}

impl Drop for Boot {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn boot_gateway(tag: &str) -> Boot {
    let mut child = Command::new(bin("modbit-sandbox-gateway"))
        .env("MODBIT_GATEWAY_ADDR", "127.0.0.1:0")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn gateway");
    let mut out = std::io::BufReader::new(child.stdout.take().expect("stdout"));
    let mut line = String::new();
    out.read_line(&mut line).expect("boot line");
    let _ = tag;
    let parts: Vec<&str> = line.split_whitespace().collect();
    assert_eq!(parts[0], "gateway", "boot line: {line}");
    Boot {
        addr: parts[1].to_string(),
        secret: parts[2].to_string(),
        child,
    }
}

fn spawn_worker(boot: &Boot, tenant: &str, db: &std::path::Path) -> Child {
    Command::new(bin("modbit-cloud-worker"))
        .env("MODBIT_GATEWAY_ADDR", &boot.addr)
        .env("MODBIT_GATEWAY_SECRET", &boot.secret)
        .env("MODBIT_WORKER_TENANT", tenant)
        .env("MODBIT_CORE_DB", db)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn worker")
}

fn connect_guest(boot: &Boot, tenant: &str) -> Connection<std::net::TcpStream> {
    let secret = BootSecret::from_hex(&boot.secret).expect("hex");
    let stream = std::net::TcpStream::connect(&boot.addr).expect("guest connect");
    let mut conn = Connection::over_stream(stream, &secret).expect("guest handshake");
    conn.send(
        &serde_json::to_vec(&Registration {
            role: "guest".into(),
            tenant: tenant.to_string(),
            task: String::new(),
        })
        .unwrap(),
    )
    .expect("register");
    conn
}

fn request(
    conn: &mut Connection<std::net::TcpStream>,
    tenant: &str,
    req: modbit_protocol::modbit::protocol::v1::surface_request::Request,
) -> Result<modbit_protocol::modbit::protocol::v1::SurfaceResponse, String> {
    use modbit_protocol::modbit::protocol::v1 as pb;
    let payload = pb::SurfaceRequest {
        request: Some(req),
    }
    .encode_to_vec();
    conn.send(
        &serde_json::to_vec(&Envelope {
            tenant: tenant.to_string(),
            task: String::new(),
            payload: Envelope::encode_payload(&payload),
        })
        .unwrap(),
    )
    .map_err(|e| e.to_string())?;
    let frame = conn.receive().map_err(|e| e.to_string())?;
    if let Ok(err) = serde_json::from_slice::<modbit_sandbox_gateway::GatewayError>(&frame) {
        return Err(err.error);
    }
    let envelope: Envelope = serde_json::from_slice(&frame).map_err(|e| e.to_string())?;
    let raw = envelope.decode_payload().map_err(|e| e)?;
    pb::SurfaceResponse::decode(raw.as_slice()).map_err(|e| e.to_string())
}

#[test]
fn gateway_relays_real_core_work_and_enforces_tenant_isolation() {
    let boot = boot_gateway("main");
    let db = tempdir("db").join("cloud.db");
    let mut worker = spawn_worker(&boot, "tenant-1", &db);
    // Give the worker a moment to lease.
    std::thread::sleep(std::time::Duration::from_millis(400));

    // Tenant-1 guest: create a task THROUGH the gateway — the worker
    // dispatches it through THE Core runtime against the real store.
    let mut guest = connect_guest(&boot, "tenant-1");
    use modbit_protocol::modbit::protocol::v1 as pb;
    let resp = request(
        &mut guest,
        "tenant-1",
        pb::surface_request::Request::CreateTask(pb::CreateTaskCommand {
            session_id: String::new(),
            title: "cloud task".into(),
            prompt: "run in the cloud worker".into(),
            repo_id: String::new(),
            base_branch: String::new(),
            parent_task_id: String::new(),
            write_scope: String::new(),
        }),
    )
    .map_err(|e| e)
    .unwrap_or_else(|e| panic!("relay failed: {e}"));
    assert!(resp.ok, "{:?}", resp.error);
    let task_id = resp.task.expect("task view").task_id;

    // The task is DURABLE in the worker's store — the cloud path writes
    // the same canonical events as local.
    let store = Arc::new(EventStore::open(&db).unwrap());
    let state: String = store
        .with_conn(|conn| {
            conn.query_row(
                "SELECT state FROM tasks WHERE task_id = ?1",
                [task_id.as_str()],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(state, "created", "cloud-created task is durable");

    // TENANT ISOLATION: a tenant-2 guest gets no worker (explicit error),
    // and CANNOT reach tenant-1's worker by forging the envelope tenant.
    let mut outsider = connect_guest(&boot, "tenant-2");
    let err = request(
        &mut outsider,
        "tenant-2",
        pb::surface_request::Request::GetFleet(pb::GetFleetRequest {}),
    )
    .expect_err("tenant-2 has no worker");
    assert!(err.contains("no worker"), "{err}");

    let err = request(
        &mut outsider,
        "tenant-1",
        pb::surface_request::Request::GetFleet(pb::GetFleetRequest {}),
    )
    .expect_err("cross-tenant envelope must be refused");
    assert!(err.contains("cross-tenant"), "{err}");

    // LOSS/RECOVERY: kill the worker; the guest gets an EXPLICIT failure
    // (never a hang, never fabricated success); the re-attached worker
    // serves the DURABLE state — the task survives the worker's death.
    let _ = worker.kill();
    let _ = worker.wait();
    let err = request(
        &mut guest,
        "tenant-1",
        pb::surface_request::Request::GetFleet(pb::GetFleetRequest {}),
    )
    .expect_err("dead worker must fail explicitly");
    assert!(
        err.contains("no worker") || err.contains("unreachable"),
        "{err}"
    );

    let mut worker2 = spawn_worker(&boot, "tenant-1", &db);
    std::thread::sleep(std::time::Duration::from_millis(400));
    let resp = request(
        &mut guest,
        "tenant-1",
        pb::surface_request::Request::GetFleet(pb::GetFleetRequest {}),
    )
    .expect("recovered worker serves");
    assert!(resp.ok, "{:?}", resp.error);
    let fleet = resp.fleet.expect("fleet");
    assert!(
        fleet.tasks.iter().any(|t| t.task_id == task_id),
        "durable task survived the worker kill"
    );
    let _ = worker2.kill();
    let _ = worker2.wait();
}
