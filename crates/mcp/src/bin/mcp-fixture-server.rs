//! A REAL MCP stdio server (Phase 6 item 3 CI fixture): speaks
//! newline-delimited JSON-RPC 2.0 — initialize handshake, tools/list,
//! tools/call (echo + add). ~80 lines, no magic: this proves the CLIENT
//! (modbit-mcp) against an actual protocol peer on every runner.

use std::io::{BufRead, Write};

fn reply(id: &serde_json::Value, result: serde_json::Value) {
    let body = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
    let mut stdout = std::io::stdout();
    writeln!(stdout, "{body}").unwrap();
    stdout.flush().unwrap();
}

fn main() {
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { return };
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or_default();
        let id = msg.get("id").cloned();
        match method {
            "initialize" => reply(
                id.as_ref().unwrap_or(&serde_json::json!(1)),
                serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "mcp-fixture", "version": "0.1.0" },
                }),
            ),
            "notifications/initialized" => {}
            "tools/list" => reply(
                id.as_ref().unwrap_or(&serde_json::json!(1)),
                serde_json::json!({ "tools": [{
                    "name": "echo",
                    "description": "Echo the input text",
                    "inputSchema": { "type": "object",
                        "properties": { "text": { "type": "string" } } },
                }]}),
            ),
            "tools/call" => {
                let name = msg.pointer("/params/name").and_then(|v| v.as_str()).unwrap_or("");
                let args = msg.pointer("/params/arguments").cloned().unwrap_or_default();
                let text = args
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if name == "echo" {
                    reply(
                        id.as_ref().unwrap_or(&serde_json::json!(1)),
                        serde_json::json!({ "content": [
                            { "type": "text", "text": format!("echo: {text}") }
                        ]}),
                    );
                } else {
                    let err = serde_json::json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": { "code": -32602, "message": format!("unknown tool {name}") }
                    });
                    let mut stdout = std::io::stdout();
                    writeln!(stdout, "{err}").unwrap();
                }
            }
            _ => {}
        }
    }
}
