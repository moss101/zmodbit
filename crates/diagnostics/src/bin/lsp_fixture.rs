//! A minimal REAL LSP server over stdio (M3.4 CI fixture): speaks the
//! actual wire protocol (Content-Length framing, JSON-RPC) with canned
//! semantic answers. It exists so the bridge in `modbit_diagnostics::lsp`
//! is tested against a genuine protocol peer on every CI runner — no
//! language server installation required. NOT a production component.
//!
//! Behavior:
//! - `initialize` → capabilities reply
//! - `initialized`, `textDocument/didOpen`, `exit` → acknowledged silently
//! - `textDocument/definition` → the location of `fn fixture_target()`
//!   in `src/lib.rs` (line 2, 0-based), when the queried position sits on
//!   a `fixture_target` occurrence; null otherwise
//! - `textDocument/references` → three canned reference locations
//! - `shutdown` → ok, then exit on `exit`
//!
//! The canned answers are keyed on the symbol text in the didOpen'd
//! document (the fixture looks up the position in the opened text), so
//! the round trip is real: the client sends a position, the server
//! resolves it against document text.

use std::io::{BufRead, BufReader, Write};
use std::process::exit;

fn read_message(reader: &mut impl BufRead) -> Option<Vec<u8>> {
    let mut content_length: Option<usize> = None;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).ok()?;
        if n == 0 {
            return None;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some(v) = trimmed
            .split_once(':')
            .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        {
            content_length = Some(v);
        }
    }
    let len = content_length?;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).ok()?;
    Some(body)
}

fn send(writer: &mut impl Write, body: &str) {
    let _ = writer.write_all(format!("Content-Length: {}\r\n\r\n{}", body.len(), body).as_bytes());
    let _ = writer.flush();
}

fn reply(writer: &mut impl Write, id: &serde_json::Value, result: serde_json::Value) {
    let body = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
    send(writer, &body.to_string());
}

fn main() {
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();

    // uri → document text (didOpen).
    let mut documents: std::collections::BTreeMap<String, String> = Default::default();

    while let Some(body) = read_message(&mut reader) {
        let Ok(msg) = serde_json::from_slice::<serde_json::Value>(&body) else { continue };
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or_default();

        match method {
            "initialize" => {
                reply(
                    &mut writer,
                    id.as_ref().unwrap_or(&serde_json::json!(1)),
                    serde_json::json!({
                        "capabilities": {
                            "textDocumentSync": 1,
                            "definitionProvider": true,
                            "referencesProvider": true,
                        },
                        "serverInfo": { "name": "lsp-fixture" },
                    }),
                );
            }
            "initialized" | "textDocument/didOpen" | "$/cancelRequest" => {
                if method == "textDocument/didOpen" {
                    if let Some(doc) = params.get("textDocument") {
                        if let (Some(uri), Some(text)) =
                            (doc.get("uri").and_then(|u| u.as_str()), doc.get("text").and_then(|t| t.as_str()))
                        {
                            documents.insert(uri.to_string(), text.to_string());
                        }
                    }
                }
            }
            "textDocument/definition" | "textDocument/references" => {
                let uri = params
                    .get("textDocument")
                    .and_then(|d| d.get("uri"))
                    .and_then(|u| u.as_str())
                    .unwrap_or("")
                    .to_string();
                let line = params
                    .pointer("/position/line")
                    .and_then(|l| l.as_u64())
                    .unwrap_or(0) as usize;
                let text = documents.get(&uri).cloned().unwrap_or_default();
                // Resolve the word at the requested position against the
                // opened document text (a real semantic lookup shape).
                let lines: Vec<&str> = text.lines().collect();
                let word = lines
                    .get(line)
                    .and_then(|l| {
                        let bytes = l.as_bytes();
                        let mut start = line_char_to_byte(l, params.pointer("/position/character").and_then(|c| c.as_u64()).unwrap_or(0) as usize);
                        if start > bytes.len() {
                            start = bytes.len();
                        }
                        let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
                        let mut s = start;
                        while s > 0 && is_word(bytes[s - 1]) {
                            s -= 1;
                        }
                        let mut e = start;
                        while e < bytes.len() && is_word(bytes[e]) {
                            e += 1;
                        }
                        if s < e { Some(l[s..e].to_string()) } else { None }
                    });

                let include_decl = params
                    .pointer("/context/includeDeclaration")
                    .and_then(|b| b.as_bool())
                    .unwrap_or(false);

                if method == "textDocument/definition" {
                    // The fixture's target: `fixture_target` is DEFINED on
                    // line 2 (0-based) of the same document.
                    if word.as_deref() == Some("fixture_target") {
                        reply(
                            &mut writer,
                            id.as_ref().unwrap(),
                            serde_json::json!({
                                "uri": uri,
                                "range": { "start": { "line": 2, "character": 7 },
                                           "end": { "line": 2, "character": 21 } },
                            }),
                        );
                    } else {
                        reply(&mut writer, id.as_ref().unwrap(), serde_json::Value::Null);
                    }
                } else {
                    // References: canned across two documents; declaration
                    // included only when asked (honored from the request).
                    let mut refs = vec![
                        serde_json::json!({
                            "uri": uri,
                            "range": { "start": { "line": 4, "character": 4 },
                                       "end": { "line": 4, "character": 18 } },
                        }),
                        serde_json::json!({
                            "uri": format!("file:///{}", "src/other.rs"),
                            "range": { "start": { "line": 8, "character": 0 },
                                       "end": { "line": 8, "character": 14 } },
                        }),
                    ];
                    if include_decl {
                        refs.push(serde_json::json!({
                            "uri": uri,
                            "range": { "start": { "line": 2, "character": 7 },
                                       "end": { "line": 2, "character": 21 } },
                        }));
                    }
                    reply(&mut writer, id.as_ref().unwrap(), serde_json::json!(refs));
                }
            }
            "shutdown" => {
                if let Some(id) = id.as_ref() {
                    reply(&mut writer, id, serde_json::Value::Null);
                }
            }
            "exit" => exit(0),
            _ => {
                // Unknown REQUESTS must still be answered (else the client
                // stalls); unknown notifications are ignored.
                if let Some(id) = id {
                    reply(&mut writer, &id, serde_json::Value::Null);
                }
            }
        }
    }
}

fn line_char_to_byte(line: &str, character: usize) -> usize {
    line.char_indices()
        .nth(character)
        .map(|(b, _)| b)
        .unwrap_or(line.len())
}
