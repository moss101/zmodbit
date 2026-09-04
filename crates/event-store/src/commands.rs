//! Idempotent command processing (docs/30 § Commands: "Mutating commands are
//! idempotent by command_id"; docs/33 § Idempotency).
//!
//! Each command runs in ONE sqlite transaction that (a) records the command
//! outcome in `command_log` and (b) appends the resulting events. A retried
//! command_id returns the recorded outcome without appending anything; a
//! command rejected by the state machine is recorded as REJECTED so retries
//! observe the same deterministic result.

use std::sync::Arc;

use modbit_domain::task::{apply_task_event, fold_task};
use modbit_domain::{Command, CommandPayload, DomainEvent, TransitionError, SCHEMA_VERSION};
use rusqlite::{params, OptionalExtension};

use crate::store::{envelope_for, EventStore, StoreError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// First execution: the listed events were appended.
    Applied { event_ids: Vec<String> },
    /// This command_id was already processed; nothing was appended again.
    Replayed { event_ids: Vec<String> },
    /// The state machine rejected the command; recorded, nothing appended.
    Rejected { reason: String },
}

#[derive(Clone)]
pub struct CommandProcessor {
    store: Arc<EventStore>,
}

impl CommandProcessor {
    pub fn new(store: Arc<EventStore>) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &EventStore {
        &self.store
    }

