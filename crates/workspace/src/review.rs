//! Per-hunk selective diff review + optimistic-concurrency revert (M2,
//! REQ-EV-0036 + REQ-EV-0065). The operator can accept or reject individual
//! hunks; a revert checks the expected current hash before applying the
//! inverse, so a concurrent edit blocks a destructive revert.

use crate::{PatchHunk, WorkspaceError, WorkspaceFileService};

/// A reviewed hunk with the operator's accept/reject decision.
#[derive(Clone, Debug)]
pub struct ReviewedHunk {
    pub hunk: PatchHunk,
    pub accepted: bool,
}

/// Applies only the accepted hunks from a review session.
/// Returns (path, new_revision) for each modified file.
pub fn apply_review(
    ws: &WorkspaceFileService,
    path: &str,
    expected_revision: u64,
    reviewed: &[ReviewedHunk],
) -> Result<Vec<(String, u64)>, WorkspaceError> {
    let accepted: Vec<&ReviewedHunk> = reviewed.iter().filter(|r| r.accepted).collect();
    if accepted.is_empty() {
        return Ok(vec![]);
    }
    let hunks: Vec<PatchHunk> = accepted.iter().map(|r| r.hunk.clone()).collect();
    let rev = ws.apply_patch(path, expected_revision, &hunks)?;
    Ok(vec![(path.to_string(), rev)])
}

/// Optimistic-concurrency revert (REQ-EV-0065): checks that the file's
/// current content hash matches `expected_current_hash` before restoring
/// `original_bytes`. A concurrent edit causes a hash mismatch and the
/// revert is rejected — preventing a destructive overwrite of someone
/// else's work.
pub fn safe_revert(
    ws: &WorkspaceFileService,
    path: &str,
    expected_current_hash: &str,
    original_bytes: &[u8],
) -> Result<(), WorkspaceError> {
    let (current_bytes, current_rev) = ws.read(path)?;
    let current_hash = WorkspaceFileService::sha256_hex(&current_bytes);
    if current_hash != expected_current_hash {
        return Err(WorkspaceError::StaleRevision {
            path: path.to_string(),
            expected: current_rev,
            actual: current_rev,
        });
    }
    let _ = original_bytes;
    ws.replace(path, original_bytes, current_rev)?;
    Ok(())
}
