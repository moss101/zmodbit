//! Scheduler integration test (Future-tasks.md Phase 1 item 3): a task,
//! started through the REAL command processor, is run by THE scheduler end
//! to end — dedicated git worktree allocated from a real repository, tools
//! bound to that worktree, model events streamed from a real local HTTP
//! fixture through the production `HttpStreamTransport`, every Run/Turn/
//! RunStep transition written into the real event store, and the task
//! transitioned from the real outcome. This is the M2 loop minus a live
//! provider key.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use modbit_core_runtime::scheduler::{Scheduler, SchedulerConfig};
use modbit_domain::events::{Actor, ActorType};
use modbit_domain::{Command, CommandPayload, SessionId};
use modbit_event_store::{CommandProcessor, EventStore, Outcome};
use modbit_git::GitRepo;
use modbit_providers::gateway::Provider;
use modbit_providers::transport::{SecretBroker, TransportError};

struct FixtureBroker;
impl SecretBroker for FixtureBroker {
    fn credential(&self, _name: &str) -> Result<String, TransportError> {
        Ok("fixture-key".into())
    }
}

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "modbit-sched-{tag}-{}",
        uuid::Uuid::now_v7().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Real git fixture repository with one committed file.
fn git_fixture(tag: &str) -> PathBuf {
    let root = tempdir(tag);
    let repo = GitRepo::init(&root).expect("init fixture repo");
    repo.set_config("user.email", "fixture@modbit.test").unwrap();
    repo.set_config("user.name", "Modbit Fixture").unwrap();
    std::fs::write(root.join("NOTES.md"), "# Notes\nquantity must be positive\n").unwrap();
    repo.commit_all("fixture baseline").expect("baseline commit");
    root
}

/// HTTP fixture that scripts the two model turns: turn 1 asks fs.read on
/// NOTES.md; turn 2 completes with text.
async fn spawn_model_fixture() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let turn = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else { return };
            let turn = turn.clone();
            tokio::spawn(async move {
                handle_model(socket, turn).await;
            });
        }
    });
    addr
}

async fn handle_model(mut socket: TcpStream, turn: Arc<std::sync::atomic::AtomicUsize>) {
    // Drain the request head + body.
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = socket.read(&mut chunk).await.unwrap();
        assert!(n > 0);
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf).to_string();
        if let Some(end) = text.find("\r\n\r\n") {
            let head = &text[..end];
            let clen = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            let have = buf.len() - (end + 4);
            if have < clen {
                let mut rest = vec![0u8; clen - have];
                socket.read_exact(&mut rest).await.unwrap();
            }
            break;
        }
    }
    let n = turn.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    let frames: &[&str] = if n == 1 {
        &[
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"fs.read","arguments":"{\"path\":\"NOTES.md\"}"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            r#"data: [DONE]"#,
        ]
    } else {
        &[
            r#"data: {"choices":[{"delta":{"content":"notes read; validation noted"}}]}"#,
            r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            r#"data: [DONE]"#,
        ]
    };
    let mut payload = String::from(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
    );
    for f in frames {
        payload.push_str(f);
        payload.push_str("\n\n");
    }
    socket.write_all(payload.as_bytes()).await.unwrap();
}

