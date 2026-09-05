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

/// Canonical request (docs/15 § Provider contract: messages/context
/// segments + tool_projection). The tool projection travels from the
/// ToolRegistry through the scheduler; adapters serialize it per provider.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    pub request_id: String,
    pub model: String,
    pub system: String,
    pub messages: Vec<ChatMessage>,
    pub max_output_tokens: u32,
    pub temperature: f32,
    /// Tool projection sent with the request (docs/15 § Provider contract).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
}

/// One projected tool (docs/15: tool_projection). `parameters` is a JSON
/// Schema object produced by the ToolRegistry from its typed schemas.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    /// Assistant message: the tool calls it issued (drives the provider's
    /// tool_use / tool_calls serialization).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallData>,
    /// Tool result message: the call this message answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Tool result message: the effector failed (provider renders the error).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
}

impl ChatMessage {
    pub fn user(content: impl Into<String>) -> Self {
        ChatMessage {
            role: Role::User,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            is_error: false,
        }
    }

    pub fn text(&self, role: Role, content: impl Into<String>) -> Self {
        ChatMessage {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            is_error: false,
        }
    }

    /// Assistant message carrying issued tool calls.
    pub fn assistant_with_tool_calls(content: impl Into<String>, tool_calls: Vec<ToolCallData>) -> Self {
        ChatMessage {
            role: Role::Assistant,
            content: content.into(),
            tool_calls,
            tool_call_id: None,
            is_error: false,
        }
    }

    /// Tool result message answering `call_id`.
    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>, is_error: bool) -> Self {
        ChatMessage {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
            is_error,
        }
    }
}

/// A tool call as issued by the model (assistant message payload).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCallData {
    pub call_id: String,
    pub name: String,
    /// Raw JSON arguments text (validated by the runtime before effects).
    pub arguments: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    /// Tool result role (OpenAI-compatible wire; Anthropic maps it onto a
    /// user tool_result block).
    Tool,
}

/// Provider-neutral usage snapshot (docs/15 § ModelEvent::Usage).
pub use crate::usage::TokenUsage;

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
        messages: vec![ChatMessage::user("ping")],
        max_output_tokens: 4096,
        temperature: 0.0,
        tools: Vec::new(),
    }
}

/// Builds the OpenAI-compatible chat-completions request body.
///
/// Tool projection: each `ToolDefinition` becomes a `{"type":"function",
/// "function":{...}}` entry. Assistant tool calls serialize as
/// `tool_calls`; tool results serialize as `role:"tool"` messages keyed by
/// `tool_call_id`.
pub fn openai_request_body(request: &ModelRequest) -> Value {
    let mut messages = Vec::new();
    if !request.system.is_empty() {
        messages.push(serde_json::json!({ "role": "system", "content": request.system }));
    }
    for m in &request.messages {
        let mut message = serde_json::json!({
            "role": m.role,
            "content": m.content,
        });
        match m.role {
            Role::Assistant if !m.tool_calls.is_empty() => {
                message["tool_calls"] = serde_json::json!(m.tool_calls.iter()
                    .map(|c| serde_json::json!({
                        "id": c.call_id,
                        "type": "function",
                        "function": { "name": c.name, "arguments": c.arguments },
                    }))
                    .collect::<Vec<_>>());
            }
            Role::Tool => {
                if let Some(id) = &m.tool_call_id {
                    message["tool_call_id"] = serde_json::json!(id);
                }
            }
            _ => {}
        }
        messages.push(message);
    }
    let mut body = serde_json::json!({
        "model": request.model,
        "messages": messages,
        "max_tokens": request.max_output_tokens,
        "temperature": request.temperature,
        "stream": true,
        // Usage capture (docs/15): OpenAI-compatible endpoints only emit the
        // final usage chunk when this option is set.
        "stream_options": { "include_usage": true },
    });
    if !request.tools.is_empty() {
        body["tools"] = serde_json::json!(request.tools.iter()
            .map(|t| serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                },
            }))
            .collect::<Vec<_>>());
    }
    body
}