    pub fn execute(&self, command: Command) -> Result<Outcome, StoreError> {
        let mut conn = self
            .store
            .connection()
            .lock()
            .expect("event store mutex poisoned");
        let tx = conn.transaction()?;

        // Idempotency: a seen command_id replays its recorded outcome.
        let seen: Option<(String, Option<String>)> = tx
            .query_row(
                "SELECT status, result_event_ids FROM command_log WHERE command_id = ?1",
                [&command.command_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((status, ids_json)) = seen {
            let event_ids: Vec<String> =
                serde_json::from_str(&ids_json.unwrap_or_else(|| "[]".into())).unwrap_or_default();
            drop(tx);
            return Ok(match status.as_str() {
                "APPLIED" => Outcome::Replayed { event_ids },
                _ => Outcome::Rejected {
                    reason: "previously rejected (idempotent replay)".into(),
                },
            });
        }

        let result = Self::plan_and_append(&tx, &command);
        let (status, error, event_ids) = match &result {
            Ok(ids) => ("APPLIED", None, ids.clone()),
            Err(reason) => ("REJECTED", Some(reason.clone()), Vec::new()),
        };
        tx.execute(
            "INSERT INTO command_log (command_id, command_type, status, result_event_ids, error, processed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                command.command_id,
                command.payload.kind(),
                status,
                serde_json::to_string(&event_ids)
                    .map_err(|e| StoreError::Io(std::io::Error::other(e)))?,
                error,
                utc_now_rfc3339(),
            ],
        )?;
        tx.commit()?;
        match result {
            Ok(event_ids) => Ok(Outcome::Applied { event_ids }),
            Err(reason) => {
                eprintln!(
                    "modbit event store: command {} rejected: {reason}",
                    command.payload.kind()
                );
                Ok(Outcome::Rejected { reason })
            }
        }
    }

    /// Validates the command against current aggregate state and appends the
    /// resulting events. Any error string is a deterministic rejection reason;
    /// the transaction is rolled back so nothing partial persists.
    fn plan_and_append(
        tx: &rusqlite::Transaction<'_>,
        command: &Command,
    ) -> Result<Vec<String>, String> {
        let session_exists = |id: &modbit_domain::SessionId| -> Result<bool, String> {
            let n: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM events WHERE aggregate_id = ?1",
                    [id.to_string()],
                    |r| r.get(0),
                )
                .map_err(|e| e.to_string())?;
            Ok(n > 0)
        };

        let mut events: Vec<modbit_domain::EventEnvelope> = Vec::new();
        match &command.payload {
            CommandPayload::CreateSession { display_name } => {
                let session_id = modbit_domain::SessionId::generate();
                events.push(envelope_for(
                    modbit_domain::AggregateType::Session,
                    session_id.to_string(),
                    session_id,
                    DomainEvent::SessionCreated {
                        display_name: display_name.clone(),
                    },
                ));
                events[0].sequence = 1;
                events[0].seal();
            }
            CommandPayload::CreateTask {
                session_id,
                title,
                prompt,
            } => {
                if !session_exists(session_id)? {
                    return Err(format!("session {session_id} does not exist"));
                }
                let task_id = modbit_domain::TaskId::generate();
                let mut e = envelope_for(
                    modbit_domain::AggregateType::Task,
                    task_id.to_string(),
                    *session_id,
                    DomainEvent::TaskCreated {
                        session_id: *session_id,
                        title: title.clone(),
                        prompt: prompt.clone(),
                    },
                );
                e.task_id = Some(task_id);
                e.sequence = 1;
                e.seal();
                events.push(e);
            }
            CommandPayload::QueueTaskInput {
                task_id,
                input_id,
                mode,
                text,
            } => {
                // REQ-EV-0191/0262: validate the dispatch mode against the
                // task's current state, then append the durable input event.
                // Steering additionally emits TaskSteered (interrupt-and-
                // replace semantics live in Core, not clients — MOD-INPUT-001).
                let history = Self::load_task_events(tx, task_id)?;
                let current = fold_task(&history)
                    .ok_or_else(|| format!("task {task_id} does not exist"))?
                    .map_err(|e: TransitionError| e.to_string())?;
                let effect = modbit_domain::input_queue::input_effect(current, *mode)?;
                let session_id = task_session(tx, task_id)?;
                let mut e = envelope_for(
                    modbit_domain::AggregateType::Task,
                    task_id.to_string(),
                    session_id,
                    DomainEvent::TaskInputQueued {
                        input_id: input_id.clone(),
                        mode: *mode,
                        text: text.clone(),
                    },
                );
                e.task_id = Some(*task_id);
                e.sequence = history.len() as u64 + 1;
                e.seal();
                events.push(e);
                if effect == modbit_domain::InputEffect::RedirectsTask {
                    let mut steered = envelope_for(
                        modbit_domain::AggregateType::Task,
                        task_id.to_string(),
                        session_id,
                        DomainEvent::TaskSteered {
                            steer_note: text.clone(),
                        },
                    );
                    steered.task_id = Some(*task_id);
                    steered.sequence = history.len() as u64 + 2;
                    steered.seal();
                    events.push(steered);
                }
            }
            CommandPayload::AskSideQuestion {
                session_id,
                question_id,
                question,
                context_event_count,
            } => {
                // REQ-EV-0261: session-level event; the main task's state and
                // event cursor remain untouched.
                if !session_exists(session_id)? {
                    return Err(format!("session {session_id} does not exist"));
                }
                let mut e = envelope_for(
                    modbit_domain::AggregateType::Session,
                    session_id.to_string(),
                    *session_id,
                    DomainEvent::SideQuestionAsked {
                        question_id: question_id.clone(),
                        question: question.clone(),
                        context_event_count: *context_event_count,
                    },
                );
                e.sequence = session_sequence(tx, &session_id.to_string())? + 1;
                e.seal();
                events.push(e);
            }
            CommandPayload::SetGoal {
                task_id,
                objective,
                acceptance_criteria,
            } => {
                // REQ-EV-0119: the host owns objective/progress/termination.
                if !task_exists(tx, task_id)? {
                    return Err(format!("task {} does not exist", task_id));
                }
                let session_id = task_session(tx, task_id)?;
                let mut e = envelope_for(
                    modbit_domain::AggregateType::Task,
                    task_id.to_string(),
                    session_id,
                    DomainEvent::GoalSet {
                        objective: objective.clone(),
                        acceptance_criteria: acceptance_criteria.clone(),
                    },
                );
                e.task_id = Some(*task_id);
                e.sequence = goal_stream_len(tx, task_id)? + 1;
                e.seal();
                events.push(e);
            }
            CommandPayload::ForkSession {
                source_session,
                at_sequence,
                carried_decisions,
                carried_evidence_refs,
            } => {
                // REQ-EV-0122: the new branch carries the selected
                // decisions/evidence capsule and NEVER pending approvals —
                // the capsule type has no approval field, by construction.
                let source_len = source_stream_len(tx, &source_session.to_string())?;
                if *at_sequence == 0 || *at_sequence > source_len as u64 {
                    return Err(format!(
                        "cannot fork {source_session} at sequence {at_sequence} (stream has {source_len} events)"
                    ));
                }
                let fork_id = modbit_domain::SessionId::generate();
                let mut e = envelope_for(
                    modbit_domain::AggregateType::Session,
                    fork_id.to_string(),
                    fork_id,
                    DomainEvent::SessionForked {
                        source_session: *source_session,
                        at_sequence: *at_sequence,
                        carried_decisions: carried_decisions.clone(),
                        carried_evidence_refs: carried_evidence_refs.clone(),
                    },
                );
                e.sequence = 1;
                e.seal();
                events.push(e);
            }
            CommandPayload::RewindSession {
                session_id,
                to_sequence,
                expected_last_hash,
            } => {
                // REQ-EV-0123: revert honors optimistic hash checks — the
                // caller states the hash the stream must currently end with.
                let stream = load_session_envelopes(tx, &session_id.to_string())?;
                let last = stream
                    .last()
                    .ok_or_else(|| format!("session {session_id} has no events"))?;
                if last.integrity_hash != *expected_last_hash {
                    return Err(format!(
                        "rewind rejected: stream ends with hash {}, caller expected {expected_last_hash}",
                        last.integrity_hash
                    ));
                }
                if *to_sequence == 0 || *to_sequence >= last.sequence {
                    return Err(format!(
                        "cannot rewind session {session_id} to sequence {to_sequence} (stream ends at {})",
                        last.sequence
                    ));
                }
                let reverted = last.sequence - to_sequence;
                let mut e = envelope_for(
                    modbit_domain::AggregateType::Session,
                    session_id.to_string(),
                    *session_id,
                    DomainEvent::SessionRewound {
                        to_sequence: *to_sequence,
                        reverted_event_count: reverted,
                        previous_last_hash: last.integrity_hash.clone(),
                    },
                );
                e.sequence = last.sequence + 1;
                e.seal();
                events.push(e);
            }
            payload => {
                let task_id = match payload.target_aggregate() {
                    Some(aggregate) => modbit_domain::TaskId::parse(&aggregate)
                        .map_err(|e| format!("bad task id: {e}"))?,
                    None => return Err("lifecycle commands target a task".into()),
                };
                let history = Self::load_task_events(tx, &task_id)?;
                let current = fold_task(&history)
                    .ok_or_else(|| format!("task {task_id} does not exist"))?
                    .map_err(|e: TransitionError| e.to_string())?;

                // REQ-EV-0119 (goal mode): the host owns termination. A model
                // claim of "done" without host-verified acceptance leaves the
                // run incomplete.
                let goal_mode = history
                    .iter()
                    .any(|e| matches!(e, DomainEvent::GoalSet { .. }));
                if let CommandPayload::CompleteTask {
                    host_verified: false,
                    ..
                } = payload
                {
                    if goal_mode {
                        return Err(
                            "goal mode: completion requires host-verified acceptance criteria;                              the model cannot self-certify"
                                .into(),
                        );
                    }
                }

                let domain_event = lifecycle_event(payload);
                let next = apply_task_event(current, &domain_event)
                    .map_err(|e: TransitionError| e.to_string())?;
                let _ = next; // state is derived by folding; the event is authoritative
                let session_id = task_session(tx, &task_id)?;
                let mut e = envelope_for(
                    modbit_domain::AggregateType::Task,
                    task_id.to_string(),
                    session_id,
                    domain_event,
                );
                e.task_id = Some(task_id);
                e.sequence = history.len() as u64 + 1;
                e.seal();
                events.push(e);
            }
        }

        // Continuity check inside the transaction (optimistic concurrency).
        // Progression tracks events already staged in this same batch.
        let mut next_expected: std::collections::HashMap<String, u64> =
            std::collections::HashMap::new();
        for e in &events {
            let expected = *next_expected
                .entry(e.aggregate_id.clone())
                .or_insert_with(|| {
                    let max: Option<i64> = tx
                        .query_row(
                            "SELECT MAX(sequence) FROM events WHERE aggregate_id = ?1",
                            [&e.aggregate_id],
                            |r| r.get::<_, Option<i64>>(0),
                        )
                        .map_err(|err| err.to_string())
                        .unwrap_or(None);
                    max.map_or(1, |v| v as u64 + 1)
                });
            if e.sequence != expected {
                return Err(format!(
                    "sequence conflict on {}: expected {expected}, got {}",
                    e.aggregate_id, e.sequence
                ));
            }
            next_expected.insert(e.aggregate_id.clone(), e.sequence + 1);
        }

        let mut event_ids = Vec::new();
        for e in &events {
            if !e.verify_integrity() {
                return Err(format!("integrity mismatch on event {}", e.event_id));
            }
            let payload = serde_json::to_string(&e.payload).map_err(|err| err.to_string())?;
            tx.execute(
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
                    format!("{}.{}", SCHEMA_VERSION.0, SCHEMA_VERSION.1),
                    e.occurred_at,
                    serde_json::to_string(&e.actor.actor_type).map_err(|err| err.to_string())?.trim_matches('"'),
                    e.actor.actor_id,
                    e.causation_id,
                    e.correlation_id,
                    payload,
                    e.payload_object_hash,
                    e.integrity_hash,
                ],
            )
            .map_err(|err| err.to_string())?;
            crate::projections::project(tx, e).map_err(|err| err.to_string())?;
            event_ids.push(e.event_id.clone());
        }
        Ok(event_ids)
    }

    fn load_task_events(
        tx: &rusqlite::Transaction<'_>,
        task_id: &modbit_domain::TaskId,
    ) -> Result<Vec<DomainEvent>, String> {
        let mut stmt = tx
            .prepare("SELECT payload_inline FROM events WHERE aggregate_id = ?1 ORDER BY sequence")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([task_id.to_string()], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            let payload: String = row.map_err(|e| e.to_string())?;
            let event: DomainEvent = serde_json::from_str(&payload).map_err(|e| e.to_string())?;
            out.push(event);
        }
        Ok(out)
    }
}

