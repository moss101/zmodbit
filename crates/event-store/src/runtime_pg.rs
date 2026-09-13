//! Postgres runtime store (Phase 8 item 6, docs/24 § Cloud API + docs/31):
//! the production Postgres binding for the RUNTIME store surface
//! (output refs, background operations, durable tool call/result pairs) —
//! the same semantics as the SQLite `RuntimeStore`, mirrored method for
//! method over a real Postgres connection. Local managed-compatible
//! Postgres (docker) executes the contract suite; the MANAGED cloud
//! boundary (RDS proper, credentials, TLS) stays operator-gated — a local
//! contract pass never claims managed-cloud proof.
//!
//! Dialect mapping from the SQLite original: `?N` → `$N`,
//! `INSERT OR REPLACE` → `ON CONFLICT (pk) DO UPDATE`,
//! `INSERT OR IGNORE` → `ON CONFLICT DO NOTHING`,
//! `datetime('now')` → `now()`, `BLOB` → `BYTEA`.

use std::fmt;

use postgres::{Client, NoTls, Row};

/// Same shapes as the SQLite runtime store.
pub use crate::runtime::{OutputRef, ToolPair};

#[derive(Debug)]
pub enum PgRuntimeError {
    Postgres(String),
    NotFound(String),
    Io(String),
}

impl fmt::Display for PgRuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PgRuntimeError::Postgres(e) => write!(f, "runtime pg: {e}"),
            PgRuntimeError::NotFound(id) => write!(f, "not found: {id}"),
            PgRuntimeError::Io(e) => write!(f, "runtime pg io: {e}"),
        }
    }
}

impl std::error::Error for PgRuntimeError {}

/// Postgres errors carry their real message only through as_db_error —
/// the bare Display is uselessly terse ("db error").
fn pg_err(e: postgres::Error) -> PgRuntimeError {
    let detail = e
        .as_db_error()
        .map(|db| format!("{} (sqlstate {})", db.message(), db.code().code()))
        .unwrap_or_else(|| e.to_string());
    PgRuntimeError::Postgres(detail)
}

const RUNTIME_PG_MIGRATIONS: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS output_refs (
        output_ref_id TEXT PRIMARY KEY,
        object_hash   TEXT NOT NULL,
        content_type  TEXT NOT NULL,
        byte_length   BIGINT NOT NULL,
        checksum      TEXT NOT NULL,
        preview_text  TEXT NOT NULL,
        payload       BYTEA NOT NULL,
        created_at    TEXT NOT NULL DEFAULT to_char(now(), 'YYYY-MM-DD HH24:MI:SS'),
        retention_class TEXT NOT NULL DEFAULT 'default'
    );",
    "CREATE TABLE IF NOT EXISTS background_tasks (
        handle_id    TEXT PRIMARY KEY,
        kind         TEXT NOT NULL,
        status       TEXT NOT NULL,
        output_ref_id TEXT,
        bounded_preview TEXT NOT NULL,
        created_at   TEXT NOT NULL DEFAULT to_char(now(), 'YYYY-MM-DD HH24:MI:SS'),
        stopped_at   TEXT
    );",
    "CREATE TABLE IF NOT EXISTS tool_calls (
        tool_call_id TEXT PRIMARY KEY,
        step_id      TEXT NOT NULL,
        tool_name    TEXT NOT NULL,
        tool_version TEXT,
        effect_class TEXT,
        status       TEXT NOT NULL,
        arguments_hash TEXT,
        result_payload BYTEA,
        dispatched_at TEXT,
        completed_at  TEXT,
        result_ref    TEXT,
        unknown_outcome_reason TEXT
    );",
    "CREATE INDEX IF NOT EXISTS idx_tool_calls_step ON tool_calls(step_id);",
];

pub struct PgRuntimeStore {
    conn: std::sync::Mutex<Client>,
}

impl fmt::Debug for PgRuntimeStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgRuntimeStore").finish_non_exhaustive()
    }
}

