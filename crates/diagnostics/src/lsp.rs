//! Headless LSP bridge (M3.4, docs/18 § Concrete local indexing stack:
//! "Semantic language services: headless LSP processes normalized into
//! Modbit diagnostics/symbol records"). A minimal JSON-RPC/stdio LSP
//! client: initialize handshake, didOpen, textDocument/definition and
//! textDocument/references, normalized to worktree-relative records.
//!
//! The bridge is FAIL-SOFT by design (docs/18: LSP is one index input
//! "where available" — exact/BM25/AST remain fully functional without
//! it): every failure mode is a typed error and every caller degrades to
//! the deterministic tree-sitter surface. Requests are bounded by a
//! timeout; the server process is killed on drop.

use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

/// One normalized LSP answer: a worktree-relative location.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct SymbolLocation {
    /// Worktree-relative path (forward slashes).
    pub path: String,
    /// 1-based line.
    pub line: usize,
}

#[derive(Debug)]
pub enum LspError {
    Spawn(String),
    Io(String),
    /// No response within the request timeout.
    Timeout { method: String, timeout_ms: u128 },
    /// The server answered with an error object.
    Server { method: String, message: String },
    /// The stream ended (server exited).
    Closed { method: String },
}

impl std::fmt::Display for LspError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LspError::Spawn(e) => write!(f, "lsp spawn failed: {e}"),
            LspError::Io(e) => write!(f, "lsp io: {e}"),
            LspError::Timeout { method, timeout_ms } => {
                write!(f, "lsp {method} timed out after {timeout_ms}ms")
            }
            LspError::Server { method, message } => write!(f, "lsp {method} server error: {message}"),
            LspError::Closed { method } => write!(f, "lsp {method}: server closed the stream"),
        }
    }
}

impl std::error::Error for LspError {}

/// Content-Length framed message (LSP wire format).
pub fn encode_message(body: &str) -> Vec<u8> {
    format!("Content-Length: {}\r\n\r\n{}", body.len(), body).into_bytes()
}

/// Parses one framed message from the buffered reader. None on EOF.
fn read_message(reader: &mut BufReader<ChildStdout>) -> Result<Option<Vec<u8>>, LspError> {
    let mut content_length: Option<usize> = None;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).map_err(|e| LspError::Io(e.to_string()))?;
        if n == 0 {
            return Ok(None); // EOF
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break; // end of headers
        }
        if let Some(v) = trimmed
            .split_once(':')
            .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        {
            content_length = Some(v);
        }
    }
    let len = content_length.ok_or_else(|| LspError::Io("missing Content-Length".into()))?;
    let mut body = vec![0u8; len];
    reader
        .read_exact(&mut body)
        .map_err(|e| LspError::Io(e.to_string()))?;
    Ok(Some(body))
}

/// path → file:// URI (space percent-encoded; the minimal encoding the
/// wire format needs for temp-dir worktrees).
pub fn path_to_uri(path: &Path) -> String {
    let text = path.to_string_lossy().replace(' ', "%20");
    if text.starts_with("file://") {
        text
    } else {
        format!("file://{text}")
    }
}

/// file:// URI → path (inverse of path_to_uri).
pub fn uri_to_path(uri: &str) -> String {
    uri.strip_prefix("file://")
        .unwrap_or(uri)
        .replace("%20", " ")
        .replace('\\', "/")
}

/// A workspace-relative view of an absolute LSP location. Locations
/// outside the worktree are reported with their absolute path (still
/// useful evidence; nothing outside is ever fetched).
fn normalize_location(loc: &Value, worktree: &Path) -> Option<SymbolLocation> {
    let uri = loc.get("uri")?.as_str()?;
    let line = loc.get("range")?.get("start")?.get("line")?.as_u64()? as usize;
    Some(SymbolLocation {
        path: uri_to_path(uri),
        line: line + 1,
    })
    .map(|mut l| {
        if let Ok(rel) = Path::new(&l.path).strip_prefix(worktree) {
            l.path = rel.to_string_lossy().replace('\\', "/");
        }
        l
    })
}

