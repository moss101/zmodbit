//! Tool-protocol round trip (Future-tasks.md Phase 1 item 2, docs/15):
//! ToolRegistry schemas → ModelRequest.tools → provider request body →
//! REAL HttpStreamTransport → local TCP fixture → fragmented tool_call
//! SSE → parsers → ToolCallAssembler → ToolRequest → tool_result message
//! fed back into the next request body. Proves the production chain, not a
//! single parser in isolation.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use modbit_providers::gateway::{
    anthropic_request_body, openai_request_body, parse_openai_sse_payload, ChatMessage,
    ModelRequest, Provider, Role, ToolCallAssembler, ToolDefinition, ToolCallData,
};
use modbit_providers::transport::{
    HttpStreamTransport, ModelTransport as _, OutgoingRequest, SecretBroker, TransportEvent,
};

/// Loopback credential: proves header wiring without touching process env.
struct FixtureBroker;
impl SecretBroker for FixtureBroker {
    fn credential(&self, _name: &str) -> Result<String, modbit_providers::transport::TransportError> {
        Ok("fixture-key".into())
    }
}
use modbit_tools::{schema::ParamSpec, schema::ParamType, schema::ToolSchema};
use std::collections::BTreeMap;

const OPEN_FRAMES: &[&str] = &[
    r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-77","function":{"name":"modbit.file.read","arguments":"{\"pa"}}]}}]}"#,
    r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"src/lib.rs\"}"}}]}}]}"#,
    r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
    r#"data: {"choices":[],"usage":{"prompt_tokens":18,"completion_tokens":6}}"#,
    r#"data: [DONE]"#,
];

const REPAIR_FRAMES: &[&str] = &[
    r#"data: {"choices":[{"delta":{"content":"file read: 12 lines"}}]}"#,
    r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
    r#"data: [DONE]"#,
];

fn file_read_schema() -> ToolSchema {
    let mut parameters = BTreeMap::new();
    parameters.insert(
        "path".to_string(),
        ParamSpec {
            param_type: ParamType::Str,
            required: true,
            default: None,
            description: "File to read".into(),
        },
    );
    ToolSchema {
        aliases: BTreeMap::new(),
        parameters,
    }
}

fn projected_request(model_request: &ModelRequest, provider: Provider) -> OutgoingRequest {
    let body = match provider {
        Provider::OpenAi => openai_request_body(model_request),
        Provider::Anthropic => anthropic_request_body(model_request),
    };
    OutgoingRequest {
        provider,
        url: format!("http://{}/v1/chat/completions", "127.0.0.1:1"), // replaced per test
        body: serde_json::to_vec(&body).unwrap(),
        timeout: Duration::from_secs(10),
    }
}

