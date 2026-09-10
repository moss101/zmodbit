//! The task-scoped repository index (Future-tasks Phase 3 item 1):
//! `RepositoryIndex` (exact/regex/path, M3.1) + `MerkleIndex`
//! (REQ-EV-0004) over one worktree, bound to the workspace revision.
//! Built once at task start by walking the real worktree, then kept
//! fresh incrementally: a delta only re-reads and recomputes the changed
//! leaves and their ancestor dir chain — never a full rebuild.

use crate::lexical::LexicalIndex;
use crate::merkle::{MerkleIndex, Recomputed};
use crate::symbols::{SymbolDef, SymbolIndex, SymbolRef};
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
/// queryable exact/regex/path index + Tantivy BM25 lexical ranking +
/// tree-sitter symbol surface, all bound to the workspace revision the
/// builder last saw. The task context engine's working set (docs/18).
pub struct TaskIndex {
    pub merkle: MerkleIndex,
    pub repo: RepositoryIndex,
    pub lexical: LexicalIndex,
    pub symbols: SymbolIndex,
    pub indexed_workspace_revision: u64,
}

/// One fused context query hit (docs/18 § Fusion): provenance of WHICH
/// index surfaces produced it, a deterministic score, and — for symbol
/// candidates — the definition line. Content bytes always come from
/// hydration (REQ freshness rule), never from this index.
#[derive(Clone, Debug, PartialEq)]
pub struct ContextHit {
    pub path: String,
    pub line: Option<usize>,
    pub score: f64,
    pub sources: Vec<&'static str>,
}

/// Deterministic source weights (rank fusion + exact-match boosts,
/// docs/18 § Fusion: exact symbol > path > lexical rank).
const SYMBOL_WEIGHT: f64 = 1.2;
const PATH_WEIGHT: f64 = 0.6;
const BM25_WEIGHT: f64 = 1.0;

impl TaskIndex {
    /// Cold build (task start): walk the real worktree, then build both
    /// index halves over the same file set at the given revision.
    pub fn build_at(root: &Path, workspace_revision: u64) -> Self {
        let (files, _stats) = walk_worktree(root);
        let owned: BTreeMap<String, Vec<u8>> = files
            .into_iter()
            .map(|f| (f.path, f.bytes))
            .collect();
        let mut lexical = LexicalIndex::new().expect("in-memory lexical index");
        let mut symbols = SymbolIndex::default();
        let mut repo = RepositoryIndex::new(workspace_revision);
        for (path, bytes) in &owned {
            repo.index_file(path, bytes, workspace_revision);
            lexical.replace(path, bytes);
            symbols.index_file(path, bytes);
        }
        lexical.commit();
        TaskIndex {
            merkle: MerkleIndex::build(&owned, workspace_revision),
            repo,
            lexical,
            symbols,
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
            self.lexical.replace(path, bytes);
            self.symbols.index_file(path, bytes);
        }
        for path in &deleted {
            self.repo.remove_file(path);
            self.lexical.remove(path);
            self.symbols.remove_file(path);
        }
        self.lexical.commit();
        self.indexed_workspace_revision = new_revision;
        self.repo.workspace_revision = new_revision;
        recomputed
    }

    pub fn root_digest(&self) -> String {
        self.merkle.root_digest()
    }

    /// Fused context query (docs/18 § Retrieval planner L1): BM25 lexical
    /// ranking + exact path match + symbol definition candidates, merged
    /// per path with deterministic weights and stable ordering. Deterministic
    /// for the same index state; content bytes are NOT returned — reads
    /// hydrate from the live file.
    pub fn context_query(&self, query: &str, limit: usize) -> Vec<ContextHit> {
        let limit = limit.clamp(1, 50);
        let query_trim = query.trim();
        if query_trim.is_empty() {
            return Vec::new();
        }
        #[derive(Default)]
        struct Acc {
            score: f64,
            sources: Vec<&'static str>,
            line: Option<usize>,
        }
        let mut by_path: BTreeMap<String, Acc> = BTreeMap::new();

        // BM25 lexical candidates (score normalized against the top hit).
        let bm = self.lexical.search(query_trim, limit * 4);
        let max = bm.first().map(|(_, s)| *s).unwrap_or(0.0);
        for (path, score) in bm {
            let entry = by_path.entry(path).or_default();
            if max > 0.0 {
                entry.score += BM25_WEIGHT * (score / max);
            }
            entry.sources.push("bm25");
        }

        // Symbol + path candidates per query term (exact, case-sensitive
        // for symbols — identifiers; case-insensitive for paths).
        let terms: Vec<String> = query_trim
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|t| !t.is_empty())
            .map(|t| t.to_string())
            .collect();
        for term in &terms {
            for def in self.symbols.definitions(term) {
                let entry = by_path.entry(def.path).or_default();
                entry.score += SYMBOL_WEIGHT;
                entry.sources.push("symbol");
                entry.line = match entry.line {
                    None => Some(def.line),
                    Some(l) => Some(l.min(def.line)),
                };
            }
            for path in self.repo.path(term) {
                let entry = by_path.entry(path).or_default();
                entry.score += PATH_WEIGHT;
                entry.sources.push("path");
            }
        }

