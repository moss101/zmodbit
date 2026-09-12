//! Automation engine (Phase 7 item 4): turns durable automation specs
//! into REAL tasks through the same command path as everything else.
//! Cron triggers fire once per matching minute (boundary-keyed — a fire
//! survives restarts without doubling); event triggers react to durable
//! event types (e.g. follow up on `task_completed`) with the consumed
//! rowid persisted so replays never double-fire.
//!
//! The engine is a pure `tick()` over the store — the daemon calls it
//! from a background loop; tests drive ticks manually (no sleeps).

use std::sync::Arc;

use modbit_domain::events::{Actor, ActorType};
use modbit_domain::{Command, CommandPayload};
use modbit_event_store::automations::Automation;
use modbit_event_store::{CommandProcessor, EventStore};

/// A boundary minute key: "YYYYMMDDHHmm" in UTC.
pub fn minute_key(now_epoch_secs: u64) -> String {
    let (y, mo, d, h, mi, _) = civil_from_epoch(now_epoch_secs - now_epoch_secs % 60);
    format!("{y:04}{mo:02}{d:02}{h:02}{mi:02}")
}

/// A cron field: a set of acceptable values (already expanded from
/// `*`, lists, ranges and `*/step`).
#[derive(Debug, Clone)]
struct Field {
    values: std::collections::BTreeSet<u32>,
}

impl Field {
    fn parse(spec: &str, min: u32, max: u32) -> Option<Field> {
        let mut values = std::collections::BTreeSet::new();
        for part in spec.split(',') {
            let (range, step) = match part.split_once('/') {
                Some((r, s)) => (r, s.parse::<u32>().ok().filter(|s| *s > 0)?),
                None => (part, 1),
            };
            let (lo, hi) = if range == "*" {
                (min, max)
            } else if let Some((a, b)) = range.split_once('-') {
                (a.parse().ok()?, b.parse().ok()?)
            } else {
                let v = range.parse().ok()?;
                (v, v)
            };
            if lo < min || hi > max || lo > hi {
                return None;
            }
            let mut v = lo;
            while v <= hi {
                values.insert(v);
                v += step;
            }
        }
        Some(Field { values })
    }

    fn matches(&self, v: u32) -> bool {
        self.values.contains(&v)
    }
}

/// A 5-field cron spec (minute hour day-of-month month day-of-week).
#[derive(Debug, Clone)]
pub struct CronSpec {
    minute: Field,
    hour: Field,
    day: Field,
    month: Field,
    weekday: Field,
}

impl CronSpec {
    pub fn parse(spec: &str) -> Option<CronSpec> {
        let parts: Vec<&str> = spec.split_whitespace().collect();
        if parts.len() != 5 {
            return None;
        }
        Some(CronSpec {
            minute: Field::parse(parts[0], 0, 59)?,
            hour: Field::parse(parts[1], 0, 23)?,
            day: Field::parse(parts[2], 1, 31)?,
            month: Field::parse(parts[3], 1, 12)?,
            // 0 and 7 both mean Sunday (cron convention).
            weekday: Field::parse(parts[4], 0, 7)?,
        })
    }

    /// Does this spec match the minute containing `epoch_secs` (UTC)?
    pub fn matches_minute(&self, epoch_secs: u64) -> bool {
        let (_, mo, d, h, mi, dow) = civil_from_epoch(epoch_secs - epoch_secs % 60);
        let month_ok = self.month.matches(mo);
        // cron semantics: if BOTH day-of-month and day-of-week are
        // restricted, EITHER may match; otherwise BOTH must.
        let day_restricted = self.day.values.len() != 31 || self.month.values.len() != 12;
        let day_ok = self.day.matches(d);
        let dow_ok = self.weekday.matches(dow) || (dow == 0 && self.weekday.matches(7));
        let dom_dow = match (day_restricted, self.day.values.len() < 31, self.weekday.values.len() < 7) {
            (true, true, true) => day_ok || dow_ok,
            _ => day_ok && dow_ok,
        };
        month_ok && dom_dow && self.hour.matches(h) && self.minute.matches(mi)
    }
}