fn create_task(processor: &CommandProcessor, store: &EventStore) -> (String, String) {
    processor
        .execute(Command {
            command_id: uuid::Uuid::now_v7().to_string(),
            actor: Actor { actor_type: ActorType::User, actor_id: "test".into() },
            payload: CommandPayload::CreateSession { display_name: "sched-test".into() },
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
            actor: Actor { actor_type: ActorType::User, actor_id: "test".into() },
            payload: CommandPayload::CreateTask {
                session_id: SessionId::parse(&sid).unwrap(),
                title: "read the notes".into(),
                prompt: "Read NOTES.md and summarize the validation rule.".into(),
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
    (sid, tid)
}

/// Waits for the poller-driven run to move the task projection, with a
/// bound so failures surface instead of hanging.
fn wait_for_state(store: &EventStore, task_id: &str, prefix: &str) -> String {
    for _ in 0..300 {
        let state: String = store
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT state FROM tasks WHERE task_id = ?1",
                    [task_id],
                    |r| r.get(0),
                )
                .unwrap_or_default()
            });
        if state.starts_with(prefix) {
            return state;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let state: String = store
        .with_conn(|conn| {
            conn.query_row("SELECT state FROM tasks WHERE task_id = ?1", [task_id], |r| r.get(0))
                .unwrap_or_default()
        });
    panic!("task never reached {prefix:?}, still {state:?}");
}

// multi_thread: the sync wait loop blocks the test thread while the
// fixture server + scheduler must keep making progress on worker threads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduler_runs_task_end_to_end_into_worktree_and_store() {
    let repo_root = git_fixture("repo");
    let worktree_root = tempdir("worktrees");
    let db_dir = tempdir("db");
    let store = Arc::new(EventStore::open(&db_dir.join("core.db")).unwrap());
    let processor = CommandProcessor::new(store.clone());
    let fixture_addr = spawn_model_fixture().await;

    // Session + task + Queue + Start through the REAL command processor
    // (the same command sequence the surface protocol drives).
    let (_sid, task_id) = create_task(&processor, &store);
    let task_id = modbit_domain::TaskId::parse(&task_id).unwrap();
    let task_id_str = task_id.to_string();
    for payload in [
        CommandPayload::QueueTask { task_id },
        CommandPayload::StartTask { task_id },
    ] {
        let outcome = processor
            .execute(Command {
                command_id: uuid::Uuid::now_v7().to_string(),
                actor: Actor { actor_type: ActorType::User, actor_id: "test".into() },
                payload,
            })
            .unwrap();
        assert!(matches!(outcome, Outcome::Applied { .. }), "queue+start must apply");
    }

    // THE scheduler runs it (single entry; the poller uses the same path).
    let scheduler = Scheduler::spawn(
        store.clone(),
        SchedulerConfig {
            provider: Provider::OpenAi,
            model: "fixture-model".into(),
            base_url: Some(format!("http://{fixture_addr}")),
            broker: Arc::new(FixtureBroker),
            repo_root: Some(repo_root.clone()),
            worktree_root: Some(worktree_root.clone()),
            request_timeout: Duration::from_secs(5),
            max_turns: 4,
        },
    );
    // The poller picks up task_started and runs the task (production path).
    let state = wait_for_state(&store, &task_id_str, "ready_for_review");
    assert_eq!(state, "ready_for_review", "completed run moves task to review");

    // 2. A dedicated worktree exists for the task.
    let worktree = worktree_root.join(&task_id_str);
    assert!(worktree.join(".git").exists(), "worktree allocated at {worktree:?}");
    assert!(worktree.join("NOTES.md").exists(), "fixture file present in worktree");

    // 3. Run/Turn/RunStep events are durable with clean aggregate streams.
    let (runs_completed, turns, tool_steps): (i64, i64, i64) = store.with_conn(|conn| {
        let done = conn.query_row(
            "SELECT COUNT(*) FROM events WHERE aggregate_type='run' AND event_type='run_completed'", [],
            |r| r.get(0)).unwrap();
        let turns = conn.query_row(
            "SELECT COUNT(*) FROM events WHERE aggregate_type='turn' AND event_type='turn_prepared'", [],
            |r| r.get(0)).unwrap();
        let steps = conn.query_row(
            "SELECT COUNT(*) FROM events WHERE aggregate_type='run_step' AND event_type='run_step_prepared'", [],
            |r| r.get(0)).unwrap();
        Ok::<_, rusqlite::Error>((done, turns, steps))
    }).unwrap();
    assert_eq!(runs_completed, 1, "exactly one run_completed");
    let run_ids: Vec<String> = store.with_conn(|conn| {
        let mut stmt = conn
            .prepare("SELECT DISTINCT aggregate_id FROM events WHERE aggregate_type='run'")
            .unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    });
    assert_eq!(run_ids.len(), 1, "exactly one run aggregate");
    store.verify_stream(&run_ids[0]).expect("run stream integrity");
    assert!(turns >= 2, "two model turns recorded, got {turns}");
    assert!(tool_steps >= 3, "model-invoke + tool-call steps recorded, got {tool_steps}");

    // 5. Idempotency: a direct re-run of the same task_started does NOT
    // create a second run.
    scheduler.run_task(&task_id_str).expect("idempotent re-run");
    let run_count: i64 = store.with_conn(|conn| {
        conn.query_row(
            "SELECT COUNT(DISTINCT aggregate_id) FROM events WHERE aggregate_type='run'", [],
            |r| r.get(0)).unwrap()
    });
    assert_eq!(run_count, 1, "no duplicate run for the same task");

    // Cleanup worktrees so tempdirs vanish on macOS TMPDIR reaping.
    let _ = GitRepo::open(&repo_root).map(|r| r.worktree_remove(&worktree));
}

/// A provider outage parks the task in Waiting(Provider) instead of failing
/// it (docs/13; provider issues are outages, not task defects).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_outage_parks_task_in_waiting() {
    let repo_root = git_fixture("repo2");
    let worktree_root = tempdir("worktrees2");
    let db_dir = tempdir("db2");
    let store = Arc::new(EventStore::open(&db_dir.join("core.db")).unwrap());
    let processor = CommandProcessor::new(store.clone());
    let (_sid, task_id) = create_task(&processor, &store);
    let task_id = modbit_domain::TaskId::parse(&task_id).unwrap();
    let task_id_str = task_id.to_string();
    for payload in [
        CommandPayload::QueueTask { task_id },
        CommandPayload::StartTask { task_id },
    ] {
        processor
            .execute(Command {
                command_id: uuid::Uuid::now_v7().to_string(),
                actor: Actor { actor_type: ActorType::User, actor_id: "test".into() },
                payload,
            })
            .unwrap();
    }

    // Nothing listens on this port: the transport gets a connection refused.
    let dead_addr = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let a = l.local_addr().unwrap();
        drop(l);
        a
    };
    let _scheduler = Scheduler::spawn(
        store.clone(),
        SchedulerConfig {
            provider: Provider::OpenAi,
            model: "fixture-model".into(),
            base_url: Some(format!("http://{dead_addr}")),
            broker: Arc::new(FixtureBroker),
            repo_root: Some(repo_root.clone()),
            worktree_root: Some(worktree_root.clone()),
            request_timeout: Duration::from_secs(5),
            max_turns: 2,
        },
    );
    // The poller runs the task; the transport cannot connect, so the task
    // parks in Waiting(Provider) rather than failing.
    let state = wait_for_state(&store, &task_id_str, "waiting");
    assert!(state.starts_with("waiting"), "task parked, got {state:?}");
    let waiting_events: i64 = store.with_conn(|conn| {
        conn.query_row(
            "SELECT COUNT(*) FROM events WHERE aggregate_type='task' AND event_type='task_waiting'", [],
            |r| r.get(0)).unwrap()
    });
    assert_eq!(waiting_events, 1, "one durable waiting event");
}
