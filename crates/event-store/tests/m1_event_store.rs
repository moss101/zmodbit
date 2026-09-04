//! Integration tests for the M1.1 slice: real SQLite (bundled), append-only
//! event store with per-aggregate sequence enforcement and integrity hashing,
//! and idempotent command processing (docs/13, docs/30, docs/31).

use std::sync::Arc;

use modbit_domain::commands::CommandPayload;
use modbit_domain::{Actor, ActorType, SessionId, TaskId};
use modbit_domain::{Command, DomainEvent};
use modbit_event_store::{CommandProcessor, EventStore, Outcome, StoreError};

fn actor() -> Actor {
    Actor {
        actor_type: ActorType::User,
        actor_id: "user-mohsin".into(),
    }
}

fn store_at(tag: &str) -> Arc<EventStore> {
    let mut path = std::env::temp_dir();
    path.push(format!("modbit-m1.1-{tag}-{}.db", uuid::Uuid::now_v7()));
    Arc::new(EventStore::open(&path).expect("open event store"))
}

fn processor(store: Arc<EventStore>) -> CommandProcessor {
    CommandProcessor::new(store)
}

#[test]
fn sqlite_pragmas_are_authoritative_grade() {
    let mut path = std::env::temp_dir();
    path.push(format!("modbit-pragma-{}.db", uuid::Uuid::now_v7()));
    let store = EventStore::open(&path).unwrap();
    store.with_conn(|conn| {
        let journal: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(journal.to_lowercase(), "wal");
        let fk: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1, "foreign_keys must be ON (docs/31)");
        let sync: i64 = conn
            .query_row("PRAGMA synchronous", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sync, 2, "synchronous must be FULL (docs/31)");
    });
}

#[test]
fn commands_append_events_in_per_aggregate_sequence_order() {
    let store = store_at("ordering");
    let proc = processor(store.clone());

    let session_id = new_session(&proc);

    let (task_id, task_count) = {
        let outcome = proc
            .execute(Command {
                command_id: uuid::Uuid::now_v7().to_string(),
                actor: actor(),
                payload: CommandPayload::CreateTask {
                    session_id,
                    title: "Implement projections".into(),
                    prompt: "p".into(),
                },
            })
            .unwrap();
        let ids = outcome.applied().expect("task creation applies");
        let event = store
            .load(&load_aggregate_of_event(&store, &ids[0]))
            .unwrap();
        let created = event.first().expect("creation event exists");
        assert_eq!(created.sequence, 1);
        let task_id = created.task_id.unwrap();
        match &created.payload {
            DomainEvent::TaskCreated { title, .. } => {
                assert_eq!(title, "Implement projections")
            }
            other => panic!("unexpected event {other:?}"),
        }
        (task_id, ids.len())
    };
    assert_eq!(task_count, 1);

    // Lifecycle: queue then start; sequences continue 2, 3 on the same aggregate.
    for payload in [
        CommandPayload::QueueTask { task_id },
        CommandPayload::StartTask { task_id },
    ] {
        proc.execute(Command {
            command_id: uuid::Uuid::now_v7().to_string(),
            actor: actor(),
            payload,
        })
        .unwrap()
        .applied()
        .expect("lifecycle command applies");
    }

    let events = store.load(&task_id.to_string()).unwrap();
    let seqs: Vec<u64> = events.iter().map(|e| e.sequence).collect();
    assert_eq!(seqs, vec![1, 2, 3], "per-aggregate sequences are gapless");
    let types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
    assert_eq!(types, vec!["task_created", "task_queued", "task_started"]);
    store
        .verify_stream(&task_id.to_string())
        .expect("stream verifies");
}

#[test]
fn idempotent_retry_appends_once_and_replays() {
    let store = store_at("idempotency");
    let proc = processor(store.clone());

    let session_id = new_session(&proc);
    let command_id = uuid::Uuid::now_v7().to_string();
    let cmd = Command {
        command_id: command_id.clone(),
        actor: actor(),
        payload: CommandPayload::CreateTask {
            session_id,
            title: "t".into(),
            prompt: "p".into(),
        },
    };

    let first = proc.execute(cmd.clone()).unwrap();
    let ids = first.applied().expect("first execution applies");
    assert_eq!(ids.len(), 1);

    let second = proc.execute(cmd).unwrap();
    match second {
        Outcome::Replayed { event_ids } => assert_eq!(event_ids, ids),
        other => panic!("expected Replayed, got {other:?}"),
    }

    // Exactly one task_created event exists despite the retry.
    let count = count_events(store.as_ref(), "task_created");
    assert_eq!(count, 1, "idempotent retry must not duplicate (docs/30)");
}

#[test]
fn rejected_command_records_outcome_and_appends_nothing() {
    let store = store_at("rejection");
    let proc = processor(store.clone());
    let session_id = new_session(&proc);
    let task_id = new_task(&proc, session_id);

    // Created → Completed directly is illegal (docs/13 task machine).
    let command_id = uuid::Uuid::now_v7().to_string();
    let cmd = Command {
        command_id: command_id.clone(),
        actor: actor(),
        payload: CommandPayload::CompleteTask {
            task_id,
            summary: "skip".into(),
        },
    };
    match proc.execute(cmd.clone()).unwrap() {
        Outcome::Rejected { reason } => assert!(reason.contains("task_completed"), "{reason}"),
        other => panic!("expected Rejected, got {other:?}"),
    }
    // Retry observes the same deterministic rejection.
    assert!(matches!(
        proc.execute(cmd).unwrap(),
        Outcome::Rejected { .. }
    ));

    // Nothing beyond task creation was appended.
    let events = store.load(&task_id.to_string()).unwrap();
    assert_eq!(events.len(), 1, "rejection must append nothing");
    store.verify_stream(&task_id.to_string()).unwrap();
}