/// Builds the Anthropic messages request body.
///
/// Tool projection: each `ToolDefinition` becomes a `tools` entry with
/// `input_schema`. Assistant tool calls serialize as `tool_use` content
/// blocks; tool results serialize as `tool_result` blocks inside a user
/// message (Anthropic has no tool role).
pub fn anthropic_request_body(request: &ModelRequest) -> Value {
    let mut messages = Vec::new();
    for m in &request.messages {
        match m.role {
            Role::Assistant if !m.tool_calls.is_empty() => {
                let mut blocks = Vec::new();
                if !m.content.is_empty() {
                    blocks.push(serde_json::json!({ "type": "text", "text": m.content }));
                }
                for c in &m.tool_calls {
                    let input: Value =
                        serde_json::from_str(&c.arguments).unwrap_or(Value::Object(Default::default()));
                    blocks.push(serde_json::json!({
                        "type": "tool_use",
                        "id": c.call_id,
                        "name": c.name,
                        "input": input,
                    }));
                }
                messages.push(serde_json::json!({ "role": "assistant", "content": blocks }));
            }
            Role::Tool => {
                let mut block = serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": m.tool_call_id,
                    "content": m.content,
                });
                if m.is_error {
                    block["is_error"] = serde_json::json!(true);
                }
                messages.push(serde_json::json!({ "role": "user", "content": [block] }));
            }
            _ => {
                messages.push(serde_json::json!({ "role": m.role, "content": m.content }));
            }
        }
    }
    let mut body = serde_json::json!({
        "model": request.model,
        "system": request.system,
        "messages": messages,
        "max_tokens": request.max_output_tokens,
        "temperature": request.temperature,
        "stream": true,
    });
    if !request.tools.is_empty() {
        body["tools"] = serde_json::json!(request.tools.iter()
            .map(|t| serde_json::json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.parameters,
            }))
            .collect::<Vec<_>>());
    }
    body
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
    /// One fragment of a streamed tool call (OpenAI `tool_calls` deltas,
    /// Anthropic `input_json_delta`). Fragments are keyed by call id when
    /// known, else by the provider's block/call index. `ToolCallAssembler`
    /// merges fragments into complete `ToolRequest` events.
    ToolCallDelta {
        call_id: Option<String>,
        index: Option<u32>,
        name: Option<String>,
        arguments_delta: String,
    },
    /// Usage snapshot parsed from a usage-bearing frame.
    Usage(TokenUsage),
    Completed {
        stop_reason: Option<String>,
    },
}

/// Merges streamed `ToolCallDelta` fragments into complete `ToolRequest`
/// events so the runtime sees one uniform contract from both providers:
/// `ToolRequest` (assembled or complete-in-one-frame) strictly BEFORE the
/// matching `Completed`. Delta/Usage events pass through unchanged.
#[derive(Debug, Default)]
pub struct ToolCallAssembler {
    /// OpenAI fragments arrive without a call id until the first fragment
    /// carries it; key by (index or call id) until identity resolves.
    partials: Vec<PartialToolCall>,
    /// Calls already emitted complete (never re-emitted).
    emitted: Vec<String>,
}

#[derive(Debug, Default)]
struct PartialToolCall {
    call_id: Option<String>,
    index: Option<u32>,
    name: String,
    arguments: String,
}

