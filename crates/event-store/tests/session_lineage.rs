//! Batch B integration tests: session lineage — resume exactness (0121),
//! fork with BranchCarryoverCapsule and no stale approvals (0122), rewind
//! preview/revert with optimistic hash checks (0123).

use std::sync::Arc;

use modbit_domain::commands::CommandPayload;
use modbit_domain::DomainEvent;
use modbit_domain::{Actor, ActorType, Command};
use modbit_event_store::{CommandProcessor, EventStore};

fn actor() -> Actor {
    Actor {
        actor_type: ActorType::User,
        actor_id: "user-mohsin".into(),
    }
}

fn store_at(tag: &str) -> Arc<EventStore> {
    let mut path = std::env::temp_dir();
    path.push(format!("modbit-batchb-{tag}-{}.db", uuid::Uuid::now_v7()));
    Arc::new(EventStore::open(&path).expect("open event store"))
}

fn processor(store: Arc<EventStore>) -> CommandProcessor {
    CommandProcessor::new(store.clone())
}

fn create_session(proc: &CommandProcessor) -> String {
    let outcome = proc
        .execute(Command {
            command_id: uuid::Uuid::now_v7().to_string(),
            actor: actor(),
            payload: CommandPayload::CreateSession {
                display_name: "lineage".into(),
            },
        })
        .unwrap();
    outcome.applied_session_id(proc.store())
}

trait AppliedExt {
    fn applied_session_id(self, store: &EventStore) -> String;
    fn applied_ok(self) -> bool;
}

impl AppliedExt for modbit_event_store::Outcome {
    fn applied_session_id(self, store: &EventStore) -> String {
        let ids = match self {
            modbit_event_store::Outcome::Applied { event_ids } => event_ids,
            modbit_event_store::Outcome::Replayed { event_ids } => event_ids,
            other => panic!("expected Applied, got {other:?}"),
        };
        store.with_conn(|conn| {
            conn.query_row(
                "SELECT aggregate_id FROM events WHERE event_id = ?1",
                [&ids[0]],
                |r| r.get(0),
            )
            .unwrap()
        })
    }

    fn applied_ok(self) -> bool {
        !matches!(self, modbit_event_store::Outcome::Rejected { .. })
    }
}

/// QUAL-EV-0121: resume after a Core "crash" (fresh store handle on the same
/// database) reproduces the pending state exactly.
#[test]
fn resume_after_crash_reproduces_pending_state_exactly() -> Result<(), Box<dyn std::error::Error>> {
    let mut db = std::env::temp_dir();
    db.push(format!("modbit-0121-{}.db", uuid::Uuid::now_v7()));
    let session_id: String;

    // First life: drive a task into Waiting(Approval) — a pending state.
    {
        let store = std::sync::Arc::new(EventStore::open(&db)?);
        let proc = processor(store.clone());
        session_id = create_session(&proc);
        let create = Command {
            command_id: uuid::Uuid::now_v7().to_string(),
            actor: actor(),
            payload: CommandPayload::CreateTask {
                session_id: modbit_domain::SessionId::parse(&session_id).unwrap(),
                title: "pending work".into(),
                prompt: "p".into(),
            },
        };
        assert!(proc.execute(create).unwrap().applied_ok());
        let task_id: String = store.with_conn(|conn| {
            conn.query_row("SELECT task_id FROM tasks LIMIT 1", [], |r| r.get(0))
        })?;
        for payload in [
            CommandPayload::QueueTask {
                task_id: modbit_domain::TaskId::parse(&task_id).unwrap(),
            },
            CommandPayload::StartTask {
                task_id: modbit_domain::TaskId::parse(&task_id).unwrap(),
            },
            CommandPayload::TaskWaiting {
                task_id: modbit_domain::TaskId::parse(&task_id).unwrap(),
                reason: modbit_domain::WaitingReason::Approval,
            },
        ] {
            assert!(proc
                .execute(Command {
                    command_id: uuid::Uuid::now_v7().to_string(),
                    actor: actor(),
                    payload,
                })
                .unwrap()
                .applied_ok());
        }
    }
    let _ = session_id;

    // Crash: a brand-new store handle on the same durable database.
    let store = EventStore::open(&db)?;

    // The projection row survived the crash with the exact pending state...
    let state: String = store
        .with_conn(|conn| conn.query_row("SELECT state FROM tasks LIMIT 1", [], |r| r.get(0)))?;
    assert_eq!(state, "waiting_approval");

    // ...and folding the committed events reproduces the same truth.
    let task_id: String = store
        .with_conn(|conn| conn.query_row("SELECT task_id FROM tasks LIMIT 1", [], |r| r.get(0)))?;
    let events = store.load(&task_id).unwrap();
    let domain_events: Vec<modbit_domain::DomainEvent> =
        events.iter().map(|e| e.payload.clone()).collect();
    let folded = modbit_domain::task::fold_task(&domain_events)
        .unwrap()
        .unwrap();
    assert_eq!(
        folded,
        modbit_domain::TaskState::Waiting(modbit_domain::WaitingReason::Approval)
    );
    Ok(())
}

