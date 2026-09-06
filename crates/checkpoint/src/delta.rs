//! Checkpoint delta journal (M4, REQ-EV-0013): a checkpoint records a
//! worktree BASELINE plus ordered deltas and the runtime/evidence cursors
//! — never transcript snapshots. Restore reconstructs the edited worktree
//! and cursors exactly after a process restart.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One worktree mutation relative to the previous state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorktreeDelta {
    pub path: String,
    /// Full new content, or None for deletion (bounded deltas: paths are
    /// whole-file for the M4 slice).
    pub content: Option<Vec<u8>>,
}

/// Runtime/evidence cursor pair captured with each checkpoint.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Cursors {
    /// Position in the runtime event stream.
    pub runtime_seq: u64,
    /// Position in the evidence stream.
    pub evidence_seq: u64,
}

/// The durable checkpoint journal: baseline + ordered deltas + cursors,
/// plus the per-surface cursor registry (M4.5: terminal/browser/sandbox
/// reattachment metadata captured through the one unified contract).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DeltaJournal {
    pub baseline: BTreeMap<String, Vec<u8>>,
    pub deltas: Vec<WorktreeDelta>,
    pub cursors: Cursors,
    /// Surface cursor metadata (crate::cursor_meta::CursorMeta) captured
    /// at checkpoint time. Absent in pre-M4.5 journals (serde default).
    #[serde(default)]
    pub surfaces: Vec<crate::cursor_meta::CursorMeta>,
}

#[derive(Debug)]
pub enum JournalError {
    MissingBaseline { path: String },
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JournalError::MissingBaseline { path } => {
                write!(f, "delta for {path:?} has no baseline entry")
            }
        }
    }
}

impl std::error::Error for JournalError {}

/// The restored state after replay.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RestoredState {
    pub files: BTreeMap<String, Vec<u8>>,
    pub cursors: Cursors,
}

impl DeltaJournal {
    /// Creates a journal from a full baseline snapshot.
    pub fn from_baseline(files: &BTreeMap<String, Vec<u8>>, cursors: Cursors) -> Self {
        Self {
            baseline: files.clone(),
            deltas: Vec::new(),
            cursors,
            surfaces: Vec::new(),
        }
    }

    /// Appends a worktree delta.
    pub fn push_delta(&mut self, delta: WorktreeDelta) {
        self.deltas.push(delta);
    }

    /// Updates the cursors recorded with this checkpoint.
    pub fn set_cursors(&mut self, runtime_seq: u64, evidence_seq: u64) {
        self.cursors = Cursors {
            runtime_seq,
            evidence_seq,
        };
    }

    /// RESTORE (QUAL-EV-0013): baseline + deltas → exact worktree and
    /// cursors after a process restart. Deletions (None content) remove
    /// the file; edits replace content.
    pub fn restore(&self) -> Result<RestoredState, JournalError> {
        let mut files = self.baseline.clone();
        for delta in &self.deltas {
            match &delta.content {
                Some(bytes) => {
                    files.insert(delta.path.clone(), bytes.clone());
                }
                None => {
                    // Deletion must target a known file.
                    if files.remove(&delta.path).is_none() {
                        return Err(JournalError::MissingBaseline {
                            path: delta.path.clone(),
                        });
                    }
                }
            }
        }
        Ok(RestoredState {
            files,
            cursors: self.cursors.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0013: restore the edited worktree and protocol cursor
    /// after a process restart from baseline+delta.
    #[test]
    fn restore_reconstructs_worktree_and_cursors_from_baseline_plus_delta() {
        // Baseline at checkpoint time.
        let mut baseline = BTreeMap::new();
        baseline.insert("src/lib.rs".to_string(), b"fn old() {}\n".to_vec());
        baseline.insert("src/retry.rs".to_string(), b"fn retry() {}\n".to_vec());
        baseline.insert("docs/notes.md".to_string(), b"notes\n".to_vec());

        let mut journal = DeltaJournal::from_baseline(
            &baseline,
            Cursors {
                runtime_seq: 10,
                evidence_seq: 4,
            },
        );

        // The task edits lib.rs, creates a NEW file, deletes notes.md.
        journal.push_delta(WorktreeDelta {
            path: "src/lib.rs".into(),
            content: Some(b"fn new_implementation() {}\n".to_vec()),
        });
        journal.push_delta(WorktreeDelta {
            path: "src/created.rs".into(),
            content: Some(b"fn created() {}\n".to_vec()),
        });
        journal.push_delta(WorktreeDelta {
            path: "docs/notes.md".into(),
            content: None,
        });
        journal.set_cursors(23, 9);

        // "Process restart": the journal is the ONLY durable artifact.
        let durable = serde_json::to_vec(&journal).unwrap();
        let journal: DeltaJournal = serde_json::from_slice(&durable).unwrap();

        let restored = journal.restore().unwrap();
        assert_eq!(
            restored.files.get("src/lib.rs").unwrap(),
            b"fn new_implementation() {}\n",
            "edit reproduced"
        );
        assert!(
            restored.files.contains_key("src/created.rs"),
            "creation reproduced"
        );
        assert!(
            !restored.files.contains_key("docs/notes.md"),
            "deletion reproduced"
        );
        assert_eq!(
            restored.files.get("src/retry.rs").unwrap(),
            b"fn retry() {}\n"
        );
        // Cursors restored exactly.
        assert_eq!(restored.cursors.runtime_seq, 23);
        assert_eq!(restored.cursors.evidence_seq, 9);

        // Deleting an unknown path is a typed error.
        let mut bad = DeltaJournal::from_baseline(&BTreeMap::new(), Cursors::default());
        bad.push_delta(WorktreeDelta {
            path: "ghost.rs".into(),
            content: None,
        });
        assert!(matches!(
            bad.restore(),
            Err(JournalError::MissingBaseline { .. })
        ));
    }
}
