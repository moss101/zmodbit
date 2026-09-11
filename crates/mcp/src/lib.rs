//! modbit-mcp — Model Context Protocol client (Phase 6 item 3, docs/26 §
//! MCP): stdio transport, JSON-RPC 2.0, newline-delimited messages.
//!
//! The client SPAWNS a server subprocess (`command` + args), completes
//! the initialize handshake, then serves tools/list and tools/call.
//! External tools remain UNTRUSTED: results are returned as data, every
//! call is receipts-bound by the caller (docs/23), and policy decides
//! whether the call is allowed at all (docs/17 § external tool family).

use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// One tool advertised by an MCP server.
#[derive(Clone, Debug, PartialEq)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub raw_schema: Value,
}

#[derive(Debug)]
pub enum McpError {
    Spawn(String),
    Io(String),
    Protocol(String),
    Closed,
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpError::Spawn(e) => write!(f, "mcp spawn failed: {e}"),
            McpError::Io(e) => write!(f, "mcp io: {e}"),
            McpError::Protocol(e) => write!(f, "mcp protocol: {e}"),
            McpError::Closed => write!(f, "mcp server closed the stream"),
        }
    }
}

impl std::error::Error for McpError {}

/// A running MCP server session over stdio.
pub struct McpClient {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: i64,
    server_name: String,
    server_version: String,
}

impl McpClient {
    /// Spawns the server and completes the MCP initialize handshake.
    pub fn spawn(command: &str, args: &[String], server_name: &str) -> Result<Self, McpError> {
        let mut parts = command.split_whitespace();
        let program = parts
            .next()
            .ok_or_else(|| McpError::Spawn("empty command".into()))?;
        let rest: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut full_args: Vec<&str> = parts.collect();
        full_args.extend(rest);
        let mut child = Command::new(program)
            .args(&full_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| McpError::Spawn(format!("{program}: {e}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Spawn("no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Spawn("no stdout".into()))?;
        let mut client = McpClient {
            child,
            stdin,
            reader: BufReader::new(stdout),
            next_id: 1,
            server_name: server_name.to_string(),
            server_version: String::new(),
        };
        client.initialize()?;
        Ok(client)
    }

    fn send(&mut self, body: &Value) -> Result<(), McpError> {
        let mut line = serde_json::to_string(body).map_err(|e| McpError::Io(e.to_string()))?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.flush())
            .map_err(|e| McpError::Io(e.to_string()))
    }

    /// Reads newline-delimited JSON-RPC responses until the one with the
    /// matching id arrives (notifications skipped).
    fn read_response(&mut self, id: i64) -> Result<Value, McpError> {
        let mut line = String::new();
        loop {
            line.clear();
            let n = self
                .reader
                .read_line(&mut line)
                .map_err(|e| McpError::Io(e.to_string()))?;
            if n == 0 {
                return Err(McpError::Closed);
            }
            let Ok(msg) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if msg.get("method").is_some() {
                continue; // notification
            }
            if msg.get("id").and_then(|v| v.as_i64()) == Some(id) {
                if let Some(err) = msg.get("error") {
                    return Err(McpError::Protocol(
                        err.get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or("server error")
                            .to_string(),
                    ));
                }
                return Ok(msg);
            }
        }
    }

    fn initialize(&mut self) -> Result<(), McpError> {
        let id = self.next_id;
        self.next_id += 1;
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "modbit-core", "version": "0.1.0" },
            }
        });
        self.send(&body)?;
        let msg = self.read_response(id)?;
        if let Some(info) = msg.pointer("/result/serverInfo") {
            self.server_version = info
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
        }
        let notified = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        });
        self.send(&notified)
    }

    /// tools/list: the server's advertised tools.
    pub fn list_tools(&mut self) -> Result<Vec<McpTool>, McpError> {
        let id = self.next_id;
        self.next_id += 1;
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/list", "params": {}
        });
        self.send(&body)?;
        let msg = self.read_response(id)?;
        let tools_json = msg.pointer("/result/tools").cloned().unwrap_or_default();
        let mut tools = Vec::new();
        if let Some(arr) = tools_json.as_array() {
            for t in arr {
                tools.push(McpTool {
                    name: t.get("name").and_then(|v| v.as_str()).unwrap_or_default().into(),
                    description: t
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .into(),
                    raw_schema: t.get("inputSchema").cloned().unwrap_or_default(),
                });
            }
        }
        Ok(tools)
    }

    /// tools/call: invokes a tool with JSON arguments, returns the content
    /// text blocks joined (MCP result shape: content[] of {type, text}).
    pub fn call_tool(&mut self, tool: &str, arguments: &Value) -> Result<String, McpError> {
        let id = self.next_id;
        self.next_id += 1;
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": tool, "arguments": arguments },
        });
        self.send(&body)?;
        let msg = self.read_response(id)?;
        if let Some(err) = msg.get("error") {
            return Err(McpError::Protocol(
                err.get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("tools/call failed")
                    .to_string(),
            ));
        }
        let mut texts = Vec::new();
        if let Some(items) = msg.pointer("/result/content").and_then(|c| c.as_array()) {
            for item in items {
                if item.get("type").and_then(|v| v.as_str()) == Some("text") {
                    if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                        texts.push(t.to_string());
                    }
                }
            }
        }
        Ok(texts.join("\n"))
    }

    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    pub fn server_version(&self) -> &str {
        &self.server_version
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A configured external MCP server (Phase 6 item 3): name → spawn spec.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ExternalServer {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
}

/// A live pool of MCP server sessions, name-keyed (the scheduler holds
/// the pool; sessions are lazily spawned on first use and killed on drop).
pub struct McpPool {
    servers: BTreeMap<String, ExternalServer>,
    clients: Vec<McpClient>,
}


impl McpPool {
    pub fn new(servers: BTreeMap<String, ExternalServer>) -> Self {
        McpPool {
            servers,
            clients: Vec::new(),
        }
    }

    pub fn server_names(&self) -> Vec<String> {
        self.servers.keys().cloned().collect()
    }

    /// Lazily spawns (or reuses) the client for `name`.
    pub fn client(&mut self, name: &str) -> Result<&mut McpClient, McpError> {
        if !self.clients.iter().any(|c| c.server_name() == name) {
            let spec = self
                .servers
                .get(name)
                .ok_or_else(|| McpError::Spawn(format!("unknown mcp server {name:?}")))?;
            let client = McpClient::spawn(&spec.command, &spec.args, name)?;
            self.clients.push(client);
        }
        self.clients
            .iter_mut()
            .find(|c| c.server_name() == name)
            .ok_or(McpError::Closed)
    }

    /// tools/list across all configured servers: (server, tools).
    pub fn list_all(&mut self) -> Vec<(String, Result<Vec<McpTool>, String>)> {
        let names: Vec<String> = self.servers.keys().cloned().collect();
        names
            .into_iter()
            .map(|name| {
                let result = self
                    .client(&name)
                    .and_then(|c| c.list_tools())
                    .map_err(|e| e.to_string());
                (name, result)
            })
            .collect()
    }
}


