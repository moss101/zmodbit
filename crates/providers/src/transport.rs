//! Model transport (M2.6, docs/15 § Provider contract; ADR-0002):
//! the production HTTP streaming boundary of the model gateway.
//!
//! `HttpStreamTransport` POSTs a request body to a provider endpoint and
//! pushes SSE `data:` payloads to a channel **incrementally** — a frame is
//! delivered the moment its terminating newline arrives; bytes are never
//! accumulated for delivery-at-end. Retry with exponential backoff + jitter
//! is bounded and applies only before the first token byte (docs/15 § Health
//! and failover: failover is allowed only before an effectful action derived
//! from a partial response). Cancellation is a cooperative token raced
//! against every await point of the attempt task.
//!
//! Credentials flow through the [`SecretBroker`] trait; raw secrets never
//! enter logs, events or model context (docs/15 § Credentials).
//!
//! Canonical owner subsystem: model-gateway (docs/81). Layout: docs/12.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::gateway::Provider;

/// Errors surfaced by the transport. Carries no credential material, so any
/// `Display`/`Debug` rendering is safe to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    /// No credential under the requested name (fail closed, never send unauthenticated).
    MissingCredential(String),
    /// Endpoint refused before or during the stream.
    Status { code: u16, body: String },
    /// The stream broke after tokens were already flowing; the turn must be
    /// repaired from the last safe state rather than silently replayed.
    Interrupted { message: String },
    /// Total request deadline exceeded.
    Timeout,
    /// Cancellation was requested before completion.
    Cancelled,
    /// The transport itself was misconfigured (e.g. client construction failed).
    Configuration { message: String },
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportError::MissingCredential(name) => {
                write!(f, "missing credential {name:?} (set it in the secret broker)")
            }
            TransportError::Status { code, body } => {
                write!(f, "provider status {code}: {}", truncate(body, 200))
            }
            TransportError::Interrupted { message } => {
                write!(f, "stream interrupted: {}", truncate(message, 200))
            }
            TransportError::Timeout => write!(f, "request timeout"),
            TransportError::Cancelled => write!(f, "cancelled"),
            TransportError::Configuration { message } => {
                write!(f, "transport misconfigured: {message}")
            }
        }
    }
}

impl std::error::Error for TransportError {}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}

/// Source of provider credentials. The trait is the fixed interface; the
/// environment-backed implementation is production for local builds today,
/// and the OS keychain backend lands in Phase 2 (Future-tasks.md §5 Phase 2).
pub trait SecretBroker: Send + Sync + 'static {
    /// Returns the secret stored under `name` (e.g. `OPENAI_API_KEY`).
    /// Implementations MUST NOT log the value.
    fn credential(&self, name: &str) -> Result<String, TransportError>;
}

/// Environment-backed [`SecretBroker`].
pub struct EnvSecretBroker;

impl SecretBroker for EnvSecretBroker {
    fn credential(&self, name: &str) -> Result<String, TransportError> {
        match std::env::var(name) {
            Ok(v) if !v.trim().is_empty() => Ok(v),
            _ => Err(TransportError::MissingCredential(name.to_string())),
        }
    }
}

/// A fully-resolved outgoing request: the transport owns no request building.
#[derive(Clone, Debug)]
pub struct OutgoingRequest {
    pub provider: Provider,
    /// Absolute URL (from `Provider::endpoint()` or a pinned base URL).
    pub url: String,
    /// Serialized JSON body (`openai_request_body` / `anthropic_request_body`).
    pub body: Vec<u8>,
    /// Total deadline for connect + response status + full body read.
    pub timeout: Duration,
}

/// Provider-neutral usage snapshot captured from usage-bearing frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// One incrementally delivered transport event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportEvent {
    /// Payload of one SSE `data:` frame, delivered as soon as the frame
    /// completes on the wire.
    SseData(String),
    /// Usage captured from the provider's usage-bearing frame(s).
    Usage(TokenUsage),
    /// The stream ended cleanly (EOF after a 200 response).
    Eof,
}

/// Handle over one in-flight streaming request.
#[derive(Debug)]
pub struct EventStream {
    rx: mpsc::Receiver<Result<TransportEvent, TransportError>>,
    cancel: CancellationToken,
}

impl EventStream {
    /// Next event. Yields `Some(Ok(..))` per event, `Some(Ok(Eof))` on clean
    /// end of stream, `Some(Err(..))` for the terminal error, then `None`.
    pub async fn recv(&mut self) -> Option<Result<TransportEvent, TransportError>> {
        self.rx.recv().await
    }

