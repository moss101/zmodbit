//! The durable approval store (Phase 5, docs/23 § Approval policy,
//! docs/13 § Waiting(Approval)): pending effect approvals SURVIVE a Core
//! kill — the decision state lives in SQLite, not in process memory.
//! Resolutions also append durable events on the task aggregate (the
//! caller owns that part); this module is the lookup/decision state.

use rusqlite::Connection;
use rusqlite::OptionalExtension;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PendingApproval {
    pub approval_id: String,
    pub task_id: String,
    pub intent_hash: String,
    pub tool: String,
    pub scope: String,
    pub state: String,
    pub created_at: String,
}

/// Creates a pending approval (idempotent per id).
pub fn insert(conn: &Connection, a: &PendingApproval) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT OR IGNORE INTO approvals
         (approval_id, task_id, intent_hash, tool, scope, state, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![a.approval_id, a.task_id, a.intent_hash, a.tool, a.scope, a.state, a.created_at],
    )?;
    Ok(())
}

/// Resolves an approval to `approved`/`denied`. Returns false when the
/// approval does not exist or was already resolved (first decision wins —
/// a replayed ApproveEffect after a Core restart must not re-resolve).
pub fn resolve(conn: &Connection, approval_id: &str, state: &str, resolved_by: &str, now: &str) -> Result<bool, rusqlite::Error> {
    let n = conn.execute(
        "UPDATE approvals
         SET state = ?2, resolved_at = ?3, resolved_by = ?4
         WHERE approval_id = ?1 AND state = 'pending'",
        rusqlite::params![approval_id, state, now, resolved_by],
    )?;
    Ok(n > 0)
}

/// The live decision for an intent hash under a task: 'approved' when a
/// resolved approval exists, 'pending' when one is awaiting a decision,
/// None when neither.
pub fn decision_for(conn: &Connection, task_id: &str, intent_hash: &str) -> Result<Option<String>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT state FROM approvals
         WHERE task_id = ?1 AND intent_hash = ?2
         ORDER BY CASE state WHEN 'approved' THEN 0 WHEN 'pending' THEN 1 ELSE 2 END, created_at
         LIMIT 1",
    )?;
    let state = stmt
        .query_row(rusqlite::params![task_id, intent_hash], |row| row.get::<_, String>(0))
        .optional()?;
    Ok(state)
}

/// Pending approvals for a task (the Needs-Attention card data).
/// Every PENDING approval across the fleet — the desktop Needs-Attention
/// source (docs/13: approvals are user-visible work, never silent).
pub fn list_pending(conn: &Connection) -> Result<Vec<PendingApproval>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT approval_id, task_id, intent_hash, tool, scope, created_at
         FROM approvals WHERE state = 'pending' ORDER BY created_at, approval_id",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(PendingApproval {
                approval_id: r.get(0)?,
                task_id: r.get(1)?,
                intent_hash: r.get(2)?,
                tool: r.get(3)?,
                scope: r.get(4)?,
                state: "pending".into(),
                created_at: r.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn pending_for_task(conn: &Connection, task_id: &str) -> Result<Vec<PendingApproval>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT approval_id, task_id, intent_hash, tool, scope, state, created_at
         FROM approvals WHERE task_id = ?1 AND state = 'pending' ORDER BY created_at",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![task_id], |row| {
            Ok(PendingApproval {
                approval_id: row.get(0)?,
                task_id: row.get(1)?,
                intent_hash: row.get(2)?,
                tool: row.get(3)?,
                scope: row.get(4)?,
                state: row.get(5)?,
                created_at: row.get(6)?,
            })
        })?
        .flatten()
        .collect();
    Ok(rows)
}
