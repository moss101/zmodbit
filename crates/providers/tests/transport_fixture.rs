//! Integration tests for `HttpStreamTransport` (ADR-0002, docs/15 § Health
//! and failover) against a real local TCP HTTP fixture server — a genuine
//! socket boundary, not a canned adapter. Proves: incremental SSE delivery,
//! usage capture, bounded retry before first token, no retry after tokens
//! started, cancellation, timeout, and fail-closed credentials.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};


use modbit_providers::transport::{
    EventStream, HttpStreamTransport, ModelTransport, OutgoingRequest, RetryPolicy, SecretBroker,
    TokenUsage, TransportError, TransportEvent,
};
use modbit_providers::gateway::Provider;

type Handler = Arc<dyn Fn(TcpStream) -> BoxFuture<'static, ()> + Send + Sync>;

/// Spawns a real HTTP fixture server on 127.0.0.1:0. `handler` runs per
/// accepted connection with the 1-based connection index. Returns the bound
/// address and the live connection counter.
async fn spawn_fixture(handler: Handler) -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind fixture");
    let addr = listener.local_addr().expect("addr");
    let connections = Arc::new(AtomicUsize::new(0));
    let counter = connections.clone();
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
            let h = handler.clone();
            tokio::spawn(async move {
                h(socket).await;
            });
            let _ = n;
        }
    });
    (addr, connections)
}

/// Reads one HTTP request head (and drains the declared body).
async fn read_request_head(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).await.expect("read request");
        assert!(n > 0, "client closed before sending a request head");
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf).to_string();
        if let Some(end) = text.find("\r\n\r\n") {
            // Drain content-length body bytes if any remain in the buffer.
            let head = &text[..end];
            let clen = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            let have = buf.len() - (end + 4);
            if have < clen {
                let mut rest = vec![0u8; clen - have];
                stream.read_exact(&mut rest).await.expect("read body");
            }
            return text[..end].to_string();
        }
    }
}

