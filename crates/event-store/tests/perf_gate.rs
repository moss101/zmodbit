//! Performance regression gates (M10.4): deterministic, always-on
//! throughput/latency floors for the durable core paths, complementary
//! to the M3.9 retrieval benchmark. The floors are set ~10x under
//! observed hardware headroom so CI variance never flips them — a
//! regression that matters (10x slowdowns) fails the build.
//!
//! Gates:
//! 1. Event append throughput ≥ 2,000 events/s (single store, batches).
//! 2. Fleet snapshot (tasks projection read) p95 < 50 ms at 500 tasks.
//! 3. Write-scope admission check < 5 ms at 200 active scopes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use modbit_domain::events::{Actor, ActorType};
use modbit_domain::{Command, CommandPayload};
use modbit_event_store::{CommandProcessor, EventStore};

fn tempdir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "modbit-perf-{tag}-{}",
        uuid::Uuid::now_v7().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn bench_setup(tag: &str) -> (Arc<EventStore>, CommandProcessor, String) {
    let db = tempdir(tag);
    let store = Arc::new(EventStore::open(&db.join("perf.db")).unwrap());
    let processor = CommandProcessor::new(store.clone());
    processor
        .execute(Command {
            command_id: uuid::Uuid::now_v7().to_string(),
            actor: Actor { actor_type: ActorType::User, actor_id: "perf".into() },
            payload: CommandPayload::CreateSession { display_name: "perf".into() },
        })
        .unwrap();
    let sid: String = store
        .with_conn(|conn| {
            conn.query_row(
                "SELECT aggregate_id FROM events WHERE aggregate_type='session' LIMIT 1",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    (store, processor, sid)
}

#[test]
fn event_append_throughput_holds_the_floor() {
    let (store, processor, sid) = bench_setup("append");

    // 2,000 tasks created+queued in batches of full command processing.
    let n = 2_000;
    let start = Instant::now();
    for i in 0..n {
        let outcome = processor
            .execute(Command {
                command_id: uuid::Uuid::now_v7().to_string(),
                actor: Actor { actor_type: ActorType::User, actor_id: "perf".into() },
                payload: CommandPayload::CreateTask {
                    session_id: modbit_domain::SessionId::parse(&sid).unwrap(),
                    title: format!("task {i}"),
                    prompt: format!("prompt {i}"),
                    repo_id: None,
                    base_branch: None,
                    parent_task_id: None,
                },
            })
            .unwrap();
        assert!(matches!(outcome, modbit_event_store::Outcome::Applied { .. }));
    }
    let elapsed = start.elapsed();
    let per_sec = n as f64 / elapsed.as_secs_f64();
    assert!(
        per_sec >= 2_000.0,
        "event append throughput {per_sec:.0}/s below the 2,000/s floor ({elapsed:?} for {n})"
    );

    // The durable truth matches the count.
    let tasks: i64 = store
        .with_conn(|conn| conn.query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(tasks, n, "all appended tasks durable");
}

#[test]
fn fleet_snapshot_p95_stays_under_the_bound() {
    let (store, processor, sid) = bench_setup("fleet");
    for i in 0..500 {
        processor
            .execute(Command {
                command_id: uuid::Uuid::now_v7().to_string(),
                actor: Actor { actor_type: ActorType::User, actor_id: "perf".into() },
                payload: CommandPayload::CreateTask {
                    session_id: modbit_domain::SessionId::parse(&sid).unwrap(),
                    title: format!("fleet task {i}"),
                    prompt: "p".into(),
                    repo_id: None,
                    base_branch: None,
                    parent_task_id: None,
                },
            })
            .unwrap();
    }
    // Sample the tasks projection 100 times; p95 under 50 ms.
    let mut samples: Vec<Duration> = Vec::new();
    for _ in 0..100 {
        let start = Instant::now();
        let count: i64 = store
            .with_conn(|conn| conn.query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0)))
            .unwrap();
        assert_eq!(count, 500);
        samples.push(start.elapsed());
    }
    samples.sort();
    let p95 = samples[94];
    assert!(
        p95 < Duration::from_millis(50),
        "fleet snapshot p95 {p95:?} exceeds 50ms"
    );
}

#[test]
fn write_scope_admission_stays_fast_at_scale() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    modbit_event_store::migrations::migrate(&conn).unwrap();
    conn.execute(
        "INSERT INTO sessions (session_id, state, generation, created_at, updated_at, last_event_sequence)
         VALUES ('s', 'active', 1, '2026-09-12T00:00:00.000Z', '2026-09-12T00:00:00.000Z', 1)",
        [],
    )
    .unwrap();
    // 200 active holders, one scope each.
    for i in 0..200 {
        conn.execute(
            "INSERT INTO tasks (task_id, session_id, goal_text, state, generation, created_at)
             VALUES (?1, 's', 't', 'running', 1, '2026-09-12T00:00:00.000Z')",
            [format!("t{i}")],
        )
        .unwrap();
        modbit_event_store::write_scopes::acquire(&conn, &format!("t{i}"), "repo", &format!("dir{i}/"))
            .unwrap();
    }
    // 200 sequential acquisitions (each scans all held scopes).
    let start = Instant::now();
    for i in 200..400 {
        conn.execute(
            "INSERT INTO tasks (task_id, session_id, goal_text, state, generation, created_at)
             VALUES (?1, 's', 't', 'running', 1, '2026-09-12T00:00:00.000Z')",
            [format!("t{i}")],
        )
        .unwrap();
        let outcome =
            modbit_event_store::write_scopes::acquire(&conn, &format!("t{i}"), "repo", &format!("fresh{i}/"))
                .unwrap();
        assert_eq!(outcome, modbit_event_store::write_scopes::AcquireOutcome::Acquired);
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "200 admissions took {elapsed:?} — the scope scan degraded"
    );
}
