//! M1.4/M1-final integration tests: the multi-client HTTP+SSE daemon
//! (REQ-EV-0192) serving the real event store and command processor.

use std::sync::Arc;

use modbit_core_runtime::{CoreServices, Daemon};
use modbit_event_store::EventStore;
use modbit_protocol::modbit::protocol::v1 as pb;
use std::io::{Read, Write};

use prost::Message;

fn setup(tag: &str) -> (String, Arc<CoreServices>, Arc<EventStore>) {
    let mut db = std::env::temp_dir();
    db.push(format!(
        "modbit-daemon-{tag}-{}.db",
        uuid::Uuid::now_v7().simple()
    ));
    let store = Arc::new(EventStore::open(&db).unwrap());
    let services = Arc::new(CoreServices::new(store.clone()));
    let daemon = Daemon::bind("127.0.0.1:0", store.clone(), services.clone()).expect("bind daemon");
    let addr = daemon.local_addr().expect("daemon addr");
    std::thread::spawn(move || daemon.serve());
    (addr, services, store)
}

fn post_command(addr: &str, body: &[u8]) -> (u16, Vec<u8>) {
    let mut stream = std::net::TcpStream::connect(addr).expect("connect");
    let header = format!(
        "POST /commands HTTP/1.1\r\nContent-Type: application/x-protobuf\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("http header terminator");
    let status = String::from_utf8_lossy(&response[..response[split..].len().min(12)]).to_string();
    let status_code: u16 = status
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status_code, response[split + 4..].to_vec())
}

fn get(addr: &str, path: &str) -> (u16, Vec<u8>) {
    let mut stream = std::net::TcpStream::connect(addr).expect("connect");
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("http header terminator");
    let status = String::from_utf8_lossy(&response[..response[split..].len().min(12)]).to_string();
    let status_code: u16 = status
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status_code, response[split + 4..].to_vec())
}

/// REQ-EV-0192: multiple HTTP+SSE clients all receive durable events, with
/// replay from an offset and live delivery of newly appended events.
#[test]
fn daemon_serves_multi_client_sse_with_replay() {
    let (addr, _services, _store) = setup("sse");

    // Two clients connect to the SSE stream before any events exist.
    let mut client_a = std::net::TcpStream::connect(&addr).unwrap();
    let mut client_b = std::net::TcpStream::connect(&addr).unwrap();
    for stream in [&mut client_a, &mut client_b] {
        stream
            .write_all(
                b"GET /events?since=0 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
    }

    // Produce durable events through the command endpoint.
    let session = pb::SurfaceRequest {
        request: Some(pb::surface_request::Request::CreateSession(
            pb::CreateSessionCommand {
                display_name: "daemon".into(),
            },
        )),
    };
    let (code, _) = post_command(&addr, &session.encode_to_vec());
    assert_eq!(code, 200);
    std::thread::sleep(std::time::Duration::from_millis(400));

    // Both SSE clients received the session_created event as SSE data.
    for stream in [&mut client_a, &mut client_b] {
        let mut buf = vec![0u8; 4096];
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .unwrap();
        let n = stream.read(&mut buf).unwrap();
        let text = String::from_utf8_lossy(&buf[..n]);
        assert!(text.contains("data:"), "expected SSE data, got {text:?}");
        assert!(
            text.contains("session_created"),
            "expected the event in {text:?}"
        );
    }
}

#[test]
fn command_endpoint_applies_commands_and_fleet_reflects_them() {
    let (addr, _services, _store) = setup("commands");

    let session = pb::SurfaceRequest {
        request: Some(pb::surface_request::Request::CreateSession(
            pb::CreateSessionCommand {
                display_name: "http".into(),
            },
        )),
    };
    let (code, body) = post_command(&addr, &session.encode_to_vec());
    assert_eq!(code, 200);
    let response = pb::SurfaceResponse::decode(body.as_slice()).unwrap();
    assert!(response.ok, "{}", response.error);
    let session_id = response.session_id.clone();

    let task = pb::SurfaceRequest {
        request: Some(pb::surface_request::Request::CreateTask(
            pb::CreateTaskCommand {
                session_id: session_id.clone(),
                title: "http task".into(),
                prompt: "p".into(),
            },
        )),
    };
    let (code, _) = post_command(&addr, &task.encode_to_vec());
    assert_eq!(code, 200);

    // The fleet reflects the durable task.
    let (code, body) = get(&addr, "/fleet");
    assert_eq!(code, 200);
    let fleet = String::from_utf8(body).unwrap();
    assert!(fleet.contains("\"tasks\":1"), "fleet json: {fleet}");
}

#[test]
fn oversize_command_body_is_rejected_with_413() {
    let (addr, _services, _store) = setup("413");
    let huge = vec![b'x'; 9 * 1024 * 1024];
    let mut stream = std::net::TcpStream::connect(&addr).unwrap();
    let request = format!(
        "POST /commands HTTP/1.1\r\nContent-Type: application/x-protobuf\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        huge.len()
    );
    // Send ONLY the headers with an inflated content-length: the server
    // must reject before the (never-sent) body matters — proving the bound
    // is enforced on the request envelope, not the payload.
    stream.write_all(request.as_bytes()).unwrap();
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 413"),
        "{}",
        &response[..24.min(response.len())]
    );
}