impl ToolCallAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one parsed event; returns zero or more normalized events.
    pub fn feed(&mut self, event: StreamEvent) -> Vec<StreamEvent> {
        match event {
            StreamEvent::ToolCallDelta {
                call_id,
                index,
                name,
                arguments_delta,
            } => {
                // Identity resolution: a fragment carrying a call id matches
                // the partial with that id; a fragment carrying only an
                // index matches by index (OpenAI/Anthropic stream id on the
                // first fragment, index on every fragment).
                let position = self
                    .partials
                    .iter()
                    .position(|p| {
                        if let Some(id) = &call_id {
                            p.call_id.as_deref() == Some(id.as_str())
                        } else {
                            index.is_some() && p.index == index
                        }
                    })
                    .unwrap_or_else(|| {
                        self.partials.push(PartialToolCall {
                            call_id: call_id.clone(),
                            index,
                            name: String::new(),
                            arguments: String::new(),
                        });
                        self.partials.len() - 1
                    });
                let partial = &mut self.partials[position];
                if partial.call_id.is_none() {
                    partial.call_id = call_id;
                }
                if partial.index.is_none() {
                    partial.index = index;
                }
                if let Some(n) = name {
                    partial.name = n;
                }
                partial.arguments.push_str(&arguments_delta);
                Vec::new()
            }
            StreamEvent::Completed { stop_reason } => {
                // A Completed event ends the turn's message: flush any
                // assembled calls (provider order) BEFORE the completion.
                // Flushing on every Completed (not only tool-turn reasons)
                // keeps the contract robust across gateways that report
                // finish reasons inconsistently.
                let mut out = Vec::new();
                let partials = std::mem::take(&mut self.partials);
                for p in partials {
                    // Identity/name never resolved: not a dispatchable
                    // call — skip rather than guess (docs/14 step 5).
                    let Some(call_id) = p.call_id else {
                        continue;
                    };
                    if p.name.is_empty() || self.emitted.contains(&call_id) {
                        continue;
                    }
                    self.emitted.push(call_id.clone());
                    out.push(StreamEvent::ToolRequest {
                        call_id,
                        name: p.name,
                        arguments: p.arguments,
                    });
                }
                out.push(StreamEvent::Completed { stop_reason });
                out
            }
            other => vec![other],
        }
    }
}

