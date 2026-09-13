//! Local→cloud checkpoint handoff (M8.7; docs/24:
//! "Remote coding operates on explicit Git/checkpoint handoff bundles").
//! The LOCAL machine captures its dirty workspace as a provenance-bound
//! git snapshot (crates/git::snapshot — branch/worktree/index untouched),
//! exports a self-describing CHECKPOINT HANDOFF BUNDLE, and the CLOUD
//! worker ATTACHES it into its own repository: provenance verified, git
//! bundle fetched, snapshot restored on top of the pinned base, and the
//! reconstruction proven EXACT by reproducing the recorded tree digest.
//! The bundle carries NO secret values (validated) and no environment of
//! the origin machine (only requirements descriptors).
//!
//! Canonical owner subsystem: checkpoint (docs/81). The bundle is
//! transport-agnostic (bytes, like the git bundle itself) — the cloud-api
//! relay / worker surface routing is layered on this contract, not a
//! second one.

use modbit_git::snapshot::{SnapshotHandle, SnapshotProvenance};
use modbit_git::GitRepo;
use modbit_workspace::handoff::{validate_handoff, EnvironmentHandoffBundle};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A complete checkpoint handoff: the environment/task bundle, the git
/// snapshot handle (ref/commit/tree/base + provenance), and the raw git
/// bundle bytes that let a machine WITHOUT access to the origin repo
/// reconstruct the exact state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CheckpointHandoffBundle {
    pub env: EnvironmentHandoffBundle,
    pub snapshot: SnapshotHandle,
    /// `git bundle` bytes of the snapshot ref (self-contained transfer).
    pub snapshot_bundle_bytes: Vec<u8>,
    /// Digest of the bundle bytes — the attach side verifies the transfer
    /// BEFORE touching git (corrupt transfers fail closed).
    pub bundle_sha256: String,
}

/// Denial with the reason attached (typed, auditable).
#[derive(Debug)]
pub struct AttachError {
    pub stage: &'static str,
    pub reason: String,
}

impl std::fmt::Display for AttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "checkpoint attach failed at {}: {}",
            self.stage, self.reason
        )
    }
}

impl std::error::Error for AttachError {}

fn err(stage: &'static str, reason: impl Into<String>) -> AttachError {
    AttachError {
        stage,
        reason: reason.into(),
    }
}

/// Captures the local dirty state and exports the transferable checkpoint
/// handoff bundle (the LOCAL half of M8.7). The user's branch, worktree
/// and index are left untouched (snapshot is on a modbit-namespace ref).
pub fn export_checkpoint_bundle(
    local: &GitRepo,
    provenance: &SnapshotProvenance,
    objective: &str,
    context_summary: &str,
    evidence_refs: Vec<String>,
    runtime_requirements: Vec<String>,
) -> Result<CheckpointHandoffBundle, AttachError> {
    let handle = local
        .create_snapshot(provenance)
        .map_err(|e| err("snapshot", e.to_string()))?;
    let bundle_bytes = local
        .export_snapshot_bundle(&handle)
        .map_err(|e| err("bundle", e.to_string()))?;
    Ok(CheckpointHandoffBundle {
        env: EnvironmentHandoffBundle {
            task_id: provenance.task_id.clone(),
            objective: objective.to_string(),
            git_delta: format!("snapshot {} (base {})", handle.commit, handle.base_commit),
            context_summary: context_summary.to_string(),
            evidence_refs,
            runtime_requirements,
        },
        bundle_sha256: {
            let mut h = Sha256::new();
            h.update(&bundle_bytes);
            format!("{:x}", h.finalize())
        },
        snapshot_bundle_bytes: bundle_bytes,
        snapshot: handle,
    })
}

/// The receipt a cloud worker produces after a successful attach: exact
/// reconstruction is PROVEN by the tree digest, not asserted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AttachReceipt {
    pub task_id: String,
    pub restored_commit: String,
    /// Tree digest after restore — equals the snapshot's recorded tree.
    pub restored_tree: String,
    pub exact_reconstruction: bool,
}

