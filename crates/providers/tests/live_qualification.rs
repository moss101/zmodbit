//! Live-model qualification (docs/15 § Live provider proof): a real
//! streaming call against the production endpoint with real credentials.
//!
//! Gated on environment variables so CI never requires paid API calls:
//!   MODBIT_LIVE_OPENAI=1 + OPENAI_API_KEY  → OpenAI streaming qualification
//!   MODBIT_LIVE_ANTHROPIC=1 + ANTHROPIC_API_KEY → Anthropic qualification
//!
//! Until run with real credentials this qualification has NOT been
//! performed; the adapter unit tests cover only wire shapes and parsers.

use modbit_providers::gateway::{
    anthropic_request_body, openai_request_body, parse_anthropic_sse_payload,
    parse_openai_sse_payload, Provider, StreamEvent,
};
use std::io::{BufRead, BufReader, Write};
use std::process::Command;

fn live_openai_enabled() -> bool {
    std::env::var("MODBIT_LIVE_OPENAI").is_ok() && std::env::var("OPENAI_API_KEY").is_ok()
}

fn live_anthropic_enabled() -> bool {
    std::env::var("MODBIT_LIVE_ANTHROPIC").is_ok() && std::env::var("ANTHROPIC_API_KEY").is_ok()
}

fn credential() -> String {
    std::env::var(if std::env::var("OPENAI_API_KEY").is_ok() {
        "OPENAI_API_KEY"
    } else {
        "ANTHROPIC_API_KEY"
    })
    .unwrap_or_default()
}

/// Streams a real completion over HTTPS and assembles the normalized event
/// stream: at least one delta and one Completed event, non-empty text.
#[test]
fn live_streaming_call_produces_normalized_events() {
    if !live_openai_enabled() && !live_anthropic_enabled() {
        eprintln!("skipped: no live provider credentials in env (docs/15 live proof pending)");
        return;
    }
    let provider = if std::env::var("OPENAI_API_KEY").is_ok() {
        Provider::OpenAi
    } else {
        Provider::Anthropic
    };
    let key = std::env::var(provider.credential_env()).unwrap();
    let body = match provider {
        Provider::OpenAi => openai_request_body(&modbit_providers::gateway::test_request()),
        _ => anthropic_request_body(&modbit_providers::gateway::test_request()),
    };
    let body_bytes = serde_json::to_vec(&body).unwrap();

    let mut child = Command::new("curl")
        .args([
            "-sS",
            "-N",
            "--max-time",
            "90",
            "-X",
            "POST",
            &provider.endpoint(),
            "-H",
            &format!("Authorization: Bearer {key}"),
            "-H",
            "Content-Type: application/json",
            "-H",
            "anthropic-version: 2023-06-01",
            "--data-binary",
            "@-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn curl");
    // take() the stdin handle: dropping it closes the pipe. With
    // --data-binary @- curl waits for stdin EOF before sending — leaving
    // it open stalls the request forever.
    std::fs::write("/tmp/live-body.json", &body_bytes).unwrap();
    {
        let mut stdin = child.stdin.take().expect("stdin");
        stdin.write_all(&body_bytes).unwrap();
    }
    let curl_stderr_pipe = child.stderr.take().expect("stderr");
    let curl_stderr = {
        let mut buf = String::new();
        use std::io::Read as _;
        let mut reader = std::io::BufReader::new(curl_stderr_pipe);
        let _ = reader.read_to_string(&mut buf);
        buf
    };
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);
    let mut deltas = 0usize;
    let mut completed = false;
    let mut assembled = String::new();
    let mut raw_lines: Vec<String> = Vec::new();
    let mut line = String::new();
    while reader.read_line(&mut line).unwrap_or(0) > 0 {
        raw_lines.push(line.trim_end().to_string());
        if let Some(payload) = modbit_providers::gateway::sse_data_line(&line) {
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
                    break;
                }
                _ => {}
            }
        }
        line.clear();
    }
    let status = child.wait().unwrap();
    std::fs::write(
        "/tmp/live-debug.log",
        format!(
            "curl_exit={:?}\nstderr={}\nraw_lines={}\n---\n{}\n",
            status.code(),
            curl_stderr,
            raw_lines.len(),
            raw_lines.join("\n")
        ),
    )
    .unwrap();

    let _ = &credential;
    assert!(
        deltas > 0,
        "expected streaming deltas; saw {} raw lines; first 300: {:?}; last 500: {}",
        raw_lines.len(),
        raw_lines
            .first()
            .map(|l| l.chars().take(300).collect::<String>()),
        raw_lines
            .iter()
            .rev()
            .take(6)
            .rev()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
            .chars()
            .take(500)
            .collect::<String>()
    );
    assert!(completed, "expected a Completed event");
    assert!(
        !assembled.trim().is_empty(),
        "assembled text must be non-empty"
    );
}
