//! Explicit schema migrations (docs/31 § Migration safety). Migrations are
//! versioned by SQLite `PRAGMA user_version`; each entry runs once, in order.
//! Migrations are append-only: Core never rewrites history or drops columns.

use rusqlite::Connection;

pub type Migration = (i64, &'static str);

pub const MIGRATIONS: &[Migration] = &[(
    1,
    "
        CREATE TABLE events (
            event_id            TEXT PRIMARY KEY,
            session_id          TEXT NOT NULL,
            aggregate_type      TEXT NOT NULL,
            aggregate_id        TEXT NOT NULL,
            sequence            INTEGER NOT NULL,
            event_type          TEXT NOT NULL,
            schema_version      TEXT NOT NULL,
            occurred_at         TEXT NOT NULL,
            actor_type          TEXT NOT NULL,
            actor_id            TEXT NOT NULL,
            causation_id        TEXT,
            correlation_id      TEXT,
            payload_inline      TEXT NOT NULL,
            payload_object_hash TEXT,
            integrity_hash      TEXT NOT NULL,
            UNIQUE(aggregate_id, sequence)
        );
        CREATE INDEX idx_events_session   ON events(session_id);
        CREATE INDEX idx_events_aggregate ON events(aggregate_id, sequence);

        CREATE TABLE command_log (
            command_id   TEXT PRIMARY KEY,
            command_type TEXT NOT NULL,
            status       TEXT NOT NULL,
            result_event_ids TEXT NOT NULL,
            error        TEXT,
            processed_at TEXT NOT NULL
        );
    ",
)];

pub fn migrate(conn: &Connection) -> Result<(), rusqlite::Error> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for (version, sql) in MIGRATIONS {
        if *version > current {
            conn.execute_batch(sql)?;
            conn.pragma_update(None, "user_version", *version)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_idempotent_and_ordered() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap(); // second run is a no-op
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 1);
    }
}
