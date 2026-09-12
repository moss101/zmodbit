//! Automations (Phase 7 item 4): scheduled (cron) and event-triggered
//! task creation on the daemon. Durable in SQLite (v11) — an automation
//! survives restarts, records its last fire (boundary-keyed so a cron
//! minute fires EXACTLY once even across restarts), and its follow-up
//! tasks carry `automation_id` linkage.

use rusqlite::{params, Connection};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Automation {
    pub automation_id: String,
    pub name: String,
    /// Cron spec when `trigger` is "cron" (5 fields, minute granularity).
    pub cron: String,
    /// Event type to react to when `trigger` is "event" (e.g.
    /// "task_completed"); empty for cron triggers.
    pub event_pattern: String,
    /// The follow-up task objective.
    pub objective: String,
    pub enabled: bool,
    /// For cron: the last fired boundary minute ("YYYYMMDDHHmm") — a
    /// boundary fires at most once, restart-safe. For event: last event
    /// rowid already consumed (-1 = none yet).
    pub last_fire_key: String,
}

fn trigger_of(a: &Automation) -> &'static str {
    if a.event_pattern.is_empty() {
        "cron"
    } else {
        "event"
    }
}

/// Registers an automation; the id is assigned here when empty.
pub fn create(conn: &Connection, mut a: Automation) -> Result<Automation, String> {
    if a.automation_id.is_empty() {
        a.automation_id = format!("auto-{}", uuid_like());
    }
    if a.cron.is_empty() && a.event_pattern.is_empty() {
        return Err("automation needs a cron spec or an event pattern".into());
    }
    if a.objective.trim().is_empty() {
        return Err("automation needs an objective".into());
    }
    conn.execute(
        "INSERT INTO automations (automation_id, name, trigger, cron, event_pattern, objective, enabled, last_fire_key)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            a.automation_id,
            a.name,
            trigger_of(&a),
            a.cron,
            a.event_pattern,
            a.objective,
            a.enabled as i64,
            a.last_fire_key,
        ],
    )
    .map_err(|e| format!("automations: {e}"))?;
    Ok(a)
}

pub fn list(conn: &Connection) -> Result<Vec<Automation>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT automation_id, name, cron, event_pattern, objective, enabled, last_fire_key
             FROM automations ORDER BY rowid",
        )
        .map_err(|e| format!("automations: {e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Automation {
                automation_id: r.get(0)?,
                name: r.get(1)?,
                cron: r.get(2)?,
                event_pattern: r.get(3)?,
                objective: r.get(4)?,
                enabled: r.get::<_, i64>(5)? != 0,
                last_fire_key: r.get(6)?,
            })
        })
        .map_err(|e| format!("automations: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("automations: {e}"))?;
    Ok(rows)
}

pub fn set_enabled(conn: &Connection, automation_id: &str, enabled: bool) -> Result<(), String> {
    let changed = conn
        .execute(
            "UPDATE automations SET enabled = ?2 WHERE automation_id = ?1",
            params![automation_id, enabled as i64],
        )
        .map_err(|e| format!("automations: {e}"))?;
    if changed == 0 {
        return Err(format!("automation {automation_id} does not exist"));
    }
    Ok(())
}

pub fn delete(conn: &Connection, automation_id: &str) -> Result<(), String> {
    let changed = conn
        .execute(
            "DELETE FROM automations WHERE automation_id = ?1",
            params![automation_id],
        )
        .map_err(|e| format!("automations: {e}"))?;
    if changed == 0 {
        return Err(format!("automation {automation_id} does not exist"));
    }
    Ok(())
}

/// Records that `boundary` (a cron minute key) fired for an automation.
pub fn record_fire(
    conn: &Connection,
    automation_id: &str,
    boundary: &str,
) -> Result<(), String> {
    conn.execute(
        "UPDATE automations SET last_fire_key = ?2 WHERE automation_id = ?1",
        params![automation_id, boundary],
    )
    .map(|_| ())
    .map_err(|e| format!("automations: {e}"))
}

fn uuid_like() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}{seq:x}")
}
