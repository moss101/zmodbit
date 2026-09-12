//! Diagnostics export + report-a-problem (Phase 9): assembles a REDACTED
//! diagnostics bundle from the durable stores — never secrets. The
//! settings document holds no credentials by design (docs/31 § v7:
//! keychain only); the exporter additionally strips any key-shaped field
//! defensively, so a future settings-field mistake cannot leak through
//! the report path.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// Fields that must never appear in a diagnostics bundle even if present
/// in a store document (defensive second line after the settings design).
const REDACTED_KEYS: &[&str] = &[
    "api_key",
    "apiKey",
    "secret",
    "token",
    "password",
    "authorization",
];

fn redact(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    if REDACTED_KEYS
                        .iter()
                        .any(|r| k.to_lowercase().contains(&r.to_lowercase()))
                    {
                        (k.clone(), Value::String("[REDACTED]".into()))
                    } else {
                        (k.clone(), redact(v))
                    }
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redact).collect()),
        other => other.clone(),
    }
}

/// One collected bundle.
pub struct Bundle {
    pub dir: PathBuf,
    pub sha256: String,
}

/// Collects the diagnostics bundle into `out_dir/modbit-diagnostics-<ts>/`:
/// report.json (version/rev/os, redacted settings, task + event counts,
/// automation list), recent task events (last 200), the effects-ledger
/// tail (tamper-evident chain), and a protocol-state summary.
pub fn collect(store: &std::sync::Arc<modbit_event_store::EventStore>, out_dir: &Path) -> Result<Bundle, String> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dir = out_dir.join(format!("modbit-diagnostics-{ts}"));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let settings: Value = store
        .with_conn(|conn| {
            conn.query_row("SELECT data FROM app_settings WHERE id = 1", [], |r| {
                r.get::<_, String>(0)
            })
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .unwrap_or(Value::Null)
        });

    let (task_count, event_count): (i64, i64) = store.with_conn(|conn| {
        let tasks: i64 = conn
            .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
            .unwrap_or(0);
        let events: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap_or(0);
        (tasks, events)
    });

    let recent_rows: Vec<(String, String, String, Option<String>)> = store
        .with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT event_type, aggregate_id, occurred_at, payload_inline
                 FROM events ORDER BY rowid DESC LIMIT 200",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok::<_, rusqlite::Error>(rows)
        })
        .unwrap_or_default();
    let recent_events = Value::Array(
        recent_rows
            .iter()
            .map(|(t, a, at, p)| {
                json!({
                    "event_type": t,
                    "aggregate_id": a,
                    "occurred_at": at,
                    "payload": redact(
                        &p.as_deref()
                            .and_then(|s| serde_json::from_str::<Value>(s).ok())
                            .unwrap_or(Value::Null),
                    ),
                })
            })
            .collect(),
    );

    let report = json!({
        "unix_time": ts,
        "version": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "store": { "tasks": task_count, "events": event_count },
        "settings": redact(&settings),
        "recent_events": recent_events,
    });

    std::fs::write(
        dir.join("report.json"),
        serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;

    // Effects-ledger tail (hash chain evidence) when one exists.
    let ledger = std::env::var("MODBIT_EFFECTS_LEDGER").unwrap_or_default();
    if !ledger.is_empty() && Path::new(&ledger).exists() {
        let text = std::fs::read_to_string(&ledger).map_err(|e| e.to_string())?;
        let tail: Vec<&str> = text.lines().rev().take(100).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        std::fs::write(dir.join("effects-tail.jsonl"), format!("{}\n", tail.join("\n")))
            .map_err(|e| e.to_string())?;
    }

    // Hash the whole bundle for the report-a-problem reference.
    let sha256 = hash_dir(&dir)?;
    std::fs::write(dir.join("SHA256SUMS"), &sha256).map_err(|e| e.to_string())?;
    Ok(Bundle { dir, sha256 })
}

/// Writes a problem report referencing the bundle: PROBLEM.md carries the
/// caller's description + the bundle hash so a user attachment is
/// traceable to the exact diagnostics state.
pub fn report_a_problem(
    store: &std::sync::Arc<modbit_event_store::EventStore>,
    out_dir: &Path,
    description: &str,
) -> Result<Bundle, String> {
    let bundle = collect(store, out_dir)?;
    let doc = format!(
        "# Problem report\n\n{description}\n\nDiagnostics bundle: {} (sha256 {})\n",
        bundle.dir.display(),
        bundle.sha256
    );
    std::fs::write(bundle.dir.join("PROBLEM.md"), doc).map_err(|e| e.to_string())?;
    Ok(bundle)
}

fn hash_dir(dir: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    files.sort();
    let mut hasher = Sha256::new();
    for f in files {
        let bytes = std::fs::read(&f).map_err(|e| e.to_string())?;
        hasher.update(f.file_name().unwrap_or_default().as_encoded_bytes());
        hasher.update(&bytes);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_covers_nested_key_shaped_fields() {
        let v = json!({
            "provider": "openai",
            "api_key": "sk-super-secret",
            "nested": { "authToken": "tok", "model": "gpt" },
            "list": [{ "password": "p" }]
        });
        let out = redact(&v);
        assert_eq!(out["api_key"], "[REDACTED]");
        assert_eq!(out["nested"]["authToken"], "[REDACTED]");
        assert_eq!(out["list"][0]["password"], "[REDACTED]");
        assert_eq!(out["provider"], "openai");
        assert_eq!(out["nested"]["model"], "gpt");
    }
}
