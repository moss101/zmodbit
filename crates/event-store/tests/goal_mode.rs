//! Goal mode integration test (M1, REQ-EV-0119/QUAL-EV-0119): the objective,
//! progress and termination are host-owned. When the model claims "done" but
//! acceptance is not host-verified, the run remains incomplete.

use std::sync::Arc;

use modbit_domain::commands::CommandPayload;
use modbit_domain::task::fold_task;
use modbit_domain::{Actor, ActorType, Command, DomainEvent, TaskId};
use modbit_event_store::{CommandProcessor, EventStore};

trait OutcomeExt {
    fn applied_ok(&self) -> bool;
}

impl OutcomeExt for modbit_event_store::Outcome {
    fn applied_ok(&self) -> bool {
        !matches!(self, modbit_event_store::Outcome::Rejected { .. })
    }
}

fn actor() -> Actor {
    Actor {
        actor_type: ActorType::User,
        actor_id: "user-mohsin".into(),
    }
}

fn system() -> Actor {
    Actor {
        actor_type: ActorType::System,
        actor_id: "verification-engine".into(),
    }
}

#[test]
fn model_claim_without_host_acceptance_leaves_the_run_incomplete() {
    let mut db = std::env::temp_dir();
    db.push(format!("modbit-goal-{}.db", uuid::Uuid::now_v7()));
    let store = Arc::new(EventStore::open(&db).unwrap());
    let proc = CommandProcessor::new(store.clone());

    // Setup: session + task, driven to ReadyForReview.
    let create_session = proc
        .execute(Command {
            command_id: uuid::Uuid::now_v7().to_string(),
            actor: actor(),
            payload: CommandPayload::CreateSession {
                display_name: "goal".into(),
            },
        })
        .unwrap();
    assert!(create_session.applied_ok());
    let session_id: String = store
        .with_conn(|conn| {
            conn.query_row(
                "SELECT aggregate_id FROM events WHERE aggregate_type = 'session' LIMIT 1",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    let session = modbit_domain::SessionId::parse(&session_id).unwrap();

    let create_task = proc
        .execute(Command {
            command_id: uuid::Uuid::now_v7().to_string(),
            actor: actor(),
            payload: CommandPayload::CreateTask {
                session_id: session,
                title: "goal-gated task".into(),
                prompt: "p".into(),
            },
        })
        .unwrap();
    assert!(create_task.applied_ok());
    let task_id: String = store
        .with_conn(|conn| conn.query_row("SELECT task_id FROM tasks LIMIT 1", [], |r| r.get(0)))
        .unwrap();
    let task = TaskId::parse(&task_id).unwrap();

    // The HOST sets the persistent goal with acceptance criteria.
    let set_goal = proc
        .execute(Command {
            command_id: uuid::Uuid::now_v7().to_string(),
            actor: system(),
            payload: CommandPayload::SetGoal {
                task_id: task,
                objective: "all tests pass and coverage gate holds".into(),
                acceptance_criteria: vec![
                    "cargo test --workspace green".into(),
                    "coverage gate >= threshold".into(),
                ],
            },
        })
        .unwrap();
    assert!(set_goal.applied_ok());

    // Drive to ReadyForReview.
    for payload in [
        CommandPayload::QueueTask { task_id: task },
        CommandPayload::StartTask { task_id: task },
        CommandPayload::TaskReadyForReview { task_id: task },
    ] {
        let outcome = proc
            .execute(Command {
                command_id: uuid::Uuid::now_v7().to_string(),
                actor: actor(),
                payload,
            })
            .unwrap();
        assert!(outcome.applied_ok());
    }

    // THE MODEL SAYS DONE — but acceptance was never host-verified.
    let model_claim = Command {
        command_id: uuid::Uuid::now_v7().to_string(),
        actor: Actor {
            actor_type: ActorType::Agent,
            actor_id: "agent-1".into(),
        },
        payload: CommandPayload::CompleteTask {
            task_id: task,
            summary: "I'm done!".into(),
            host_verified: false,
        },
    };
    match proc.execute(model_claim.clone()).unwrap() {
        modbit_event_store::Outcome::Rejected { reason } => {
            assert!(reason.contains("self-certify"), "{reason}");
        }
        other => panic!("goal mode must reject unverified completion, got {other:?}"),
    }

    // The run remains incomplete: the task is still ReadyForReview, the
    // stream gained no TaskCompleted event.
    let events = store.load(&task_id).unwrap();
    assert!(!events.iter().any(|e| e.event_type == "task_completed"));
    let domain: Vec<DomainEvent> = events.iter().map(|e| e.payload.clone()).collect();
    assert_eq!(
        fold_task(&domain).unwrap().unwrap(),
        modbit_domain::TaskState::ReadyForReview
    );

    // A retried model claim is also rejected (idempotent rejection).
    assert!(matches!(
        proc.execute(model_claim).unwrap(),
        modbit_event_store::Outcome::Rejected { .. }
    ));

    // With host-verified acceptance the completion applies.
    let host_complete = Command {
        command_id: uuid::Uuid::now_v7().to_string(),
        actor: system(),
        payload: CommandPayload::CompleteTask {
            task_id: task,
            summary: "acceptance criteria met".into(),
            host_verified: true,
        },
    };
    match proc.execute(host_complete).unwrap() {
        modbit_event_store::Outcome::Applied { .. } => {}
        other => panic!("expected host completion to apply, got {other:?}"),
    }

    // Now the task is complete and the goal gate is satisfied.
    let events = store.load(&task_id).unwrap();
    let domain: Vec<DomainEvent> = events.iter().map(|e| e.payload.clone()).collect();
    assert_eq!(
        fold_task(&domain).unwrap().unwrap(),
        modbit_domain::TaskState::Completed
    );
}
