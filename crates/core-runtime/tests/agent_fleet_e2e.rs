//! Agent fleet E2E (Phase 7 item 1): children through the scheduler with
//! transactional admission (docs/14) — exercised through the production
//! dispatch (`CoreServices::handle`) against a real store and a real fleet
//! journal. The spawned child IS a task: it gets its own isolated worktree
//! from the scheduler's per-task allocation when the run loop starts
//! (E2E-001 machinery), parent linkage rides on TaskCreated, and admission
//! refuses atomically (capacity ticket, generation fencing, write-scope
//! conflict, terminal parent) with no partial reservation.

use std::path::PathBuf;
use std::sync::Arc;

use prost::Message;

use modbit_core_runtime::CoreServices;
use modbit_domain::events::{Actor, ActorType};
use modbit_domain::{Command, CommandPayload};
use modbit_event_store::{CommandProcessor, EventStore};
use modbit_protocol::modbit::protocol::v1 as pb;

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "modbit-fleet-{tag}-{}",
        uuid::Uuid::now_v7().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn roundtrip(
    services: &CoreServices,
    request: pb::surface_request::Request,
) -> pb::SurfaceResponse {
    let bytes = pb::SurfaceRequest { request: Some(request) }.encode_to_vec();
    let response = services.handle(&bytes);
    pb::SurfaceResponse::decode(response.as_slice()).unwrap()
}

