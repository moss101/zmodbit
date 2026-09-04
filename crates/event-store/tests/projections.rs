//! M1.2 integration tests: transactional projections, rebuild equivalence and
//! migration safety (docs/31 § Core tables / § Migration safety).

use std::sync::Arc;

use modbit_domain::commands::CommandPayload;
use modbit_domain::{Actor, ActorType, Command, DomainEvent};
use modbit_event_store::{CommandProcessor, EventStore};

trait OutcomeExt {
    fn applied(self) -> Result<Vec<String>, String>;
}

impl OutcomeExt for modbit_event_store::Outcome {
    fn applied(self) -> Result<Vec<String>, String> {
        match self {
            modbit_event_store::Outcome::Applied { event_ids } => Ok(event_ids),
            modbit_event_store::Outcome::Replayed { event_ids } => Ok(event_ids),
            modbit_event_store::Outcome::Rejected { reason } => Err(reason),
        }
    }
}

fn actor() -> Actor {
    Actor {
        actor_type: ActorType::User,
        actor_id: "user-mohsin".into(),
    }
}

fn store_at(tag: &str) -> Arc<EventStore> {
    let mut path = std::env::temp_dir();
    path.push(format!("modbit-m1.2-{tag}-{}.db", uuid::Uuid::now_v7()));
    Arc::new(EventStore::open(&path).expect("open event store"))
}

fn processor(store: Arc<EventStore>) -> CommandProcessor {
    CommandProcessor::new(store)
}

fn snapshot(store: &EventStore) -> Vec<String> {
    store.with_conn(|conn| {
        let mut out = Vec::new();
        for table in ["sessions", "tasks", "runs", "turns", "run_steps"] {
            let mut stmt = conn
                .prepare(&format!("SELECT * FROM {table} ORDER BY 1"))
                .unwrap();
            let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
            let mut rows = stmt.query([]).unwrap();
            while let Some(row) = rows.next().unwrap() {
                let mut fields = vec![table.to_string()];
                for (i, col) in cols.iter().enumerate() {
                    let value: Option<String> = row.get(i).unwrap_or(None);
                    fields.push(format!("{col}={value:?}"));
                }
                out.push(fields.join("|"));
            }
        }
        out
    })
}

fn run_full_lifecycle(proc: &CommandProcessor) {
    let session_out = proc
        .execute(Command {
            command_id: uuid::Uuid::now_v7().to_string(),
            actor: actor(),
            payload: CommandPayload::CreateSession {
                display_name: "work".into(),
            },
        })
        .unwrap();
    let session_ids = session_out.applied().expect("session applies");
    let session_id: String = proc
        .store()
        .with_conn(|conn| {
            conn.query_row(
                "SELECT aggregate_id FROM events WHERE event_id = ?1",
                [&session_ids[0]],
                |r| r.get(0),
            )
        })
        .unwrap();
    let task_out = proc
        .execute(Command {
            command_id: uuid::Uuid::now_v7().to_string(),
            actor: actor(),
            payload: CommandPayload::CreateTask {
                session_id: modbit_domain::SessionId::parse(&session_id).unwrap(),
                title: "t".into(),
                prompt: "p".into(),
            },
        })
        .unwrap();
    let task_ids = task_out.applied().expect("task applies");
    let task_id: String = proc
        .store()
        .with_conn(|conn| {
            conn.query_row(
                "SELECT aggregate_id FROM events WHERE event_id = ?1",
                [&task_ids[0]],
                |r| r.get(0),
            )
        })
        .unwrap();
    let task_id = modbit_domain::TaskId::parse(&task_id).unwrap();
    for payload in [
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
            summary: "done".into(),
        },
    ] {
        proc.execute(Command {
            command_id: uuid::Uuid::now_v7().to_string(),
            actor: actor(),
            payload,
        })
        .unwrap()
        .applied()
        .expect("lifecycle step applies");
    }
}

#[test]
fn projections_track_the_lifecycle() {
    let store = store_at("track");
    let proc = processor(store.clone());
    run_full_lifecycle(&proc);

    store.with_conn(|conn| {
        let (state, completed_at, failure): (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT state, completed_at, failure_code FROM tasks LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, "completed");
        assert!(completed_at.is_some(), "completion timestamp projected");
        assert!(failure.is_none());

        let sessions: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sessions, 1, "session projected");

        // The task's lifecycle waiting phase must not linger in the final state.
        let started_at: Option<String> = conn
            .query_row("SELECT started_at FROM tasks LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert!(started_at.is_some(), "start timestamp projected");
    });
}

#[test]
fn rebuild_reproduces_identical_projections() {
    let store = store_at("rebuild");
    let proc = processor(store.clone());
    run_full_lifecycle(&proc);

    let before = snapshot(&store);
    assert!(!before.is_empty());
    let applied = store.rebuild_projections().unwrap();
    assert!(applied > 0);
    let after = snapshot(&store);
    assert_eq!(
        before, after,
        "rebuild must reproduce identical projection rows"
    );
}

#[test]
fn v1_database_migrates_to_v2_preserving_events() {
    use modbit_event_store::migrations::MIGRATIONS;

    // Simulate a pre-upgrade deployment: schema v1 only, one committed event.
    let mut path = std::env::temp_dir();
    path.push(format!("modbit-m1.2-fixture-{}.db", uuid::Uuid::now_v7()));
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(MIGRATIONS[0].1).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();

        let session = modbit_domain::SessionId::generate();
        let payload = DomainEvent::SessionCreated {
            display_name: "legacy".into(),
        };
        let mut e = modbit_event_store::envelope_for(
            modbit_domain::AggregateType::Session,
            session.to_string(),
            session,
            payload,
        );
        e.sequence = 1;
        e.seal();
        conn.execute(
            "INSERT INTO events (event_id, session_id, aggregate_type, aggregate_id, sequence,
                 event_type, schema_version, occurred_at, actor_type, actor_id,
                 causation_id, correlation_id, payload_inline, payload_object_hash, integrity_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            rusqlite::params![
                e.event_id,
                e.session_id.to_string(),
                e.aggregate_type.as_str(),
                e.aggregate_id,
                e.sequence as i64,
                e.event_type,
                format!("{}.{}", e.schema_version.0, e.schema_version.1),
                e.occurred_at,
                e.actor.actor_type.as_str(),
                e.actor.actor_id,
                e.causation_id,
                e.correlation_id,
                serde_json::to_string(&e.payload).unwrap(),
                e.payload_object_hash,
                e.integrity_hash,
            ],
        )
        .unwrap();
    }

    // Reopening migrates v1 → current, preserves events, and rebuilds
    // projections.
    let store = EventStore::open(&path).unwrap();
    store.with_conn(|conn| {
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 5, "migration applied through fork lineage + input queue tables");
    });
    let sessions = all_session_aggregates(&store);
    assert_eq!(sessions.len(), 1);
    store.verify_stream(&sessions[0]).unwrap();
    let events = store.load(&sessions[0]).unwrap();
    assert!(matches!(
        events[0].payload,
        DomainEvent::SessionCreated { .. }
    ));
    store.rebuild_projections().unwrap();
    store.with_conn(|conn| {
        let state: String = conn
            .query_row("SELECT state FROM sessions LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(state, "active");
    });
}

fn all_session_aggregates(store: &EventStore) -> Vec<String> {
    store.with_conn(|conn| {
        let mut stmt = conn
            .prepare("SELECT DISTINCT aggregate_id FROM events WHERE aggregate_type = 'session'")
            .unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.map(|r| r.unwrap()).collect()
    })
}
