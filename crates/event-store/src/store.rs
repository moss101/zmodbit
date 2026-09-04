//! The append-only event store (docs/13 § Canonical event envelope,
//! docs/31 § Local storage / `events`).
//!
//! Invariants enforced here (docs/13 § Invariants):
//! - append-only: no update or delete paths exist;
//! - per-aggregate `sequence` is gapless and monotonic; writers supply the
//!   expected next sequence and races surface as `SequenceConflict`;
//! - every event's `integrity_hash` is verified on append and on read-back,
//!   so silent corruption or tampering is detected.

use std::fmt;
use std::path::Path;
use std::sync::Mutex;

use modbit_domain::{AggregateType, EventEnvelope};
use rusqlite::{params, Connection};

use crate::migrations::migrate;

#[derive(Debug)]
pub enum StoreError {
    Sqlite(rusqlite::Error),
    /// The writer's expected sequence loses against committed state
    /// (optimistic concurrency, docs/13 § Fencing and epochs).
    SequenceConflict {
        aggregate_id: String,
        expected: u64,
        actual: u64,
    },
    /// An event's integrity hash does not match its content.
    IntegrityMismatch {
        event_id: String,
    },
    Io(std::io::Error),
    Uuid(uuid::Error),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Sqlite(e) => write!(f, "sqlite: {e}"),
            StoreError::SequenceConflict { aggregate_id, expected, actual } => write!(
                f,
                "sequence conflict on aggregate {aggregate_id}: expected next {expected}, committed {actual}"
            ),
            StoreError::IntegrityMismatch { event_id } => {
                write!(f, "integrity hash mismatch on event {event_id}")
            }
            StoreError::Io(e) => write!(f, "io: {e}"),
            StoreError::Uuid(e) => write!(f, "uuid: {e}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sqlite(e)
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}

pub struct EventStore {
    conn: Mutex<Connection>,
}

impl EventStore {
    pub(crate) fn connection(&self) -> &Mutex<Connection> {
        &self.conn
    }
}

impl EventStore {
    /// Opens (creating if needed) the durable store at `path` and applies
    /// pragmas required by docs/31: WAL journal, `foreign_keys=ON`,
    /// `synchronous=FULL` for authoritative commits.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn next_sequence(conn: &Connection, aggregate_id: &str) -> Result<u64, StoreError> {
        let max: Option<i64> = conn.query_row(
            "SELECT MAX(sequence) FROM events WHERE aggregate_id = ?1",
            [aggregate_id],
            |r| r.get::<_, Option<i64>>(0),
        )?;
        Ok(max.map_or(0, |v| v as u64 + 1))
    }