#[tokio::test]
async fn tool_projection_streams_calls_and_feeds_results_back() {
    // The fixture asserts what the PROVIDER would see: tools projected from
    // the registry on turn 1, and the tool_result turn on turn 2.
    let seen_bodies: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let bodies = seen_bodies.clone();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let bodies = bodies.clone();
            tokio::spawn(async move {
                handle(socket, bodies).await;
            });
        }
    });

    async fn handle(mut socket: TcpStream, bodies: Arc<Mutex<Vec<serde_json::Value>>>) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = socket.read(&mut chunk).await.unwrap();
            assert!(n > 0);
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf).to_string();
            if let Some(end) = text.find("\r\n\r\n") {
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
                    socket.read_exact(&mut rest).await.unwrap();
                    buf.extend_from_slice(&rest);
                }
                let body_start = end + 4;
                let raw = String::from_utf8_lossy(&buf[body_start..]).to_string();
                bodies
                    .lock()
                    .unwrap()
                    .push(serde_json::from_str(&raw).expect("fixture got valid JSON body"));
                break;
            }
        }
        let turn = bodies.lock().unwrap().len();
        let frames: &[&str] = if turn == 1 { OPEN_FRAMES } else { REPAIR_FRAMES };
        let mut payload = String::from(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        );
        for f in frames {
            payload.push_str(f);
            payload.push_str("\n\n");
        }
        socket.write_all(payload.as_bytes()).await.unwrap();
    }

    // Turn 1: registry schemas project into the request.
    let registry = modbit_tools::ToolRegistry::new();
    registry
        .register_with_schema(
            "modbit.file.read",
            "1.0.0",
            modbit_policy::EffectClass::ReadOnly,
            "Read a file from the workspace",
            Some(file_read_schema()),
            Arc::new(|_args| Ok(serde_json::json!({"lines": 12}))),
        )
        .unwrap();

    let mut request = ModelRequest {
        request_id: "roundtrip-1".into(),
        model: "test-model".into(),
        system: "use tools".into(),
        messages: vec![ChatMessage::user("read src/lib.rs")],
        max_output_tokens: 256,
        temperature: 0.0,
        reasoning_effort: None,
        tools: registry
            .tool_definitions()
            .into_iter()
            .map(|t| ToolDefinition {
                name: t.name,
                description: t.description,
                parameters: t.parameters,
            })
            .collect(),
    };
    assert_eq!(request.tools.len(), 1, "registry schema must project");

    let (events1, usage) = stream_once(request.clone(), addr).await;
    // Assembler produced the dispatchable call from fragments.
    let tool_request = events1
        .iter()
        .find(|e| matches!(e, modbit_providers::gateway::StreamEvent::ToolRequest { .. }))
        .expect("assembler must yield a dispatchable ToolRequest");
    let modbit_providers::gateway::StreamEvent::ToolRequest {
        call_id,
        name,
        arguments,
    } = tool_request
    else {
        unreachable!()
    };
    assert_eq!(name, "modbit.file.read");
    assert_eq!(arguments, r#"{"path":"src/lib.rs"}"#);
    assert_eq!(usage, Some((18, 6)), "usage captured from the usage chunk");

    // Turn 2: the tool result rides back as a typed tool message.
    let executed = serde_json::json!({"lines": 12});
    request.messages.push(ChatMessage::assistant_with_tool_calls(
        "",
        vec![ToolCallData {
            call_id: call_id.clone(),
            name: name.clone(),
            arguments: arguments.clone(),
        }],
    ));
    request
        .messages
        .push(ChatMessage::tool_result(call_id, executed.to_string(), false));
    request.request_id = "roundtrip-2".into();

    let (events2, _) = stream_once(request.clone(), addr).await;
    assert!(events2.contains(&modbit_providers::gateway::StreamEvent::Delta(
        "file read: 12 lines".into()
    )));

    // Provider-side assertions: what actually travelled on the wire.
    let bodies = seen_bodies.lock().unwrap();
    assert_eq!(bodies.len(), 2);
    let tools = bodies[0]["tools"].as_array().unwrap();
    assert_eq!(tools[0]["function"]["name"], "modbit.file.read");
    assert_eq!(
        tools[0]["function"]["parameters"]["properties"]["path"]["type"],
        "string"
    );
    assert_eq!(tools[0]["function"]["parameters"]["required"][0], "path");
    let messages = bodies[1]["messages"].as_array().unwrap();
    let tool_msg = messages
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("tool result must travel as a tool-role message");
    assert_eq!(tool_msg["tool_call_id"], "call-77");
    assert_eq!(tool_msg["content"], executed.to_string());
    let assistant = messages
        .iter()
        .find(|m| m["role"] == "assistant")
        .expect("assistant turn with tool_calls must travel");
    assert_eq!(assistant["tool_calls"][0]["id"], "call-77");
}

/// Drives the REAL HttpStreamTransport against the fixture and normalizes
/// the stream exactly like the runtime: transport events → parsers →
/// assembler. Returns normalized events plus the (input, output) usage.
async fn stream_once(
    request: ModelRequest,
    addr: std::net::SocketAddr,
) -> (
    Vec<modbit_providers::gateway::StreamEvent>,
    Option<(u64, u64)>,
) {
    let transport = HttpStreamTransport::new(Arc::new(FixtureBroker)).unwrap();
    let mut outgoing = projected_request(&request, Provider::OpenAi);
    outgoing.url = format!("http://{addr}/v1/chat/completions");
    let mut stream = transport.stream(outgoing).unwrap();

    let mut assembler = ToolCallAssembler::new();
    let mut events = Vec::new();
    let mut usage = None;
    while let Some(event) = stream.recv().await {
        match event.unwrap() {
            TransportEvent::SseData(payload) => {
                if let Some(parsed) = parse_openai_sse_payload(&payload) {
                    events.extend(assembler.feed(parsed));
                }
            }
            TransportEvent::Usage(u) => usage = Some((u.input_tokens, u.output_tokens)),
            TransportEvent::Eof => break,
        }
    }
    (events, usage)
}

/// The Anthropic variant of the tool_result turn: results serialize as
/// tool_result blocks inside a user message (no tool role on the wire).
#[test]
fn anthropic_tool_result_turn_serializes_without_tool_role() {
    let request = ModelRequest {
        request_id: "a-1".into(),
        model: "test-model".into(),
        system: String::new(),
        messages: vec![
            ChatMessage {
                role: Role::User,
                content: "read src/lib.rs".into(),
                tool_calls: Vec::new(),
                tool_call_id: None,
                is_error: false,
            },
            ChatMessage::assistant_with_tool_calls(
                "",
                vec![ToolCallData {
                    call_id: "toolu-1".into(),
                    name: "modbit.file.read".into(),
                    arguments: r#"{"path":"src/lib.rs"}"#.into(),
                }],
            ),
            ChatMessage::tool_result("toolu-1", "boom: denied", true),
        ],
        max_output_tokens: 64,
        temperature: 0.0,
        reasoning_effort: None,
        tools: Vec::new(),
    };
    let body = anthropic_request_body(&request);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(messages[2]["content"][0]["type"], "tool_result");
    assert_eq!(messages[2]["content"][0]["tool_use_id"], "toolu-1");
    assert_eq!(messages[2]["content"][0]["is_error"], true);
    assert_eq!(messages[2]["content"][0]["content"], "boom: denied");
}