/// Parent task created, queued and started through the real processor —
/// the state a live orchestrator parent is in when it spawns children.
fn setup(tag: &str) -> (Arc<EventStore>, CoreServices, String) {
    let db = tempdir(&format!("{tag}-db"));
    let store = Arc::new(EventStore::open(&db.join("core.db")).unwrap());
    let services = CoreServices::new(store.clone())
        .with_agent_fleet(tempdir(&format!("{tag}-fleet")).join("agents.jsonl"))
        .unwrap();

    let processor = CommandProcessor::new(store.clone());
    let execute =
        |payload: CommandPayload| processor.execute(Command {
            command_id: uuid::Uuid::now_v7().to_string(),
            actor: Actor { actor_type: ActorType::User, actor_id: "t".into() },
            payload,
        });

    execute(CommandPayload::CreateSession { display_name: "s".into() }).unwrap();
    let sid: String = store
        .with_conn(|conn| {
            conn.query_row(
                "SELECT aggregate_id FROM events WHERE aggregate_type='session' ORDER BY rowid DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    let session_id = modbit_domain::SessionId::parse(&sid).unwrap();
    execute(CommandPayload::CreateTask {
        session_id,
        title: "parent".into(),
        prompt: "parent objective".into(),
        repo_id: None,
        base_branch: None,
        parent_task_id: None,
    })
    .unwrap();
    let tid: String = store
        .with_conn(|conn| {
            conn.query_row(
                "SELECT aggregate_id FROM events WHERE aggregate_type='task' ORDER BY rowid DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    let task_id = modbit_domain::TaskId::parse(&tid).unwrap();
    execute(CommandPayload::QueueTask { task_id }).unwrap();
    execute(CommandPayload::StartTask { task_id }).unwrap();
    (store, services, tid)
}

fn spawn_req(
    parent: &str,
    objective: &str,
    scope: &str,
    key: &str,
    generation: u64,
) -> pb::surface_request::Request {
    pb::surface_request::Request::SpawnAgent(pb::SpawnAgentCommand {
        parent_task_id: parent.to_string(),
        objective: objective.to_string(),
        write_scope: scope.to_string(),
        idempotency_key: key.to_string(),
        parent_generation: generation,
    })
}

#[test]
fn spawn_admits_a_real_child_task_with_parent_linkage() {
    let (_store, services, parent) = setup("spawn");

    let resp = roundtrip(&services, spawn_req(&parent, "write the tests", "tests/", "k-1", 0));
    assert!(resp.ok, "{:?}", resp.error);
    let child = resp.task.expect("child task view");
    assert_eq!(child.parent_task_id, parent, "linkage recorded");
    assert_eq!(child.state, pb::TaskStatus::Started as i32, "child queued AND started");

    // The linkage is durable: the tasks projection carries it and the
    // child's event stream records it on TaskCreated.
    let stored_parent: String = _store
        .with_conn(|conn| {
            conn.query_row(
                "SELECT COALESCE(parent_task_id,'') FROM tasks WHERE task_id = ?1",
                [child.task_id.as_str()],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(stored_parent, parent);
    let has_created: i64 = _store
        .with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM events WHERE aggregate_id = ?1 AND event_type = 'task_created'",
                [child.task_id.as_str()],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(has_created, 1, "child is a real task aggregate");

    // Idempotent re-attach: the same key returns the SAME child.
    let resp = roundtrip(&services, spawn_req(&parent, "write the tests", "tests/", "k-1", 0));
    assert!(resp.ok, "{:?}", resp.error);
    assert_eq!(resp.task.unwrap().task_id, child.task_id);
}

#[test]
fn admission_refuses_without_partial_reservation() {
    let (_store, services, parent) = setup("admission");

    // Generation fencing: a stale expected generation refuses BEFORE any
    // task is minted.
    let resp = roundtrip(&services, spawn_req(&parent, "x", "", "", 999));
    assert!(!resp.ok, "stale generation must refuse");
    assert!(resp.error.contains("fenced"), "{}", resp.error);

    // Write-scope conflict against live siblings, proven while capacity is
    // still available: child 0 declares "src/api.rs".
    let resp = roundtrip(
        &services,
        spawn_req(&parent, "scope holder", "src/api.rs", "cap-0", 0),
    );
    assert!(resp.ok, "{:?}", resp.error);
    let resp = roundtrip(
        &services,
        spawn_req(&parent, "conflicter", "src/", "conflict-1", 0),
    );
    assert!(!resp.ok, "overlapping write scope must refuse");
    assert!(resp.error.contains("write-set conflict"), "{}", resp.error);

    // Capacity ticket: 4 active children by default, the 5th refuses
    // (cap-0 from the scope step counts toward the ticket).
    for i in 1..4 {
        let resp = roundtrip(
            &services,
            spawn_req(&parent, &format!("child {i}"), "", &format!("cap-{i}"), 0),
        );
        assert!(resp.ok, "child {i}: {:?}", resp.error);
    }
    let resp = roundtrip(&services, spawn_req(&parent, "child 5", "", "cap-5", 0));
    assert!(!resp.ok, "5th child must exceed the ticket");
    assert!(resp.error.contains("capacity"), "{}", resp.error);

    // Refusals mint no tasks: only the 4 admitted children exist.
    let count: i64 = _store
        .with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM tasks WHERE parent_task_id = ?1",
                [&parent],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(count, 4, "refused admissions leave no reservation");
}

#[test]
fn park_resume_and_result_round_trip_through_the_surface() {
    let (_store, services, parent) = setup("park");

    let resp = roundtrip(&services, spawn_req(&parent, "child work", "", "pr-1", 0));
    assert!(resp.ok, "{:?}", resp.error);
    let child = resp.task.unwrap().task_id;

    // Bounded wait on a NON-terminal child returns the current state —
    // never a fabricated result.
    let resp = roundtrip(
        &services,
        pb::surface_request::Request::AgentResult(pb::AgentResultRequest {
            task_id: child.clone(),
            timeout_ms: 200,
        }),
    );
    assert!(resp.ok, "{:?}", resp.error);
    let view = resp.agent_result.unwrap();
    assert_eq!(view.state, "running");
    assert_eq!(view.parent_task_id, parent);
    assert!(view.summary.is_empty(), "no summary is fabricated before completion");

    // Park → Waiting(UserInput); resume → running again.
    let resp = roundtrip(
        &services,
        pb::surface_request::Request::ParkAgent(pb::ParkAgentCommand {
            task_id: child.clone(),
            reason: "hold for review".into(),
        }),
    );
    assert!(resp.ok, "{:?}", resp.error);
    assert_eq!(
        resp.task.as_ref().unwrap().state,
        pb::TaskStatus::Waiting as i32,
        "parked child waits"
    );

    let resp = roundtrip(
        &services,
        pb::surface_request::Request::ResumeAgent(pb::ResumeAgentCommand {
            task_id: child.clone(),
        }),
    );
    assert!(resp.ok, "{:?}", resp.error);
    assert_eq!(resp.task.as_ref().unwrap().state, pb::TaskStatus::Started as i32);

    // Terminal path: ReadyForReview → CompleteTask → result with summary.
    let processor = CommandProcessor::new(_store.clone());
    let task_id = modbit_domain::TaskId::parse(&child).unwrap();
    let tid = task_id;
    for payload in [
        CommandPayload::TaskReadyForReview { task_id: tid },
        CommandPayload::CompleteTask {
            task_id: tid,
            summary: "tests written and green".into(),
            host_verified: true,
        },
    ] {
        processor
            .execute(Command {
                command_id: uuid::Uuid::now_v7().to_string(),
                actor: Actor { actor_type: ActorType::System, actor_id: "t".into() },
                payload,
            })
            .unwrap();
    }
    let resp = roundtrip(
        &services,
        pb::surface_request::Request::AgentResult(pb::AgentResultRequest {
            task_id: child.clone(),
            timeout_ms: 2_000,
        }),
    );
    assert!(resp.ok, "{:?}", resp.error);
    let view = resp.agent_result.unwrap();
    assert_eq!(view.state, "completed");
    assert_eq!(view.summary, "tests written and green");
}

#[test]
fn terminal_parents_refuse_new_children() {
    let (_store, services, parent) = setup("terminal");

    let processor = CommandProcessor::new(_store.clone());
    let task_id = modbit_domain::TaskId::parse(&parent).unwrap();
    processor
        .execute(Command {
            command_id: uuid::Uuid::now_v7().to_string(),
            actor: Actor { actor_type: ActorType::User, actor_id: "t".into() },
            payload: CommandPayload::CancelTask { task_id, reason: "done".into() },
        })
        .unwrap();

    let resp = roundtrip(&services, spawn_req(&parent, "orphan", "", "t-1", 0));
    assert!(!resp.ok, "terminal parent must refuse children");
    assert!(resp.error.contains("not active"), "{}", resp.error);
}
