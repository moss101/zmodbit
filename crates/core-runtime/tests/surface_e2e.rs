//! M1.4 end-to-end proof: spawns the REAL `modbit-core` binary, reads the
//! boot channel (socket + secret) from its inherited stdout, authenticates
//! over the local transport, and drives Fleet/New-Task surface requests —
//! the same flow the Electron main process performs.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

use prost::Message;

use modbit_protocol::modbit::protocol::v1 as pb;
use modbit_protocol::transport::{self, BootSecret, EndpointName};

struct CoreHandle {
    child: Child,
    endpoint: EndpointName,
    secret: BootSecret,
}

impl Drop for CoreHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_core(tag: &str) -> CoreHandle {
    let bin = env!("CARGO_BIN_EXE_modbit-core");
    let mut db = std::env::temp_dir();
    db.push(format!("modbit-m1.4-{tag}-{}.db", uuid::Uuid::now_v7()));
    let mut socket = std::env::temp_dir();
    // Keep names short (macOS sun_path capacity is 104 bytes) and use the
    // RANDOM tail of the uuid — the prefix is timestamp-derived, so
    // concurrently spawned cores in the same millisecond would collide.
    let short = &uuid::Uuid::now_v7().simple().to_string()[24..];
    socket.push(format!("m14-{short}.sock"));

    let mut command = Command::new(bin);
    command.env("MODBIT_CORE_DB", &db);
    #[cfg(unix)]
    command.env("MODBIT_SOCKET", &socket);
    // Windows: let core mint a namespace name; fs paths are not pipe names.
    #[cfg(windows)]
    let _ = &socket;

    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn modbit-core");

    let stdout = child.stdout.take().expect("core stdout piped");
    let mut reader = BufReader::new(stdout);
    let mut boot_line = String::new();
    reader.read_line(&mut boot_line).expect("read boot line");
    let boot: serde_json::Value = serde_json::from_str(boot_line.trim()).expect("boot json");
    let mut ready = String::new();
    reader.read_line(&mut ready).expect("read ready line");
    assert_eq!(ready.trim(), "ready", "core must signal readiness");

    let socket_name = boot["socket"]
        .as_str()
        .expect("socket in boot json")
        .to_string();
    let secret_hex = boot["secret"]
        .as_str()
        .expect("secret in boot json")
        .to_string();
    drop(reader);

    #[cfg(unix)]
    let endpoint = EndpointName::fs_path(socket_name.into());
    #[cfg(windows)]
    let endpoint = EndpointName::namespace(&socket_name);

    CoreHandle {
        child,
        endpoint: endpoint.expect("endpoint name"),
        secret: BootSecret::from_hex(&secret_hex).expect("boot secret hex"),
    }
}

#[test]
fn core_serves_fleet_and_new_task_surface_requests() {
    let core = spawn_core("e2e");
    let mut conn = transport::connect(&core.endpoint, &core.secret).expect("authenticated connect");
    assert!(!conn.read_only, "matching major versions are read-write");

    // Composer: create_task with an empty session id — Core ensures the
    // session first (docs/32 § Task composer behavior).
    let request = pb::SurfaceRequest {
        request: Some(pb::surface_request::Request::CreateTask(
            pb::CreateTaskCommand {
                session_id: String::new(),
                title: "E2E fleet task".into(),
                prompt: "Prove the M1.4 vertical slice.".into(),
            },
        )),
    };
    conn.send(&request.encode_to_vec())
        .expect("send create_task");
    let response_bytes = conn.receive().expect("surface response");
    let response = pb::SurfaceResponse::decode(response_bytes.as_slice()).expect("decode response");
    assert!(response.ok, "create_task failed: {}", response.error);
    let task = response.task.as_ref().expect("created task view");
    assert_eq!(task.title, "E2E fleet task");
    assert_eq!(task.state, pb::TaskStatus::Created as i32);

    // Fleet snapshot reflects the durable task.
    let fleet_request = pb::SurfaceRequest {
        request: Some(pb::surface_request::Request::GetFleet(
            pb::GetFleetRequest {},
        )),
    };
    conn.send(&fleet_request.encode_to_vec())
        .expect("send get_fleet");
    let response_bytes = conn.receive().expect("surface response");
    let response = pb::SurfaceResponse::decode(response_bytes.as_slice()).expect("decode response");
    assert!(response.ok, "get_fleet failed: {}", response.error);
    let fleet = response.fleet.expect("fleet payload");
    assert_eq!(fleet.tasks.len(), 1, "exactly the created task");
    assert_eq!(fleet.tasks[0].title, "E2E fleet task");
    assert_eq!(fleet.default_session_id, fleet.tasks[0].session_id);
}

#[test]
fn core_rejects_unauthenticated_peers_and_keeps_serving() {
    let core = spawn_core("auth");
    let wrong_secret = BootSecret::generate().expect("entropy");
    match transport::connect(&core.endpoint, &wrong_secret) {
        Err(transport::TransportError::AuthRejected { .. }) => {}
        other => panic!("expected auth rejection, got {other:?}"),
    }
    // Server survives; the legit principal still authenticates.
    let mut conn = transport::connect(&core.endpoint, &core.secret).expect("legit connect");
    let request = pb::SurfaceRequest {
        request: Some(pb::surface_request::Request::GetFleet(
            pb::GetFleetRequest {},
        )),
    };
    conn.send(&request.encode_to_vec()).expect("send");
    let response = pb::SurfaceResponse::decode(conn.receive().unwrap().as_slice()).expect("decode");
    assert!(response.ok);
}