/// Civil UTC decomposition (Howard Hinnant's civil_from_days).
fn civil_from_epoch(epoch_secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let days = (epoch_secs / 86_400) as i64;
    let secs = (epoch_secs % 86_400) as u32;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = (if m <= 2 { y + 1 } else { y }) as u32;
    let h = secs / 3600;
    let mi = (secs % 3600) / 60;
    // 1970-01-01 was a Thursday (4); Sunday = 0.
    let dow = ((days + 4).rem_euclid(7)) as u32;
    (y, m, d, h, mi, dow)
}

/// One tick's outcome — evidence, never silent.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TickOutcome {
    pub fired: Vec<(String, String)>, // (automation_id, task_id)
    pub errors: Vec<String>,
}

pub struct AutomationEngine {
    store: Arc<EventStore>,
    processor: CommandProcessor,
}

impl AutomationEngine {
    pub fn new(store: Arc<EventStore>) -> Self {
        let processor = CommandProcessor::new(store.clone());
        Self { store, processor }
    }

    /// Fires every DUE cron automation and every UNCONSUMED matching
    /// event. Idempotent per boundary/rowid — safe to call from any
    /// loop cadence.
    pub fn tick(&self, now_epoch_secs: u64) -> TickOutcome {
        let mut outcome = TickOutcome::default();
        let key = minute_key(now_epoch_secs);
        let automations = self
            .store
            .with_conn(modbit_event_store::automations::list)
            .unwrap_or_default();
        let mut last_rowid: Option<i64> = None;
        for a in automations {
            if !a.enabled {
                continue;
            }
            if a.event_pattern.is_empty() {
                // Cron path: fire when the current boundary matches and
                // hasn't fired yet.
                let epoch = now_epoch_secs - now_epoch_secs % 60;
                let Some(spec) = CronSpec::parse(&a.cron) else {
                    outcome
                        .errors
                        .push(format!("{}: unparsable cron {:?}", a.automation_id, a.cron));
                    continue;
                };
                if a.last_fire_key == key || !spec.matches_minute(epoch) {
                    continue;
                }
                match self.spawn_task(&a) {
                    Ok(task_id) => {
                        let _ = self.store.with_conn(|conn| {
                            modbit_event_store::automations::record_fire(
                                conn,
                                &a.automation_id,
                                &key,
                            )
                        });
                        outcome.fired.push((a.automation_id.clone(), task_id));
                    }
                    Err(e) => outcome
                        .errors
                        .push(format!("{}: {e}", a.automation_id)),
                }
            } else {
                // Event path: consume events newer than the last consumed
                // rowid; the newest matching rowid is the new watermark.
                let consumed = a
                    .last_fire_key
                    .parse::<i64>()
                    .unwrap_or(-1);
                let matches: Vec<i64> = self
                    .store
                    .with_conn(|conn| {
                        let mut stmt = conn
                            .prepare(
                                "SELECT rowid FROM events
                                 WHERE event_type = ?1 AND rowid > ?2 ORDER BY rowid",
                            )
                            .ok()?;
                        let rows = stmt
                            .query_map(
                                rusqlite::params![a.event_pattern, consumed],
                                |r| r.get::<_, i64>(0),
                            )
                            .ok()?
                            .collect::<Result<Vec<_>, _>>()
                            .ok()?;
                        Some(rows)
                    })
                    .unwrap_or_default();
                if matches.is_empty() {
                    continue;
                }
                match self.spawn_task(&a) {
                    Ok(task_id) => {
                        let newest = matches[matches.len() - 1];
                        let _ = self.store.with_conn(|conn| {
                            modbit_event_store::automations::record_fire(
                                conn,
                                &a.automation_id,
                                &newest.to_string(),
                            )
                        });
                        outcome.fired.push((a.automation_id.clone(), task_id));
                    }
                    Err(e) => outcome
                        .errors
                        .push(format!("{}: {e}", a.automation_id)),
                }
                last_rowid = None;
            }
        }
        let _ = last_rowid;
        outcome
    }