#[test]
fn full_lifecycle_folds_to_completed_and_terminal_state_rejects_more() {
    let store = store_at("lifecycle");
    let proc = processor(store.clone());
    let session_id = new_session(&proc);
    let task_id = new_task(&proc, session_id);

    let steps: Vec<CommandPayload> = vec![
        CommandPayload::QueueTask { task_id },
        CommandPayload::StartTask { task_id },
        CommandPayload::TaskWaiting {
            task_id,
            reason: modbit_domain::WaitingReason::Approval,
        },
        CommandPayload::StartTask { task_id },
        CommandPayload::TaskReadyForReview { task_id },
        CommandPayload::CompleteTask {
            task_id,
            summary: "shipped".into(),
        },
    ];
    for payload in steps {
        proc.execute(Command {
            command_id: uuid::Uuid::now_v7().to_string(),
            actor: actor(),
            payload,
        })
        .unwrap()
        .applied()
        .expect("legal lifecycle step applies");
    }

    // Terminal: cancelling a completed task is rejected by the machine.
    let outcome = proc.execute(Command {
        command_id: uuid::Uuid::now_v7().to_string(),
        actor: actor(),
        payload: CommandPayload::CancelTask {
            task_id,
            reason: "late".into(),
        },
    });
    assert!(matches!(outcome.unwrap(), Outcome::Rejected { .. }));

    let events = store.load(&task_id.to_string()).unwrap();
    let domain_events: Vec<DomainEvent> = events.iter().map(|e| e.payload.clone()).collect();
    assert_eq!(
        modbit_domain::task::fold_task(&domain_events),
        Some(Ok(modbit_domain::TaskState::Completed))
    );
    store.verify_stream(&task_id.to_string()).unwrap();
}

#[test]
fn tampered_stream_is_detected() {
    let store = store_at("tamper");
    let proc = processor(store.clone());
    let session_id = new_session(&proc);
    let task_id = new_task(&proc, session_id);

    // Simulate out-of-band tampering: rewrite a payload directly.
    store.with_conn(|conn| {
        conn.execute(
            "UPDATE events SET payload_inline = '{\"event\":\"task_failed\"}' WHERE aggregate_id = ?1",
            [task_id.to_string()],
        )
        .unwrap();
    });
    match store.verify_stream(&task_id.to_string()) {
        Err(StoreError::IntegrityMismatch { .. }) => {}
        other => panic!("expected integrity mismatch, got {other:?}"),
    }
}

#[test]
fn create_task_against_missing_session_is_rejected() {
    let store = store_at("no-session");
    let proc = processor(store.clone());
    let outcome = proc.execute(Command {
        command_id: uuid::Uuid::now_v7().to_string(),
        actor: actor(),
        payload: CommandPayload::CreateTask {
            session_id: SessionId::generate(),
            title: "t".into(),
            prompt: "p".into(),
        },
    });
    assert!(matches!(outcome.unwrap(), Outcome::Rejected { .. }));
}

// ---- helpers ------------------------------------------------------------

fn new_session(proc: &CommandProcessor) -> SessionId {
    let outcome = proc
        .execute(Command {
            command_id: uuid::Uuid::now_v7().to_string(),
            actor: actor(),
            payload: CommandPayload::CreateSession {
                display_name: "work".into(),
            },
        })
        .unwrap();
    let ids = outcome.applied().expect("session creation applies");
    let (aggregate_id, _) = event_by_id(proc.store(), &ids[0]);
    SessionId::parse(&aggregate_id).unwrap()
}

fn new_task(proc: &CommandProcessor, session_id: SessionId) -> TaskId {
    let outcome = proc
        .execute(Command {
            command_id: uuid::Uuid::now_v7().to_string(),
            actor: actor(),
            payload: CommandPayload::CreateTask {
                session_id,
                title: "t".into(),
                prompt: "p".into(),
            },
        })
        .unwrap();
    let ids = outcome.applied().expect("task creation applies");
    let (aggregate_id, _) = event_by_id(proc.store(), &ids[0]);
    TaskId::parse(&aggregate_id).unwrap()
}

/// Resolves the aggregate a committed event belongs to.
fn load_aggregate_of_event(store: &EventStore, event_id: &str) -> String {
    store.with_conn(|conn| {
        conn.query_row(
            "SELECT aggregate_id FROM events WHERE event_id = ?1",
            [event_id],
            |r| r.get(0),
        )
        .unwrap()
    })
}

/// `payload_inline` holds the serialized domain event, not the envelope.
fn event_by_id(store: &EventStore, event_id: &str) -> (String, DomainEvent) {
    store.with_conn(|conn| {
        conn.query_row(
            "SELECT aggregate_id, payload_inline FROM events WHERE event_id = ?1",
            [event_id],
            |r| {
                let aggregate_id: String = r.get(0)?;
                let payload: String = r.get(1)?;
                Ok((aggregate_id, serde_json::from_str(&payload).unwrap()))
            },
        )
        .unwrap()
    })
}

fn count_events(store: &EventStore, event_type: &str) -> i64 {
    store.with_conn(|conn| {
        conn.query_row(
            "SELECT COUNT(*) FROM events WHERE event_type = ?1",
            [event_type],
            |r| r.get(0),
        )
        .unwrap()
    })
}

trait OutcomeExt {
    fn applied(self) -> Result<Vec<String>, String>;
}

impl OutcomeExt for Outcome {
    fn applied(self) -> Result<Vec<String>, String> {
        match self {
            Outcome::Applied { event_ids } => Ok(event_ids),
            Outcome::Replayed { event_ids } => Ok(event_ids),
            Outcome::Rejected { reason } => Err(reason),
        }
    }
}