impl PgRuntimeStore {
    /// Opens (and migrates) the runtime schema on a real Postgres.
    /// `config` is a libpq-style connection string, e.g.
    /// `host=127.0.0.1 user=modbit password=... dbname=modbit`.
    pub fn open(config: &str) -> Result<Self, PgRuntimeError> {
        let mut conn = Client::connect(config, NoTls).map_err(pg_err)?;
        for migration in RUNTIME_PG_MIGRATIONS {
            conn.batch_execute(migration).map_err(pg_err)?;
        }
        Ok(PgRuntimeStore {
            conn: std::sync::Mutex::new(conn),
        })
    }

    /// Stores bytes as a content-addressed output reference with a bounded
    /// preview — parity with `RuntimeStore::write_output_ref`.
    pub fn write_output_ref(
        &self,
        output_ref_id: &str,
        content_type: &str,
        payload: &[u8],
    ) -> Result<OutputRef, PgRuntimeError> {
        let object_hash = {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(payload);
            format!("{:x}", h.finalize())
        };
        // Postgres TEXT refuses NUL bytes (SQLite tolerated them): the
        // preview is display text, so NULs are stripped on this backend.
        let preview_text: String = String::from_utf8_lossy(&payload[..payload.len().min(512)])
            .chars()
            .filter(|c| *c != '\0')
            .take(512)
            .collect();
        let mut conn = self.conn.lock().expect("runtime pg lock");
        conn.execute(
            "INSERT INTO output_refs (output_ref_id, object_hash, content_type, byte_length,
                 checksum, preview_text, payload)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (output_ref_id) DO UPDATE SET
                 object_hash = EXCLUDED.object_hash,
                 content_type = EXCLUDED.content_type,
                 byte_length = EXCLUDED.byte_length,
                 checksum = EXCLUDED.checksum,
                 preview_text = EXCLUDED.preview_text,
                 payload = EXCLUDED.payload",
            &[
                &output_ref_id,
                &object_hash,
                &content_type,
                &(payload.len() as i64),
                &object_hash,
                &preview_text,
                &payload,
            ],
        )
        .map_err(pg_err)?;
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

    fn fetch_payload_row(
        conn: &mut Client,
        output_ref_id: &str,
    ) -> Result<(Vec<u8>, i64), PgRuntimeError> {
        let row = conn
            .query_opt(
                "SELECT payload, byte_length FROM output_refs WHERE output_ref_id = $1",
                &[&output_ref_id],
            )
            .map_err(pg_err)?
            .ok_or_else(|| PgRuntimeError::NotFound(output_ref_id.to_string()))?;
        let payload: Vec<u8> = row.get(0);
        let total: i64 = row.get(1);
        Ok((payload, total))
    }

    /// Bounds-clamped range read — parity with
    /// `RuntimeStore::read_output_range`.
    pub fn read_output_range(
        &self,
        output_ref_id: &str,
        offset: u64,
        max: u64,
    ) -> Result<(Vec<u8>, u64), PgRuntimeError> {
        let mut conn = self.conn.lock().expect("runtime pg lock");
        let (payload, total) = Self::fetch_payload_row(&mut conn, output_ref_id)?;
        let start = (offset as usize).min(payload.len());
        let end = (offset.saturating_add(max) as usize).min(payload.len());
        Ok((payload[start..end].to_vec(), total.max(0) as u64))
    }

    /// Reads the full output behind a reference.
    pub fn read_output(&self, output_ref_id: &str) -> Result<Vec<u8>, PgRuntimeError> {
        let mut conn = self.conn.lock().expect("runtime pg lock");
        Ok(Self::fetch_payload_row(&mut conn, output_ref_id)?.0)
    }

    /// Registers a background operation durably.
    pub fn register_background(
        &self,
        handle_id: &str,
        kind: &str,
        output_ref_id: Option<&str>,
        bounded_preview: &str,
    ) -> Result<(), PgRuntimeError> {
        let mut conn = self.conn.lock().expect("runtime pg lock");
        conn.execute(
            "INSERT INTO background_tasks (handle_id, kind, status, output_ref_id, bounded_preview)
             VALUES ($1, $2, 'running', $3, $4)
             ON CONFLICT (handle_id) DO UPDATE SET
                 kind = EXCLUDED.kind, status = 'running',
                 output_ref_id = EXCLUDED.output_ref_id,
                 bounded_preview = EXCLUDED.bounded_preview, stopped_at = NULL",
            &[&handle_id, &kind, &output_ref_id, &bounded_preview],
        )
        .map_err(pg_err)?;
        Ok(())
    }

