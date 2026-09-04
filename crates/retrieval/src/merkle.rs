use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// One recomputation event, for the "only affected segments" proof.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recomputed {
    FileLeaf { path: String },
    DirNode { dir: String },
}

/// The Merkle index over the repository tree. `""` is the root pseudo-dir
/// whose digest is the tree root. Incremental updates and full rebuilds
/// share the same bottom-up `compute_dir` fold, so their roots are
/// identical by construction.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MerkleIndex {
    /// path -> content digest (the file leaves).
    pub leaves: BTreeMap<String, String>,
    /// directory path -> canonical subtree digest.
    pub dirs: BTreeMap<String, String>,
    /// Repository revision the root digest is bound to.
    pub revision: u64,
}

pub fn dir_of(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, _)) => dir.to_string(),
        None => String::new(),
    }
}

fn combine(dir: &str, children: &[(String, String)]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(dir.as_bytes());
    for (name, digest) in children {
        hasher.update(b"\x00");
        hasher.update(name.as_bytes());
        hasher.update(b"\x00");
        hasher.update(digest.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

/// All ancestor dirs of `path`, nearest first.
fn ancestor_chain(path: &str) -> Vec<String> {
    let mut chain = Vec::new();
    let mut current = dir_of(path);
    while !current.is_empty() {
        chain.push(current.clone());
        current = dir_of(&current);
    }
    chain
}

impl MerkleIndex {
    /// Canonical digest of one directory from direct file leaves and
    /// already-computed child dir digests.
    fn compute_dir(&self, dir: &str) -> String {
        let mut children: Vec<(String, String)> = self
            .leaves
            .iter()
            .filter(|(p, _)| dir_of(p) == dir)
            .map(|(p, d)| (p.rsplit('/').next().unwrap_or(p).to_string(), d.clone()))
            .collect();
        for (sub, digest) in &self.dirs {
            if dir_of(sub) == dir {
                let name = sub.rsplit('/').next().unwrap_or(sub).to_string();
                children.push((name, digest.clone()));
            }
        }
        children.sort();
        combine(dir, &children)
    }

    /// Full build (initial indexing): bottom-up over every directory,
    /// deepest first.
    pub fn build(files: &BTreeMap<String, Vec<u8>>, revision: u64) -> Self {
        let mut index = Self {
            revision,
            ..Default::default()
        };
        for (path, bytes) in files {
            index.leaves.insert(path.clone(), sha256_hex(bytes));
        }
        let mut dirs: Vec<String> = index
            .leaves
            .keys()
            .flat_map(|p| {
                let mut chain = ancestor_chain(p);
                chain.push(dir_of(p));
                chain
            })
            .collect();
        dirs.sort();
        dirs.dedup();
        dirs.reverse(); // deepest first
        for dir in dirs {
            let digest = index.compute_dir(&dir);
            index.dirs.insert(dir, digest);
        }
        index
    }

    /// The tree root: canonical digest over top-level files + top-level
    /// dir nodes (computed on demand — the root pseudo-dir is "" and is
    /// never materialized in `dirs`).
    pub fn root_digest(&self) -> String {
        self.compute_dir("")
    }

    /// INCREMENTAL update (REQ-EV-0004): applies changed/deleted files and
    /// recomputes only the affected leaves and their ancestor dir chain.
    /// Returns the recomputation evidence.
    pub fn apply_changes(
        &mut self,
        changed: &BTreeMap<String, Vec<u8>>,
        deleted: &[String],
        new_revision: u64,
    ) -> Vec<Recomputed> {
        let mut recomputed = Vec::new();
        let mut touched: Vec<String> = Vec::new();

        for path in deleted {
            if self.leaves.remove(path).is_some() {
                recomputed.push(Recomputed::FileLeaf { path: path.clone() });
            }
            touched.extend(ancestor_chain(path));
            touched.push(dir_of(path));
        }
        for (path, bytes) in changed {
            let digest = sha256_hex(bytes);
            let unchanged = self.leaves.get(path) == Some(&digest);
            self.leaves.insert(path.clone(), digest);
            if !unchanged {
                recomputed.push(Recomputed::FileLeaf { path: path.clone() });
            }
            touched.extend(ancestor_chain(path));
            touched.push(dir_of(path));
        }

        touched.sort();
        touched.dedup();
        touched.reverse(); // deepest first
        for dir in &touched {
            let digest = self.compute_dir(dir);
            if self.dirs.get(dir) != Some(&digest) {
                self.dirs.insert(dir.clone(), digest);
                recomputed.push(Recomputed::DirNode { dir: dir.clone() });
            }
        }

        self.revision = new_revision;
        recomputed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> BTreeMap<String, Vec<u8>> {
        BTreeMap::from([
            ("src/a.rs".to_string(), b"fn a() {}\n".to_vec()),
            ("src/b.rs".to_string(), b"fn b() {}\n".to_vec()),
            ("src/deep/c.rs".to_string(), b"fn c() {}\n".to_vec()),
            ("docs/readme.md".to_string(), b"# docs\n".to_vec()),
        ])
    }

    fn tree_v2() -> BTreeMap<String, Vec<u8>> {
        BTreeMap::from([
            ("src/a.rs".to_string(), b"fn a() {}\n".to_vec()),
            ("src/b.rs".to_string(), b"fn b_edited() {}\n".to_vec()),
            ("src/deep/c.rs".to_string(), b"fn c() {}\n".to_vec()),
            ("docs/readme.md".to_string(), b"# docs\n".to_vec()),
        ])
    }

    /// QUAL-EV-0004: an incremental edit updates only affected index
    /// segments, stays revision-correct, and equals a full rebuild.
    #[test]
    fn incremental_edit_updates_only_affected_segments() {
        let mut index = MerkleIndex::build(&tree(), 1);
        let root_v1 = index.root_digest();
        assert!(!root_v1.is_empty());

        // Change ONE file; everything else stays byte-identical.
        let mut changed = BTreeMap::new();
        changed.insert("src/b.rs".to_string(), b"fn b_edited() {}\n".to_vec());
        let recomputed = index.apply_changes(&changed, &[], 2);

        // Evidence: the file leaf plus ONLY its ancestor chain segments.
        assert!(matches!(
            recomputed.first(),
            Some(Recomputed::FileLeaf { path }) if path == "src/b.rs"
        ));
        assert!(recomputed
            .iter()
            .any(|r| matches!(r, Recomputed::DirNode { dir } if dir == "src")));
        // No unrelated segment (docs, src/deep) was recomputed.
        assert!(!recomputed.iter().any(
            |r| matches!(r, Recomputed::DirNode { dir } if dir == "docs" || dir.contains("deep"))
        ));
        // Unchanged leaves kept their digests.
        assert_eq!(
            index.leaves.get("src/a.rs"),
            Some(&sha256_hex(b"fn a() {}\n"))
        );
        assert_eq!(index.revision, 2, "revision-correct");

        // Incremental root equals a FULL rebuild over the same tree.
        let root_v2 = index.root_digest();
        let rebuilt = MerkleIndex::build(&tree_v2(), 2);
        assert_ne!(root_v2, root_v1);
        assert_eq!(
            root_v2,
            rebuilt.root_digest(),
            "incremental == full rebuild"
        );

        // A no-op change (same bytes) recomputes nothing.
        let mut noop = BTreeMap::new();
        noop.insert("src/b.rs".to_string(), b"fn b_edited() {}\n".to_vec());
        assert!(index.apply_changes(&noop, &[], 3).is_empty());

        // Deletion updates the affected segment.
        let recomputed = index.apply_changes(&BTreeMap::new(), &["docs/readme.md".to_string()], 4);
        assert!(recomputed
            .iter()
            .any(|r| matches!(r, Recomputed::FileLeaf { path } if path == "docs/readme.md")));
        assert!(!index.leaves.contains_key("docs/readme.md"));
    }
}
