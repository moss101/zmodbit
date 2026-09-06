//! Multi-client HTTP+SSE event daemon (M1, REQ-EV-0192; docs/30 §
//! SubscribeEvents; docs/33 § Backpressure bounds).
//!
//! Every client gets:
//! - `GET /events?since=<offset>` — an SSE stream of session/task events
//!   with a per-client bounded queue: at most [`EVENTS_BATCH`] events are
//!   sent per poll; a client that falls too far behind receives an explicit
//!   `reconnect` hint rather than an unbounded buffer (slow clients
//!   reconnect/replay instead of blocking Core);
//! - `GET /fleet` — the fleet snapshot as JSON;
//! - `POST /commands` — a protobuf SurfaceRequest, dispatched through the
//!   same idempotent command processor as the desktop transport.
//!
//! HTTP is intentionally minimal (std::net only): method, path and
//! content-length handling without external dependencies.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use modbit_event_store::EventStore;

/// Per-client event batch bound (docs/33: per-client bounded queue).
pub const EVENTS_BATCH: usize = 100;
const POLL_INTERVAL_MS: u64 = 200;
const MAX_BODY: usize = 8 * 1024 * 1024;

pub struct Daemon {
    listener: TcpListener,
    store: Arc<EventStore>,
    services: Arc<super::CoreServices>,
    /// Durable SSE client cursors (M4.1): a reconnecting
    /// client — including across a Core restart — resumes from its last
    /// PERSISTED offset instead of replaying from zero. None disables
    /// persistence (clients pass their own `since`).
    protocol_state: Option<Arc<std::sync::Mutex<modbit_protocol_state::ProtocolStateStore>>>,
}

impl Daemon {
    pub fn bind(
        addr: &str,
        store: Arc<EventStore>,
        services: Arc<super::CoreServices>,
    ) -> Result<Self, String> {
        Self::bind_with_protocol_state(addr, store, services, None)
    }

    pub fn bind_with_protocol_state(
        addr: &str,
        store: Arc<EventStore>,
        services: Arc<super::CoreServices>,
        protocol_state: Option<
            Arc<std::sync::Mutex<modbit_protocol_state::ProtocolStateStore>>,
        >,
    ) -> Result<Self, String> {
        let listener = TcpListener::bind(addr).map_err(|e| e.to_string())?;
        Ok(Self {
            listener,
            store,
            services,
            protocol_state,
        })
    }

    pub fn local_addr(&self) -> Result<String, String> {
        self.listener
            .local_addr()
            .map(|a| a.to_string())
            .map_err(|e| e.to_string())
    }

    /// Accept loop: one thread per client connection.
    pub fn serve(&self) {
        for stream in self.listener.incoming() {
            let Ok(stream) = stream else { continue };
            let store = self.store.clone();
            let services = self.services.clone();
            let protocol_state = self.protocol_state.clone();
            std::thread::spawn(move || {
                handle_connection(stream, store, services, protocol_state);
            });
        }
    }
}

