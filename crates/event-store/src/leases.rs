//! Session leases and fencing generations (M1, docs/13 § Fencing and epochs;
//! REQ-EV-0054/0273): exactly one mutation owner per session. A writer holds
//! a lease id + generation; a stale lease is rejected, never applied
//! silently.

use rusqlite::{params, Connection, OptionalExtension};

use crate::store::StoreError;

pub const MIGRATION_V3_LEASES: i64 = 3;
pub const SQL_V3_LEASES: &str = "
    CREATE TABLE session_leases (
        session_id  TEXT PRIMARY KEY,
        lease_id    TEXT NOT NULL,
        owner       TEXT NOT NULL,
        generation  INTEGER NOT NULL,
        acquired_at TEXT NOT NULL
    );
";

pub struct Lease {
    pub lease_id: String,
    pub generation: u64,
}

/// Acquires (or renews) the mutation lease for a session, bumping the
/// generation. Any previously held lease becomes stale immediately.
pub fn acquire(
    conn: &Connection,
    session_id: &str,
    lease_id: &str,
    owner: &str,
) -> Result<Lease, StoreError> {
    let current: Option<(String, i64)> = conn
        .query_row(
            "SELECT lease_id, generation FROM session_leases WHERE session_id = ?1",
            [session_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let generation = current.as_ref().map_or(1, |(_, g)| *g as u64 + 1);
    conn.execute(
        "INSERT INTO session_leases (session_id, lease_id, owner, generation, acquired_at)
         VALUES (?1, ?2, ?3, ?4, datetime('now'))
         ON CONFLICT(session_id) DO UPDATE SET
            lease_id = excluded.lease_id,
            owner = excluded.owner,
            generation = excluded.generation,
            acquired_at = excluded.acquired_at",
        params![session_id, lease_id, owner, generation as i64],
    )?;
    Ok(Lease {
        lease_id: lease_id.to_string(),
        generation,
    })
}

/// The current lease for a session, if one was ever acquired.
pub fn current(conn: &Connection, session_id: &str) -> Result<Option<Lease>, StoreError> {
    let row = conn
        .query_row(
            "SELECT lease_id, generation FROM session_leases WHERE session_id = ?1",
            [session_id],
            |r| {
                Ok(Lease {
                    lease_id: r.get(0)?,
                    generation: r.get::<_, i64>(1)? as u64,
                })
            },
        )
        .optional()?;
    Ok(row)
}

/// Fencing check: is `lease_id` still the session's current mutation owner?
pub fn is_current(conn: &Connection, session_id: &str, lease_id: &str) -> Result<bool, StoreError> {
    Ok(current(conn, session_id)?
        .map(|l| l.lease_id == lease_id)
        .unwrap_or(false))
}
