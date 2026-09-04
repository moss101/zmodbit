//! M1.5 proof: kill the real Core process with SIGKILL mid-life, restart it
//! on the same durable store, and verify the fleet recovers exactly from
//! committed events — no fabricated state (docs/43 M1 proof; docs/13 §
//! Invariants; MOD-STATE-001 "memory is not recovery").

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

impl CoreHandle {
    fn connect(&self) -> transport::Connection {
        transport::connect(&self.endpoint, &self.secret).expect("authenticated connect")
    }
}

impl Drop for CoreHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawns Core with a FIXED db + socket path so a restart re-binds the same
/// endpoint and reopens the same durable store.
fn spawn_core(db: &std::path::Path, socket: &std::path::Path) -> CoreHandle {
    let bin = env!("CARGO_BIN_EXE_modbit-core");
    let mut command = Command::new(bin);
    command.env("MODBIT_CORE_DB", db);
    #[cfg(unix)]
    command.env("MODBIT_SOCKET", socket);
    #[cfg(windows)]
    {
        let stem = socket.file_stem().unwrap().to_string_lossy().to_string();
        command.env("MODBIT_NS_NAME", stem);
    }

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
    assert_eq!(ready.trim(), "ready");
    drop(reader);

    let socket_name = boot["socket"].as_str().expect("socket").to_string();
    let secret_hex = boot["secret"].as_str().expect("secret").to_string();

    let endpoint = EndpointName::from_boot_name(socket_name).expect("endpoint name");

    CoreHandle {
        child,
        endpoint,
        secret: BootSecret::from_hex(&secret_hex).expect("secret hex"),
    }
}

fn surface_call(
    conn: &mut transport::Connection,
    request: pb::SurfaceRequest,
) -> pb::SurfaceResponse {
    conn.send(&request.encode_to_vec()).expect("send");
    pb::SurfaceResponse::decode(conn.receive().expect("response").as_slice()).expect("decode")
}

fn create_task(core: &CoreHandle, title: &str) -> String {
    let mut conn = core.connect();
    let response = surface_call(
        &mut conn,
        pb::SurfaceRequest {
            request: Some(pb::surface_request::Request::CreateTask(
                pb::CreateTaskCommand {
                    session_id: String::new(),
                    title: title.into(),
                    prompt: "p".into(),
                },
            )),
        },
    );
    assert!(response.ok, "create failed: {}", response.error);
    response.task.expect("task view").task_id
}

/// Runs the full lifecycle through the surface: Created → Queued → Running →
/// ReadyForReview → Completed.
fn complete_task(core: &CoreHandle, task_id: &str) {
    let mut conn = core.connect();
    let steps: Vec<pb::surface_request::Request> = vec![
        pb::surface_request::Request::QueueTask(pb::QueueTaskCommand {
            task_id: task_id.into(),
        }),
        pb::surface_request::Request::StartTask(pb::StartTaskCommand {
            task_id: task_id.into(),
        }),
        pb::surface_request::Request::TaskReadyForReview(pb::TaskReadyForReviewCommand {
            task_id: task_id.into(),
        }),
        pb::surface_request::Request::CompleteTask(pb::CompleteTaskCommand {
            task_id: task_id.into(),
            summary: "done before crash".into(),
        }),
    ];
    for step in steps {
        let response = surface_call(
            &mut conn,
            pb::SurfaceRequest {
                request: Some(step),
            },
        );
        assert!(response.ok, "lifecycle step rejected: {}", response.error);
    }
}

/// Fleet as (task_id, title, state) triples, sorted by task id.
fn fleet_snapshot(core: &CoreHandle) -> Vec<(String, String, i32)> {
    let mut conn = core.connect();
    let response = surface_call(
        &mut conn,
        pb::SurfaceRequest {
            request: Some(pb::surface_request::Request::GetFleet(
                pb::GetFleetRequest {},
            )),
        },
    );
    assert!(response.ok, "fleet failed: {}", response.error);
    let mut rows: Vec<(String, String, i32)> = response
        .fleet
        .expect("fleet")
        .tasks
        .into_iter()
        .map(|t| (t.task_id, t.title, t.state))
        .collect();
    rows.sort();
    rows
}

#[test]
fn crash_and_restart_recovers_the_fleet_exactly() {
    let tag = uuid::Uuid::now_v7().simple().to_string()[24..].to_string();
    let mut db = std::env::temp_dir();
    db.push(format!("m15-{tag}.db"));
    let mut socket = std::env::temp_dir();
    socket.push(format!("m15-{tag}.sock"));

    // 1. First Core: create durable tasks with varied lifecycle states.
    let mut fleet_before: Vec<(String, String, i32)>;
    {
        let core = spawn_core(&db, &socket);
        let _t0 = create_task(&core, "durable task zero");
        let t1 = create_task(&core, "durable task one");
        complete_task(&core, &t1);
        fleet_before = fleet_snapshot(&core);
        assert_eq!(fleet_before.len(), 2);
        assert!(fleet_before
            .iter()
            .any(|(_, _, state)| *state == pb::TaskStatus::Completed as i32));
    }
    // CoreHandle::drop kills the process — the hard crash, no shutdown path.

    // 2. Restart on the SAME durable store and socket path.
    let fleet_after: Vec<(String, String, i32)>;
    {
        let core = spawn_core(&db, &socket);
        fleet_after = fleet_snapshot(&core);
    }

    // 3. The fleet recovers exactly: same ids, titles, and states — derived
    //    from committed events, nothing fabricated.
    fleet_before.sort();
    assert_eq!(fleet_before, fleet_after, "fleet must recover exactly");

    // 4. A third boot on the same store still recovers the same truth.
    let core3 = spawn_core(&db, &socket);
    let fleet_third = fleet_snapshot(&core3);
    assert_eq!(fleet_before, fleet_third);
}
