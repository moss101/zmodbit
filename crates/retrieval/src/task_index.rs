//! The task-scoped repository index (Future-tasks Phase 3 item 1):
//! `RepositoryIndex` (exact/regex/path, M3.1) + `MerkleIndex`
//! (REQ-EV-0004) over one worktree, bound to the workspace revision.
//! Built once at task start by walking the real worktree, then kept
//! fresh incrementally: a delta only re-reads and recomputes the changed
//! leaves and their ancestor dir chain — never a full rebuild.

use crate::merkle::{MerkleIndex, Recomputed};
use crate::walker::{is_indexable, walk_worktree};
use crate::RepositoryIndex;
use std::collections::BTreeMap;
use std::path::Path;

/// One change to fold into the index (REQ-EV-0004). The scheduler converts
/// these from the workspace change journal (the canonical FS delta source);
/// `deleted` is a journal tombstone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexChange {
    pub path: String,
    pub deleted: bool,
}

/// Task-scoped index over one worktree: content-identity Merkle tree +
/// queryable exact/regex/path index, both bound to the workspace revision
/// the builder last saw.
#[derive(Debug)]
pub struct TaskIndex {
    pub merkle: MerkleIndex,
    pub repo: RepositoryIndex,
    pub indexed_workspace_revision: u64,
}

impl TaskIndex {
    /// Cold build (task start): walk the real worktree, then build both
    /// index halves over the same file set at the given revision.
    pub fn build_at(root: &Path, workspace_revision: u64) -> Self {
        let (files, _stats) = walk_worktree(root);
        let owned: BTreeMap<String, Vec<u8>> = files
            .into_iter()
            .map(|f| (f.path, f.bytes))
            .collect();
        let mut repo = RepositoryIndex::new(workspace_revision);
        for (path, bytes) in &owned {
            repo.index_file(path, bytes, workspace_revision);
        }
        TaskIndex {
            merkle: MerkleIndex::build(&owned, workspace_revision),
            repo,
            indexed_workspace_revision: workspace_revision,
        }
    }

    /// Incremental refresh (REQ-EV-0004 / QUAL-EV-0004): folds the changes
    /// into both index halves and recomputes ONLY the affected leaves and
    /// their ancestor dir chain. Current bytes are read from the worktree;
    /// a changed path that is now missing, unreadable, or no longer
    /// indexable (grew past the cap, turned binary, entered a policy dir)
    /// is evicted from the index. Returns the recomputation evidence.
    pub fn apply_delta(
        &mut self,
        root: &Path,
        changes: &[IndexChange],
        new_revision: u64,
    ) -> Vec<Recomputed> {
        // Latest state per path wins when a path changed several times
        // since the last refresh.
        let mut latest: BTreeMap<&str, bool> = BTreeMap::new();
        for change in changes {
            latest.insert(change.path.as_str(), change.deleted);
        }
        let mut changed: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        let mut deleted: Vec<String> = Vec::new();
        for (path, tombstoned) in latest {
            if tombstoned {
                deleted.push(path.to_string());
                continue;
            }
            match std::fs::read(root.join(path)) {
                Ok(bytes) if is_indexable(&bytes) => {
                    changed.insert(path.to_string(), bytes);
                }
                _ => deleted.push(path.to_string()),
            }
        }
        let recomputed = self
            .merkle
            .apply_changes(&changed, &deleted, new_revision);
        for (path, bytes) in &changed {
            self.repo.index_file(path, bytes, new_revision);
        }
        for path in &deleted {
            self.repo.remove_file(path);
        }
        self.indexed_workspace_revision = new_revision;
        self.repo.workspace_revision = new_revision;
        recomputed
    }

    pub fn root_digest(&self) -> String {
        self.merkle.root_digest()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tidx-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/deep")).unwrap();
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::write(dir.join("src/a.rs"), b"fn a() {}\n").unwrap();
        std::fs::write(dir.join("src/deep/c.rs"), b"fn c() {}\n").unwrap();
        std::fs::write(dir.join("docs/readme.md"), b"# docs\n").unwrap();
        dir
    }

