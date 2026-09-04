//! Retrieve-before-edit gate (M3, REQ-EV-0168): a material mutation
//! requires ADEQUATE repository understanding — the change engine refuses
//! edits to paths with no retrieval evidence. Blind edits are blocked and
//! surfaced, not silently allowed.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Retrieval evidence the runtime gathered BEFORE proposing an edit.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RetrievalEvidence {
    /// Paths actually retrieved (read) during context gathering.
    pub retrieved_paths: BTreeSet<String>,
    /// Canonical revision the retrieval happened at.
    pub at_revision: u64,
}

impl RetrievalEvidence {
    pub fn of(paths: &[&str], at_revision: u64) -> Self {
        Self {
            retrieved_paths: paths.iter().map(|p| p.to_string()).collect(),
            at_revision,
        }
    }

    pub fn covers(&self, path: &str) -> bool {
        self.retrieved_paths.contains(path)
    }
}

#[derive(Debug)]
pub enum EditGateError {
    BlindEdit { path: String },
}

impl std::fmt::Display for EditGateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditGateError::BlindEdit { path } => write!(
                f,
                "BLIND EDIT BLOCKED: {path:?} was never retrieved — gather repository understanding before mutating"
            ),
        }
    }
}

impl std::error::Error for EditGateError {}

/// The gate the change engine consults before applying a material
/// mutation: every edited path must appear in the retrieval evidence.
pub fn check_edit_allowed(
    evidence: &RetrievalEvidence,
    paths_to_edit: &[&str],
) -> Result<(), EditGateError> {
    for path in paths_to_edit {
        if !evidence.covers(path) {
            return Err(EditGateError::BlindEdit {
                path: path.to_string(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0168: a blind edit with missing context is blocked and
    /// surfaced; a retrieved path passes.
    #[test]
    fn blind_edit_blocked_and_surfaced() {
        let evidence = RetrievalEvidence::of(&["src/lib.rs", "src/retry.rs"], 41);

        // The retrieved path passes the gate.
        assert!(check_edit_allowed(&evidence, &["src/lib.rs"]).is_ok());

        // An unretrieved path is BLOCKED with a surfacing message.
        let err = check_edit_allowed(&evidence, &["src/config.rs"]).unwrap_err();
        assert!(err.to_string().contains("BLIND EDIT BLOCKED"));
        assert!(err.to_string().contains("src/config.rs"));

        // Multiple paths: one miss blocks the whole transaction.
        assert!(check_edit_allowed(&evidence, &["src/lib.rs", "src/config.rs"]).is_err());

        // Empty evidence blocks everything.
        let empty = RetrievalEvidence::default();
        assert!(check_edit_allowed(&empty, &["src/lib.rs"]).is_err());
    }
}