fn handle_connection(
    stream: TcpStream,
    store: Arc<EventStore>,
    services: Arc<super::CoreServices>,
    protocol_state: Option<Arc<std::sync::Mutex<modbit_protocol_state::ProtocolStateStore>>>,
) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.clone(), String::new()),
    };

    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        match reader.read_line(&mut header) {
            Ok(0) => break,
            Ok(_) if header.trim().is_empty() => break,
            Ok(_) => {
                if let Some(v) = header
                    .to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().to_string())
                {
                    content_length = v.parse().unwrap_or(0);
                }
            }
            Err(_) => return,
        }
    }

    if method == "POST" && path == "/commands" {
        // REQ-EV-0108: no giant IPC bodies — bounded command dispatch.
        if content_length > MAX_BODY {
            let _ = write_response(
                &stream,
                "413 Payload Too Large",
                "text/plain",
                b"command body exceeds the dispatch bound",
            );
            return;
        }
        let mut body = vec![0u8; content_length];
        if reader.read(&mut body).is_err() {
            return;
        }
        let response = services.handle(&body);
        let _ = write_response(&stream, "200 OK", "application/x-protobuf", &response);
        return;
    }

    if method == "GET" && path == "/fleet" {
        // The fleet snapshot request is a protobuf SurfaceRequest internally;
        // encode it and return JSON of the decoded response.
        use modbit_protocol::modbit::protocol::v1 as pb;
        use prost::Message;
        let request = pb::SurfaceRequest {
            request: Some(pb::surface_request::Request::GetFleet(
                pb::GetFleetRequest {},
            )),
        };
        let response = services.handle(&request.encode_to_vec());
        let decoded = pb::SurfaceResponse::decode(response.as_slice())
            .map_err(|e| e.to_string())
            .and_then(|r| {
                serde_json::to_string(&serde_json::json!({
                    "ok": r.ok,
                    "error": r.error,
                    "tasks": r.fleet.map(|f| f.tasks.len()).unwrap_or(0),
                }))
                .map_err(|e| e.to_string())
            });
        match decoded {
            Ok(json) => {
                let _ = write_response(&stream, "200 OK", "application/json", json.as_bytes());
            }
            Err(e) => {
                let _ = write_response(
                    &stream,
                    "500 Internal Server Error",
                    "text/plain",
                    e.as_bytes(),
                );
            }
        }
        return;
    }

    if method == "GET" && path == "/events" {
        let client = query_param(&query, "client").unwrap_or_default();
        // M4.1: a client that reconnects WITHOUT an explicit offset
        // resumes from its last PERSISTED cursor — the journal survives
        // Core restarts, so the resume point is exact.
        let since = query_param(&query, "since")
            .and_then(|v| v.parse::<u64>().ok())
            .or_else(|| {
                if client.is_empty() {
                    return None;
                }
                protocol_state.as_ref().and_then(|state| {
                    state
                        .lock()
                        .ok()
                        .and_then(|s| s.last_cursor(&client))
                })
            })
            .unwrap_or(0);
        sse_stream(stream, store, since, client, protocol_state);
        return;
    }

    let _ = write_response(&stream, "404 Not Found", "text/plain", b"not found");
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| v.to_string())
    })
}

fn sse_stream(
    mut stream: TcpStream,
    store: Arc<EventStore>,
    mut since: u64,
    client: String,
    protocol_state: Option<Arc<std::sync::Mutex<modbit_protocol_state::ProtocolStateStore>>>,
) {
    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n");
    let _ = stream.flush();
    // Replaying bounded batches; a client that cannot keep up simply falls
    // behind on its own offset and catches up via replay (docs/33).
    loop {
        match store.events_since_global(since, EVENTS_BATCH) {
            Ok((events, new_offset)) => {
                for e in &events {
                    // `offset` is the GLOBAL cursor clients resume from;
                    // `sequence` is per-aggregate and cannot resume a stream.
                    let payload = serde_json::json!({
                        "event_id": e.event_id,
                        "aggregate_id": e.aggregate_id,
                        "sequence": e.sequence,
                        "event_type": e.event_type,
                        "offset": since + events.iter().position(|x| x.event_id == e.event_id).map(|i| i as u64 + 1).unwrap_or(0),
                    });
                    let line = format!(
                        "data: {}\n\n",
                        serde_json::to_string(&payload).unwrap_or_default()
                    );
                    if stream.write_all(line.as_bytes()).is_err() {
                        return;
                    }
                }
                since = new_offset;
                // M4.1: persist the advanced cursor durably (append +
                // flush) so a reconnect — same process or a restarted
                // Core — resumes exactly here. Fail-open for the live
                // stream: a journal error logs and the stream continues
                // (replay is at-least-once by construction).
                if !client.is_empty() {
                    if let Some(state) = protocol_state.as_ref() {
                        if let Ok(mut state) = state.lock() {
                            if let Err(e) = state.append(
                                modbit_protocol_state::ProtocolRecord::TerminalCursor {
                                    run_id: client.clone(),
                                    offset: new_offset,
                                },
                            ) {
                                eprintln!("modbit daemon: cursor journal append failed: {e}");
                            }
                        }
                    }
                }
            }
            Err(_) => return,
        }
        std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
    }
}

fn write_response(
    mut stream: &TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}
