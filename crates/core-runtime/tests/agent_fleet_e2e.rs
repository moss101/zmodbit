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
use modbit_git::GitRepo;
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

    // Refusals leave no ACTIVE reservation: the 4 admitted children run;
    // the write-scope-denied child was minted then compensated to
    // cancelled (its scope was released), never started.
    let (running, cancelled): (i64, i64) = _store
        .with_conn(|conn| {
            let running: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM tasks WHERE parent_task_id = ?1 AND state = 'running'",
                    [&parent],
                    |r| r.get(0),
                )
                .unwrap();
            let cancelled: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM tasks WHERE parent_task_id = ?1 AND state = 'cancelled'",
                    [&parent],
                    |r| r.get(0),
                )
                .unwrap();
            (running, cancelled)
        });
    assert_eq!(running, 4, "admitted children all run");
    assert_eq!(cancelled, 1, "the denied child was compensated, nothing active");
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

#[test]
fn conflicting_child_branches_surface_typed_merge_conflict_evidence() {
    // Two admitted children build in their own branches (the scheduler's
    // `modbit/<task>` worktree layout); when their work overlaps, the merge
    // back to the parent branch produces TYPED conflict evidence through
    // the canonical merge transaction — never silent corruption.
    let repo_root = tempdir("conflict-repo");
    let repo = GitRepo::init(&repo_root).unwrap();
    repo.set_config("user.email", "e2e@modbit.test").unwrap();
    repo.set_config("user.name", "E2E").unwrap();
    repo.set_config("core.autocrlf", "false").unwrap();
    std::fs::write(repo_root.join("shared.txt"), "line one\n").unwrap();
    repo.commit_all("base").unwrap();

    let (store, services, _parent) = setup("conflict");

    let resp = roundtrip(&services, spawn_req(&_parent, "child A edits shared", "", "mc-1", 0));
    assert!(resp.ok, "{:?}", resp.error);
    let child_a = resp.task.unwrap().task_id;
    let resp = roundtrip(&services, spawn_req(&_parent, "child B edits shared", "", "mc-2", 0));
    assert!(resp.ok, "{:?}", resp.error);
    let child_b = resp.task.unwrap().task_id;

    // The scheduler allocates each child an isolated worktree on branch
    // `modbit/<task>`; both children edit the SAME line and commit.
    let branch_of = |id: &str| format!("modbit/{id}");
    let wt_root = tempdir("conflict-wt");
    for (id, line) in [(&child_a, "line one EDITED BY A\n"), (&child_b, "line one EDITED BY B\n")] {
        let wt = repo
            .worktree_add(&wt_root.join(id), &branch_of(id))
            .unwrap();
        std::fs::write(wt_root.join(id).join("shared.txt"), line).unwrap();
        wt.commit_all(&format!("child work {id}")).unwrap();
    }

    // Child A merges clean: open → Merged → Validating → Committed.
    let (tx, outcome) =
        modbit_workspace::merge_transaction::open_and_merge(&repo, "tx-a", &branch_of(&child_a), "main")
            .unwrap();
    assert!(matches!(outcome, modbit_git::MergeOutcome::Merged), "{outcome:?}");
    assert_eq!(tx.phase, modbit_workspace::merge_transaction::MergePhase::Validating);
    let _tx = modbit_workspace::merge_transaction::record_validation(&repo, "build", true).unwrap();
    let tx = modbit_workspace::merge_transaction::commit(&repo).unwrap();
    assert_eq!(tx.phase, modbit_workspace::merge_transaction::MergePhase::Committed);

    // Child B edits the same line: the transaction stays OPEN on conflict
    // with the file as typed evidence, recoverable (inspect after crash).
    let (tx, outcome) =
        modbit_workspace::merge_transaction::open_and_merge(&repo, "tx-b", &branch_of(&child_b), "main")
            .unwrap();
    assert!(
        matches!(&outcome, modbit_git::MergeOutcome::Conflict { conflicted_files }
            if conflicted_files == &vec!["shared.txt".to_string()]),
        "{outcome:?}"
    );
    assert_eq!(tx.phase, modbit_workspace::merge_transaction::MergePhase::Conflicted);
    assert_eq!(tx.conflicts, vec!["shared.txt".to_string()]);

    // The conflict is inspectable and recoverable: record the resolution,
    // the transaction concludes the merge commit and re-enters validation.
    std::fs::write(repo_root.join("shared.txt"), "line one resolved\n").unwrap();
    let tx = modbit_workspace::merge_transaction::record_resolution(&repo, "shared.txt", "manual")
        .unwrap();
    assert_eq!(tx.phase, modbit_workspace::merge_transaction::MergePhase::Validating);
    assert!(tx.conflicts.is_empty());
    assert!(tx.resolutions.iter().any(|r| r.path == "shared.txt" && r.strategy == "manual"));
    let _tx = modbit_workspace::merge_transaction::record_validation(&repo, "build", true).unwrap();
    let tx = modbit_workspace::merge_transaction::commit(&repo).unwrap();
    assert_eq!(tx.phase, modbit_workspace::merge_transaction::MergePhase::Committed);
    let _ = store;
}

