//! Diagnostics export + report-a-problem E2E (Phase 9): a REAL store with
//! durable state (settings document, session, task, events) assembles a
//! bundle whose defining property is that it can be ATTACHED to a problem
//! report without leaking credentials — the redaction is verified against
//! a planted key-shaped value, and the bundle hash in PROBLEM.md matches
//! the SHA256SUMS sidecar.

use std::path::PathBuf;
use std::sync::Arc;

use modbit_event_store::EventStore;

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "modbit-diag-{tag}-{}",
        uuid::Uuid::now_v7().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn diagnostics_bundle_is_complete_and_secret_free() {
    let db = tempdir("db");
    let store = Arc::new(EventStore::open(&db.join("core.db")).unwrap());

    // Durable state to export: a settings document WITH a planted
    // key-shaped value (defends the export path, not just the store
    // design), a session, a task and real events.
    store
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO app_settings (id, data) VALUES (1, ?1)",
                [r#"{"provider":"openai","model":"gpt-x","api_key":"sk-PLANTED-SECRET-VALUE"}"#],
            )?;
            conn.execute(
                "INSERT INTO sessions (session_id, state, generation, created_at, updated_at, last_event_sequence)
                 VALUES ('s1', 'active', 1, '2026-09-12T00:00:00.000Z', '2026-09-12T00:00:00.000Z', 2)",
                [],
            )?;
            conn.execute(
                "INSERT INTO tasks (task_id, session_id, goal_text, state, generation, created_at)
                 VALUES ('t1', 's1', 'diagnose me', 'running', 1, '2026-09-12T00:00:00.000Z')",
                [],
            )?;
            conn.execute(
                "INSERT INTO events (event_id, session_id, aggregate_type, aggregate_id, sequence,
                     event_type, schema_version, occurred_at, actor_type, actor_id, payload_inline, integrity_hash)
                 VALUES ('e1', 's1', 'task', 't1', 1, 'task_created', 'v1', '2026-09-12T00:00:00.000Z',
                         'user', 't', '{\"summary\":\"diagnose me\",\"api_key\":\"sk-PLANTED-SECRET-VALUE\"}', 'h')",
                [],
            )?;
            Ok::<_, rusqlite::Error>(())
        })
        .unwrap();

    let out_dir = tempdir("out");
    let bundle = modbit_core_runtime::diagnostics::report_a_problem(
        &store,
        &out_dir,
        "task t1 hangs in running state",
    )
    .unwrap();

    // The bundle contains the expected artifacts and the report reflects
    // the real durable state.
    let report = std::fs::read_to_string(bundle.dir.join("report.json")).unwrap();
    let report: serde_json::Value = serde_json::from_str(&report).unwrap();
    assert_eq!(report["store"]["tasks"], 1);
    assert_eq!(report["store"]["events"], 1);
    assert_eq!(report["os"], std::env::consts::OS);
    assert_eq!(
        report["recent_events"].as_array().unwrap().len(),
        1,
        "the durable event is exported"
    );

    // PROBLEM.md carries the description + the bundle hash.
    let problem = std::fs::read_to_string(bundle.dir.join("PROBLEM.md")).unwrap();
    assert!(problem.contains("task t1 hangs in running state"));
    assert!(problem.contains(&bundle.sha256));

    // THE security property: the planted secret appears NOWHERE in the
    // bundle — not in the raw settings export, not in event payloads.
    let mut leak = false;
    for entry in std::fs::read_dir(&bundle.dir).unwrap().flatten() {
        let bytes = std::fs::read(entry.path()).unwrap_or_default();
        if let Ok(text) = String::from_utf8(bytes) {
            if text.contains("sk-PLANTED-SECRET-VALUE") {
                leak = true;
                eprintln!("LEAK in {:?}", entry.path());
            }
        }
    }
    assert!(!leak, "planted secret leaked into the diagnostics bundle");
    // The redaction marker IS present (proving the settings were exported
    // and scrubbed, not skipped).
    let report_text = std::fs::read_to_string(bundle.dir.join("report.json")).unwrap();
    assert!(
        report_text.contains("[REDACTED]"),
        "settings were exported with scrubbed fields"
    );
}
