//! Write-scope coordination (Phase 7 item 2 / REQ ledger EV-0150): parallel tasks
//! on the same repository declare the paths they intend to write; the
//! coordinator denies an acquisition that overlaps an ACTIVE task's scope
//! BEFORE that task runs, and releases scopes when the holding task
//! reaches a terminal state. Durable in SQLite (v10) so coordination
//! survives restarts; the tasks projection remains the liveness truth.
//!
//! Canonical owner subsystem: workspace change coordination (docs/81).
//! Spawn admission (docs/14 transactional subagent admission) and plain
//! task creation share THIS registry — there is no second coordinator.

use rusqlite::{params, Connection};

/// One declared write path held by an active task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldScope {
    pub task_id: String,
    pub path: String,
}

/// Terminal states free their ticket (docs/14 § capacity tickets):
/// completed/failed/cancelled tasks no longer hold scopes.
const TERMINAL_STATES: &[&str] = &["completed", "failed", "cancelled"];

fn active_states_sql() -> String {
    // 'created' holds its declared scope too: denial must happen BEFORE
    // execution, i.e. at admission time, not first run.
    "('created','queued','running','ready_for_review')".to_string()
}

/// Exact match or directory containment in either direction; trailing
/// separators are normalized so `src/` and `src/api.rs` compare correctly.
fn covers(a: &str, b: &str) -> bool {
    let a = a.trim_end_matches('/');
    let b = b.trim_end_matches('/');
    a == b || b.starts_with(&format!("{a}/"))
}

fn parse_scope(spec: &str) -> Vec<String> {
    spec.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Outcome of a scope acquisition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AcquireOutcome {
    /// All declared paths are free; they are now held by `task_id`.
    Acquired,
    /// Overlap with an ACTIVE holder — denied BEFORE execution (QUAL-EV-0150).
    Denied { holder_task_id: String, path: String },
}

/// Attempts to acquire the declared write scope for a task on a repo.
/// Prunes scopes of terminal tasks first (release-on-terminal), then
/// checks overlap against remaining ACTIVE holders, then records the
/// new scope. `scope_spec` is the comma-separated path list; empty means
/// "no declaration" (nothing acquired, nothing denied — undeclared work
/// is handled by merge verification, not admission).
pub fn acquire(
    conn: &Connection,
    task_id: &str,
    repo_key: &str,
    scope_spec: &str,
) -> Result<AcquireOutcome, String> {
    let paths = parse_scope(scope_spec);
    if paths.is_empty() {
        return Ok(AcquireOutcome::Acquired);
    }
    release_terminal(conn)?;
    let placeholders = active_states_sql();
    let sql = format!(
        "SELECT s.task_id, s.path FROM task_write_scopes s
         JOIN tasks t ON t.task_id = s.task_id
         WHERE s.repo_key = ?1 AND t.state IN {placeholders}"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("write scopes: {e}"))?;
    let held: Vec<HeldScope> = stmt
        .query_map(params![repo_key], |r| {
            Ok(HeldScope {
                task_id: r.get(0)?,
                path: r.get(1)?,
            })
        })
        .map_err(|e| format!("write scopes: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("write scopes: {e}"))?;
    for path in &paths {
        for h in &held {
            if covers(&h.path, path) || covers(path, &h.path) {
                return Ok(AcquireOutcome::Denied {
                    holder_task_id: h.task_id.clone(),
                    path: path.clone(),
                });
            }
        }
    }
    conn.execute(
        "DELETE FROM task_write_scopes WHERE task_id = ?1",
        params![task_id],
    )
    .map_err(|e| format!("write scopes: {e}"))?;
    for path in &paths {
        conn.execute(
            "INSERT INTO task_write_scopes (task_id, repo_key, path) VALUES (?1, ?2, ?3)",
            params![task_id, repo_key, path],
        )
        .map_err(|e| format!("write scopes: {e}"))?;
    }
    Ok(AcquireOutcome::Acquired)
}

/// Releases a task's scope explicitly (cancel/complete/merge flows).
pub fn release(conn: &Connection, task_id: &str) -> Result<(), String> {
    conn.execute(
        "DELETE FROM task_write_scopes WHERE task_id = ?1",
        params![task_id],
    )
    .map_err(|e| format!("write scopes: {e}"))?;
    Ok(())
}

/// Drops scopes held by terminal tasks (idempotent sweep; also the
/// restart-recovery path for tasks that died between events).
fn release_terminal(conn: &Connection) -> Result<(), String> {
    let list = TERMINAL_STATES
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(",");
    conn.execute(
        &format!(
            "DELETE FROM task_write_scopes WHERE task_id IN (
                 SELECT task_id FROM tasks WHERE state IN ({list})
             )"
        ),
        [],
    )
    .map_err(|e| format!("write scopes: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::migrations::migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO sessions (session_id, state, generation, created_at, updated_at, last_event_sequence)
             VALUES ('s', 'active', 1, '2026-09-11T00:00:00.000Z', '2026-09-11T00:00:00.000Z', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tasks (task_id, session_id, goal_text, state, generation, created_at)
             VALUES ('t-holder', 's', 'holder', 'running', 1, '2026-09-11T00:00:00.000Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tasks (task_id, session_id, goal_text, state, generation, created_at)
             VALUES ('t-other', 's', 'other', 'running', 1, '2026-09-11T00:00:00.000Z')",
            [],
        )
        .unwrap();
        conn
    }

    #[test]
    fn denies_overlapping_scope_before_execution_then_releases_on_terminal() {
        let conn = setup();
        let first = acquire(&conn, "t-holder", "repo-1", "src/api.rs, docs/guide.md").unwrap();
        assert_eq!(first, AcquireOutcome::Acquired);

        // Same path — denied with the holder named.
        let second = acquire(&conn, "t-other", "repo-1", "src/api.rs").unwrap();
        assert_eq!(
            second,
            AcquireOutcome::Denied {
                holder_task_id: "t-holder".into(),
                path: "src/api.rs".into()
            }
        );
        // Directory containment — denied either direction.
        let dir = acquire(&conn, "t-other", "repo-1", "src/").unwrap();
        assert!(matches!(dir, AcquireOutcome::Denied { .. }));
        // Different repo — the same path is free.
        let other_repo = acquire(&conn, "t-other", "repo-2", "src/api.rs").unwrap();
        assert_eq!(other_repo, AcquireOutcome::Acquired);

        // Holder completes: its scope is released by the next acquire.
        conn.execute("UPDATE tasks SET state='completed' WHERE task_id='t-holder'", [])
            .unwrap();
        let after = acquire(&conn, "t-other", "repo-1", "src/api.rs").unwrap();
        assert_eq!(after, AcquireOutcome::Acquired);
    }

    #[test]
    fn empty_declaration_acquires_nothing() {
        let conn = setup();
        let out = acquire(&conn, "t-other", "repo-1", "").unwrap();
        assert_eq!(out, AcquireOutcome::Acquired);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM task_write_scopes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}