async fn write_response_head(stream: &mut TcpStream, status_line: &str, extra: &str) {
    stream
        .write_all(
            format!(
                "{status_line}\r\nContent-Type: text/event-stream\r\nConnection: close\r\n{extra}\r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("write head");
}

async fn write_frames(stream: &mut TcpStream, frames: &[String]) {
    for f in frames {
        stream
            .write_all(format!("data: {f}\n\n").as_bytes())
            .await
            .expect("write frame");
        stream.flush().await.expect("flush frame");
    }
}

fn sse_frames(frames: &[&str]) -> Vec<String> {
    frames.iter().map(|s| s.to_string()).collect()
}

/// Static-value broker so tests never depend on process env.
struct StaticBroker(&'static str);

impl SecretBroker for StaticBroker {
    fn credential(&self, _name: &str) -> Result<String, TransportError> {
        Ok(self.0.to_string())
    }
}

struct RejectingBroker;

impl SecretBroker for RejectingBroker {
    fn credential(&self, name: &str) -> Result<String, TransportError> {
        Err(TransportError::MissingCredential(name.to_string()))
    }
}

fn openai_request(url: String, body: &'static str) -> OutgoingRequest {
    OutgoingRequest {
        provider: Provider::OpenAi,
        url,
        body: body.as_bytes().to_vec(),
        timeout: Duration::from_secs(10),
    }
}

async fn collect(stream: &mut EventStream) -> (Vec<TransportEvent>, Option<TransportError>) {
    let mut events = Vec::new();
    loop {
        match stream.recv().await {
            Some(Ok(TransportEvent::Eof)) => return (events, None),
            Some(Ok(e)) => events.push(e),
            Some(Err(e)) => return (events, Some(e)),
            None => return (events, None),
        }
    }
}

#[tokio::test]
async fn streams_incrementally_with_usage_and_eof() {
    // Server parks on this after writing frame 1 until the client confirms
    // it arrived — a buffering transport cannot pass the 1s assertion below.
    let go = Arc::new(tokio::sync::Notify::new());
    let handler: Handler = {
        let go = go.clone();
        Arc::new(move |mut socket: TcpStream| {
            let go = go.clone();
            Box::pin(async move {
                let _ = read_request_head(&mut socket).await;
                write_response_head(&mut socket, "HTTP/1.1 200 OK", "").await;
                write_frames(
                    &mut socket,
                    &sse_frames(&[r#"{"choices":[{"delta":{"content":"first"}}]}"#]),
                )
                .await;
                let _ = tokio::time::timeout(Duration::from_secs(5), go.notified()).await;
                write_frames(
                    &mut socket,
                    &sse_frames(&[
                        r#"{"choices":[{"delta":{"content":"second"}}]}"#,
                        r#"{"choices":[{"delta":{}}],"usage":{"prompt_tokens":11,"completion_tokens":7}}"#,
                        "[DONE]",
                    ]),
                )
                .await;
            })
        })
    };
    let (addr, connections) = spawn_fixture(handler).await;

    let transport = HttpStreamTransport::new(Arc::new(StaticBroker("test-key-1"))).unwrap();
    let mut stream = transport
        .stream(openai_request(format!("http://{addr}/v1/chat/completions"), "{}"))
        .unwrap();

    // First frame must arrive while the server is parked (incrementality).
    let first = tokio::time::timeout(Duration::from_secs(1), stream.recv())
        .await
        .expect("first frame must arrive before the server writes more (incremental delivery)")
        .unwrap()
        .unwrap();
    assert_eq!(
        first,
        TransportEvent::SseData(r#"{"choices":[{"delta":{"content":"first"}}]}"#.to_string())
    );
    go.notify_one();

    let (events, error) = collect(&mut stream).await;
    assert!(error.is_none(), "unexpected error: {error:?}");
    assert_eq!(
        events,
        vec![
            TransportEvent::SseData(r#"{"choices":[{"delta":{"content":"second"}}]}"#.to_string()),
            TransportEvent::Usage(TokenUsage {
                input_tokens: 11,
                output_tokens: 7
            }),
            // The usage frame is also forwarded as raw data for the parsers.
            TransportEvent::SseData(
                r#"{"choices":[{"delta":{}}],"usage":{"prompt_tokens":11,"completion_tokens":7}}"#
                    .to_string(),
            ),
            TransportEvent::SseData("[DONE]".to_string()),
        ]
    );
    assert_eq!(connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retries_429_then_succeeds_before_first_token() {
    // Queue-driven fixture: connection 1 -> 429, connection 2 -> success.
    let responses: Arc<Mutex<VecDeque<&'static str>>> =
        Arc::new(Mutex::new(VecDeque::from(["429", "success"])));
    let handler: Handler = {
        let responses = responses.clone();
        Arc::new(move |mut socket: TcpStream| {
            let responses = responses.clone();
            Box::pin(async move {
                let _ = read_request_head(&mut socket).await;
                let next = responses.lock().unwrap().pop_front().unwrap_or("429");
                if next == "429" {
                    let _ = socket
                        .write_all(
                            b"HTTP/1.1 429 Too Many Requests\r\nConnection: close\r\n\r\nslow down",
                        )
                        .await;
                } else {
                    write_response_head(&mut socket, "HTTP/1.1 200 OK", "").await;
                    write_frames(
                        &mut socket,
                        &sse_frames(&[
                            r#"{"choices":[{"delta":{"content":"after-retry"}}]}"#,
                            "[DONE]",
                        ]),
                    )
                    .await;
                }
            })
        })
    };
    let (addr, connections) = spawn_fixture(handler).await;

    let transport = HttpStreamTransport::new(Arc::new(StaticBroker("k")))
        .unwrap()
        .with_retry(RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
        });
    let mut stream = transport
        .stream(openai_request(format!("http://{addr}/v1/chat/completions"), "{}"))
        .unwrap();
    let (events, error) = collect(&mut stream).await;
    assert!(error.is_none(), "retry should have recovered: {error:?}");
    assert!(events.contains(&TransportEvent::SseData(
        r#"{"choices":[{"delta":{"content":"after-retry"}}]}"#.to_string()
    )));
    assert_eq!(connections.load(Ordering::SeqCst), 2, "exactly one retry");
}

#[tokio::test]
async fn no_retry_after_tokens_started_interrupts() {
    // Server declares a larger Content-Length than it writes, then closes:
    // the body read fails mid-stream (a real interrupted response).
    let handler: Handler = Arc::new(move |mut socket: TcpStream| {
        Box::pin(async move {
            let _ = read_request_head(&mut socket).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 5000\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
                )
                .await
                .expect("write partial body");
            socket.flush().await.expect("flush");
            drop(socket); // abrupt truncation
        })
    });
    let (addr, connections) = spawn_fixture(handler).await;

    let transport = HttpStreamTransport::new(Arc::new(StaticBroker("k")))
        .unwrap()
        .with_retry(RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        });
    let mut stream = transport
        .stream(openai_request(format!("http://{addr}/v1/chat/completions"), "{}"))
        .unwrap();
    let first = stream.recv().await.unwrap().unwrap();
    assert!(matches!(first, TransportEvent::SseData(_)));
    let err = tokio::time::timeout(Duration::from_secs(5), stream.recv())
        .await
        .expect("truncation must surface")
        .unwrap()
        .unwrap_err();
    assert!(
        matches!(err, TransportError::Interrupted { .. }),
        "expected Interrupted, got {err}"
    );
    // Retry is forbidden once tokens flowed: exactly one connection.
    assert_eq!(connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancel_terminates_the_stream() {
    let handler: Handler = Arc::new(move |mut socket: TcpStream| {
        Box::pin(async move {
            let _ = read_request_head(&mut socket).await;
            write_response_head(&mut socket, "HTTP/1.1 200 OK", "").await;
            write_frames(&mut socket, &sse_frames(&[r#"{"d":1}"#])).await;
            // Park forever: the client must break the connection.
            tokio::time::sleep(Duration::from_secs(30)).await;
        })
    });
    let (addr, _c) = spawn_fixture(handler).await;

    let transport = HttpStreamTransport::new(Arc::new(StaticBroker("k"))).unwrap();
    let mut stream = transport
        .stream(openai_request(format!("http://{addr}/v1/chat/completions"), "{}"))
        .unwrap();
    let _ = stream.recv().await.unwrap().unwrap();
    stream.cancel_handle().cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(5), stream.recv())
        .await
        .expect("cancellation must surface promptly");
    assert!(
        matches!(outcome.unwrap().unwrap_err(), TransportError::Cancelled),
        "expected Cancelled"
    );
}

#[tokio::test]
async fn timeout_surfaces_and_counts_as_retriable() {
    let handler: Handler = Arc::new(move |mut socket: TcpStream| {
        Box::pin(async move {
            let _ = read_request_head(&mut socket).await;
            // Never respond; hold the connection.
            tokio::time::sleep(Duration::from_secs(30)).await;
        })
    });
    let (addr, connections) = spawn_fixture(handler).await;

    let transport = HttpStreamTransport::new(Arc::new(StaticBroker("k")))
        .unwrap()
        .with_retry(RetryPolicy {
            max_attempts: 2,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        });
    let mut stream = transport
        .stream(OutgoingRequest {
            provider: Provider::OpenAi,
            url: format!("http://{addr}/v1/chat/completions"),
            body: b"{}".to_vec(),
            timeout: Duration::from_millis(200),
        })
        .unwrap();
    let err = tokio::time::timeout(Duration::from_secs(5), stream.recv())
        .await
        .expect("timeout must surface")
        .unwrap()
        .unwrap_err();
    assert!(matches!(err, TransportError::Timeout), "got {err}");
    assert_eq!(
        connections.load(Ordering::SeqCst),
        2,
        "timeout before first token is retriable"
    );
}

#[tokio::test]
async fn missing_credential_fails_closed_without_connecting() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let c2 = connections.clone();
    tokio::spawn(async move {
        while let Ok((s, _)) = listener.accept().await {
            c2.fetch_add(1, Ordering::SeqCst);
            drop(s);
        }
    });

    let transport = HttpStreamTransport::new(Arc::new(RejectingBroker)).unwrap();
    let err = transport
        .stream(openai_request(format!("http://{addr}/v1/chat/completions"), "{}"))
        .unwrap_err();
    assert!(matches!(err, TransportError::MissingCredential(_)));
    // Fail closed: not a single byte was sent to the endpoint.
    assert_eq!(connections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn broker_provided_credential_reaches_the_endpoint_and_anthropic_usage_merges() {
    let seen_auth: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let handler: Handler = {
        let seen_auth = seen_auth.clone();
        Arc::new(move |mut socket: TcpStream| {
            let seen_auth = seen_auth.clone();
            Box::pin(async move {
                let head = read_request_head(&mut socket).await;
                let auth = head
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("x-api-key:"))
                    .unwrap_or("<missing>")
                    .to_string();
                seen_auth.lock().unwrap().push(auth);
                write_response_head(&mut socket, "HTTP/1.1 200 OK", "").await;
                // Anthropic splits usage across message_start/message_delta.
                write_frames(
                    &mut socket,
                    &sse_frames(&[
                        r#"{"type":"message_start","message":{"usage":{"input_tokens":42,"output_tokens":1}}}"#,
                        r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}"#,
                        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9}}"#,
                        r#"{"type":"message_stop"}"#,
                    ]),
                )
                .await;
            })
        })
    };
    let (addr, _c) = spawn_fixture(handler).await;

    struct AnthropicBroker;
    impl SecretBroker for AnthropicBroker {
        fn credential(&self, name: &str) -> Result<String, TransportError> {
            assert_eq!(name, "ANTHROPIC_API_KEY");
            Ok("sk-anth-test".to_string())
        }
    }

    let transport = HttpStreamTransport::new(Arc::new(AnthropicBroker)).unwrap();
    let mut stream = transport
        .stream(OutgoingRequest {
            provider: Provider::Anthropic,
            url: format!("http://{addr}/v1/messages"),
            body: b"{}".to_vec(),
            timeout: Duration::from_secs(10),
        })
        .unwrap();
    let (events, error) = collect(&mut stream).await;
    assert!(error.is_none(), "{error:?}");
    assert_eq!(
        seen_auth.lock().unwrap().last().unwrap(),
        "x-api-key: sk-anth-test"
    );
    // The final merged usage snapshot wins.
    let usage = events
        .iter()
        .filter_map(|e| match e {
            TransportEvent::Usage(u) => Some(*u),
            _ => None,
        })
        .next_back()
        .expect("usage captured");
    assert_eq!(
        usage,
        TokenUsage {
            input_tokens: 42,
            output_tokens: 9
        }
    );
    assert!(events.contains(&TransportEvent::SseData(
        r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}"#.to_string()
    )));
}