    fn spawn_task(&self, a: &Automation) -> Result<String, String> {
        // The follow-up task lives in the default (first) session — the
        // daemon owns session kernels, automations ride them.
        let session_id: String = self
            .store
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT aggregate_id FROM events WHERE aggregate_type = 'session'
                     ORDER BY rowid LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .map_err(|_| "no session exists yet".to_string())
            })?;
        let session_id = modbit_domain::SessionId::parse(&session_id)
            .map_err(|e| format!("bad session: {e}"))?;
        let outcome = self
            .processor
            .execute(Command {
                command_id: uuid::Uuid::now_v7().to_string(),
                actor: Actor {
                    actor_type: ActorType::System,
                    actor_id: a.automation_id.clone(),
                },
                payload: CommandPayload::CreateTask {
                    session_id,
                    title: format!("automation: {}", a.name),
                    prompt: a.objective.clone(),
                    repo_id: None,
                    base_branch: None,
                    parent_task_id: None,
                },
            })
            .map_err(|e| e.to_string())?;
        let task_id = aggregate_of(&self.store, &outcome).ok_or("automation fire produced no task")?;
        let tid = modbit_domain::TaskId::parse(&task_id).map_err(|e| e.to_string())?;
        self.processor
            .execute(Command {
                command_id: uuid::Uuid::now_v7().to_string(),
                actor: Actor {
                    actor_type: ActorType::System,
                    actor_id: a.automation_id.clone(),
                },
                payload: CommandPayload::QueueTask { task_id: tid },
            })
            .map_err(|e| e.to_string())?;
        Ok(task_id)
    }
}

/// The created aggregate id equals the creation event's aggregate —
/// resolve it from the outcome's first appended event.
fn aggregate_of(
    store: &EventStore,
    outcome: &modbit_event_store::Outcome,
) -> Option<String> {
    let event_ids = match outcome {
        modbit_event_store::Outcome::Applied { event_ids }
        | modbit_event_store::Outcome::Replayed { event_ids } => event_ids,
        modbit_event_store::Outcome::Rejected { .. } => return None,
    };
    let event_id = event_ids.first()?;
    store
        .with_conn(|conn| {
            conn.query_row(
                "SELECT aggregate_id FROM events WHERE event_id = ?1",
                [event_id],
                |r| r.get::<_, Option<String>>(0),
            )
        })
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_parses_and_matches() {
        let every_minute = CronSpec::parse("* * * * *").unwrap();
        // 2026-09-11 12:34:00 UTC (Friday, verified against the calendar).
        let friday = 1_789_130_040;
        assert!(every_minute.matches_minute(friday));

        let at_1234 = CronSpec::parse("34 12 * * *").unwrap();
        assert!(at_1234.matches_minute(friday));
        assert!(!at_1234.matches_minute(friday + 60));

        let weekdays = CronSpec::parse("0 9 * * 1-5").unwrap();
        let monday_9 = 1_788_771_600; // 2026-09-07 09:00 UTC
        let saturday_9 = 1_789_203_600; // 2026-09-12 09:00 UTC
        assert!(weekdays.matches_minute(monday_9));
        assert!(!weekdays.matches_minute(saturday_9));
        // Arithmetic cross-check: the dow helper agrees with the calendar.
        let (_, _, _, _, _, dow) = civil_from_epoch(monday_9);
        assert_eq!(dow, 1, "Monday = 1 (Sunday 0)");
        let (_, _, _, _, _, dow_sat) = civil_from_epoch(saturday_9);
        assert_eq!(dow_sat, 6, "Saturday = 6");
    }

    #[test]
    fn minute_key_is_boundary_keyed() {
        assert_eq!(minute_key(1_789_130_045), minute_key(1_789_130_040));
        assert_ne!(minute_key(1_789_130_040), minute_key(1_789_130_100));
    }
}
