//! Provenance-bound dirty-state snapshots (M2, REQ-EV-0022): a dirty local
//! workspace is captured as a temporary commit on a `refs/modbit/snapshot/`
//! ref — the user's branch, worktree, and index are left untouched. The
//! snapshot carries provenance (task, machine, origin, base commit) so a
//! remote ("cloud") run can reconstruct the dirty state exactly, and cleanup
//! only ever removes refs inside the modbit snapshot namespace.

use super::{GitError, GitRepo};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Namespace that is exclusively modbit's. Cleanup refuses to touch
/// anything outside it.
pub const SNAPSHOT_NAMESPACE: &str = "refs/modbit/snapshot/";

/// Who/what produced the snapshot and where it belongs. Bound into the
/// snapshot commit message and carried on the handle so a remote run can
/// verify it is applying the right snapshot for the right task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotProvenance {
    pub task_id: String,
    pub machine_id: String,
    /// Where the snapshot was taken (e.g. "local-workspace").
    pub origin: String,
    /// Unix epoch milliseconds at creation.
    pub created_at_ms: i64,
}

impl SnapshotProvenance {
    pub fn new(task_id: &str, machine_id: &str, origin: &str) -> Self {
        Self {
            task_id: task_id.to_string(),
            machine_id: machine_id.to_string(),
            origin: origin.to_string(),
            created_at_ms: now_ms(),
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Handle to a snapshot ref. `tree` is the content digest of the captured
/// state — a remote run proves exact reconstruction by reproducing this
/// tree after applying the snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotHandle {
    pub ref_name: String,
    pub commit: String,
    /// Git tree digest of the captured dirty state.
    pub tree: String,
    /// Commit the snapshot was taken on top of.
    pub base_commit: String,
    pub provenance: SnapshotProvenance,
}

fn sanitize_id(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "task".to_string()
    } else {
        cleaned
    }
}

fn require_snapshot_ref(operation: &str, ref_name: &str) -> Result<(), GitError> {
    if !ref_name.starts_with(SNAPSHOT_NAMESPACE)
        || ref_name[SNAPSHOT_NAMESPACE.len()..].contains("//")
        || ref_name.contains("..")
    {
        return Err(GitError::Git {
            operation: operation.to_string(),
            message: format!("ref {ref_name} outside the modbit snapshot namespace"),
        });
    }
    Ok(())
}

impl GitRepo {
    /// Captures the dirty state (staged + unstaged changes, plus untracked
    /// files) as a temporary commit on `refs/modbit/snapshot/<id>` WITHOUT
    /// moving the current branch or disturbing the index. Fails clean: if
    /// there is nothing to snapshot, no ref is created and the index is
    /// restored.
    pub fn create_snapshot(
        &self,
        provenance: &SnapshotProvenance,
    ) -> Result<SnapshotHandle, GitError> {
        let base_commit = self.head()?;

        // Preserve the caller's index so snapshotting is invisible to the
        // user's in-progress staging.
        let orig_index_tree = Self::stdout_text(&self.git("write-tree", &[])?);

        self.git("add", &["-A"])?;
        let dirty_tree = Self::stdout_text(&self.git("write-tree", &[])?);

        // Restore the caller's index now that we have the dirty tree.
        self.git("read-tree", &[&orig_index_tree])?;

        let head_tree = Self::stdout_text(&self.git("rev-parse", &["HEAD^{tree}"])?);
        if dirty_tree == head_tree {
            return Err(GitError::Git {
                operation: "create_snapshot".into(),
                message: "nothing to snapshot".into(),
            });
        }

        let short_tree: String = dirty_tree.chars().take(8).collect();
        let snapshot_id = format!("{}-{}", sanitize_id(&provenance.task_id), short_tree);
        let ref_name = format!("{SNAPSHOT_NAMESPACE}{snapshot_id}");
        let message = format!(
            "modbit snapshot: task={} machine={} origin={} at={}",
            provenance.task_id, provenance.machine_id, provenance.origin, provenance.created_at_ms
        );
        let commit = Self::stdout_text(&self.git(
            "commit-tree",
            &[&dirty_tree, "-p", &base_commit, "-m", &message],
        )?);
        self.git("update-ref", &[&ref_name, &commit])?;

        Ok(SnapshotHandle {
            ref_name,
            commit,
            tree: dirty_tree,
            base_commit,
            provenance: provenance.clone(),
        })
    }

