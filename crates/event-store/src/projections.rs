//! Transactional projections over the event stream (docs/13 § Invariants:
//! "Only Event Store append + transactional projection update can advance
//! authoritative state"; docs/31 § Core tables). Projection rows are a
//! derived read model: they are always rebuildable by folding the events.

use super::store::EventRow;
use modbit_domain::{AggregateType, DomainEvent};
use rusqlite::{params, Connection};

/// Applies one committed event to the projection rows. Must run inside the
/// SAME sqlite transaction as the event insert.
pub fn project(conn: &Connection, e: &modbit_domain::EventEnvelope) -> Result<(), rusqlite::Error> {
    match e.aggregate_type {
        AggregateType::Session => project_session(conn, e),
        AggregateType::Task => project_task(conn, e),
        AggregateType::Run | AggregateType::Turn | AggregateType::RunStep => Ok(()),
    }
}

fn project_session(
    conn: &Connection,
    e: &modbit_domain::EventEnvelope,
) -> Result<(), rusqlite::Error> {
    if let DomainEvent::SessionCreated { .. } = &e.payload {
        conn.execute(
            "INSERT INTO sessions (session_id, state, generation, created_at, updated_at, last_event_sequence)
             VALUES (?1, 'active', ?2, ?3, ?3, ?4)",
            params![e.aggregate_id, e.sequence, e.occurred_at, e.sequence],
        )?;
    }
    Ok(())
}

fn project_task(
    conn: &Connection,
    e: &modbit_domain::EventEnvelope,
) -> Result<(), rusqlite::Error> {
    match &e.payload {
        DomainEvent::TaskCreated { title, prompt, .. } => {
            conn.execute(
                "INSERT INTO tasks (task_id, session_id, goal_text, state, generation, created_at)
                 VALUES (?1, ?2, ?3, 'created', ?4, ?5)",
                params![
                    e.aggregate_id,
                    e.session_id.to_string(),
                    format!("{title}\n{prompt}"),
                    e.sequence,
                    e.occurred_at,
                ],
            )?;
        }
        DomainEvent::TaskQueued => task_state(conn, e, "queued")?,
        DomainEvent::TaskStarted => {
            let changed = conn.execute(
                "UPDATE tasks SET state = 'running', started_at = COALESCE(started_at, ?2), generation = ?3
                 WHERE task_id = ?1",
                params![e.aggregate_id, e.occurred_at, e.sequence],
            )?;
            if changed == 0 {
                task_state(conn, e, "running")?;
            }
        }
        DomainEvent::TaskWaiting { reason } => {
            conn.execute(
                "UPDATE tasks SET state = ?2, generation = ?3 WHERE task_id = ?1",
                params![
                    e.aggregate_id,
                    format!("waiting_{:?}", reason).to_lowercase(),
                    e.sequence
                ],
            )?;
        }
        DomainEvent::TaskReadyForReview => task_state(conn, e, "ready_for_review")?,
        DomainEvent::TaskCompleted { .. } => task_state_with_completion(conn, e, "completed")?,
        DomainEvent::TaskFailed { failure_code, .. } => {
            task_state_with_completion(conn, e, "failed")?;
            conn.execute(
                "UPDATE tasks SET failure_code = ?2 WHERE task_id = ?1",
                params![e.aggregate_id, failure_code],
            )?;
        }
        DomainEvent::TaskCancelled { .. } => task_state_with_completion(conn, e, "cancelled")?,
        DomainEvent::TaskSteered { .. } => {
            conn.execute(
                "UPDATE tasks SET generation = ?2 WHERE task_id = ?1",
                params![e.aggregate_id, e.sequence],
            )?;
        }
        DomainEvent::TaskInputQueued {
            input_id,
            mode,
            text,
        } => {
            conn.execute(
                "INSERT INTO task_inputs (input_id, task_id, mode, text, sequence, state)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'pending')",
                params![input_id, e.aggregate_id, mode.as_str(), text, e.sequence,],
            )?;
        }
        _ => {}
    }
    Ok(())
}

fn task_state(
    conn: &Connection,
    e: &modbit_domain::EventEnvelope,
    state: &str,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "UPDATE tasks SET state = ?2, generation = ?3 WHERE task_id = ?1",
        params![e.aggregate_id, state, e.sequence],
    )?;
    Ok(())
}

fn task_state_with_completion(
    conn: &Connection,
    e: &modbit_domain::EventEnvelope,
    state: &str,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "UPDATE tasks SET state = ?2, completed_at = ?3, generation = ?4 WHERE task_id = ?1",
        params![e.aggregate_id, state, e.occurred_at, e.sequence],
    )?;
    Ok(())
}

/// Drops and recomputes every projection row from the committed event stream
/// (docs/31: projections are derived state; recovery never trusts them).
pub fn rebuild(conn: &Connection) -> Result<usize, crate::StoreError> {
    use super::store::{map_event_row, reconstruct_envelope};
    conn.execute_batch(
        "DELETE FROM run_steps; DELETE FROM turns; DELETE FROM runs;
         DELETE FROM tasks; DELETE FROM sessions;",
    )?;
    let mut stmt = conn.prepare(
        "SELECT event_id, session_id, aggregate_type, aggregate_id, sequence, event_type,
                schema_version, occurred_at, actor_type, actor_id, causation_id,
                correlation_id, payload_inline, payload_object_hash, integrity_hash
         FROM events ORDER BY rowid",
    )?;
    let rows = stmt
        .query_map([], map_event_row)?
        .collect::<Result<Vec<EventRow>, rusqlite::Error>>()?;
    drop(stmt);

    let mut applied = 0;
    for row in rows {
        let envelope = reconstruct_envelope(row)?;
        project(conn, &envelope).map_err(crate::StoreError::Sqlite)?;
        applied += 1;
    }
    Ok(applied)
}
