//! Dispatch cluster integration tests (M1): typed input dispatch modes
//! (REQ-EV-0191), non-disruptive side questions (REQ-EV-0261), and durable
//! queued prompts with ordering across reconnects (REQ-EV-0262).

use std::path::PathBuf;
use std::sync::Arc;

use modbit_domain::commands::CommandPayload;
use modbit_domain::input_queue::InputMode;
use modbit_domain::task::fold_task;
use modbit_domain::{Actor, ActorType, Command, DomainEvent, TaskId};
use modbit_event_store::{CommandProcessor, EventStore};

fn actor() -> Actor {
    Actor {
        actor_type: ActorType::User,
        actor_id: "user-mohsin".into(),
    }
}

fn store_path(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "modbit-dispatch-{tag}-{}.db",
        uuid::Uuid::now_v7().simple()
    ));
    path
}

fn processor_at(path: &std::path::Path) -> CommandProcessor {
    CommandProcessor::new(Arc::new(EventStore::open(path).expect("open event store")))
}

/// Sets up a session + a task in the Running state. Returns (session, task).
trait OutcomeExt {
    fn applied_ok(&self) -> bool;
}

impl OutcomeExt for modbit_event_store::Outcome {
    fn applied_ok(&self) -> bool {
        !matches!(self, modbit_event_store::Outcome::Rejected { .. })
    }
}