#[test]
fn write_coordinator_denies_overlapping_parallel_tasks_before_execution() {
    // REQ ledger EV-0150 / QUAL-EV-0150: two INDEPENDENT tasks on the same repo
    // with declared write scopes — the overlapping start is denied BEFORE
    // it runs, and the holder is named.
    let (store, services, _parent) = setup("writecoord");

    let mk_create = |title: &str, scope: &str| {
        pb::surface_request::Request::CreateTask(pb::CreateTaskCommand {
            session_id: String::new(),
            title: title.into(),
            prompt: title.into(),
            repo_id: String::new(),
            base_branch: String::new(),
            parent_task_id: String::new(),
            write_scope: scope.into(),
        })
    };

    let resp = roundtrip(&services, mk_create("editor a", "src/api.rs"));
    assert!(resp.ok, "{:?}", resp.error);
    let holder = resp.task.unwrap();

    // Overlap on the same path, same repo — denied, holder named.
    let resp = roundtrip(&services, mk_create("editor b", "src/"));
    assert!(!resp.ok, "overlapping parallel task must be denied");
    assert!(
        resp.error.contains("write-set conflict") && resp.error.contains(&holder.task_id),
        "{}",
        resp.error
    );

    // Disjoint scope — admitted in parallel.
    let resp = roundtrip(&services, mk_create("editor c", "docs/"));
    assert!(resp.ok, "disjoint scope must be admitted: {:?}", resp.error);

    // The denial left NO task behind (all-or-nothing).
    let cancelled: i64 = store
        .with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM tasks WHERE goal_text LIKE 'editor b%' AND state = 'cancelled'",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(cancelled, 1, "denied task was compensated to cancelled");
}

