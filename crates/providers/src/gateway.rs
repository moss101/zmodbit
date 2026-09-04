//! Provider gateway (M2.6, docs/15 § Provider contract + § Credentials):
//! one normalized streaming inference contract over typed adapters.
//!
//! This module is transport-pure: request serialization, SSE line parsing
//! and normalized `ModelEvent` assembly are pure functions over bytes —
//! unit-tested against recorded provider wire shapes. The live client reads
//! credentials from the environment only (docs/15 § Credentials: raw secrets
//! never enter context or the repo).
//!
//! Canonical owner subsystem: model-gateway (docs/81). Layout: docs/12.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Canonical request (docs/15 § Provider contract — M1 slice: messages +
/// max tokens + temperature; tool projection lands with the tool runtime).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    pub request_id: String,
    pub model: String,
    pub system: String,
    pub messages: Vec<ChatMessage>,
    pub max_output_tokens: u32,
    pub temperature: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

/// Normalized streaming events (docs/15 § ModelEvent).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ModelEvent {
    MessageDelta { text: String },
    ProviderMetadata { raw: String },
    Completed { stop_reason: String },
    Error { message: String },
}

/// The two first-class adapters (docs/15: OpenAI-compatible and
/// Anthropic-style first).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    OpenAi,
    Anthropic,
}

impl Provider {
    /// Production endpoint for this adapter. The base URL is overridable
    /// for OpenAI-compatible production gateways (e.g. OPENAI_BASE_URL=
    /// https://openrouter.ai/api/v1) — the wire protocol is identical
    /// chat-completions (docs/15).
    pub fn endpoint(&self) -> String {
        match self {
            Provider::OpenAi => {
                let base = std::env::var("OPENAI_BASE_URL")
                    .unwrap_or_else(|_| "https://api.openai.com/v1".into());
                format!("{base}/chat/completions")
            }
            Provider::Anthropic => {
                let base = std::env::var("ANTHROPIC_BASE_URL")
                    .unwrap_or_else(|_| "https://api.anthropic.com".into());
                format!("{base}/v1/messages")
            }
        }
    }

    pub fn credential_env(&self) -> &'static str {
        match self {
            Provider::OpenAi => "OPENAI_API_KEY",
            Provider::Anthropic => "ANTHROPIC_API_KEY",
        }
    }
}

/// Canonical smoke-test request for live qualification. The model is
/// overridable via MODBIT_LIVE_MODEL for gateways with different catalogs.
pub fn test_request() -> ModelRequest {
    ModelRequest {
        request_id: "live-qualification".into(),
        model: std::env::var("MODBIT_LIVE_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into()),
        system: "Reply with exactly: pong".into(),
        messages: vec![ChatMessage {
            role: Role::User,
            content: "ping".into(),
        }],
        max_output_tokens: 4096,
        temperature: 0.0,
    }
}

/// Builds the OpenAI-compatible chat-completions request body.
pub fn openai_request_body(request: &ModelRequest) -> Value {
    let mut messages = Vec::new();
    if !request.system.is_empty() {
        messages.push(serde_json::json!({ "role": "system", "content": request.system }));
    }
    for m in &request.messages {
        messages.push(serde_json::json!({
            "role": m.role,
            "content": m.content,
        }));
    }
    serde_json::json!({
        "model": request.model,
        "messages": messages,
        "max_tokens": request.max_output_tokens,
        "temperature": request.temperature,
        "stream": true,
    })
}

/// Builds the Anthropic messages request body.
pub fn anthropic_request_body(request: &ModelRequest) -> Value {
    let messages: Vec<Value> = request
        .messages
        .iter()
        .map(|m| serde_json::json!({ "role": m.role, "content": m.content }))
        .collect();
    serde_json::json!({
        "model": request.model,
        "system": request.system,
        "messages": messages,
        "max_tokens": request.max_output_tokens,
        "temperature": request.temperature,
        "stream": true,
    })
}

/// Normalized streaming model event assembled from parsed provider deltas.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamEvent {
    Delta(String),
    /// A typed tool-use request from the model. The runtime MUST validate
    /// the payload (JSON object, known tool) BEFORE any side effect
    /// (docs/14 step 5). Arguments travel as their raw JSON text.
    ToolRequest {
        call_id: String,
        name: String,
        arguments: String,
    },
    Completed {
        stop_reason: Option<String>,
    },
}

