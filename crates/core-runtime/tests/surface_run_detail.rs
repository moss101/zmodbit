//! Surface protocol slice (Phase 1 item 5): SteerTask / PauseTask / StopTask
//! commands, GetRunDetail from the durable run-plane aggregates, and GetDiff
//! bound to the task's worktree — exercised through the production dispatch
//! (`CoreServices::handle`) against a real store and a real git worktree.

use std::path::PathBuf;
use std::sync::Arc;

use prost::Message;

use modbit_core_runtime::CoreServices;
use modbit_domain::events::{Actor, ActorType};
use modbit_domain::{Command, CommandPayload};
use modbit_event_store::{CommandProcessor, EventStore};
use modbit_git::GitRepo;
use modbit_protocol::modbit::protocol::v1 as pb;

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("modbit-surface-{tag}-{}", uuid::Uuid::now_v7().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Explicit worktree source per test — no process-env mutation (tests run
/// in parallel inside one binary).
struct TestSource {
    repo_root: PathBuf,
    worktree_root: PathBuf,
    base_revision: String,
}

impl modbit_core_runtime::scheduler::WorktreeSource for TestSource {
    fn layout(&self, task_id: &str) -> Option<modbit_core_runtime::scheduler::WorktreeLayout> {
        Some(modbit_core_runtime::scheduler::WorktreeLayout {
            worktree: self.worktree_root.join(task_id),
            branch: format!("modbit/{}", &task_id[..12]),
            base_revision: self.base_revision.clone(),
        })
    }

    fn repo_root(&self) -> Option<PathBuf> {
        Some(self.repo_root.clone())
    }
}

fn setup(tag: &str) -> (PathBuf, PathBuf, Arc<EventStore>, CoreServices, String) {
    let repo_root = tempdir(tag);
    let repo = GitRepo::init(&repo_root).unwrap();
    repo.set_config("user.email", "t@modbit.test").unwrap();
    repo.set_config("user.name", "T").unwrap();
    std::fs::write(repo_root.join("f.txt"), "one\n").unwrap();
    repo.commit_all("base").unwrap();

    let db = tempdir(&format!("{tag}-db"));
    let store = Arc::new(EventStore::open(&db.join("core.db")).unwrap());
    let services = CoreServices::new(store.clone());

    // Session + task + queue + start through the real command processor.
    let processor = CommandProcessor::new(store.clone());
    processor
        .execute(Command {
            command_id: uuid::Uuid::now_v7().to_string(),
            actor: Actor { actor_type: ActorType::User, actor_id: "t".into() },
            payload: CommandPayload::CreateSession { display_name: "s".into() },
        })
        .unwrap();
    let sid: String = store
        .with_conn(|conn| {
            conn.query_row(
                "SELECT aggregate_id FROM events WHERE aggregate_type='session' ORDER BY rowid DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    processor
        .execute(Command {
            command_id: uuid::Uuid::now_v7().to_string(),
            actor: Actor { actor_type: ActorType::User, actor_id: "t".into() },
            payload: CommandPayload::CreateTask {
                session_id: modbit_domain::SessionId::parse(&sid).unwrap(),
                title: "task".into(),
                prompt: "prompt".into(),
            },
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
    for payload in [
        CommandPayload::QueueTask { task_id },
        CommandPayload::StartTask { task_id },
    ] {
        processor
            .execute(Command {
                command_id: uuid::Uuid::now_v7().to_string(),
                actor: Actor { actor_type: ActorType::User, actor_id: "t".into() },
                payload,
            })
            .unwrap();
    }

    // Allocate the task worktree exactly like the scheduler does.
    let worktree_root = tempdir(&format!("{tag}-wt"));
    let branch = format!("modbit/{}", &tid[..12]);
    repo.worktree_add(&worktree_root.join(&tid), &branch).unwrap();
    let base_revision = repo.head().unwrap();

    let services = services.with_task_worktrees(Arc::new(TestSource {
        repo_root: repo_root.clone(),
        worktree_root: worktree_root.clone(),
        base_revision,
    }));
    (repo_root, worktree_root, store, services, tid)
}

fn roundtrip(
    services: &CoreServices,
    request: pb::surface_request::Request,
) -> pb::SurfaceResponse {
    let bytes = pb::SurfaceRequest { request: Some(request) }.encode_to_vec();
    let response = services.handle(&bytes);
    pb::SurfaceResponse::decode(response.as_slice()).unwrap()
}

#[test]
fn steer_pause_stop_transition_the_task_through_the_surface() {
    let (_repo, _wt, _store, services, tid) = setup("steer");

    let resp = roundtrip(
        &services,
        pb::surface_request::Request::SteerTask(pb::SteerTaskCommand {
            task_id: tid.clone(),
            note: "also add docs".into(),
        }),
    );
    assert!(resp.ok, "{:?}", resp.error);

    let resp = roundtrip(
        &services,
        pb::surface_request::Request::PauseTask(pb::PauseTaskCommand { task_id: tid.clone() }),
    );
    assert!(resp.ok, "{:?}", resp.error);
    let state = _store
        .with_conn(|conn| {
            conn.query_row("SELECT state FROM tasks WHERE task_id=?1", [&tid], |r| {
                r.get::<_, String>(0)
            })
        })
        .unwrap();
    assert!(state.starts_with("waiting"), "paused → waiting, got {state}");

    let resp = roundtrip(
        &services,
        pb::surface_request::Request::StopTask(pb::StopTaskCommand {
            task_id: tid.clone(),
            reason: String::new(),
        }),
    );
    assert!(resp.ok, "{:?}", resp.error);
    let state = _store
        .with_conn(|conn| {
            conn.query_row("SELECT state FROM tasks WHERE task_id=?1", [&tid], |r| {
                r.get::<_, String>(0)
            })
        })
        .unwrap();
    assert!(state.starts_with("cancelled"), "stopped → cancelled, got {state}");
}

#[test]
fn run_detail_assembles_run_turns_and_steps_diff_reads_worktree() {
    let (_repo, worktree_root, store, services, tid) = setup("detail");

    // Append a real run plane through the store, exactly as the scheduler
    // observer writes it.
    let run_id = modbit_domain::RunId::generate();
    let session_id = modbit_domain::SessionId::generate();
    let task_id = modbit_domain::TaskId::parse(&tid).unwrap();
    let turn_id = modbit_domain::TurnId::generate();
    let step_id = modbit_domain::RunStepId::generate();
    let envelopes = [
        (modbit_domain::events::AggregateType::Run, run_id.to_string(),
         modbit_domain::DomainEvent::RunStarted { task_id, attempt: 1 }),
        (modbit_domain::events::AggregateType::Turn, turn_id.to_string(),
         modbit_domain::DomainEvent::TurnPrepared { run_id, ordinal: 1 }),
        (modbit_domain::events::AggregateType::RunStep, step_id.to_string(),
         modbit_domain::DomainEvent::RunStepPrepared {
             turn_id,
             step_type: modbit_domain::events::StepType::ModelInvoke,
             ordinal: 1,
         }),
    ];
    let mut batch = Vec::new();
    for (aggregate, aggregate_id, payload) in envelopes {
        let mut e = modbit_domain::EventEnvelope {
            event_id: uuid::Uuid::now_v7().to_string(),
            session_id,
            task_id: Some(task_id),
            run_id: None,
            turn_id: None,
            step_id: None,
            aggregate_type: aggregate,
            aggregate_id: aggregate_id.clone(),
            sequence: 0,
            event_type: modbit_domain::EventEnvelope::event_type_of(&payload).to_string(),
            schema_version: modbit_domain::SCHEMA_VERSION,
            occurred_at: "2026-09-05T00:00:00.000Z".into(),
            actor: Actor { actor_type: ActorType::System, actor_id: "t".into() },
            causation_id: None,
            correlation_id: None,
            payload,
            payload_object_hash: None,
            integrity_hash: String::new(),
        };
        e.sequence = 1;
        e.seal();
        batch.push(e);
    }
    store.append(&mut batch).unwrap();

    let resp = roundtrip(
        &services,
        pb::surface_request::Request::GetRunDetail(pb::GetRunDetailRequest { task_id: tid.clone() }),
    );
    assert!(resp.ok, "{:?}", resp.error);
    let detail = resp.run_detail.unwrap();
    assert_eq!(detail.task_id, tid);
    assert_eq!(detail.run_state, "running");
    assert_eq!(detail.turns.len(), 1);
    assert_eq!(detail.turns[0].steps.len(), 1);
    assert_eq!(detail.turns[0].steps[0].step_type, "model_invoke");

    // Diff: change a file in the worktree; GetDiff must show it.
    std::fs::write(worktree_root.join(&tid).join("f.txt"), "one\ntwo\n").unwrap();
    let resp = roundtrip(
        &services,
        pb::surface_request::Request::GetDiff(pb::GetDiffRequest { task_id: tid.clone() }),
    );
    assert!(resp.ok, "{:?}", resp.error);
    let diff = resp.diff.unwrap();
    assert_eq!(diff.task_id, tid);
    assert!(diff.files.iter().any(|f| f.path == "f.txt"), "{:?}", diff.files);

    // An agent that COMMITS its work must not empty the diff: GetDiff is
    // bound to the task's base revision, not the worktree's moved HEAD
    // (observed live: a coding agent naturally commits).
    let worktree = worktree_root.join(&tid);
    let repo = GitRepo::open(&worktree).unwrap();
    repo.set_config("user.email", "agent@modbit.test").unwrap();
    repo.set_config("user.name", "Agent").unwrap();
    std::fs::write(worktree.join("f.txt"), "one\ntwo\nthree\n").unwrap();
    std::fs::write(worktree.join("extra.txt"), "new file\n").unwrap();
    repo.commit_all("agent commits its work").unwrap();
    let resp = roundtrip(
        &services,
        pb::surface_request::Request::GetDiff(pb::GetDiffRequest { task_id: tid.clone() }),
    );
    assert!(resp.ok, "{:?}", resp.error);
    let diff = resp.diff.unwrap();
    assert!(
        diff.files.iter().any(|f| f.path == "f.txt"),
        "committed work stays in the diff: {:?}",
        diff.files
    );
}