        let mut hits: Vec<ContextHit> = by_path
            .into_iter()
            .map(|(path, acc)| ContextHit {
                path,
                line: acc.line,
                score: acc.score,
                sources: acc.sources,
            })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.path.cmp(&b.path))
        });
        hits.truncate(limit);
        hits
    }

    /// Symbol definitions by exact name (M3.3).
    pub fn symbol_definitions(&self, name: &str) -> Vec<SymbolDef> {
        self.symbols.definitions(name)
    }

    /// Symbol references by exact name (M3.3).
    pub fn symbol_references(&self, name: &str) -> Vec<SymbolRef> {
        self.symbols.references(name)
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

    /// Fused context query (BM25 + symbol + path, docs/18 § Fusion) is
    /// deterministic, survives incremental deltas identically to a cold
    /// rebuild, and never serves content bytes — only provenance.
    #[test]
    fn context_query_fuses_sources_and_survives_deltas() {
        let root = scratch("ctx");
        std::fs::create_dir_all(root.join("src/ui")).unwrap();
        std::fs::write(root.join("src/retry.rs"), b"retry backoff timeout retry policy\n").unwrap();
        std::fs::write(root.join("src/ui/button.rs"), b"button click render widget\n").unwrap();
        std::fs::write(root.join("docs/retry-notes.md"), b"retry notes about backoff\n").unwrap();
        std::fs::write(root.join("src/caller.rs"), b"fn run_task() {\n    let _ = retry_helpers();\n}\n").unwrap();
        std::fs::write(root.join("src/helpers.rs"), b"pub fn retry_helpers() -> bool { true }\n").unwrap();

        let mut index = TaskIndex::build_at(&root, 1);

        // BM25 top hit with provenance.
        let hits = index.context_query("retry backoff", 10);
        assert_eq!(hits[0].path, "src/retry.rs", "{hits:?}");
        assert!(hits[0].sources.contains(&"bm25"));

        // Symbol candidates outrank lexical-only ones and carry the line.
        let hits = index.context_query("retry_helpers", 10);
        assert_eq!(hits[0].path, "src/helpers.rs", "{hits:?}");
        assert!(hits[0].sources.contains(&"symbol"));
        assert_eq!(hits[0].line, Some(1));
        assert!(hits[0].score >= SYMBOL_WEIGHT);

        // Incremental delta: query OUTPUT equals a cold rebuild of the
        // same final tree (lexical, symbols, and fusion agree).
        std::fs::write(
            root.join("src/helpers.rs"),
            b"pub fn retry_helpers() -> bool { false }\npub fn brand_new_marker() {}\n",
        )
        .unwrap();
        index.apply_delta(
            &root,
            &[IndexChange { path: "src/helpers.rs".into(), deleted: false }],
            2,
        );
        let cold = TaskIndex::build_at(&root, 2);
        assert_eq!(
            index.context_query("brand_new_marker", 10),
            cold.context_query("brand_new_marker", 10),
            "incremental query output == cold rebuild"
        );
        assert_eq!(index.symbol_definitions("brand_new_marker").len(), 1);
        assert_eq!(index.symbol_definitions("brand_new_marker"), cold.symbol_definitions("brand_new_marker"));
        assert_eq!(index.symbol_references("retry_helpers"), cold.symbol_references("retry_helpers"));

        // Deletion drops lexical + symbol candidates.
        std::fs::remove_file(root.join("docs/retry-notes.md")).unwrap();
        index.apply_delta(
            &root,
            &[IndexChange { path: "docs/retry-notes.md".into(), deleted: true }],
            3,
        );
        assert!(index
            .context_query("backoff notes", 10)
            .iter()
            .all(|h| h.path != "docs/retry-notes.md"));

        // Empty query is empty; the limit truncates deterministically.
        assert!(index.context_query("   ", 10).is_empty());
        assert_eq!(index.context_query("retry", 1).len(), 1);
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