fn goal_stream_len(
    tx: &rusqlite::Transaction<'_>,
    task_id: &modbit_domain::TaskId,
) -> Result<u64, String> {
    let n: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM events WHERE aggregate_id = ?1",
            [task_id.to_string()],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    Ok(n as u64)
}

fn task_exists(
    tx: &rusqlite::Transaction<'_>,
    task_id: &modbit_domain::TaskId,
) -> Result<bool, String> {
    let n: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM events WHERE aggregate_id = ?1",
            [task_id.to_string()],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    Ok(n > 0)
}

fn session_sequence(tx: &rusqlite::Transaction<'_>, session_id: &str) -> Result<u64, String> {
    let max: Option<i64> = tx
        .query_row(
            "SELECT MAX(sequence) FROM events WHERE aggregate_type = 'session' AND aggregate_id = ?1",
            [session_id],
            |r| r.get::<_, Option<i64>>(0),
        )
        .map_err(|e| e.to_string())?;
    Ok(max.map_or(0, |v| v as u64))
}

fn source_stream_len(tx: &rusqlite::Transaction<'_>, session_id: &str) -> Result<i64, String> {
    tx.query_row(
        "SELECT COUNT(*) FROM events WHERE aggregate_type = 'session' AND aggregate_id = ?1",
        [session_id],
        |r| r.get(0),
    )
    .map_err(|e| e.to_string())
}

