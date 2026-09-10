//! The recent-repos registry (Phase 4.1): the repositories a user has
//! registered with the daemon — by local path or cloned by URL — with
//! their default branches. Backed by the `recent_repos` table (migration
//! v6, docs/31); rows are runtime configuration state, NOT event history.

use rusqlite::Connection;
use rusqlite::OptionalExtension;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RecentRepo {
    pub repo_id: String,
    pub path: String,
    pub clone_url: String,
    pub default_branch: String,
    pub registered_at: String,
    pub last_used_at: String,
}

/// Inserts (or refreshes) a registered repo by id.
pub fn register(
    conn: &Connection,
    repo: &RecentRepo,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO recent_repos (repo_id, path, clone_url, default_branch, registered_at, last_used_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)
         ON CONFLICT(repo_id) DO UPDATE SET
           path = excluded.path,
           clone_url = excluded.clone_url,
           default_branch = excluded.default_branch,
           last_used_at = excluded.last_used_at",
        rusqlite::params![
            repo.repo_id,
            repo.path,
            repo.clone_url,
            repo.default_branch,
            repo.registered_at,
        ],
    )?;
    Ok(())
}

/// Touches last_used_at when a task is created against the repo.
pub fn touch(conn: &Connection, repo_id: &str, now: &str) -> Result<(), rusqlite::Error> {
    conn.execute(
        "UPDATE recent_repos SET last_used_at = ?2 WHERE repo_id = ?1",
        rusqlite::params![repo_id, now],
    )?;
    Ok(())
}

/// The registry, most-recently-used first.
pub fn list(conn: &Connection) -> Result<Vec<RecentRepo>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT repo_id, path, clone_url, default_branch, registered_at, last_used_at
         FROM recent_repos ORDER BY last_used_at DESC, registered_at DESC",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(RecentRepo {
                repo_id: row.get(0)?,
                path: row.get(1)?,
                clone_url: row.get(2)?,
                default_branch: row.get(3)?,
                registered_at: row.get(4)?,
                last_used_at: row.get(5)?,
            })
        })?
        .flatten()
        .collect();
    Ok(rows)
}

/// One repo by id.
pub fn get(conn: &Connection, repo_id: &str) -> Result<Option<RecentRepo>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT repo_id, path, clone_url, default_branch, registered_at, last_used_at
         FROM recent_repos WHERE repo_id = ?1",
    )?;
    stmt.query_row(rusqlite::params![repo_id], |row| {
        Ok(RecentRepo {
            repo_id: row.get(0)?,
            path: row.get(1)?,
            clone_url: row.get(2)?,
            default_branch: row.get(3)?,
            registered_at: row.get(4)?,
            last_used_at: row.get(5)?,
        })
    })
    .optional()
}
