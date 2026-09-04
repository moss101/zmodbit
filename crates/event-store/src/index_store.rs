//! Index store (M1, REQ-EV-0101): docs/31 mandates two separated durable
//! databases — `core.db` (authoritative events/projections/protocol) and
//! `index.db` (repository/index metadata). Separation keeps the index
//! rebuildable and the core authoritative; both are independently resumable
//! after a hard kill.

use std::fmt;
use std::path::Path;
use std::sync::Mutex;

use rusqlite::Connection;

pub const INDEX_MIGRATIONS: &[(&str, &str)] = &[(
    "index_generations",
    "CREATE TABLE IF NOT EXISTS index_generations (
        id            INTEGER PRIMARY KEY,
        generation    INTEGER NOT NULL,
        built_at      TEXT NOT NULL,
        object_hash   TEXT NOT NULL
    );
",
)];

pub struct IndexStore {
    conn: Mutex<Connection>,
}

#[derive(Debug)]
pub enum IndexError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IndexError::Sqlite(e) => write!(f, "index sqlite: {e}"),
            IndexError::Io(e) => write!(f, "index io: {e}"),
        }
    }
}

impl std::error::Error for IndexError {}

impl IndexStore {
    /// Opens (creating if needed) the separate index database.
    pub fn open(path: &Path) -> Result<Self, IndexError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(IndexError::Io)?;
        }
        let conn = Connection::open(path).map_err(IndexError::Sqlite)?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(IndexError::Sqlite)?;
        for (_, sql) in INDEX_MIGRATIONS {
            conn.execute_batch(sql).map_err(IndexError::Sqlite)?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Records an index generation (content-addressed external index files
    /// are referenced, never inlined — docs/31 § Local storage).
    pub fn record_generation(
        &self,
        generation: u64,
        built_at: &str,
        object_hash: &str,
    ) -> Result<(), IndexError> {
        let conn = self.conn.lock().expect("index store mutex poisoned");
        conn.execute(
            "INSERT INTO index_generations (generation, built_at, object_hash)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![generation as i64, built_at, object_hash],
        )
        .map_err(IndexError::Sqlite)?;
        Ok(())
    }

    /// The latest recorded index generation, if any.
    pub fn latest_generation(&self) -> Result<Option<u64>, IndexError> {
        let conn = self.conn.lock().expect("index store mutex poisoned");
        let row: Option<i64> = conn
            .query_row("SELECT MAX(generation) FROM index_generations", [], |r| {
                r.get(0)
            })
            .map_err(IndexError::Sqlite)?;
        Ok(row.map(|v| v as u64))
    }
}
