//! Read-only worktree walker (Future-tasks Phase 3 item 1, docs/18 §
//! Index inputs): decides WHICH files the repository index sees. `.gitignore`
//! (global, per-repo, per-dir, excludes) is respected, hidden entries and
//! symlinks are never traversed, and generated/vendor paths are
//! policy-tagged and not indexed by default (docs/18: "Generated/vendor/
//! binary paths are policy-tagged and not indexed by default").
//!
//! The walker output is the single input to the initial Merkle build, and
//! the same indexability predicate (`is_indexable`) drives every
//! incremental refresh (task_index) — so an incrementally maintained
//! index and a full rebuild over the same tree agree by construction.

use std::collections::BTreeMap;
use std::path::Path;

/// Per-file content cap for indexing (byte length). Matches the search
/// tool's bound: a larger file is not indexed at all.
pub const MAX_INDEXED_BYTES: u64 = 256 * 1024;

/// One indexed file: worktree-relative path (forward slashes) + bytes.
pub struct IndexedFile {
    pub path: String,
    pub bytes: Vec<u8>,
}

/// What the walker saw (evidence for the index-build event; the counts
/// make the index policy visible instead of silent).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WalkStats {
    pub indexed: usize,
    /// Committed generated/vendor entries, pruned by policy.
    pub skipped_policy: usize,
    /// NUL byte inside the sniff window.
    pub skipped_binary: usize,
    /// Larger than MAX_INDEXED_BYTES.
    pub skipped_size: usize,
}

/// Directory names that are generated/vendor output and are not indexed
/// even when committed (docs/18 policy tags). The `ignore` crate already
/// prunes anything gitignored; this list prunes the same trees when they
/// are checked in.
fn policy_tagged(name: &str) -> bool {
    matches!(
        name,
        "target" | "node_modules" | "dist" | "vendor" | "vendors" | "vendored"
    )
}

/// The shared indexability predicate: byte-level checks applied
/// identically on the cold walk and on every incremental refresh, so a
/// file can only move in or out of the index through the same rule both
/// paths use.
pub fn is_indexable(bytes: &[u8]) -> bool {
    !bytes.iter().take(8192).any(|b| *b == 0)
}

/// Walks `root` and returns the files to index, sorted by path for a
/// deterministic order (the Merkle fold is order-independent, but the
/// build evidence and tests rely on stable iteration).
pub fn walk_worktree(root: &Path) -> (Vec<IndexedFile>, WalkStats) {
    let mut stats = WalkStats::default();
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .ignore(true)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        // Ignore files govern the index policy even outside a git repo
        // (the policy must not depend on where the tree happens to sit).
        .require_git(false)
        .parents(true)
        .follow_links(false)
        .build();
    for entry in walker.flatten() {
        let path = entry.into_path();
        let Ok(rel) = path.strip_prefix(root) else { continue };
        if rel.as_os_str().is_empty() {
            continue; // the root itself
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
        if meta.is_dir() {
            // The iterator descends into committed generated/vendor dirs;
            // their files are rejected by the per-component check below.
            // (Gitignored dirs never reach us at all.)
            continue;
        }
        if !meta.is_file() {
            continue; // symlinks are never followed
        }
        if rel
            .components()
            .any(|c| c.as_os_str().to_str().is_some_and(policy_tagged))
        {
            stats.skipped_policy += 1;
            continue;
        }
        if meta.len() > MAX_INDEXED_BYTES {
            stats.skipped_size += 1;
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else { continue };
        if !is_indexable(&bytes) {
            stats.skipped_binary += 1;
            continue;
        }
        let rel_display = rel.to_string_lossy().replace('\\', "/");
        files.insert(rel_display, bytes);
        stats.indexed += 1;
    }
    let indexed = files
        .into_iter()
        .map(|(path, bytes)| IndexedFile { path, bytes })
        .collect();
    (indexed, stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, content: &[u8]) {
        let full = root.join(rel);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    }

    /// .gitignore respect (root + nested), negation, hidden skip, policy
    /// dirs pruned even when not ignored, binary sniff and size cap.
    #[test]
    fn walker_respects_gitignore_hidden_policy_binary_and_cap() {
        let root = std::env::temp_dir().join(format!(
            "walker-it-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        write(&root, ".gitignore", b"ignored.txt\nbuild/\n");
        write(&root, "src/main.rs", b"fn main() {}\n");
        write(&root, "src/util.rs", b"pub fn u() {}\n");
        write(&root, "ignored.txt", b"root-level ignore\n");
        write(&root, "src/ignored.txt", b"nested-level ignore\n");
        write(&root, "build/out.rs", b"generated\n");
        write(&root, "docs/readme.md", b"# doc\n");
        write(&root, "docs/.gitignore", b"secret.md\n");
        write(&root, "docs/secret.md", b"nested ignore hit\n");
        write(&root, "node_modules/pkg/index.js", b"committed vendor tree\n");
        write(&root, ".modbit/changes.jsonl", b"{}\n");
        // Negation: one whitelisted file overrides the drop rule.
        write(&root, "src/.gitignore", b"generated*.rs\n!generated_keep.rs\n");
        write(&root, "src/generated_drop.rs", b"generated\n");
        write(&root, "src/generated_keep.rs", b"kept by negation\n");
        // Binary (NUL in the sniff window) and oversized files.
        write(&root, "assets/logo.bin", &[0u8, 1, 2, 3, 4]);
        write(
            &root,
            "assets/big.txt",
            &vec![b'x'; (MAX_INDEXED_BYTES + 1) as usize],
        );

        let (files, stats) = walk_worktree(&root);
        let mut got: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        got.sort_unstable();
        assert_eq!(
            got,
            vec!["docs/readme.md", "src/generated_keep.rs", "src/main.rs", "src/util.rs"],
            "unexpected index set: {got:?}"
        );

        assert_eq!(stats.indexed, 4);
        assert_eq!(stats.skipped_policy, 1, "node_modules file pruned by policy");
        assert_eq!(stats.skipped_binary, 1, "NUL byte sniffed as binary");
        assert_eq!(stats.skipped_size, 1, "oversized file skipped");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The shared predicate: text is indexable, NUL bytes are not.
    #[test]
    fn indexability_predicate_is_stable() {
        assert!(is_indexable(b"plain text\n"));
        assert!(!is_indexable(&[0x00, 0x01]));
        assert!(is_indexable(&vec![b'a'; 8192]));
    }
}