fn load_session_envelopes(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
) -> Result<Vec<modbit_domain::EventEnvelope>, String> {
    let mut stmt = tx
        .prepare("SELECT payload_inline FROM events WHERE aggregate_id = ?1 ORDER BY sequence")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([session_id], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        let payload: String = row.map_err(|e| e.to_string())?;
        let envelope: modbit_domain::EventEnvelope =
            serde_json::from_str(&payload).map_err(|e| e.to_string())?;
        out.push(envelope);
    }
    Ok(out)
}

fn lifecycle_event(payload: &CommandPayload) -> DomainEvent {
    match payload.clone() {
        CommandPayload::QueueTask { .. } => DomainEvent::TaskQueued,
        CommandPayload::StartTask { .. } => DomainEvent::TaskStarted,
        CommandPayload::TaskWaiting { reason, .. } => DomainEvent::TaskWaiting { reason },
        CommandPayload::TaskReadyForReview { .. } => DomainEvent::TaskReadyForReview,
        CommandPayload::CompleteTask { summary, .. } => DomainEvent::TaskCompleted { summary },
        CommandPayload::FailTask {
            failure_code,
            message,
            ..
        } => DomainEvent::TaskFailed {
            failure_code,
            message,
        },
        CommandPayload::CancelTask { reason, .. } => DomainEvent::TaskCancelled { reason },
        CommandPayload::SteerTask { steer_note, .. } => DomainEvent::TaskSteered { steer_note },
        other => unreachable!("creation commands are handled separately: {other:?}"),
    }
}

fn task_session(
    tx: &rusqlite::Transaction<'_>,
    task_id: &modbit_domain::TaskId,
) -> Result<modbit_domain::SessionId, String> {
    let session: String = tx
        .query_row(
            "SELECT session_id FROM events WHERE aggregate_id = ?1 ORDER BY sequence LIMIT 1",
            [task_id.to_string()],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("task {task_id} does not exist"))?;
    modbit_domain::SessionId::parse(&session).map_err(|e| e.to_string())
}

/// RFC3339 UTC timestamp with millisecond precision, no external deps
/// (Howard Hinnant's civil-from-days algorithm).
pub fn utc_now_rfc3339() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before epoch");
    utc_millis_to_rfc3339(now.as_millis() as i64)
}

pub fn utc_millis_to_rfc3339(millis: i64) -> String {
    let secs = millis.div_euclid(1000);
    let ms = millis.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{ms:03}Z",
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_known_values() {
        assert_eq!(utc_millis_to_rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            utc_millis_to_rfc3339(1_785_000_000_123),
            "2026-07-25T17:20:00.123Z"
        );
    }
}