    fn insert_event(conn: &Connection, e: &EventEnvelope) -> Result<(), StoreError> {
        if !e.verify_integrity() {
            return Err(StoreError::IntegrityMismatch {
                event_id: e.event_id.clone(),
            });
        }
        let payload = serde_json::to_string(&e.payload)
            .map_err(|e| StoreError::Io(std::io::Error::other(e)))?;
        conn.execute(
            "INSERT INTO events (event_id, session_id, aggregate_type, aggregate_id, sequence,
                 event_type, schema_version, occurred_at, actor_type, actor_id,
                 causation_id, correlation_id, payload_inline, payload_object_hash, integrity_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                e.event_id,
                e.session_id.to_string(),
                e.aggregate_type.as_str(),
                e.aggregate_id,
                e.sequence as i64,
                e.event_type,
                format!("{}.{}", e.schema_version.0, e.schema_version.1),
                e.occurred_at,
                serde_json::to_string(&e.actor.actor_type)
                    .map_err(|err| StoreError::Io(std::io::Error::other(err)))?
                    .trim_matches('"'),
                e.actor.actor_id,
                e.causation_id,
                e.correlation_id,
                payload,
                e.payload_object_hash,
                e.integrity_hash,
            ],
        )?;
        Ok(())
    }

    /// Appends events atomically. Events for the same aggregate must carry
    /// strictly consecutive `sequence` values continuing the committed stream;
    /// the first event of an aggregate carries sequence 1. Any race loses as
    /// `SequenceConflict` and nothing partial is committed.
    pub fn append(&self, events: &mut [EventEnvelope]) -> Result<(), StoreError> {
        if events.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().expect("event store mutex poisoned");
        let tx = conn.transaction()?;
        let mut sequences: std::collections::HashMap<String, u64> =
            std::collections::HashMap::new();
        for e in events.iter_mut() {
            let aggregate = e.aggregate_id.clone();
            let expected = *sequences
                .entry(aggregate.clone())
                .or_insert_with(|| EventStore::next_sequence(&tx, &aggregate).unwrap_or(u64::MAX));
            if e.sequence != expected {
                let actual = EventStore::next_sequence(&tx, &aggregate)?;
                return Err(StoreError::SequenceConflict {
                    aggregate_id: aggregate,
                    expected: e.sequence,
                    actual,
                });
            }
            EventStore::insert_event(&tx, e)?;
            crate::projections::project(&tx, e)?;
            sequences.insert(aggregate, expected + 1);
        }
        tx.commit()?;
        Ok(())
    }

    /// Loads one aggregate's stream in sequence order (docs/13: ordering is
    /// per aggregate sequence). Envelopes are reconstructed from the row
    /// columns; `payload_inline` deserializes as the typed domain event.
    pub fn load(&self, aggregate_id: &str) -> Result<Vec<EventEnvelope>, StoreError> {
        let conn = self.conn.lock().expect("event store mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT event_id, session_id, aggregate_type, aggregate_id, sequence, event_type,
                    schema_version, occurred_at, actor_type, actor_id, causation_id,
                    correlation_id, payload_inline, payload_object_hash, integrity_hash
             FROM events WHERE aggregate_id = ?1 ORDER BY sequence",
        )?;
        let rows = stmt
            .query_map([aggregate_id], map_event_row)?
            .collect::<Result<Vec<EventRow>, rusqlite::Error>>()?;
        rows.into_iter().map(reconstruct_envelope).collect()
    }

    /// Verifies continuity and integrity of a whole aggregate stream.
    pub fn verify_stream(&self, aggregate_id: &str) -> Result<(), StoreError> {
        let events = self.load(aggregate_id)?;
        for (idx, e) in events.iter().enumerate() {
            if e.sequence != idx as u64 + 1 {
                return Err(StoreError::SequenceConflict {
                    aggregate_id: aggregate_id.to_string(),
                    expected: idx as u64 + 1,
                    actual: e.sequence,
                });
            }
            if !e.verify_integrity() {
                return Err(StoreError::IntegrityMismatch {
                    event_id: e.event_id.clone(),
                });
            }
        }
        Ok(())
    }

    /// Recomputes every projection row from committed events (docs/31:
    /// projections are derived state; recovery never trusts them).
    pub fn rebuild_projections(&self) -> Result<usize, StoreError> {
        let conn = self.conn.lock().expect("event store mutex poisoned");
        crate::projections::rebuild(&conn)
    }

    /// Read access for integrity tooling and tests. Read-only by convention:
    /// every write path must go through `append` (docs/13 § Invariants).
    pub fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> T) -> T {
        let conn = self.conn.lock().expect("event store mutex poisoned");
        f(&conn)
    }
}

/// Raw row projection of the `events` table (docs/31 § `events`).
pub(crate) type EventRow = (
    String,
    String,
    String,
    String,
    i64,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
    String,
);

pub(crate) fn map_event_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<EventRow> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
        r.get(9)?,
        r.get(10)?,
        r.get(11)?,
        r.get(12)?,
        r.get(13)?,
        r.get(14)?,
    ))
}