    /// Stops a background operation durably: the status survives restarts.
    pub fn stop_background(&self, handle_id: &str) -> Result<(), PgRuntimeError> {
        let mut conn = self.conn.lock().expect("runtime pg lock");
        let changed = conn
            .execute(
                "UPDATE background_tasks SET status = 'stopped', stopped_at = to_char(now(), 'YYYY-MM-DD HH24:MI:SS')
                 WHERE handle_id = $1",
                &[&handle_id],
            )
            .map_err(pg_err)?;
        if changed == 0 {
            return Err(PgRuntimeError::NotFound(handle_id.to_string()));
        }
        Ok(())
    }

    /// Lists background handles: (handle_id, kind, status, bounded_preview).
    pub fn list_background(&self) -> Result<Vec<(String, String, String, String)>, PgRuntimeError> {
        let mut conn = self.conn.lock().expect("runtime pg lock");
        let rows = conn
            .query(
                "SELECT handle_id, kind, status, bounded_preview FROM background_tasks ORDER BY created_at",
                &[],
            )
            .map_err(pg_err)?;
        Ok(rows
            .iter()
            .map(|r: &Row| (r.get(0), r.get(1), r.get(2), r.get(3)))
            .collect())
    }

    /// Records a typed tool call awaiting completion.
    pub fn record_tool_call(
        &self,
        tool_call_id: &str,
        step_id: &str,
        tool_name: &str,
        effect_class: &str,
        arguments_hash: &str,
    ) -> Result<(), PgRuntimeError> {
        let mut conn = self.conn.lock().expect("runtime pg lock");
        conn.execute(
            "INSERT INTO tool_calls (tool_call_id, step_id, tool_name, effect_class, status, arguments_hash, dispatched_at)
             VALUES ($1, $2, $3, $4, 'dispatched', $5, to_char(now(), 'YYYY-MM-DD HH24:MI:SS'))
             ON CONFLICT (tool_call_id) DO NOTHING",
            &[
                &tool_call_id,
                &step_id,
                &tool_name,
                &effect_class,
                &arguments_hash,
            ],
        )
        .map_err(pg_err)?;
        Ok(())
    }

    /// Records the typed result for a tool call.
    pub fn record_tool_result(
        &self,
        tool_call_id: &str,
        result_payload: &[u8],
    ) -> Result<(), PgRuntimeError> {
        let mut conn = self.conn.lock().expect("runtime pg lock");
        let changed = conn
            .execute(
                "UPDATE tool_calls SET status = 'succeeded', result_payload = $2,
                    completed_at = to_char(now(), 'YYYY-MM-DD HH24:MI:SS') WHERE tool_call_id = $1",
                &[&tool_call_id, &result_payload],
            )
            .map_err(pg_err)?;
        if changed == 0 {
            return Err(PgRuntimeError::NotFound(tool_call_id.to_string()));
        }
        Ok(())
    }

