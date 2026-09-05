//! Live-model qualification (docs/15 § Live provider proof): a real
//! streaming call against the production endpoint with real credentials,
//! driven through the production `HttpStreamTransport` (ADR-0002) — no
//! subprocess transport.
//!
//! Gated on environment variables so CI never requires paid API calls:
//!   MODBIT_LIVE_OPENAI=1 + OPENAI_API_KEY  → OpenAI streaming qualification
//!   MODBIT_LIVE_ANTHROPIC=1 + ANTHROPIC_API_KEY → Anthropic qualification
//!
//! Until run with real credentials this qualification has NOT been
//! performed; the adapter unit tests cover only wire shapes and parsers.

use std::sync::Arc;
use std::time::Duration;

use modbit_providers::gateway::{
    anthropic_request_body, openai_request_body, parse_anthropic_sse_payload,
    parse_openai_sse_payload, test_request, Provider, StreamEvent,
};
use modbit_providers::transport::{
    EnvSecretBroker, HttpStreamTransport, ModelTransport, OutgoingRequest, TransportEvent,
};

fn live_enabled() -> Option<Provider> {
    if std::env::var("MODBIT_LIVE_OPENAI").is_ok() && std::env::var("OPENAI_API_KEY").is_ok() {
        return Some(Provider::OpenAi);
    }
    if std::env::var("MODBIT_LIVE_ANTHROPIC").is_ok() && std::env::var("ANTHROPIC_API_KEY").is_ok()
    {
        return Some(Provider::Anthropic);
    }
    None
}

/// Streams a real completion over HTTPS through `HttpStreamTransport` and
/// assembles the normalized event stream: at least one delta and one
/// Completed event, non-empty text, usage captured.
#[test]
fn live_streaming_call_produces_normalized_events() {
    let Some(provider) = live_enabled() else {
        eprintln!("skipped: no live provider credentials in env (docs/15 live proof pending)");
        return;
    };

    let body = match provider {
        Provider::OpenAi => openai_request_body(&test_request()),
        Provider::Anthropic => anthropic_request_body(&test_request()),
    };

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            let transport =
                HttpStreamTransport::new(Arc::new(EnvSecretBroker)).expect("transport");
            let mut stream = transport
                .stream(OutgoingRequest {
                    provider,
                    url: provider.endpoint(),
                    body: serde_json::to_vec(&body).unwrap(),
                    timeout: Duration::from_secs(90),
                })
                .expect("open stream");

            let mut deltas = 0usize;
            let mut assembled = String::new();
            let mut completed = false;
            let mut usage = None;
            loop {
                let event = match tokio::time::timeout(Duration::from_secs(90), stream.recv()).await
                {
                    Err(_) => panic!("stream stalled for 90s"),
                    Ok(None) => panic!("stream ended without a terminal event"),
                    Ok(Some(Err(e))) => panic!("stream error: {e}"),
                    Ok(Some(Ok(event))) => event,
                };
                match event {
                    TransportEvent::SseData(payload) => {
                        let event = match provider {
                            Provider::OpenAi => parse_openai_sse_payload(&payload),
                            Provider::Anthropic => parse_anthropic_sse_payload(&payload),
                        };
                        match event {
                            Some(StreamEvent::Delta(text)) => {
                                deltas += 1;
                                assembled.push_str(&text);
                            }
                            Some(StreamEvent::Completed { .. }) => {
                                completed = true;
                            }
                            _ => {}
                        }
                    }
                    TransportEvent::Usage(u) => usage = Some(u),
                    TransportEvent::Eof => break,
                }
            }

            assert!(deltas > 0, "expected streaming deltas");
            assert!(completed, "expected a Completed event");
            assert!(!assembled.trim().is_empty(), "assembled text must be non-empty");
            eprintln!(
                "live qualification ok: deltas={deltas} usage={usage:?} text={:?}",
                assembled.chars().take(120).collect::<String>()
            );
        });
}