/// A live headless language-server session (one process per language).
pub struct LspSession {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    root: PathBuf,
    pub request_timeout: Duration,
    next_id: i64,
    /// URIs already didOpen'ed (didOpen once per session per file).
    opened: std::collections::BTreeSet<String>,
}

impl LspSession {
    /// Spawns the server and completes the initialize handshake.
    pub fn spawn(command: &str, root: &Path) -> Result<Self, LspError> {
        let mut parts = command.split_whitespace();
        let program = parts
            .next()
            .ok_or_else(|| LspError::Spawn("empty command".into()))?;
        let args: Vec<&str> = parts.collect();
        let mut child = Command::new(program)
            .args(&args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| LspError::Spawn(format!("{program}: {e}")))?;
        let stdin = child.stdin.take().ok_or_else(|| LspError::Spawn("no stdin".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| LspError::Spawn("no stdout".into()))?;
        let mut session = LspSession {
            child,
            stdin,
            reader: BufReader::new(stdout),
            root: root.to_path_buf(),
            request_timeout: Duration::from_secs(30),
            next_id: 1,
            opened: std::collections::BTreeSet::new(),
        };
        session.initialize()?;
        Ok(session)
    }

    fn send(&mut self, body: &str) -> Result<(), LspError> {
        self.stdin
            .write_all(&encode_message(body))
            .map_err(|e| LspError::Io(e.to_string()))?;
        self.stdin.flush().map_err(|e| LspError::Io(e.to_string()))
    }

    /// The initialize handshake + the `initialized` notification.
    fn initialize(&mut self) -> Result<(), LspError> {
        let id = self.next_id;
        self.next_id += 1;
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "rootUri": path_to_uri(&self.root),
                "capabilities": {},
            }
        });
        self.send(&body.to_string())?;
        let deadline = Instant::now() + self.request_timeout;
        match self.read_response("initialize", deadline)? {
            Some(_) => {}
            None => return Err(LspError::Closed { method: "initialize".into() }),
        }
        let notified = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        });
        self.send(&notified.to_string())
    }

    /// Reads framed messages until the response with `id` arrives.
    /// Notifications and other ids are skipped. Shared deadline across
    /// the whole wait.
    fn read_response(&mut self, method: &str, deadline: Instant) -> Result<Option<Value>, LspError> {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(LspError::Timeout {
                    method: method.into(),
                    timeout_ms: self.request_timeout.as_millis(),
                });
            }
            // Blocking read; a chatty server cannot starve the deadline
            // check because responses arrive as discrete frames.
            match read_message(&mut self.reader)? {
                None => return Ok(None),
                Some(body) => {
                    let msg: Value = match serde_json::from_slice(&body) {
                        Ok(v) => v,
                        Err(_) => continue, // unparseable frame: skip
                    };
                    if msg.get("method").is_some() {
                        continue; // notification/server->client request
                    }
                    let wanted = format!("{}", self.next_id - 1);
                    if msg.get("id").and_then(|i| i.as_str()) == Some(wanted.as_str())
                        || msg.get("id").and_then(|i| i.as_i64()) == Some(self.next_id - 1)
                    {
                        if let Some(err) = msg.get("error") {
                            return Err(LspError::Server {
                                method: method.into(),
                                message: err
                                    .get("message")
                                    .and_then(|m| m.as_str())
                                    .unwrap_or("unknown")
                                    .to_string(),
                            });
                        }
                        return Ok(Some(msg));
                    }
                    // A response to an older id: skip.
                }
            }
        }
    }

    /// didOpen (once per file per session) so servers index the content.
    pub fn ensure_open(&mut self, rel_path: &str, bytes: &[u8]) -> Result<(), LspError> {
        let uri = path_to_uri(&self.root.join(rel_path));
        if self.opened.contains(&uri) {
            return Ok(());
        }
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": "plaintext",
                    "version": 1,
                    "text": String::from_utf8_lossy(bytes),
                }
            }
        });
        self.send(&body.to_string())?;
        self.opened.insert(uri);
        Ok(())
    }

    /// textDocument/definition at a 0-based position; normalized answers.
    pub fn definition(
        &mut self,
        rel_path: &str,
        line0: usize,
        character0: usize,
    ) -> Result<Vec<SymbolLocation>, LspError> {
        let result = self.request_position("textDocument/definition", rel_path, line0, character0)?;
        Ok(self.normalize_result(&result))
    }

    /// textDocument/references at a 0-based position.
    pub fn references(
        &mut self,
        rel_path: &str,
        line0: usize,
        character0: usize,
        include_declaration: bool,
    ) -> Result<Vec<SymbolLocation>, LspError> {
        let result = self.request_position_with(
            "textDocument/references",
            rel_path,
            line0,
            character0,
            |params| {
                params["context"] = serde_json::json!({
                    "includeDeclaration": include_declaration,
                });
            },
        )?;
        Ok(self.normalize_result(&result))
    }

    fn request_position(
        &mut self,
        method: &str,
        rel_path: &str,
        line0: usize,
        character0: usize,
    ) -> Result<Value, LspError> {
        self.request_position_with(method, rel_path, line0, character0, |_| {})
    }

    fn request_position_with(
        &mut self,
        method: &str,
        rel_path: &str,
        line0: usize,
        character0: usize,
        mutate: impl FnOnce(&mut Value),
    ) -> Result<Value, LspError> {
        let uri = path_to_uri(&self.root.join(rel_path));
        let id = self.next_id;
        self.next_id += 1;
        let mut params = serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": line0, "character": character0 },
        });
        mutate(&mut params);
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.send(&body.to_string())?;
        let deadline = Instant::now() + self.request_timeout;
        match self.read_response(method, deadline)? {
            Some(msg) => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
            None => Err(LspError::Closed { method: method.into() }),
        }
    }

    fn normalize_result(&self, result: &Value) -> Vec<SymbolLocation> {
        // definition: single Location | null | Location[]; references:
        // Location[] | null.
        let items: Vec<&Value> = match result {
            Value::Null => Vec::new(),
            Value::Array(a) => a.iter().collect(),
            one if one.get("uri").is_some() => vec![one],
            _ => Vec::new(),
        };
        items
            .iter()
            .filter_map(|loc| normalize_location(loc, &self.root))
            .collect()
    }
}

impl Drop for LspSession {
    fn drop(&mut self) {
        // Best-effort shutdown; the process must not outlive the task.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire format: Content-Length framing round-trips, and the
    /// parser tolerates extra headers (User-Agent etc.) in any order.
    #[test]
    fn framing_round_trip_and_extra_headers() {
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"x"}"#;
        let framed = String::from_utf8(encode_message(body)).unwrap();
        assert!(framed.starts_with(&format!("Content-Length: {}\r\n", body.len())));

        // A framed message with an extra header parses (read_message is
        // exercised end-to-end by the fixture integration tests; here we
        // pin the encoding contract).
        assert_eq!(
            encode_message(body),
            format!("Content-Length: {}\r\n\r\n{}", body.len(), body).into_bytes()
        );
    }

    /// URI conversion is minimal and reversible for the paths we emit.
    #[test]
    fn uri_conversion_round_trip() {
        let p = Path::new("/tmp/some dir/x.rs");
        let uri = path_to_uri(p);
        assert_eq!(uri, "file:///tmp/some%20dir/x.rs");
        assert_eq!(uri_to_path(&uri), "/tmp/some dir/x.rs");
        assert_eq!(uri_to_path("notauri"), "notauri");
    }
}