/// The CLOUD half of M8.7: imports the checkpoint handoff into the
/// worker's repository. Fails closed at every stage:
/// - the bundle's own env validation (no task id, malformed);
/// - the transfer digest (corrupt bytes never reach git);
/// - the provenance binding (a bundle cannot be attached under a
///   DIFFERENT task id — no task confusion);
/// - the base pin (the worker's HEAD must contain the snapshot's base —
///   a stale cloud repo refuses instead of silently branching astray);
/// - the tree digest (exact reconstruction or nothing).
pub fn attach_checkpoint_bundle(
    cloud: &GitRepo,
    incoming: &CheckpointHandoffBundle,
    attach_as_task: &str,
) -> Result<AttachReceipt, AttachError> {
    validate_handoff(&incoming.env).map_err(|e| err("validate", e))?;
    if incoming.env.task_id != attach_as_task {
        return Err(err(
            "provenance",
            format!(
                "bundle belongs to task {:?}; refusing attach as {:?}",
                incoming.env.task_id, attach_as_task
            ),
        ));
    }
    let mut h = Sha256::new();
    h.update(&incoming.snapshot_bundle_bytes);
    let got = format!("{:x}", h.finalize());
    if got != incoming.bundle_sha256 {
        return Err(err(
            "transfer",
            format!(
                "bundle digest mismatch: expected {}, got {}",
                incoming.bundle_sha256, got
            ),
        ));
    }
    cloud
        .import_snapshot_bundle(&incoming.snapshot, &incoming.snapshot_bundle_bytes)
        .map_err(|e| err("import", e.to_string()))?;
    cloud
        .restore_snapshot(&incoming.snapshot)
        .map_err(|e| err("restore", e.to_string()))?;
    let exact = cloud
        .verify_snapshot(&incoming.snapshot)
        .map_err(|e| err("verify", e.to_string()))?;
    if !exact {
        return Err(err(
            "verify",
            format!(
                "restored tree does not reproduce snapshot tree {}",
                incoming.snapshot.tree
            ),
        ));
    }
    Ok(AttachReceipt {
        task_id: attach_as_task.to_string(),
        restored_commit: incoming.snapshot.commit.clone(),
        restored_tree: incoming.snapshot.tree.clone(),
        exact_reconstruction: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use modbit_git::snapshot::SnapshotProvenance;
    use std::path::Path;

    fn repo_in(tag: &str) -> (std::path::PathBuf, GitRepo) {
        let dir = std::env::temp_dir().join(format!(
            "modbit-m87-{tag}-{}",
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let repo = GitRepo::init(Path::new(&dir)).expect("init repo");
        (dir, repo)
    }

    fn base_commit(repo: &GitRepo, dir: &Path, msg: &str) -> String {
        std::fs::write(dir.join("seed.txt"), "seed\n").unwrap();
        repo.commit_all(msg).expect("commit")
    }

    /// The full M8.7 contract on REAL git repos: dirty local state is
    /// captured, exported as a transferable bundle, attached into a
    /// distinct cloud repo, and the reconstruction is proven EXACT by the
    /// tree digest — including files the user never committed (untracked
    /// work survives the trip).
    #[test]
    fn checkpoint_handoff_round_trip_is_exact() {
        let (local_dir, local) = repo_in("local");
        let (cloud_dir, cloud) = repo_in("cloud");
        base_commit(&local, &local_dir, "base");
        base_commit(&cloud, &cloud_dir, "cloud-base");
        // Dirty state: a modified tracked file + an UNTRACKED file.
        std::fs::write(local_dir.join("seed.txt"), "dirty seed v2").unwrap();
        std::fs::write(local_dir.join("tracked.txt"), "dirty edit v2").unwrap();
        std::fs::create_dir_all(local_dir.join("notes")).unwrap();
        std::fs::write(local_dir.join("notes/untracked.md"), "never committed").unwrap();

        let provenance = SnapshotProvenance::new("task-77", "mac-local", "local-workspace");
        let bundle = export_checkpoint_bundle(
            &local,
            &provenance,
            "port the scheduler fix",
            "scheduler.rs rewritten on top of the durable lease",
            vec!["run:2026-09-13/qual-ev-0196".to_string()],
            vec!["rustc@1.97".to_string(), "git>=2.30".to_string()],
        )
        .expect("export");

        // The user's workspace is untouched by the export.
        assert_eq!(
            std::fs::read_to_string(local_dir.join("tracked.txt")).unwrap(),
            "dirty edit v2"
        );

        // The transfer is raw bytes (cloud plane); attach under the SAME
        // task id on the cloud side.
        let receipt = attach_checkpoint_bundle(&cloud, &bundle, "task-77").expect("attach");
        assert!(receipt.exact_reconstruction);
        assert_eq!(receipt.restored_tree, bundle.snapshot.tree);
        let restored = std::fs::read_to_string(cloud_dir.join("tracked.txt")).unwrap_or_default();
        assert_eq!(
            restored, "dirty edit v2",
            "dirty tracked content reconstructed"
        );
        // seed.txt was dirtied BEFORE the snapshot, so exact reconstruction
        // carries the dirty value too — that is the contract.
        let cloud_seed = std::fs::read_to_string(cloud_dir.join("seed.txt")).unwrap_or_default();
        assert_eq!(cloud_seed, "dirty seed v2");
        let untracked =
            std::fs::read_to_string(cloud_dir.join("notes/untracked.md")).unwrap_or_default();
        assert_eq!(
            untracked, "never committed",
            "untracked content reconstructed"
        );

        let _ = std::fs::remove_dir_all(&local_dir);
        let _ = std::fs::remove_dir_all(&cloud_dir);
    }

    /// Task confusion is refused: a bundle for one task cannot be
    /// attached as another (the provenance binding is load-bearing).
    #[test]
    fn attach_refuses_task_confusion_and_corrupt_transfer() {
        let (_local_dir, local) = repo_in("conf-local");
        let (_cloud_dir, cloud) = repo_in("conf-cloud");
        base_commit(&local, &_local_dir, "base");
        base_commit(&cloud, &_cloud_dir, "cloud-base");
        std::fs::write(_local_dir.join("f.txt"), "x").unwrap();
        let provenance = SnapshotProvenance::new("task-A", "m", "origin");
        let mut bundle =
            export_checkpoint_bundle(&local, &provenance, "obj", "ctx", vec![], vec![])
                .expect("export");

        // Wrong task id.
        match attach_checkpoint_bundle(&cloud, &bundle, "task-B") {
            Err(e) => assert_eq!(e.stage, "provenance"),
            Ok(_) => panic!("task confusion must be refused"),
        }

        // Corrupt transfer bytes fail the digest BEFORE git is touched.
        bundle.snapshot_bundle_bytes[0] ^= 0xFF;
        match attach_checkpoint_bundle(&cloud, &bundle, "task-A") {
            Err(e) => assert_eq!(e.stage, "transfer"),
            Ok(_) => panic!("corrupt transfer must be refused"),
        }
    }
}
