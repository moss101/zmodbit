//! Phase 6 item 3 — MCP client against a REAL stdio server: initialize
//! handshake, tools/list, tools/call (echo), and kill. The fixture is a
//! real JSON-RPC protocol peer built in this workspace.

use modbit_mcp::{ExternalServer, McpPool};

#[test]
fn mcp_client_handshakes_lists_and_calls_over_stdio() {
    let fixture = env!("CARGO_BIN_EXE_mcp-fixture-server").to_string();
    let mut servers = std::collections::BTreeMap::new();
    servers.insert(
        "fixture".to_string(),
        ExternalServer {
            name: "fixture".into(),
            command: fixture.clone(),
            args: vec![],
        },
    );
    let mut pool = McpPool::new(servers);

    // tools/list: the fixture advertises exactly "echo".
    let all = pool.list_all();
    assert_eq!(all.len(), 1);
    let (server, tools) = &all[0];
    assert_eq!(server, "fixture");
    let tools = tools.as_ref().expect("tools/list");
    assert_eq!(tools.len(), 1, "{tools:?}");
    assert_eq!(tools[0].name, "echo");
    assert!(tools[0].description.contains("Echo"));

    // tools/call echo: content text round-trips.
    let client = pool.client("fixture").expect("fixture client");
    let out = client
        .call_tool("echo", &serde_json::json!({ "text": "modbit" }))
        .expect("tools/call");
    assert_eq!(out, "echo: modbit");

    // Unknown tool: typed protocol error.
    let err = client.call_tool("nope", &serde_json::json!({}));
    assert!(err.is_err(), "unknown tool must be a typed error");
}