    /// Full QUAL-EV-0004 loop over a REAL directory: build → modify one
    /// file + create one + delete one → incremental refresh recomputes
    /// only the affected segments and the root equals a full rebuild.
    #[test]
    fn incremental_delta_matches_full_rebuild_on_real_tree() {
        let root = scratch("eq");
        let mut index = TaskIndex::build_at(&root, 1);
        let digest_v1 = index.root_digest();
        assert_eq!(index.repo.files.len(), 3, "every walked file indexed");
        assert_eq!(index.repo.exact("fn a()").len(), 1);

        // Mutate the tree the way the run would: real FS writes.
        std::fs::write(root.join("src/b.rs"), b"fn b() {}\n").unwrap();
        std::fs::write(root.join("src/deep/c.rs"), b"fn c_v2() {}\n").unwrap();
        std::fs::remove_file(root.join("docs/readme.md")).unwrap();

        let changes = vec![
            IndexChange { path: "src/b.rs".into(), deleted: false },
            IndexChange { path: "src/deep/c.rs".into(), deleted: false },
            IndexChange { path: "docs/readme.md".into(), deleted: true },
        ];
        let recomputed = index.apply_delta(&root, &changes, 2);

        // Evidence: each touched leaf + only its ancestor chain. The docs
        // dir was EMPTIED by the tombstone, so it appears as pruned.
        assert!(recomputed.iter().any(|r| matches!(r,
            Recomputed::FileLeaf { path } if path == "src/deep/c.rs")));
        assert!(recomputed.iter().any(|r| matches!(r,
            Recomputed::DirNode { dir } if dir == "src/deep")));
        assert!(recomputed.iter().any(|r| matches!(r,
            Recomputed::DirNode { dir } if dir == "docs")),
            "the emptied docs dir is pruned with evidence");
        assert!(!index.merkle.dirs.contains_key("docs"), "pruned dir carries no digest");
        assert_eq!(index.indexed_workspace_revision, 2, "revision-correct");

        // Incremental root == full rebuild over the same final tree.
        let rebuilt = TaskIndex::build_at(&root, 2);
        assert_ne!(index.root_digest(), digest_v1);
        assert_eq!(
            index.root_digest(),
            rebuilt.root_digest(),
            "incremental refresh equals full rebuild"
        );
        assert_eq!(index.repo.files.len(), 3);
        assert!(index.repo.path("b.rs").len() == 1, "new file is queryable");
        assert!(index.repo.path("readme").is_empty(), "deleted file evicted");

        // A delta with no effective change recomputes nothing.
        let same = vec![IndexChange { path: "src/b.rs".into(), deleted: false }];
        assert!(index.apply_delta(&root, &same, 3).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// QUAL-EV-0004 large-repo shape: thousands of files, one edit — the
    /// evidence names ONLY the edited leaf and its ancestor chain.
    #[test]
    fn large_repo_edit_touches_only_the_affected_chain() {
        let root = scratch("large");
        let fanout = 60u32;
        for i in 0..fanout {
            for j in 0..fanout {
                let dir = root.join(format!("pkg{i}/mod{j}"));
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join("lib.rs"), format!("pub fn f{i}_{j}() {{}}\n")).unwrap();
            }
        }
        let mut index = TaskIndex::build_at(&root, 1);
        assert_eq!(index.repo.files.len(), (fanout * fanout + 3) as usize);

        std::fs::write(root.join("pkg7/mod11/lib.rs"), b"pub fn edited() {}\n").unwrap();
        let recomputed = index.apply_delta(
            &root,
            &[IndexChange { path: "pkg7/mod11/lib.rs".into(), deleted: false }],
            2,
        );
        let dirs: Vec<&str> = recomputed
            .iter()
            .filter_map(|r| match r {
                Recomputed::DirNode { dir } => Some(dir.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            dirs,
            vec!["pkg7/mod11", "pkg7"],
            "only the edited file's ancestor chain: {dirs:?}"
        );
        assert_eq!(
            recomputed.len(),
            3,
            "one leaf + two dir nodes, nothing else recomputed"
        );

        let rebuilt = TaskIndex::build_at(&root, 2);
        assert_eq!(index.root_digest(), rebuilt.root_digest());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A path that is no longer indexable (binary now, or gone) is evicted
    /// even without a journal tombstone — the index never serves stale
    /// bytes for an evicted file (QUAL-EV-0002 freshness rule).
    #[test]
    fn unindexable_and_missing_paths_are_evicted() {
        let root = scratch("evict");
        let mut index = TaskIndex::build_at(&root, 1);
        std::fs::write(root.join("docs/readme.md"), [0u8, 1, 2]).unwrap(); // binary now
        std::fs::remove_file(root.join("src/a.rs")).unwrap(); // gone, no tombstone
        let changes = vec![
            IndexChange { path: "docs/readme.md".into(), deleted: false },
            IndexChange { path: "src/a.rs".into(), deleted: false },
        ];
        index.apply_delta(&root, &changes, 2);
        let rebuilt = TaskIndex::build_at(&root, 2);
        assert_eq!(
            index.root_digest(),
            rebuilt.root_digest(),
            "evictions match a cold rebuild of the same tree"
        );
        assert!(index.repo.path("readme").is_empty());
        assert!(index.repo.path("a.rs").is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
