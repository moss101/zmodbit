//! Runtime record store (M1 backend batch: REQ-EV-0221, REQ-EV-0098,
//! REQ-EV-0108). Durable tables for background operations, tool-call/result
//! pairing and content-addressed output references, per docs/31 § Core
//! tables. All survive hard kills: they live in `core.db` next to the events.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

pub const RUNTIME_MIGRATIONS: &[(&str, &str)] = &[
    (
        "output_refs",
        "CREATE TABLE IF NOT EXISTS output_refs (
            output_ref_id TEXT PRIMARY KEY,
            object_hash   TEXT NOT NULL,
            content_type  TEXT NOT NULL,
            byte_length   INTEGER NOT NULL,
            checksum      TEXT NOT NULL,
            preview_text  TEXT NOT NULL,
            payload       BLOB NOT NULL,
            created_at    TEXT NOT NULL,
            retention_class TEXT NOT NULL DEFAULT 'default'
        );",
    ),
    (
        "background_tasks",
        "CREATE TABLE IF NOT EXISTS background_tasks (
            handle_id    TEXT PRIMARY KEY,
            kind         TEXT NOT NULL,
            status       TEXT NOT NULL,
            output_ref_id TEXT REFERENCES output_refs(output_ref_id),
            bounded_preview TEXT NOT NULL,
            created_at   TEXT NOT NULL,
            stopped_at   TEXT
        );",
    ),
    (
        "tool_calls",
        "CREATE TABLE IF NOT EXISTS tool_calls (
            tool_call_id TEXT PRIMARY KEY,
            step_id      TEXT NOT NULL,
            tool_name    TEXT NOT NULL,
            tool_version TEXT,
            effect_class TEXT,
            status       TEXT NOT NULL,
            arguments_hash TEXT,
            result_payload BLOB,
            dispatched_at TEXT,
            completed_at  TEXT,
            result_ref    TEXT,
            unknown_outcome_reason TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_tool_calls_step ON tool_calls(step_id);
    ",
    ),
];

/// Bounded preview length for background listings (docs/31: bounded preview;
/// full output lives behind the OutputRef).
pub const PREVIEW_BYTES: usize = 256;

/// A tool call and its re-entry result payload (REQ-EV-0098).
pub type ToolPair = (String, String, Option<Vec<u8>>);

pub struct RuntimeStore {
    conn: Mutex<Connection>,
}

#[derive(Debug)]
pub enum RuntimeError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    NotFound(String),
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RuntimeError::Sqlite(e) => write!(f, "runtime sqlite: {e}"),
            RuntimeError::Io(e) => write!(f, "runtime io: {e}"),
            RuntimeError::NotFound(id) => write!(f, "not found: {id}"),
        }
    }
}

impl std::error::Error for RuntimeError {}

/// A content-addressed output reference: large payloads spill here instead
/// of crossing IPC (REQ-EV-0108).
#[derive(Clone, Debug, PartialEq)]
pub struct OutputRef {
    pub output_ref_id: String,
    pub object_hash: String,
    pub content_type: String,
    pub byte_length: u64,
    pub checksum: String,
    pub preview_text: String,
    pub payload: Vec<u8>,
}

