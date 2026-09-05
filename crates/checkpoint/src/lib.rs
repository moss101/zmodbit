//! modbit-checkpoint — workspace/runtime checkpoint store (M4, docs/22 §
//! Checkpoint/rollback): monotonic epoch fencing (REQ-EV-0012), the
//! baseline+delta journal (REQ-EV-0013), and the typed failure taxonomy
//! with fault injection (REQ-EV-0073/0245).
//!
//! Canonical owner subsystem: durability (docs/81). Layout: docs/12.

use serde::{Deserialize, Serialize};
use std::fmt;

pub mod cursor_meta;
pub mod delta;
pub mod failure;
pub mod hook_bus;
pub mod importers_plugins;
pub mod lease;
pub mod mcp_memory;

// ---------------------------------------------------------------------------
// Epoch fencing (REQ-EV-0012)
// ---------------------------------------------------------------------------

/// A durable checkpoint: epoch + payload (delta journal reference).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Monotonic epoch — strictly increasing across writes.
    pub epoch: u64,
    pub payload: String,
    pub created_at_ms: i64,
}

#[derive(Debug)]
pub enum CheckpointError {
    /// A stale asynchronous writer tried to overwrite newer state.
    StaleEpoch { attempted: u64, current: u64 },
}

impl fmt::Display for CheckpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckpointError::StaleEpoch { attempted, current } => write!(
                f,
                "stale checkpoint epoch {attempted} rejected (current {current})"
            ),
        }
    }
}

impl std::error::Error for CheckpointError {}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The fenced checkpoint store.
#[derive(Default)]
pub struct CheckpointStore {
    current_epoch: u64,
    pub latest: Option<Checkpoint>,
    /// Rejection audit: every stale write attempt.
    pub rejections: Vec<(u64, u64)>,
}

impl CheckpointStore {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn current_epoch(&self) -> u64 {
        self.current_epoch
    }

    /// Writes a checkpoint. Monotonic fencing: only STRICTLY newer epochs
    /// are accepted — a stale async writer can never overwrite newer
    /// state (QUAL-EV-0012).
    pub fn write(&mut self, epoch: u64, payload: &str) -> Result<(), CheckpointError> {
        if epoch <= self.current_epoch {
            self.rejections.push((epoch, self.current_epoch));
            return Err(CheckpointError::StaleEpoch {
                attempted: epoch,
                current: self.current_epoch,
            });
        }
        self.current_epoch = epoch;
        self.latest = Some(Checkpoint {
            epoch,
            payload: payload.to_string(),
            created_at_ms: now_ms(),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0012: race two checkpoint writes; the old epoch cannot
    /// overwrite newer state.
    #[test]
    fn stale_epoch_cannot_overwrite_newer_state() {
        let mut store = CheckpointStore::new();
        // Newer epoch lands FIRST (the race outcome being fenced against).
        store.write(7, "newer state").unwrap();
        // The older async writer lands SECOND: rejected.
        let err = store.write(3, "older async write").unwrap_err();
        assert!(matches!(
            err,
            CheckpointError::StaleEpoch {
                attempted: 3,
                current: 7
            }
        ));
        // Equal epoch also rejected (strictly monotonic).
        assert!(store.write(7, "duplicate").is_err());
        // State is still the NEWER payload.
        assert_eq!(store.latest.as_ref().unwrap().payload, "newer state");
        // Rejections are audited.
        assert_eq!(store.rejections.len(), 2);
        // A genuinely newer epoch still lands.
        store.write(8, "newest").unwrap();
        assert_eq!(store.latest.as_ref().unwrap().epoch, 8);
    }
}