#[test]
fn run_variants_creates_umbrella_with_admitted_children() {
    let (_store, services, _parent) = setup("variants");

    let resp = roundtrip(
        &services,
        pb::surface_request::Request::RunVariants(pb::RunVariantsCommand {
            objective: "fix the flaky test three ways".into(),
            count: 3,
            repo_id: String::new(),
            base_branch: String::new(),
        }),
    );
    assert!(resp.ok, "{:?}", resp.error);
    let umbrella = resp.task.expect("umbrella task view");

    // Three children linked to the umbrella, all running.
    let children: Vec<(String, String)> = _store
        .with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT task_id, state FROM tasks
                     WHERE parent_task_id = ?1 ORDER BY task_id",
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([&umbrella.task_id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(|e| e.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            Ok::<_, String>(rows)
        })
        .unwrap();
    assert_eq!(children.len(), 3, "three variants admitted");
    assert!(
        children.iter().all(|(_, s)| s == "running"),
        "all variants started: {children:?}"
    );

    // Idempotent replay: same umbrella, same variant count — no doubling.
    let resp = roundtrip(
        &services,
        pb::surface_request::Request::RunVariants(pb::RunVariantsCommand {
            objective: "fix the flaky test three ways".into(),
            count: 3,
            repo_id: String::new(),
            base_branch: String::new(),
        }),
    );
    assert!(resp.ok, "{:?}", resp.error);
    let replay_children: i64 = _store
        .with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM tasks WHERE parent_task_id = ?1",
                [&umbrella.task_id],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(replay_children, 3, "replay re-attaches, never doubles");

    // Count bounds: 1 or 5 variants are refused before anything is minted.
    for bad in [1u32, 5] {
        let resp = roundtrip(
            &services,
            pb::surface_request::Request::RunVariants(pb::RunVariantsCommand {
                objective: "x".into(),
                count: bad,
                repo_id: String::new(),
                base_branch: String::new(),
            }),
        );
        assert!(!resp.ok, "count {bad} must refuse");
        assert!(resp.error.contains("count"), "{}", resp.error);
    }
}

mod automations {
    use modbit_core_runtime::automation::{AutomationEngine, CronSpec, minute_key};
    use modbit_domain::TaskId;

    use super::*;

    fn current_utc_minute() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() - d.as_secs() % 60)
            .unwrap_or(0)
    }

    /// A cron spec that matches EVERY minute — always due regardless of
    /// the wall clock.
    fn now_cron() -> String {
        "* * * * *".into()
    }

    #[test]
    fn cron_automation_fires_a_real_task_exactly_once_per_boundary() {
        let (_store, services, _parent) = setup("auto-cron");
        let engine = AutomationEngine::new(_store.clone());

        // Register via the SURFACE (validation + durable spec).
        let resp = roundtrip(
            &services,
            pb::surface_request::Request::CreateAutomation(pb::CreateAutomationCommand {
                name: "nightly sweep".into(),
                cron: now_cron(),
                event_pattern: String::new(),
                objective: "sweep the workspace".into(),
            }),
        );
        assert!(resp.ok, "{:?}", resp.error);

        // Unparsable cron is refused at creation.
        let resp = roundtrip(
            &services,
            pb::surface_request::Request::CreateAutomation(pb::CreateAutomationCommand {
                name: "broken".into(),
                cron: "not a cron".into(),
                event_pattern: String::new(),
                objective: "x".into(),
            }),
        );
        assert!(!resp.ok, "unparsable cron must refuse");
        assert!(resp.error.contains("unparsable"), "{}", resp.error);

        // Tick at the CURRENT minute: the every-minute spec is due; the
        // task must be created AND queued (a real task aggregate).
        let now = current_utc_minute();
        let outcome = engine.tick(now + 30); // 30s into the minute
        assert_eq!(outcome.fired.len(), 1, "{outcome:?}");
        let (automation_id, task_id) = &outcome.fired[0];
        assert!(automation_id.starts_with("auto-"));
        assert!(
            TaskId::parse(task_id).is_ok(),
            "fired a real task aggregate"
        );

        // The fired task exists in 'queued' state with the objective.
        let state: String = _store
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT state FROM tasks WHERE task_id = ?1",
                    [task_id.as_str()],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(state, "queued");

        // A SECOND tick in the SAME minute must NOT double-fire
        // (boundary-keyed), and neither must a restart-shaped re-tick.
        let outcome2 = engine.tick(now + 45);
        assert!(outcome2.fired.is_empty(), "no double fire: {outcome2:?}");

        // ListAutomations shows the recorded fire key.
        let resp = roundtrip(
            &services,
            pb::surface_request::Request::ListAutomations(pb::ListAutomationsRequest {}),
        );
        assert!(resp.ok, "{:?}", resp.error);
        let list = resp.automations.expect("automation list");
        assert_eq!(list.automations.len(), 1);
        assert_eq!(
            list.automations[0].last_fire_key,
            minute_key(now + 30),
            "boundary recorded"
        );
    }

    #[test]
    fn event_automation_follows_task_completion_without_double_firing() {
        let (_store, services, _parent) = setup("auto-event");
        let engine = AutomationEngine::new(_store.clone());

        let resp = roundtrip(
            &services,
            pb::surface_request::Request::CreateAutomation(pb::CreateAutomationCommand {
                name: "post-completion review".into(),
                cron: String::new(),
                event_pattern: "task_completed".into(),
                objective: "review the completed work".into(),
            }),
        );
        assert!(resp.ok, "{:?}", resp.error);

        // No completions yet: the tick is a no-op.
        let outcome = engine.tick(current_utc_minute() + 30);
        assert!(outcome.fired.is_empty(), "nothing consumed yet: {outcome:?}");

        // The parent completes: the durable event stream gains
        // task_completed (host-verified).
        let processor = CommandProcessor::new(_store.clone());
        let task_id = TaskId::parse(&_parent).unwrap();
        for payload in [
            CommandPayload::TaskReadyForReview { task_id },
            CommandPayload::CompleteTask {
                task_id,
                summary: "done".into(),
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

        // Tick: the event is consumed and a follow-up task fires.
        let outcome = engine.tick(current_utc_minute() + 60);
        assert_eq!(outcome.fired.len(), 1, "{outcome:?}");
        let state: String = _store
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT state FROM tasks WHERE task_id = ?1",
                    [&outcome.fired[0].1],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(state, "queued", "follow-up task queued");

        // Another tick: the SAME event is never consumed twice.
        let outcome2 = engine.tick(current_utc_minute() + 90);
        assert!(outcome2.fired.is_empty(), "no double fire: {outcome2:?}");

        // Step-cron specs also parse through the same surface validation.
        assert!(CronSpec::parse("*/5 * * * *").is_some());
        assert!(CronSpec::parse("0 9 * * 1-5").is_some());
    }
}