/// Parses one OpenAI `data:` SSE payload into a normalized stream event.
/// Returns None for keep-alives.
///
/// Tool calls: streamed `tool_calls` delta entries are emitted as
/// `ToolCallDelta` fragments (id/name on the first fragment, index on every
/// fragment); the `ToolCallAssembler` merges them at the completion. Usage:
/// the final usage chunk (stream_options.include_usage) emits `Usage`.
pub fn parse_openai_sse_payload(payload: &str) -> Option<StreamEvent> {
    let payload = payload.trim();
    if payload == "[DONE]" {
        return Some(StreamEvent::Completed {
            stop_reason: Some("stop".into()),
        });
    }
    let value: Value = serde_json::from_str(payload).ok()?;
    let choices = value.get("choices")?.as_array()?;
    // Terminal frames carry finish_reason and may omit content entirely —
    // check finish first so an empty delta is not misread as no-event.
    if let Some(choice) = choices.first() {
        if let Some(reason) = choice.get("finish_reason").and_then(|v| v.as_str()) {
            return Some(StreamEvent::Completed {
                stop_reason: Some(reason.into()),
            });
        }
        // Tool calls: every streamed entry is a fragment of the call at its
        // index (the assembler assembles and validates before dispatch).
        if let Some(tool_calls) = choice
            .get("delta")
            .and_then(|d| d.get("tool_calls"))
            .and_then(|v| v.as_array())
        {
            let call = tool_calls.first()?;
            let index = call.get("index").and_then(|v| v.as_u64()).map(|i| i as u32);
            let call_id = call
                .get("id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let name = call
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let arguments_delta = call
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            return Some(StreamEvent::ToolCallDelta {
                call_id,
                index,
                name,
                arguments_delta,
            });
        }
    }
    // Usage-only frame (stream_options.include_usage): choices are empty.
    if choices.is_empty() {
        if let Some(frame) = extract_usage_frame(Provider::OpenAi, payload) {
            if frame.input_tokens.is_some() || frame.output_tokens.is_some() {
                return Some(StreamEvent::Usage(TokenUsage {
                    input_tokens: frame.input_tokens.unwrap_or(0),
                    output_tokens: frame.output_tokens.unwrap_or(0),
                }));
            }
        }
        return None;
    }
    let text = choices
        .first()?
        .get("delta")?
        .get("content")?
        .as_str()?
        .to_string();
    if text.is_empty() {
        None
    } else {
        Some(StreamEvent::Delta(text))
    }
}

/// Parses one Anthropic SSE payload into a normalized stream event.
///
/// Tool calls: `content_block_start` (tool_use) opens the call and
/// `input_json_delta` fragments stream its arguments — both normalize to
/// `ToolCallDelta`; the `ToolCallAssembler` assembles at the completion.
/// `message_delta` carries the authoritative `stop_reason`.
pub fn parse_anthropic_sse_payload(payload: &str) -> Option<StreamEvent> {
    let value: Value = serde_json::from_str(payload).ok()?;
    let kind = value.get("type")?.as_str()?;
    match kind {
        "content_block_delta" => {
            let delta = value.get("delta")?;
            match delta.get("type").and_then(|v| v.as_str()) {
                Some("input_json_delta") => Some(StreamEvent::ToolCallDelta {
                    call_id: None,
                    index: value.get("index").and_then(|v| v.as_u64()).map(|i| i as u32),
                    name: None,
                    arguments_delta: delta
                        .get("partial_json")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                }),
                _ => {
                    let text = delta.get("text")?.as_str()?.to_string();
                    if text.is_empty() {
                        None
                    } else {
                        Some(StreamEvent::Delta(text))
                    }
                }
            }
        }
        // Anthropic tool_use blocks open in content_block_start; their
        // arguments stream (or arrive) in the input field.
        "content_block_start" => {
            let block = value.get("content_block")?;
            if block.get("type").and_then(|v| v.as_str()) != Some("tool_use") {
                return None;
            }
            let id = block.get("id").and_then(|v| v.as_str())?;
            let name = block.get("name").and_then(|v| v.as_str())?;
            let input = block.get("input").cloned().unwrap_or(Value::Null);
            // An empty input object is the streaming handshake: the real
            // arguments follow as input_json_delta fragments. Seeding the
            // buffer with "{}" would corrupt the assembled JSON.
            let arguments_delta = match input {
                Value::Null => String::new(),
                Value::Object(ref o) if o.is_empty() => String::new(),
                v => v.to_string(),
            };
            Some(StreamEvent::ToolCallDelta {
                call_id: Some(id.to_string()),
                index: value.get("index").and_then(|v| v.as_u64()).map(|i| i as u32),
                name: Some(name.to_string()),
                arguments_delta,
            })
        }
        // The authoritative stop reason (e.g. "tool_use") travels here.
        "message_delta" => {
            let reason = value
                .get("delta")
                .and_then(|d| d.get("stop_reason"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Some(StreamEvent::Completed {
                stop_reason: reason,
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

/// Usage fields carried by ONE provider frame. Providers split usage across
/// frames (Anthropic `message_start` → input, `message_delta` → output), so
/// the transport merges successive frames into a full snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct UsageFrame {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

/// Extracts usage fields from one usage-bearing SSE payload, if the frame
/// carries any. OpenAI-compatible: the final chunk carries a top-level
/// `usage` object (requested via `stream_options.include_usage`).
/// Anthropic: `message_start` → `message.usage.input_tokens`;
/// `message_delta` → `usage.output_tokens`.
pub fn extract_usage_frame(provider: Provider, payload: &str) -> Option<UsageFrame> {
    let value: Value = serde_json::from_str(payload).ok()?;
    match provider {
        Provider::OpenAi => {
            let usage = value.get("usage")?;
            Some(UsageFrame {
                input_tokens: usage.get("prompt_tokens").and_then(|v| v.as_u64()),
                output_tokens: usage.get("completion_tokens").and_then(|v| v.as_u64()),
            })
        }
        Provider::Anthropic => {
            let kind = value.get("type")?.as_str()?;
            let usage = match kind {
                "message_start" => value.get("message")?.get("usage")?,
                "message_delta" => value.get("usage")?,
                _ => return None,
            };
            Some(UsageFrame {
                input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()),
                output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()),
            })
        }
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
            messages: vec![ChatMessage::user("hello")],
            max_output_tokens: 256,
            temperature: 0.2,
            tools: Vec::new(),
        }
    }

    fn tool_definition() -> ToolDefinition {
        ToolDefinition {
            name: "fs.read".into(),
            description: "Read a file".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
            }),
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
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert!(body.get("tools").is_none(), "no tools -> no tools field");
    }

    #[test]
    fn openai_body_sends_tools_and_tool_result_turns() {
        let mut request = request();
        request.tools = vec![tool_definition()];
        request.messages.push(ChatMessage::assistant_with_tool_calls(
            "",
            vec![ToolCallData {
                call_id: "call-9".into(),
                name: "fs.read".into(),
                arguments: r#"{"path":"a.rs"}"#.into(),
            }],
        ));
        request
            .messages
            .push(ChatMessage::tool_result("call-9", r#"{"lines": 12}"#, false));
        let body = openai_request_body(&request);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "fs.read");
        assert_eq!(tools[0]["function"]["parameters"]["type"], "object");
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["tool_calls"][0]["id"], "call-9");
        assert_eq!(messages[2]["tool_calls"][0]["function"]["name"], "fs.read");
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call-9");
    }

    #[test]
    fn anthropic_body_sends_tools_and_tool_result_turns() {
        let mut request = request();
        request.tools = vec![tool_definition()];
        request.messages.push(ChatMessage::assistant_with_tool_calls(
            "reading",
            vec![ToolCallData {
                call_id: "toolu-1".into(),
                name: "fs.read".into(),
                arguments: r#"{"path":"a.rs"}"#.into(),
            }],
        ));
        request
            .messages
            .push(ChatMessage::tool_result("toolu-1", r#"{"lines": 12}"#, true));
        let body = anthropic_request_body(&request);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools[0]["name"], "fs.read");
        assert_eq!(tools[0]["input_schema"]["type"], "object");
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["type"], "text");
        assert_eq!(messages[1]["content"][1]["type"], "tool_use");
        assert_eq!(messages[1]["content"][1]["id"], "toolu-1");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "toolu-1");
        assert_eq!(messages[2]["content"][0]["is_error"], true);
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
    fn anthropic_sse_deltas_parse_and_message_delta_carries_stop_reason() {
        let line =
            r#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"Yo"}}"#;
        assert_eq!(
            parse_anthropic_sse_payload(line.trim_start_matches("data: ")),
            Some(StreamEvent::Delta("Yo".into()))
        );
        assert_eq!(
            parse_anthropic_sse_payload(
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}"#
            ),
            Some(StreamEvent::Completed {
                stop_reason: Some("tool_use".into())
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

    #[test]
    fn usage_frames_extract_from_both_providers() {
        // OpenAI-compatible final usage chunk.
        let f = extract_usage_frame(
            Provider::OpenAi,
            r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":4}}"#,
        )
        .unwrap();
        assert_eq!(f.input_tokens, Some(10));
        assert_eq!(f.output_tokens, Some(4));
        // Anthropic message_start (input) and message_delta (output).
        let f = extract_usage_frame(
            Provider::Anthropic,
            r#"{"type":"message_start","message":{"usage":{"input_tokens":42,"output_tokens":1}}}"#,
        )
        .unwrap();
        assert_eq!(f.input_tokens, Some(42));
        let f = extract_usage_frame(
            Provider::Anthropic,
            r#"{"type":"message_delta","usage":{"output_tokens":9}}"#,
        )
        .unwrap();
        assert_eq!(f.output_tokens, Some(9));
        // Plain deltas carry no usage.
        assert!(extract_usage_frame(
            Provider::OpenAi,
            r#"{"choices":[{"delta":{"content":"x"}}]}"#
        )
        .is_none());
    }

    /// OpenAI streaming: fragmented tool_calls deltas assemble into one
    /// dispatchable ToolRequest before the completion (docs/14 step 5).
    #[test]
    fn openai_fragmented_tool_calls_assemble() {
        let mut assembler = ToolCallAssembler::new();
        let frames = [
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-a","function":{"name":"fs.read","arguments":"{\"pa"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"x.rs\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ];
        let mut events = Vec::new();
        for f in frames {
            if let Some(e) = parse_openai_sse_payload(f) {
                events.extend(assembler.feed(e));
            }
        }
        let expected = vec![
            StreamEvent::ToolRequest {
                call_id: "call-a".into(),
                name: "fs.read".into(),
                arguments: r#"{"path":"x.rs"}"#.into(),
            },
            StreamEvent::Completed {
                stop_reason: Some("tool_calls".into()),
            },
            StreamEvent::Completed {
                stop_reason: Some("stop".into()),
            },
        ];
        assert_eq!(events, expected);
    }

    /// Anthropic streaming: content_block_start opens the call,
    /// input_json_delta streams arguments, message_delta completes.
    #[test]
    fn anthropic_streamed_tool_use_assembles() {
        let mut assembler = ToolCallAssembler::new();
        let frames = [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu-7","name":"fs.read","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"th\":\"y.rs\"}"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}"#,
            r#"{"type":"message_stop"}"#,
        ];
        let mut events = Vec::new();
        for f in frames {
            if let Some(e) = parse_anthropic_sse_payload(f) {
                events.extend(assembler.feed(e));
            }
        }
        // message_stop ALSO completes: the second flush must not re-emit.
        assert_eq!(
            events,
            vec![
                StreamEvent::ToolRequest {
                    call_id: "toolu-7".into(),
                    name: "fs.read".into(),
                    arguments: r#"{"path":"y.rs"}"#.into(),
                },
                StreamEvent::Completed {
                    stop_reason: Some("tool_use".into()),
                },
                StreamEvent::Completed {
                    stop_reason: Some("end_turn".into()),
                },
            ]
        );
    }

    /// A fragment stream whose identity never resolves is skipped, never
    /// dispatched (docs/14 step 5: reject before side effects).
    #[test]
    fn unidentifiable_fragments_are_never_dispatched() {
        let mut assembler = ToolCallAssembler::new();
        let frames = [
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"x\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ];
        let mut events = Vec::new();
        for f in frames {
            if let Some(e) = parse_openai_sse_payload(f) {
                events.extend(assembler.feed(e));
            }
        }
        assert_eq!(
            events,
            vec![StreamEvent::Completed {
                stop_reason: Some("tool_calls".into())
            }]
        );
    }

    /// Two parallel calls stream by index; both assemble in provider order.
    #[test]
    fn parallel_tool_calls_assemble_in_provider_order() {
        let mut assembler = ToolCallAssembler::new();
        let frames = [
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"fs.read","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call-2","function":{"name":"fs.list","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ];
        let mut events = Vec::new();
        for f in frames {
            if let Some(e) = parse_openai_sse_payload(f) {
                events.extend(assembler.feed(e));
            }
        }
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], StreamEvent::ToolRequest { call_id, name, .. } if call_id == "call-1" && name == "fs.read"));
        assert!(matches!(&events[1], StreamEvent::ToolRequest { call_id, name, .. } if call_id == "call-2" && name == "fs.list"));
    }

    /// OpenAI usage chunk parses to a Usage stream event.
    #[test]
    fn openai_usage_chunk_parses() {
        let payload = r#"{"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":7}}"#;
        assert_eq!(
            parse_openai_sse_payload(payload),
            Some(StreamEvent::Usage(TokenUsage {
                input_tokens: 11,
                output_tokens: 7
            }))
        );
    }
}