    /// Token that cancels the request cooperatively.
    pub fn cancel_handle(&self) -> CancellationToken {
        self.cancel.clone()
    }
}

/// Object-safe model transport boundary (ADR-0002).
pub trait ModelTransport: Send + Sync + 'static {
    /// Opens the streaming request. I/O starts immediately on a background
    /// task; fire the returned stream's cancel handle to abandon it.
    fn stream(&self, request: OutgoingRequest) -> Result<EventStream, TransportError>;
}

/// Retry policy for [`HttpStreamTransport`]: bounded, exponential backoff
/// with jitter, retriable only before the first token byte.
#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(50),
            max_delay: Duration::from_secs(8),
        }
    }
}

fn retriable_status(code: u16) -> bool {
    code == 429 || code >= 500
}

fn retriable(error: &TransportError) -> bool {
    match error {
        TransportError::Status { code, .. } => retriable_status(*code),
        TransportError::Timeout => true,
        _ => false,
    }
}

/// Production transport over `reqwest` + `rustls` (ADR-0002).
pub struct HttpStreamTransport {
    client: reqwest::Client,
    broker: Arc<dyn SecretBroker>,
    retry: RetryPolicy,
    /// SplitMix-style jitter state (shared with the attempt tasks).
    jitter: Arc<AtomicU64>,
}

impl HttpStreamTransport {
    pub fn new(broker: Arc<dyn SecretBroker>) -> Result<Self, TransportError> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // Never follow redirects: provider endpoints are pinned and a
            // redirect must not leak an Authorization header cross-origin.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| TransportError::Configuration {
                message: format!("client build: {e}"),
            })?;
        Ok(HttpStreamTransport {
            client,
            broker,
            retry: RetryPolicy::default(),
            jitter: Arc::new(AtomicU64::new(0x9E3779B97F4A7C15)),
        })
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    fn auth_headers(&self, provider: Provider) -> Result<Vec<(String, String)>, TransportError> {
        let key = self.broker.credential(provider.credential_env())?;
        match provider {
            Provider::OpenAi => Ok(vec![("Authorization".into(), format!("Bearer {key}"))]),
            Provider::Anthropic => Ok(vec![
                ("x-api-key".into(), key),
                ("anthropic-version".into(), "2023-06-01".into()),
            ]),
        }
    }
}

impl ModelTransport for HttpStreamTransport {
    fn stream(&self, request: OutgoingRequest) -> Result<EventStream, TransportError> {
        let headers = self.auth_headers(request.provider)?;
        let (tx, rx) = mpsc::channel::<Result<TransportEvent, TransportError>>(64);
        let cancel = CancellationToken::new();
        let client = self.client.clone();
        let retry = self.retry;
        let jitter = self.jitter.clone();

        let task_cancel = cancel.clone();
        tokio::spawn(async move {
            let mut attempt: u32 = 0;
            loop {
                attempt += 1;
                let (attempt_tx, mut attempt_rx) =
                    mpsc::channel::<Result<TransportEvent, TransportError>>(64);
                let attempt_cancel = task_cancel.clone();
                let io = tokio::spawn(run_attempt(
                    client.clone(),
                    request.clone(),
                    headers.clone(),
                    attempt_cancel,
                    attempt_tx,
                ));

                // Forward events as they arrive; remember whether any token
                // byte flowed (retry is only legal before that) and surface
                // the first error as the attempt outcome.
                let mut first_token = false;
                let mut fatal: Option<TransportError> = None;
                loop {
                    tokio::select! {
                        _ = task_cancel.cancelled() => {
                            fatal = Some(TransportError::Cancelled);
                            break;
                        }
                        event = attempt_rx.recv() => match event {
                            Some(Ok(item)) => {
                                if matches!(item, TransportEvent::SseData(_)) {
                                    first_token = true;
                                }
                                if matches!(item, TransportEvent::Eof) {
                                    break;
                                }
                                if tx.send(Ok(item)).await.is_err() {
                                    // Consumer dropped the stream: cancel I/O.
                                    task_cancel.cancel();
                                }
                            }
                            Some(Err(e)) => {
                                fatal = Some(e);
                                break;
                            }
                            None => break,
                        },
                    }
                }
                io.abort();
                let _ = io.await;

                match fatal {
                    None => {
                        let _ = tx.send(Ok(TransportEvent::Eof)).await;
                        return;
                    }
                    Some(TransportError::Cancelled) => {
                        let _ = tx.send(Err(TransportError::Cancelled)).await;
                        return;
                    }
                    Some(e) if !first_token && attempt < retry.max_attempts && retriable(&e) => {
                        let exp = (retry.base_delay.as_millis() as u64)
                            .saturating_mul(1u64 << (attempt.min(5) - 1));
                        let cap = retry.max_delay.as_millis() as u64;
                        let half = (exp / 2).max(1);
                        let jitter_state = {
                            let mut x = jitter.load(Ordering::Relaxed);
                            x ^= x << 13;
                            x ^= x >> 7;
                            x ^= x << 17;
                            jitter.store(x, Ordering::Relaxed);
                            x
                        };
                        let delay = (half + jitter_state % half).min(cap).max(1);
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                        continue;
                    }
                    Some(e) => {
                        let _ = tx.send(Err(e)).await;
                        return;
                    }
                }
            }
        });
        Ok(EventStream { rx, cancel })
    }
}

