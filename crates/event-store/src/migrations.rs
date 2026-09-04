//! Explicit schema migrations (docs/31 § Migration safety). Migrations are
//! versioned by SQLite `PRAGMA user_version`; each entry runs once, in order.
//! Migrations are append-only: Core never rewrites history or drops columns.

use rusqlite::Connection;

pub type Migration = (i64, &'static str);

pub const MIGRATIONS: &[Migration] = &[
    (
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
    ),
    (2, SQL_V2_PROJECTIONS),
    (
        crate::leases::MIGRATION_V3_LEASES,
        crate::leases::SQL_V3_LEASES,
    ),
];

/// Projection tables (docs/31 § Core tables): derived read models, always
/// rebuildable from `events` via `projections::rebuild`.
pub const SQL_V2_PROJECTIONS: &str = "
    CREATE TABLE sessions (
        session_id         TEXT PRIMARY KEY,
        tenant_id          TEXT,
        user_id            TEXT,
        space_id           TEXT,
        state              TEXT NOT NULL,
        generation         INTEGER NOT NULL,
        created_at         TEXT NOT NULL,
        updated_at         TEXT NOT NULL,
        current_task_id    TEXT,
        last_event_sequence INTEGER NOT NULL
    );
    CREATE TABLE tasks (
        task_id           TEXT PRIMARY KEY,
        session_id        TEXT NOT NULL REFERENCES sessions(session_id),
        goal_text         TEXT NOT NULL,
        workspace_id      TEXT,
        base_revision     TEXT,
        execution_profile TEXT,
        policy_profile_id TEXT,
        state             TEXT NOT NULL,
        generation        INTEGER NOT NULL,
        created_at        TEXT NOT NULL,
        started_at        TEXT,
        completed_at      TEXT,
        failure_code      TEXT
    );
    CREATE TABLE runs (
        run_id                  TEXT PRIMARY KEY,
        task_id                 TEXT NOT NULL REFERENCES tasks(task_id),
        attempt                 INTEGER NOT NULL,
        owner_location          TEXT,
        kernel_lease_generation INTEGER,
        state                   TEXT NOT NULL,
        started_at              TEXT,
        ended_at                TEXT
    );
    CREATE TABLE turns (
        turn_id              TEXT PRIMARY KEY,
        run_id               TEXT NOT NULL REFERENCES runs(run_id),
        ordinal              INTEGER NOT NULL,
        state                TEXT NOT NULL,
        model_route_json     TEXT,
        tool_projection_hash TEXT,
        context_pack_id      TEXT,
        started_at           TEXT,
        ended_at             TEXT
    );
    CREATE TABLE run_steps (
        step_id      TEXT PRIMARY KEY,
        turn_id      TEXT NOT NULL REFERENCES turns(turn_id),
        step_type    TEXT NOT NULL,
        state        TEXT NOT NULL,
        ordinal      INTEGER NOT NULL,
        started_at   TEXT,
        ended_at     TEXT,
        input_ref    TEXT,
        output_ref   TEXT,
        failure_code TEXT
    );
";

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
        assert_eq!(v, 3);
    }
}