/// Parses one OpenAI `data:` SSE payload into a normalized stream event.
/// Returns None for keep-alives and the terminal `[DONE]` marker.
pub fn parse_openai_sse_payload(payload: &str) -> Option<StreamEvent> {
    let payload = payload.trim();
    if payload == "[DONE]" {
        return Some(StreamEvent::Completed {
            stop_reason: Some("stop".into()),
        });
    }
    let value: Value = serde_json::from_str(payload).ok()?;
    let choices = value.get("choices")?.as_array()?;
    let choice = choices.first()?;
    // Terminal frames carry finish_reason and may omit content entirely —
    // check finish first so an empty delta is not misread as no-event.
    if let Some(reason) = choice.get("finish_reason").and_then(|v| v.as_str()) {
        return Some(StreamEvent::Completed {
            stop_reason: Some(reason.into()),
        });
    }
    // Tool calls: a delta carrying a COMPLETE call (id + name + arguments)
    // maps to one typed ToolRequest. Fragmented argument streaming is a
    // later slice (docs/15) — fragments without identity are ignored here
    // rather than mis-parsed.
    if let Some(tool_calls) = choice
        .get("delta")
        .and_then(|d| d.get("tool_calls"))
        .and_then(|v| v.as_array())
    {
        for call in tool_calls {
            let id = call.get("id").and_then(|v| v.as_str());
            let name = call
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str());
            let arguments = call
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if let (Some(id), Some(name)) = (id, name) {
                if !arguments.is_empty() {
                    return Some(StreamEvent::ToolRequest {
                        call_id: id.to_string(),
                        name: name.to_string(),
                        arguments: arguments.to_string(),
                    });
                }
            }
        }
    }
    let text = choice.get("delta")?.get("content")?.as_str()?.to_string();
    if text.is_empty() {
        None
    } else {
        Some(StreamEvent::Delta(text))
    }
}

/// Parses one Anthropic SSE payload into a normalized stream event.
pub fn parse_anthropic_sse_payload(payload: &str) -> Option<StreamEvent> {
    let value: Value = serde_json::from_str(payload).ok()?;
    let kind = value.get("type")?.as_str()?;
    match kind {
        "content_block_delta" => {
            let delta = value.get("delta")?;
            let text = delta.get("text")?.as_str()?.to_string();
            if text.is_empty() {
                None
            } else {
                Some(StreamEvent::Delta(text))
            }
        }
        // Anthropic tool_use blocks arrive COMPLETE in content_block_start.
        "content_block_start" => {
            let block = value.get("content_block")?;
            if block.get("type").and_then(|v| v.as_str()) != Some("tool_use") {
                return None;
            }
            let id = block.get("id").and_then(|v| v.as_str())?;
            let name = block.get("name").and_then(|v| v.as_str())?;
            let input = block.get("input").cloned().unwrap_or(Value::Null);
            Some(StreamEvent::ToolRequest {
                call_id: id.to_string(),
                name: name.to_string(),
                arguments: input.to_string(),
            })
        }
        "message_stop" => Some(StreamEvent::Completed {
            stop_reason: Some("end_turn".into()),
        }),
        _ => None,
    }
}

/// Extracts the payload after `data:` from one SSE line, if it is a data
/// line. Returns None for empty/comment lines.
pub fn sse_data_line(line: &str) -> Option<String> {
    let trimmed = line.trim_end_matches(['\n', '\r']);
    let payload = trimmed.strip_prefix("data: ")?;
    if payload.is_empty() {
        None
    } else {
        Some(payload.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ModelRequest {
        ModelRequest {
            request_id: "r-1".into(),
            model: "test-model".into(),
            system: "be precise".into(),
            messages: vec![ChatMessage {
                role: Role::User,
                content: "hello".into(),
            }],
            max_output_tokens: 256,
            temperature: 0.2,
        }
    }

    #[test]
    fn openai_body_shape_matches_the_api_contract() {
        let body = openai_request_body(&request());
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["max_tokens"], 256);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn anthropic_body_shape_matches_the_api_contract() {
        let body = anthropic_request_body(&request());
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["system"], "be precise");
        assert_eq!(body["max_tokens"], 256);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn openai_sse_deltas_parse_and_done_terminates() {
        let line = r#"data: {"choices":[{"delta":{"content":"Hi"}}]}"#;
        assert_eq!(
            parse_openai_sse_payload(line.trim_start_matches("data: ")),
            Some(StreamEvent::Delta("Hi".into()))
        );
        assert_eq!(
            parse_openai_sse_payload("[DONE]"),
            Some(StreamEvent::Completed {
                stop_reason: Some("stop".into())
            })
        );
        assert_eq!(parse_openai_sse_payload(""), None);
    }

    #[test]
    fn anthropic_sse_deltas_parse_and_message_stop_terminates() {
        let line =
            r#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"Yo"}}"#;
        assert_eq!(
            parse_anthropic_sse_payload(line.trim_start_matches("data: ")),
            Some(StreamEvent::Delta("Yo".into()))
        );
        assert_eq!(
            parse_anthropic_sse_payload(r#"{"type":"message_stop"}"#),
            Some(StreamEvent::Completed {
                stop_reason: Some("end_turn".into())
            })
        );
    }

    #[test]
    fn finish_reason_beats_empty_delta() {
        let line = r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#;
        let payload = line.strip_prefix("data: ").unwrap();
        assert!(matches!(
            parse_openai_sse_payload(payload),
            Some(StreamEvent::Completed { .. })
        ));
    }
}