impl RuntimeStore {
    pub fn open(path: &Path) -> Result<Self, RuntimeError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(RuntimeError::Io)?;
        }
        let conn = Connection::open(path).map_err(RuntimeError::Sqlite)?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(RuntimeError::Sqlite)?;
        for (_, sql) in RUNTIME_MIGRATIONS {
            conn.execute_batch(sql).map_err(RuntimeError::Sqlite)?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Stores bytes as a content-addressed output reference with a bounded
    /// preview (REQ-EV-0108: large payloads use OutputRef, never a giant IPC
    /// body).
    pub fn write_output_ref(
        &self,
        output_ref_id: &str,
        content_type: &str,
        payload: &[u8],
    ) -> Result<OutputRef, RuntimeError> {
        let mut hasher = Sha256::new();
        hasher.update(payload);
        let object_hash = format!("{:x}", hasher.finalize());
        let preview_text: String =
            String::from_utf8_lossy(&payload[..payload.len().min(PREVIEW_BYTES)])
                .chars()
                .take(PREVIEW_BYTES)
                .collect();
        let conn = self.conn.lock().expect("runtime store mutex poisoned");
        conn.execute(
            "INSERT OR REPLACE INTO output_refs (output_ref_id, object_hash, content_type, byte_length,
                 checksum, preview_text, payload, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, datetime('now'))",
            params![
                output_ref_id,
                object_hash,
                content_type,
                payload.len() as i64,
                object_hash,
                preview_text,
                payload,
            ],
        )
        .map_err(RuntimeError::Sqlite)?;
        Ok(OutputRef {
            output_ref_id: output_ref_id.to_string(),
            object_hash: object_hash.clone(),
            content_type: content_type.to_string(),
            byte_length: payload.len() as u64,
            checksum: object_hash,
            preview_text,
            payload: payload.to_vec(),
        })
    }

    /// Reads a bounded range of the output behind a reference (Phase 2.6
    /// pagination): bytes [offset, offset+max), clamped to the payload.
    pub fn read_output_range(
        &self,
        output_ref_id: &str,
        offset: u64,
        max: u64,
    ) -> Result<(Vec<u8>, u64), RuntimeError> {
        let conn = self.conn.lock().expect("runtime store mutex poisoned");
        let row = conn
            .query_row(
                "SELECT payload, byte_length FROM output_refs WHERE output_ref_id = ?1",
                [output_ref_id],
                |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(RuntimeError::Sqlite)?
            .ok_or_else(|| RuntimeError::NotFound(output_ref_id.to_string()))?;
        let (payload, total) = (row.0, row.1.max(0) as u64);
        let start = (offset as usize).min(payload.len());
        let end = (offset.saturating_add(max) as usize).min(payload.len());
        Ok((payload[start..end].to_vec(), total))
    }

    /// Reads the full output behind a reference.
    pub fn read_output(&self, output_ref_id: &str) -> Result<Vec<u8>, RuntimeError> {
        let conn = self.conn.lock().expect("runtime store mutex poisoned");
        let payload: Vec<u8> = conn
            .query_row(
                "SELECT payload FROM output_refs WHERE output_ref_id = ?1",
                [output_ref_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(RuntimeError::Sqlite)?
            .ok_or_else(|| RuntimeError::NotFound(output_ref_id.to_string()))?;
        Ok(payload)
    }

    /// Registers a durable background-operation handle.
    pub fn register_background(
        &self,
        handle_id: &str,
        kind: &str,
        output_ref_id: Option<&str>,
        bounded_preview: &str,
    ) -> Result<(), RuntimeError> {
        let conn = self.conn.lock().expect("runtime store mutex poisoned");
        conn.execute(
            "INSERT OR REPLACE INTO background_tasks (handle_id, kind, status, output_ref_id, bounded_preview, created_at)
             VALUES (?1, ?2, 'running', ?3, ?4, datetime('now'))",
            params![handle_id, kind, output_ref_id, bounded_preview],
        )
        .map_err(RuntimeError::Sqlite)?;
        Ok(())
    }

    /// Stops a background operation durably: the status survives restarts.
    pub fn stop_background(&self, handle_id: &str) -> Result<(), RuntimeError> {
        let conn = self.conn.lock().expect("runtime store mutex poisoned");
        let changed = conn
            .execute(
                "UPDATE background_tasks SET status = 'stopped', stopped_at = datetime('now')
                 WHERE handle_id = ?1",
                [handle_id],
            )
            .map_err(RuntimeError::Sqlite)?;
        if changed == 0 {
            return Err(RuntimeError::NotFound(handle_id.to_string()));
        }
        Ok(())
    }

    /// Lists background handles: (handle_id, kind, status, bounded_preview).
    pub fn list_background(&self) -> Result<Vec<(String, String, String, String)>, RuntimeError> {
        let conn = self.conn.lock().expect("runtime store mutex poisoned");
        let mut stmt = conn
            .prepare(
                "SELECT handle_id, kind, status, bounded_preview FROM background_tasks ORDER BY created_at",
            )
            .map_err(RuntimeError::Sqlite)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map_err(RuntimeError::Sqlite)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(RuntimeError::Sqlite)?;
        Ok(rows)
    }

    /// Records a typed tool call awaiting/completing with its result
    /// (REQ-EV-0098: tool results re-enter as typed payloads, call↔result
    /// pairing is durable).
    pub fn record_tool_call(
        &self,
        tool_call_id: &str,
        step_id: &str,
        tool_name: &str,
        effect_class: &str,
        arguments_hash: &str,
    ) -> Result<(), RuntimeError> {
        let conn = self.conn.lock().expect("runtime store mutex poisoned");
        conn.execute(
            "INSERT OR IGNORE INTO tool_calls (tool_call_id, step_id, tool_name, effect_class, status, arguments_hash, dispatched_at)
             VALUES (?1, ?2, ?3, ?4, 'dispatched', ?5, datetime('now'))",
            params![tool_call_id, step_id, tool_name, effect_class, arguments_hash],
        )
        .map_err(RuntimeError::Sqlite)?;
        Ok(())
    }

    /// Records the typed result for a tool call — the re-entry payload that
    /// survives provider/processor restarts.
    pub fn record_tool_result(
        &self,
        tool_call_id: &str,
        result_payload: &[u8],
    ) -> Result<(), RuntimeError> {
        let conn = self.conn.lock().expect("runtime store mutex poisoned");
        let changed = conn
            .execute(
                "UPDATE tool_calls SET status = 'succeeded', result_payload = ?2,
                    completed_at = datetime('now') WHERE tool_call_id = ?1",
                params![tool_call_id, result_payload],
            )
            .map_err(RuntimeError::Sqlite)?;
        if changed == 0 {
            return Err(RuntimeError::NotFound(tool_call_id.to_string()));
        }
        Ok(())
    }

    /// Tool call/result pairs for a step, preserving pairing across restarts.
    pub fn tool_pairs(&self, step_id: &str) -> Result<Vec<ToolPair>, RuntimeError> {
        let conn = self.conn.lock().expect("runtime store mutex poisoned");
        let mut stmt = conn
            .prepare(
                "SELECT tool_call_id, tool_name, result_payload FROM tool_calls
                 WHERE step_id = ?1 ORDER BY dispatched_at, tool_call_id",
            )
            .map_err(RuntimeError::Sqlite)?;
        let rows = stmt
            .query_map([step_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(RuntimeError::Sqlite)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(RuntimeError::Sqlite)?;
        Ok(rows)
    }
}

#[cfg(test)]
mod range_tests {
    use super::*;

    /// Phase 2.6 pagination: ranges clamp to the payload; offsets beyond
    /// the end return empty with the true total.
    #[test]
    fn output_ranges_clamp_and_report_total() {
        let dir = std::env::temp_dir().join(format!(
            "modbit-outrange-{}",
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = RuntimeStore::open(&dir.join("runtime.db")).unwrap();
        store
            .write_output_ref("ref-1", "text/plain", b"hello paginated world")
            .unwrap();

        let (head, total) = store.read_output_range("ref-1", 0, 5).unwrap();
        assert_eq!((head.as_slice(), total), (b"hello".as_slice(), 21));

        let (tail, total) = store.read_output_range("ref-1", 6, 1000).unwrap();
        assert_eq!((tail.as_slice(), total), (b"paginated world".as_slice(), 21));

        let (beyond, total) = store.read_output_range("ref-1", 50, 10).unwrap();
        assert_eq!((beyond.as_slice(), total), (b"".as_slice(), 21));

        assert!(store.read_output_range("missing", 0, 1).is_err());
    }
}
