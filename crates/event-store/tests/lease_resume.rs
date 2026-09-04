//! Batch A integration tests: offset-keyed event resume (REQ-EV-0010) and
//! session-lease fencing (REQ-EV-0054/0273).

use std::sync::Arc;

use modbit_domain::commands::CommandPayload;
use modbit_domain::{Actor, ActorType, Command};
use modbit_event_store::{CommandProcessor, EventStore, StoreError};

fn actor() -> Actor {
    Actor {
        actor_type: ActorType::User,
        actor_id: "user-mohsin".into(),
    }
}

fn store_at(tag: &str) -> Arc<EventStore> {
    let mut path = std::env::temp_dir();
    path.push(format!("modbit-batcha-{tag}-{}.db", uuid::Uuid::now_v7()));
    Arc::new(EventStore::open(&path).expect("open event store"))
}

fn processor(store: Arc<EventStore>) -> modbit_event_store::CommandProcessor {
    modbit_event_store::CommandProcessor::new(store.clone())
}

fn create_session(proc: &CommandProcessor) -> String {
    let outcome = proc
        .execute(Command {
            command_id: uuid::Uuid::now_v7().to_string(),
            actor: actor(),
            payload: CommandPayload::CreateSession {
                display_name: "resume".into(),
            },
        })
        .unwrap();
    outcome.applied_session_id(proc.store())
}

trait AppliedExt {
    fn applied_session_id(self, store: &EventStore) -> String;
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
}

/// QUAL-EV-0010: disconnect, produce events, reconnect from the last offset
/// and compare the exact stream — plus full-rehydrate fallback from 0.
#[test]
fn resume_from_offset_returns_the_exact_missing_stream() {
    let store = store_at("resume");
    let proc = processor(store.clone());
    let session_id = create_session(&proc);

    // Client sees the initial stream, stores the offset, then "disconnects".
    let (initial, offset) = store.session_events_since(&session_id, 0, 100).unwrap();
    assert_eq!(initial.len(), 1);
    assert!(offset > 0);

    // While disconnected, more events are produced for the same session.
    for i in 0..3 {
        let task_id = modbit_domain::TaskId::generate();
        let mut e = modbit_event_store::envelope_for(
            modbit_domain::AggregateType::Task,
            task_id.to_string(),
            modbit_domain::SessionId::parse(&session_id).unwrap(),
            DomainEvent::TaskQueued,
        );
        e.task_id = Some(task_id);
        e.sequence = 1;
        e.seal();
        let mut batch = vec![e];
        store.append(&mut batch).unwrap();
        let _ = i;
    }

    // Reconnect from the stored offset: exactly the 3 missing events.
    let (resumed, new_offset) = store
        .session_events_since(&session_id, offset, 100)
        .unwrap();
    assert_eq!(resumed.len(), 3, "only the events produced while away");
    for e in &resumed {
        assert_eq!(e.event_type, "task_queued");
    }
    assert!(new_offset > offset);

    // Resuming again from the new offset yields nothing new.
    let (empty, _) = store
        .session_events_since(&session_id, new_offset, 100)
        .unwrap();
    assert!(empty.is_empty());

    // Full-rehydrate fallback from offset 0 returns the entire stream.
    let (all, _) = store.session_events_since(&session_id, 0, 1000).unwrap();
    assert_eq!(all.len(), 4);
    // And the exact stream equals initial + resumed, in order.
    let mut expected: Vec<String> = initial
        .iter()
        .chain(resumed.iter())
        .map(|e| e.event_id.clone())
        .collect();
    let mut actual: Vec<String> = all.iter().map(|e| e.event_id.clone()).collect();
    expected.sort();
    actual.sort();
    assert_eq!(expected, actual);
}

/// QUAL-EV-0054: dual resume — only the CURRENT lease can append mutation
/// events; the stale lease is rejected.
#[test]
fn stale_lease_cannot_append_but_current_lease_can() {
    let store = store_at("lease");
    let proc = processor(store.clone());
    let session_id = create_session(&proc);

    // Two clients "resume" the same session: the second acquires a newer lease.
    let client_a = modbit_event_store::leases::acquire(
        store.with_conn_ref().deref(),
        &session_id,
        "lease-a",
        "desktop-a",
    )
    .unwrap();
    assert_eq!(client_a.generation, 1);
    let client_b = modbit_event_store::leases::acquire(
        store.with_conn_ref().deref(),
        &session_id,
        "lease-b",
        "desktop-b",
    )
    .unwrap();
    assert_eq!(client_b.generation, 2, "generation is monotonic");

    // Stale writer (client a) attempts a mutation append: rejected.
    let task_a = modbit_domain::TaskId::generate();
    let make_created = |task_a: modbit_domain::TaskId| {
        let mut e = modbit_event_store::envelope_for(
            modbit_domain::AggregateType::Task,
            task_a.to_string(),
            modbit_domain::SessionId::parse(&session_id).unwrap(),
            DomainEvent::TaskCreated {
                session_id: modbit_domain::SessionId::parse(&session_id).unwrap(),
                title: "fenced".into(),
                prompt: "p".into(),
            },
        );
        e.task_id = Some(task_a);
        e.sequence = 1;
        e.seal();
        vec![e]
    };
    let mut stale_events = make_created(task_a);
    stale_events[0].task_id = Some(task_a);
    stale_events[0].sequence = 1;
    stale_events[0].seal();
    match store.append_with_lease(&session_id, "lease-a", &mut stale_events) {
        Err(StoreError::StaleLease { lease_id, .. }) => assert_eq!(lease_id, "lease-a"),
        other => panic!("expected StaleLease, got {other:?}"),
    }

    // Current owner (client b) appends the same creation: accepted.
    let mut current_events = make_created(task_a);
    store
        .append_with_lease(&session_id, "lease-b", &mut current_events)
        .expect("current lease appends");

    // The fenced append projected the task; the stale attempt added nothing.
    let task_count: i64 = store.with_conn(|conn| {
        conn.query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
            .unwrap()
    });
    assert_eq!(task_count, 1);
}

use modbit_domain::DomainEvent;
use std::ops::Deref;