    /// Remote (cloud) side: fetches the snapshot ref from the local
    /// repository path into this repository. Only refs inside the modbit
    /// snapshot namespace are transferable through this API.
    pub fn fetch_snapshot(
        &self,
        from_repo: &Path,
        handle: &SnapshotHandle,
    ) -> Result<(), GitError> {
        require_snapshot_ref("fetch_snapshot", &handle.ref_name)?;
        let from = from_repo.to_string_lossy().to_string();
        let spec = format!("{}:{}", handle.ref_name, handle.ref_name);
        self.git("fetch", &[&from, &spec])?;
        Ok(())
    }

    /// Reconstructs the snapshot's dirty state in this repository's
    /// worktree, exactly: files added/modified by the snapshot are written,
    /// files the snapshot deleted (relative to base) are removed. The
    /// reconstruction is verifiable — `verify_snapshot` must reproduce the
    /// handle's tree digest.
    pub fn restore_snapshot(&self, handle: &SnapshotHandle) -> Result<(), GitError> {
        require_snapshot_ref("restore_snapshot", &handle.ref_name)?;
        // Index := snapshot tree, then materialize every index entry.
        self.git("read-tree", &[&handle.commit])?;
        self.git("checkout-index", &["-a", "-f"])?;

        // Files the snapshot deleted relative to base are present in HEAD
        // but absent from the snapshot tree: remove them from the worktree.
        let deleted = self.git(
            "diff-tree",
            &[
                "-r",
                "--name-only",
                "--diff-filter=D",
                "HEAD",
                &handle.commit,
            ],
        )?;
        for path in Self::stdout_text(&deleted)
            .lines()
            .filter(|l| !l.is_empty())
        {
            let p = self.root.join(path);
            if p.is_file() {
                std::fs::remove_file(&p).map_err(GitError::Io)?;
            }
        }
        Ok(())
    }

    /// Proves exact reconstruction: re-derives the worktree-as-tree digest
    /// and compares it to the snapshot's tree. `Ok(true)` means the remote
    /// state matches the local dirty state byte-for-byte. The index is left
    /// at HEAD afterwards so the caller decides the next step.
    pub fn verify_snapshot(&self, handle: &SnapshotHandle) -> Result<bool, GitError> {
        self.git("add", &["-A"])?;
        let actual = Self::stdout_text(&self.git("write-tree", &[])?);
        let _ = self.git("reset", &["-q", "--mixed", "HEAD"]);
        Ok(actual == handle.tree)
    }

    /// Removes the temporary snapshot ref. SAFETY: refuses any ref outside
    /// `refs/modbit/snapshot/` — cleanup can never delete user branches,
    /// tags, or other refs.
    pub fn cleanup_snapshot(&self, handle: &SnapshotHandle) -> Result<(), GitError> {
        require_snapshot_ref("cleanup_snapshot", &handle.ref_name)?;
        self.git("update-ref", &["-d", &handle.ref_name])?;
        // Confirm it is really gone.
        let gone = self.git(
            "rev-parse",
            &["--verify", &format!("{}^{{commit}}", handle.ref_name)],
        );
        if gone.is_ok() {
            return Err(GitError::Git {
                operation: "cleanup_snapshot".into(),
                message: format!("ref {} still present after delete", handle.ref_name),
            });
        }
        Ok(())
    }

    /// Removes every leftover snapshot ref (e.g. after a task completes).
    /// Same namespace safety as `cleanup_snapshot`.
    pub fn cleanup_all_snapshots(&self) -> Result<usize, GitError> {
        let out = self.git("for-each-ref", &["--format=%(refname)", SNAPSHOT_NAMESPACE])?;
        let refs: Vec<String> = Self::stdout_text(&out)
            .lines()
            .filter(|l| l.starts_with(SNAPSHOT_NAMESPACE))
            .map(String::from)
            .collect();
        let count = refs.len();
        for r in refs {
            self.git("update-ref", &["-d", &r])?;
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Snapshot ids are sanitized into safe ref components.
    #[test]
    fn snapshot_ids_are_sanitized() {
        assert_eq!(sanitize_id("task-42"), "task-42");
        assert_eq!(sanitize_id("../../etc"), "------etc");
        assert_eq!(sanitize_id(""), "task");
    }
}