    /// Tool call/result pairs for a step, preserving pairing across restarts.
    pub fn tool_pairs(&self, step_id: &str) -> Result<Vec<ToolPair>, PgRuntimeError> {
        let mut conn = self.conn.lock().expect("runtime pg lock");
        let rows = conn
            .query(
                "SELECT tool_call_id, tool_name, result_payload FROM tool_calls
                 WHERE step_id = $1 ORDER BY dispatched_at, tool_call_id",
                &[&step_id],
            )
            .map_err(pg_err)?;
        Ok(rows
            .iter()
            .map(|r: &Row| (r.get(0), r.get(1), r.get(2)))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_CONFIG: &str =
        "host=127.0.0.1 port=5432 user=modbit password=modbit-test-pw dbname=modbit";

    fn pg_or_skip() -> Option<PgRuntimeStore> {
        match std::net::TcpStream::connect("127.0.0.1:5432") {
            Ok(_) => match PgRuntimeStore::open(DEFAULT_CONFIG) {
                Ok(store) => Some(store),
                Err(e) => {
                    println!("pg runtime tests skipped: cannot open store (recorded gap): {e}");
                    None
                }
            },
            Err(_) => {
                println!(
                    "pg runtime tests skipped: no Postgres on 127.0.0.1:5432 (recorded gap; recipe: docker run -p 5432:5432 -e POSTGRES_USER=modbit -e POSTGRES_PASSWORD=modbit-test-pw -e POSTGRES_DB=modbit postgres:16-alpine)"
                );
                None
            }
        }
    }

    /// The FULL output-ref contract on real Postgres: round trip, preview
    /// bound, replace semantics, range clamp.
    #[test]
    fn pg_output_ref_contract() {
        let Some(store) = pg_or_skip() else { return };
        let payload: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
        let r1 = store
            .write_output_ref("pg-out-1", "application/octet-stream", &payload)
            .expect("write");
        assert_eq!(r1.byte_length, payload.len() as u64);
        assert_eq!(r1.object_hash.len(), 64);
        assert!(r1.preview_text.chars().count() <= 512);

        let got = store.read_output("pg-out-1").expect("read");
        assert_eq!(got, payload, "exact payload survives the PG round trip");

        let (head, total) = store.read_output_range("pg-out-1", 0, 5).expect("range");
        assert_eq!(
            (head.as_slice(), total),
            (&payload[..5], payload.len() as u64)
        );
        let (tail, _) = store
            .read_output_range("pg-out-1", (payload.len() - 3) as u64, 1000)
            .expect("range tail");
        assert_eq!(tail, &payload[payload.len() - 3..]);
        let (past, total) = store
            .read_output_range("pg-out-1", payload.len() as u64 + 100, 10)
            .expect("range past end");
        assert!(past.is_empty());
        assert_eq!(total, payload.len() as u64);

        // Replace semantics.
        store
            .write_output_ref("pg-out-1", "text/plain", b"replaced")
            .expect("replace");
        assert_eq!(store.read_output("pg-out-1").expect("read"), b"replaced");

        match store.read_output("pg-never-written") {
            Err(PgRuntimeError::NotFound(id)) => assert_eq!(id, "pg-never-written"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    /// Background operations and tool call/result pairing survive in PG
    /// exactly as in SQLite (durable semantics parity).
    #[test]
    fn pg_background_and_tool_pair_contract() {
        let Some(store) = pg_or_skip() else { return };
        store
            .register_background("pg-h-1", "shell", Some("pg-out-1"), "compiling…")
            .expect("register");
        store
            .register_background("pg-h-2", "search", None, "scan")
            .expect("register 2");
        store.stop_background("pg-h-1").expect("stop");
        match store.stop_background("pg-h-never") {
            Err(PgRuntimeError::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
        let listed = store.list_background().expect("list");
        let h1 = listed.iter().find(|(id, _, _, _)| id == "pg-h-1").unwrap();
        assert_eq!(h1.2, "stopped", "stop is durable");
        assert_eq!(h1.3, "compiling…");
        let h2 = listed.iter().find(|(id, _, _, _)| id == "pg-h-2").unwrap();
        assert_eq!(h2.2, "running");

        store
            .record_tool_call("pg-tc-1", "pg-step-1", "fs.read", "read-only", "hash-a")
            .expect("call");
        // Idempotent re-record (INSERT OR IGNORE parity).
        store
            .record_tool_call("pg-tc-1", "pg-step-1", "fs.read", "read-only", "hash-a")
            .expect("call again");
        store
            .record_tool_result("pg-tc-1", b"result-bytes")
            .expect("result");
        match store.record_tool_result("pg-tc-never", b"x") {
            Err(PgRuntimeError::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
        store
            .record_tool_call("pg-tc-2", "pg-step-1", "git.diff", "read-only", "hash-b")
            .expect("call 2");
        let pairs = store.tool_pairs("pg-step-1").expect("pairs");
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0, "pg-tc-1");
        assert_eq!(pairs[0].2.as_deref(), Some(b"result-bytes".as_slice()));
        assert_eq!(pairs[1].0, "pg-tc-2");
        assert!(pairs[1].2.is_none(), "unanswered call has no payload");
    }
}
