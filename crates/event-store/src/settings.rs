//! The app-settings store (Phase 4.1/4.2, docs/31 § Runtime configuration
//! tables): the user's persisted daemon configuration — provider, model,
//! base URL, max turns, execution mode — as a single JSON document
//! (migration v7). Secrets NEVER live here (docs/31; the secret broker
//! owns credentials). Partial updates merge into the stored document.

use rusqlite::Connection;
use rusqlite::OptionalExtension;
use serde_json::Value;

/// Reads the stored settings document, or None when unset.
pub fn get(conn: &Connection) -> Result<Option<Value>, rusqlite::Error> {
    let data: Option<String> = conn
        .query_row("SELECT data FROM app_settings WHERE id = 1", [], |row| {
            row.get(0)
        })
        .optional()?;
    Ok(data.and_then(|d| serde_json::from_str(&d).ok()))
}

/// Persists the settings document (replaces; callers merge).
pub fn set(conn: &Connection, data: &Value) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO app_settings (id, data) VALUES (1, ?1)
         ON CONFLICT(id) DO UPDATE SET data = excluded.data",
        rusqlite::params![serde_json::to_string(data).unwrap_or_else(|_| "{}".into())],
    )?;
    Ok(())
}

/// Merges `patch` (an object) into the stored settings, returning the
/// merged document. Non-object patches are rejected by the caller.
pub fn merge(conn: &Connection, patch: &Value) -> Result<Value, rusqlite::Error> {
    let mut merged = get(conn)?.unwrap_or_else(|| Value::Object(Default::default()));
    if let (Some(base), Some(patch_obj)) = (merged.as_object_mut(), patch.as_object()) {
        for (k, v) in patch_obj {
            base.insert(k.clone(), v.clone());
        }
    }
    set(conn, &merged)?;
    Ok(merged)
}