/// Rebuilds an [`EventEnvelope`] from stored columns. `payload_inline`
/// deserializes as the typed domain event; a payload that no longer decodes
/// is corrupt by definition and surfaces as an integrity failure.
pub(crate) fn reconstruct_envelope(row: EventRow) -> Result<EventEnvelope, StoreError> {
    let (
        event_id,
        session_id,
        aggregate_type,
        aggregate_id,
        sequence,
        event_type,
        schema_version,
        occurred_at,
        actor_type,
        actor_id,
        causation_id,
        correlation_id,
        payload_inline,
        payload_object_hash,
        integrity_hash,
    ) = row;
    use modbit_domain::{Actor, ActorType, RunId, RunStepId, SessionId, TaskId, TurnId};
    let aggregate_type = match aggregate_type.as_str() {
        "session" => AggregateType::Session,
        "task" => AggregateType::Task,
        "run" => AggregateType::Run,
        "turn" => AggregateType::Turn,
        "run_step" => AggregateType::RunStep,
        other => {
            return Err(StoreError::Io(std::io::Error::other(format!(
                "unknown aggregate_type {other}"
            ))))
        }
    };
    let (task_id, run_id, turn_id, step_id) = match aggregate_type {
        AggregateType::Task => (
            Some(TaskId::parse(&aggregate_id).map_err(StoreError::Uuid)?),
            None,
            None,
            None,
        ),
        AggregateType::Run => (
            None,
            Some(RunId::parse(&aggregate_id).map_err(StoreError::Uuid)?),
            None,
            None,
        ),
        AggregateType::Turn => (
            None,
            None,
            Some(TurnId::parse(&aggregate_id).map_err(StoreError::Uuid)?),
            None,
        ),
        AggregateType::RunStep => (
            None,
            None,
            None,
            Some(RunStepId::parse(&aggregate_id).map_err(StoreError::Uuid)?),
        ),
        AggregateType::Session => (None, None, None, None),
    };
    let (major, minor) = schema_version.split_once('.').ok_or_else(|| {
        StoreError::Io(std::io::Error::other(format!(
            "bad schema_version {schema_version}"
        )))
    })?;
    let payload = match serde_json::from_str::<modbit_domain::DomainEvent>(&payload_inline) {
        Ok(payload) => payload,
        // A payload that no longer decodes is corrupt by definition —
        // surface it as an integrity failure.
        Err(_) => {
            return Err(StoreError::IntegrityMismatch {
                event_id: event_id.clone(),
            });
        }
    };
    Ok(EventEnvelope {
        event_id,
        session_id: SessionId::parse(&session_id).map_err(StoreError::Uuid)?,
        task_id,
        run_id,
        turn_id,
        step_id,
        aggregate_type,
        aggregate_id,
        sequence: sequence as u64,
        event_type,
        schema_version: (
            major
                .parse()
                .map_err(|e: std::num::ParseIntError| StoreError::Io(std::io::Error::other(e)))?,
            minor
                .parse()
                .map_err(|e: std::num::ParseIntError| StoreError::Io(std::io::Error::other(e)))?,
        ),
        occurred_at,
        actor: Actor {
            actor_type: ActorType::parse(&actor_type).ok_or_else(|| {
                StoreError::Io(std::io::Error::other(format!(
                    "bad actor_type {actor_type}"
                )))
            })?,
            actor_id,
        },
        causation_id,
        correlation_id,
        payload,
        payload_object_hash,
        integrity_hash,
    })
}

/// Creates an event envelope for `aggregate` with the correct aggregate ids.
/// The caller assigns `sequence` and MUST call `seal()` afterwards — the
/// integrity hash covers the sequence, so sealing happens last.
#[allow(clippy::too_many_arguments)]
pub fn envelope_for(
    aggregate_type: AggregateType,
    aggregate_id: String,
    session_id: modbit_domain::SessionId,
    payload: modbit_domain::DomainEvent,
) -> EventEnvelope {
    EventEnvelope {
        event_id: uuid::Uuid::now_v7().to_string(),
        session_id,
        task_id: None,
        run_id: None,
        turn_id: None,
        step_id: None,
        aggregate_type,
        aggregate_id,
        sequence: 0, // assigned by the caller before append
        event_type: EventEnvelope::event_type_of(&payload).to_string(),
        schema_version: modbit_domain::SCHEMA_VERSION,
        occurred_at: crate::commands::utc_now_rfc3339(),
        actor: modbit_domain::Actor {
            actor_type: modbit_domain::ActorType::System,
            actor_id: "core".into(),
        },
        causation_id: None,
        correlation_id: None,
        payload,
        payload_object_hash: None,
        integrity_hash: String::new(),
    }
}