fn running_task(proc: &CommandProcessor) -> (modbit_domain::SessionId, TaskId) {
    let outcome = proc.execute(Command {
        command_id: uuid::Uuid::now_v7().to_string(),
        actor: actor(),
        payload: CommandPayload::CreateSession {
            display_name: "dispatch".into(),
        },
    });
    assert!(outcome.unwrap().applied_ok());

    let session_id: String = proc
        .store()
        .with_conn(|conn| {
            conn.query_row(
                "SELECT aggregate_id FROM events WHERE aggregate_type = 'session' LIMIT 1",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    let session = modbit_domain::SessionId::parse(&session_id).unwrap();

    let create = proc
        .execute(Command {
            command_id: uuid::Uuid::now_v7().to_string(),
            actor: actor(),
            payload: CommandPayload::CreateTask {
                session_id: session,
                title: "running task".into(),
                prompt: "p".into(),
            },
        })
        .unwrap();
    assert!(create.applied_ok());
    let task_id: String = proc
        .store()
        .with_conn(|conn| conn.query_row("SELECT task_id FROM tasks LIMIT 1", [], |r| r.get(0)))
        .unwrap();
    let task = TaskId::parse(&task_id).unwrap();

    for payload in [
        CommandPayload::QueueTask { task_id: task },
        CommandPayload::StartTask { task_id: task },
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
    (session, task)
}

/// REQ-EV-0191: steer interrupts-and-replaces (durable input + TaskSteered),
/// collect coalesces, follow-up queues — exact durable ordering.
#[test]
fn steer_collect_and_followup_have_exact_ordering_semantics() {
    let path = store_path("modes");
    let proc = processor_at(&path);
    let (_session, task) = running_task(&proc);

    // Steer mid-run: input + TaskSteered (interrupt-and-replace).
    let steer = proc
        .execute(Command {
            command_id: uuid::Uuid::now_v7().to_string(),
            actor: actor(),
            payload: CommandPayload::QueueTaskInput {
                task_id: task,
                input_id: "in-steer".into(),
                mode: InputMode::Steer,
                text: "stop, change approach".into(),
            },
        })
        .unwrap();
    let steer_ids = match steer {
        modbit_event_store::Outcome::Applied { event_ids } => {
            assert_eq!(event_ids.len(), 2, "input + steered events");
            event_ids
        }
        other => panic!("steer should apply, got {other:?}"),
    };

    for (input_id, mode, text) in [
        ("in-collect", InputMode::Collect, "also check tests"),
        ("in-followup", InputMode::FollowUp, "then update docs"),
    ] {
        assert!(proc
            .execute(Command {
                command_id: uuid::Uuid::now_v7().to_string(),
                actor: actor(),
                payload: CommandPayload::QueueTaskInput {
                    task_id: task,
                    input_id: input_id.into(),
                    mode,
                    text: text.into(),
                },
            })
            .unwrap()
            .applied_ok());
    }

    // Durable queue order for the task: steer, collect, follow-up.
    let order: Vec<(String, String)> = proc.store().with_conn(|conn| {
        let mut stmt = conn
            .prepare("SELECT input_id, mode FROM task_inputs WHERE task_id = ?1 ORDER BY sequence")
            .unwrap();
        stmt.query_map([&task.to_string()], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    });
    assert_eq!(
        order,
        vec![
            ("in-steer".to_string(), "steer".to_string()),
            ("in-collect".to_string(), "collect".to_string()),
            ("in-followup".to_string(), "follow_up".to_string()),
        ]
    );

    // The steering is visible in the stream and the task remains running.
    let events = proc.store().load(&task.to_string()).unwrap();
    assert!(events.iter().any(|e| e.event_type == "task_steered"));
    assert_eq!(events.last().unwrap().event_type, "task_input_queued");
    let domain: Vec<DomainEvent> = events.iter().map(|e| e.payload.clone()).collect();
    assert!(matches!(
        fold_task(&domain).unwrap().unwrap(),
        modbit_domain::TaskState::Running
    ));
    let _ = steer_ids;
}

/// REQ-EV-0261: a side question is a session-level event; the main task's
/// stream and projection state remain untouched.
#[test]
fn side_question_does_not_touch_main_task_state() {
    let path = store_path("sideq");
    let proc = processor_at(&path);
    let (session, task) = running_task(&proc);

    let events_before = proc.store().load(&task.to_string()).unwrap().len();
    let state_before: String = proc
        .store()
        .with_conn(|conn| {
            conn.query_row(
                "SELECT state FROM tasks WHERE task_id = ?1",
                [&task.to_string()],
                |r| r.get(0),
            )
        })
        .unwrap();

    let question = proc
        .execute(Command {
            command_id: uuid::Uuid::now_v7().to_string(),
            actor: actor(),
            payload: CommandPayload::AskSideQuestion {
                session_id: session,
                question_id: "q-1".into(),
                question: "what changed in the last turn?".into(),
                context_event_count: 10,
            },
        })
        .unwrap();
    assert!(question.applied_ok());

    assert_eq!(
        proc.store().load(&task.to_string()).unwrap().len(),
        events_before,
        "side question must not append to the task aggregate"
    );
    let state_after: String = proc
        .store()
        .with_conn(|conn| {
            conn.query_row(
                "SELECT state FROM tasks WHERE task_id = ?1",
                [&task.to_string()],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(
        state_before, state_after,
        "side question must not mutate task state"
    );

    let asked: i64 = proc
        .store()
        .with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM events WHERE event_type = 'side_question_asked'",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(asked, 1, "side question is durable at the session level");
}

/// REQ-EV-0262: queued prompts are durable input events whose per-task
/// ordering survives a full reconnect (fresh handles on the same database).
#[test]
fn queued_prompts_preserve_ordering_across_reconnect() {
    let path = store_path("queue");
    {
        let proc = processor_at(&path);
        let (_session, task) = running_task(&proc);
        for i in 0..3u32 {
            let outcome = proc.execute(Command {
                command_id: uuid::Uuid::now_v7().to_string(),
                actor: actor(),
                payload: CommandPayload::QueueTaskInput {
                    task_id: task,
                    input_id: format!("queued-{i}"),
                    mode: InputMode::FollowUp,
                    text: format!("follow-up {i}"),
                },
            });
            assert!(outcome.unwrap().applied_ok());
        }
        let _ = task;
    }

    // Reconnect: brand-new handles on the same durable database.
    let proc2 = processor_at(&path);
    let task_id: String = proc2
        .store()
        .with_conn(|conn| conn.query_row("SELECT task_id FROM tasks LIMIT 1", [], |r| r.get(0)))
        .unwrap();
    let rows: Vec<String> = proc2.store().with_conn(|conn| {
        let mut stmt = conn
            .prepare("SELECT input_id FROM task_inputs WHERE task_id = ?1 ORDER BY sequence")
            .unwrap();
        stmt.query_map([&task_id], |r| r.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    });
    assert_eq!(
        rows,
        vec![
            "queued-0".to_string(),
            "queued-1".to_string(),
            "queued-2".to_string()
        ],
        "queued input ordering survives the reconnect"
    );
}

/// Steer on a terminal task is rejected by the dispatch policy.
#[test]
fn steer_on_completed_task_is_rejected() {
    let path = store_path("terminal");
    let proc = processor_at(&path);
    let (session, task) = running_task(&proc);
    let _ = session;
    for payload in [
        CommandPayload::TaskReadyForReview { task_id: task },
        CommandPayload::CompleteTask {
            task_id: task,
            summary: "done".into(),
            host_verified: false,
        },
    ] {
        assert!(proc
            .execute(Command {
                command_id: uuid::Uuid::now_v7().to_string(),
                actor: actor(),
                payload: payload.clone(),
            })
            .unwrap()
            .applied_ok());
    }

    let outcome = proc.execute(Command {
        command_id: uuid::Uuid::now_v7().to_string(),
        actor: actor(),
        payload: CommandPayload::QueueTaskInput {
            task_id: task,
            input_id: "late-steer".into(),
            mode: InputMode::Steer,
            text: "too late".into(),
        },
    });
    match outcome {
        Ok(modbit_event_store::Outcome::Rejected { reason }) => {
            assert!(reason.contains("interrupt-and-replace"), "{reason}");
        }
        other => panic!("expected rejection, got {other:?}"),
    }
}