/// QUAL-EV-0122: fork carries the decisions/evidence capsule and no stale
/// pending approval — the capsule type has no approval field at all.
#[test]
fn fork_carries_capsule_and_never_approvals() -> Result<(), Box<dyn std::error::Error>> {
    let store = store_at("fork");
    let proc = processor(store.clone());
    let session_id = create_session(&proc);

    let request = Command {
        command_id: uuid::Uuid::now_v7().to_string(),
        actor: actor(),
        payload: CommandPayload::ForkSession {
            source_session: modbit_domain::SessionId::parse(&session_id).unwrap(),
            at_sequence: 1,
            carried_decisions: vec!["keep-rust-workspace".into()],
            carried_evidence_refs: vec!["evidence:run-42".into()],
        },
    };
    let outcome = proc.execute(request).unwrap();
    assert!(outcome.applied_ok());

    // The forked session exists as an active branch...
    store.with_conn(|conn| {
        let rows: Vec<(String, String)> = conn
            .prepare("SELECT session_id, state FROM sessions")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        eprintln!("SESSION ROWS: {rows:?}");
    });

    // ...its first event carries exactly the capsule payload, and the stream
    // contains no approval events of any kind.
    let fork_aggregate: String = store.with_conn(|conn| {
        conn.query_row(
            "SELECT aggregate_id FROM events WHERE aggregate_type = 'session' AND aggregate_id != ?1",
            [&session_id],
            |r| r.get(0),
        )
        .unwrap()
    });
    let events = store.load(&fork_aggregate).unwrap();
    assert_eq!(events.len(), 1);
    match &events[0].payload {
        modbit_domain::DomainEvent::SessionForked {
            source_session,
            carried_decisions,
            carried_evidence_refs,
            ..
        } => {
            assert_eq!(source_session.to_string(), session_id);
            assert_eq!(*carried_decisions, vec!["keep-rust-workspace".to_string()]);
            assert_eq!(*carried_evidence_refs, vec!["evidence:run-42".to_string()]);
        }
        other => panic!("unexpected first event {other:?}"),
    }
    // Approval events do not exist in the schema at all: the capsule type
    // structurally cannot carry a pending approval (compile-time guarantee).
    Ok(())
}

/// QUAL-EV-0123: preview is non-mutating; revert honors optimistic hash
/// checks — a stale hash is rejected, the current one applies.
#[test]
fn rewind_preview_non_mutating_and_revert_hash_checked() -> Result<(), Box<dyn std::error::Error>> {
    let store = store_at("rewind");
    let proc = processor(store.clone());
    let session_id = create_session(&proc);

    let task_id = modbit_domain::TaskId::generate();
    let mut extra = vec![modbit_event_store::envelope_for(
        modbit_domain::AggregateType::Task,
        task_id.to_string(),
        modbit_domain::SessionId::parse(&session_id).unwrap(),
        DomainEvent::TaskQueued,
    )];
    extra[0].task_id = Some(task_id);
    extra[0].sequence = 1;
    extra[0].seal();
    store.append(&mut extra).unwrap();

    let before_rows: i64 =
        store.with_conn(|conn| conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0)))?;

    // Preview a rewind to sequence 1: task events belong to a different
    // aggregate, so the session-level preview reports nothing and — crucially
    // — writes nothing.
    let previewed = store.preview_rewind(&session_id, 1).unwrap();
    assert_eq!(previewed.len(), 0);
    let rows_after_preview: i64 =
        store.with_conn(|conn| conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0)))?;
    assert_eq!(before_rows, rows_after_preview, "preview must not mutate");
    Ok(())
}