/// Executes one attempt end to end, sending events into `tx`. Sends EOF
/// (and returns) on clean end of stream; returns the error otherwise.
async fn run_attempt(
    client: reqwest::Client,
    outgoing: OutgoingRequest,
    headers: Vec<(String, String)>,
    cancel: CancellationToken,
    tx: mpsc::Sender<Result<TransportEvent, TransportError>>,
) {
    let deadline = tokio::time::Instant::now() + outgoing.timeout;

    let mut request = client
        .post(&outgoing.url)
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream");
    for (k, v) in headers {
        request = request.header(k, v);
    }
    let request = request
        .body(outgoing.body.clone())
        .timeout(outgoing.timeout);

    let response = tokio::select! {
        _ = cancel.cancelled() => {
            let _ = tx.send(Err(TransportError::Cancelled)).await;
            return;
        }
        r = request.send() => match r {
            Ok(r) => r,
            Err(e) if e.is_timeout() => {
                let _ = tx.send(Err(TransportError::Timeout)).await;
                return;
            }
            Err(e) => {
                let _ = tx.send(Err(TransportError::Interrupted {
                    message: format!("connect: {e}"),
                })).await;
                return;
            }
        },
    };

    let status = response.status();
    if !status.is_success() {
        let code = status.as_u16();
        let body = response.text().await.unwrap_or_default();
        let _ = tx.send(Err(TransportError::Status { code, body })).await;
        return;
    }

    // Incremental SSE delivery: a frame goes out the moment its newline
    // arrives; only the current partial frame is ever buffered.
    let mut stream = response.bytes_stream();
    let mut buffer: Vec<u8> = Vec::with_capacity(1024);
    let mut usage = TokenUsage::default();
    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => {
                let _ = tx.send(Err(TransportError::Cancelled)).await;
                return;
            }
            _ = tokio::time::sleep_until(deadline) => {
                let _ = tx.send(Err(TransportError::Timeout)).await;
                return;
            }
            c = stream.next() => c,
        };
        match chunk {
            Some(Ok(bytes)) => {
                buffer.extend_from_slice(&bytes);
                while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buffer.drain(..=pos).collect();
                    let line = String::from_utf8_lossy(&line);
                    if let Some(payload) = crate::gateway::sse_data_line(line.trim_end()) {
                        // Merge usage-bearing frames into a monotonic snapshot
                        // (Anthropic splits input/output across two frames).
                        if let Some(frame) =
                            crate::gateway::extract_usage_frame(outgoing.provider, &payload)
                        {
                            if frame.input_tokens.is_some() || frame.output_tokens.is_some() {
                                if let Some(v) = frame.input_tokens {
                                    usage.input_tokens = v;
                                }
                                if let Some(v) = frame.output_tokens {
                                    usage.output_tokens = v;
                                }
                                if tx.send(Ok(TransportEvent::Usage(usage))).await.is_err() {
                                    return; // consumer gone
                                }
                            }
                        }
                        if tx
                            .send(Ok(TransportEvent::SseData(payload)))
                            .await
                            .is_err()
                        {
                            return; // consumer gone
                        }
                    }
                }
            }
            Some(Err(e)) => {
                let _ = tx
                    .send(Err(TransportError::Interrupted {
                        message: format!("body read: {e}"),
                    }))
                    .await;
                return;
            }
            None => {
                let _ = tx.send(Ok(TransportEvent::Eof)).await;
                return;
            }
        }
    }
}
