//! Dependency/call evidence graph (M3.6, docs/18 § Index inputs: "import/
//! dependency edges"; docs/02 EV ledger 0157-class): per-file IMPORT
//! edges extracted with tree-sitter (Rust `use`, TS/TSX/JS `import`,
//! Python `import`/`from`), resolved against the indexed corpus, plus
//! impact queries (transitive dependents) — the structural expansion the
//! L3 engineering level builds on.
//!
//! Only edges whose target resolves to an INDEXED file are kept: the
//! graph describes the task's corpus, not the universe. Resolution is
//! deterministic (fixed candidate extensions, sorted iteration).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::symbols::{language_of, parse_dep_strings, DepKind};

/// One resolved dependency edge: `from` references `to`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DependencyEdge {
    pub from: String,
    pub to: String,
    pub kind: &'static str,
}

/// Per-file import edges over the indexed corpus.
#[derive(Debug, Default)]
pub struct DependencyIndex {
    /// from -> targets (sorted; deterministic iteration).
    edges: BTreeMap<String, BTreeMap<String, &'static str>>,
}

/// Resolves one raw dep string from `from_path` to an indexed corpus file.
/// Returns the corpus-relative path or None (external/unresolvable).
fn resolve(from_path: &str, raw: &str, kind: DepKind, known: &BTreeSet<String>) -> Option<String> {
    match kind {
        DepKind::Ts => {
            // Relative specifiers only; bare module names are external.
            let spec = raw.trim_matches(|c| c == '\'' || c == '"');
            if !spec.starts_with("./") && !spec.starts_with("../") {
                return None;
            }
            let dir = std::path::Path::new(from_path).parent()?;
            let base = normalize(dir.join(spec).to_string_lossy().as_ref())?;
            ["ts", "tsx", "js", "tsx", "ts"]
                .iter()
                .find_map(|ext| {
                    let cand = format!("{base}.{ext}");
                    known.contains(&cand).then_some(cand)
                })
                .or_else(|| {
                    ["ts", "tsx", "js"].iter().find_map(|ext| {
                        let cand = format!("{base}/index.{ext}");
                        known.contains(&cand).then_some(cand)
                    })
                })
        }
        DepKind::Python => {
            // Relative imports (leading dots) climb from the file's dir
            // (one dot = current package dir); absolute imports are
            // root-relative (the corpus root is the sys.path root).
            let (spec, base) = if let Some(rest) = raw.strip_prefix('.') {
                let ups = raw.len() - rest.len();
                let mut base = std::path::Path::new(from_path)
                    .parent()
                    .map(|p| p.to_path_buf());
                for _ in 1..ups {
                    base = base.and_then(|b| b.parent().map(|p| p.to_path_buf()));
                }
                (rest.to_string(), base?)
            } else {
                (raw.to_string(), std::path::PathBuf::new())
            };
            let rel = if spec.is_empty() {
                base.to_string_lossy().to_string()
            } else {
                normalize(base.join(spec.replace('.', "/")).to_string_lossy().as_ref())?
            };
            let cand = format!("{rel}.py");
            if known.contains(&cand) {
                return Some(cand);
            }
            let init = format!("{rel}/__init__.py");
            known.contains(&init).then_some(init)
        }
        DepKind::Rust => {
            // `crate::...` is root-relative; `super::...` climbs one dir
            // per super; `self::` stays. External crates (no crate/super/
            // self prefix) resolve only if a top-level module matches.
            let spec;
            let mut base = std::path::Path::new(from_path)
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_default();
            if let Some(rest) = raw.strip_prefix("crate::") {
                spec = rest;
                // base stays the corpus root; the candidate list below
                // tries the src/ tree (crate roots) first.
                base = std::path::PathBuf::new();
            } else {
                let mut supers = 0usize;
                let mut cursor = raw;
                while let Some(rest) = cursor.strip_prefix("super::") {
                    supers += 1;
                    cursor = rest;
                }
                if supers > 0 {
                    spec = cursor;
                    for _ in 0..supers {
                        base = base.parent().map(|p| p.to_path_buf()).unwrap_or_default();
                    }
                } else if raw.strip_prefix("self::").is_some() {
                    spec = raw.strip_prefix("self::").unwrap();
                } else {
                    // Bare crate name: skip (external unless it IS the root
                    // module dir, which single-file corpora never have).
                    return None;
                }
            }
            let rel = normalize(base.join(spec.replace("::", "/")).to_string_lossy().as_ref())?;
            // Crate roots vary (src/ trees vs root-level corpora): try both.
            for cand in [
                format!("src/{rel}.rs"),
                format!("{rel}.rs"),
                format!("src/{rel}/mod.rs"),
                format!("{rel}/mod.rs"),
            ] {
                if known.contains(&cand) {
                    return Some(cand);
                }
            }
            None
        }
    }
}

fn normalize(p: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "." => {}
            ".." => {
                parts.pop()?;
            }
            "" => {}
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

impl DependencyIndex {
    /// Re-extracts the edges of one file. `known` is the indexed corpus
    /// path set; edges to unknown targets are dropped.
    pub fn index_file(
        &mut self,
        root: &Path,
        from_path: &str,
        bytes: &[u8],
        known: &BTreeSet<String>,
    ) {
        let mut targets: BTreeMap<String, &'static str> = BTreeMap::new();
        if let Some(lang) = language_of(from_path) {
            for (raw, kind) in parse_dep_strings(lang, bytes) {
                if let Some(to) = resolve(from_path, &raw, kind, known) {
                    if to != from_path {
                        targets.insert(to, kind.as_str());
                    }
                }
            }
        }
        let _ = root; // resolution is corpus-relative
        if targets.is_empty() {
            self.edges.remove(from_path);
        } else {
            self.edges.insert(from_path.to_string(), targets);
        }
    }

    /// Removes one file's out-edges.
    pub fn remove_file(&mut self, from_path: &str) {
        self.edges.remove(from_path);
    }

    /// Drops edges pointing at paths no longer in the corpus.
    pub fn prune(&mut self, known: &BTreeSet<String>) {
        for targets in self.edges.values_mut() {
            targets.retain(|to, _| known.contains(to));
        }
        self.edges.retain(|_, t| !t.is_empty());
    }

    /// Direct dependents of `path` (files that import it), sorted.
    pub fn imported_by(&self, path: &str) -> Vec<String> {
        let mut deps: Vec<String> = self
            .edges
            .iter()
            .filter(|(_, targets)| targets.contains_key(path))
            .map(|(from, _)| from.clone())
            .collect();
        deps.sort();
        deps
    }

    /// Transitive dependents with hop counts (bounded BFS) — the impact
    /// set of changing `path`. Sorted by (hops, path).
    pub fn impact(&self, path: &str, max_depth: usize) -> Vec<(String, usize)> {
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        let mut frontier: Vec<String> = vec![path.to_string()];
        for depth in 1..=max_depth.max(1) {
            let mut next: BTreeSet<String> = BTreeSet::new();
            for node in &frontier {
                for dependent in self.imported_by(node) {
                    if dependent != path && !seen.contains_key(&dependent) {
                        seen.insert(dependent.clone(), depth);
                        next.insert(dependent);
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next.into_iter().collect();
        }
        let mut out: Vec<(String, usize)> = seen.into_iter().collect();
        out.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
        out
    }

    /// All edges, deterministic order (docs/evidence shape).
    pub fn all(&self) -> Vec<DependencyEdge> {
        self.edges
            .iter()
            .flat_map(|(from, targets)| {
                targets
                    .iter()
                    .map(move |(to, kind)| DependencyEdge {
                        from: from.clone(),
                        to: to.clone(),
                        kind,
                    })
            })
            .collect()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.values().map(BTreeMap::len).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    /// Rust `use` resolution: crate::/super::/self:: forms map onto the
    /// corpus; externals are dropped.
    #[test]
    fn rust_use_edges_resolve_against_the_corpus() {
        let known = known(&["src/retry.rs", "src/net/mod.rs", "src/net/tcp.rs", "src/parent.rs", "src/child.rs"]);
        let mut idx = DependencyIndex::default();
        let src = b"use crate::retry;\nuse crate::net::tcp;\nuse modbit_git::GitRepo;\n";
        idx.index_file(Path::new("/"), "src/main.rs", src, &known);
        let from = |idx: &DependencyIndex| {
            let mut all = idx.all();
            all.retain(|e| e.from == "src/main.rs");
            all.iter().map(|e| e.to.clone()).collect::<Vec<_>>()
        };
        assert_eq!(from(&idx), vec!["src/net/tcp.rs", "src/retry.rs"]);

        // super:: climbs one directory per super.
        idx.index_file(Path::new("/"), "src/child.rs", b"use super::parent;", &known);
        let edge = idx.all().into_iter().find(|e| e.from == "src/child.rs").unwrap();
        assert_eq!(edge.to, "src/parent.rs");

        // Unresolvable externals leave no edge; self-edge dropped.
        idx.index_file(Path::new("/"), "src/retry.rs", b"use self::retry;\nuse serde_json;", &known);
        assert!(!idx.all().iter().any(|e| e.from == "src/retry.rs"));
    }

    /// TS relative imports resolve to corpus files with candidate
    /// extensions; bare specifiers are external.
    #[test]
    fn ts_relative_imports_resolve() {
        let known = known(&["src/util.ts", "src/app/index.ts"]);
        let mut idx = DependencyIndex::default();
        idx.index_file(
            Path::new("/"),
            "src/main.ts",
            b"import { u } from './util';\nimport app from './app';\nimport react from 'react';\n",
            &known,
        );
        let mut tos: Vec<String> = idx.all().into_iter().map(|e| e.to).collect();
        tos.sort();
        assert_eq!(tos, vec!["src/app/index.ts", "src/util.ts"]);
    }

    /// Python imports resolve dotted paths (file or package __init__).
    #[test]
    fn python_imports_resolve() {
        let known = known(&["pkg/util.py", "pkg/__init__.py"]);
        let mut idx = DependencyIndex::default();
        idx.index_file(
            Path::new("/"),
            "pkg/main.py",
            b"from .util import thing\nimport pkg\n",
            &known,
        );
        let mut tos: Vec<String> = idx.all().into_iter().map(|e| e.to).collect();
        tos.sort();
        assert_eq!(tos, vec!["pkg/__init__.py", "pkg/util.py"]);
    }

    /// Impact: transitive dependents with hops, pruned on deletion.
    #[test]
    fn impact_bfs_and_prune() {
        let known = known(&["a.rs", "b.rs", "c.rs"]);
        let mut idx = DependencyIndex::default();
        // a <- b <- c (c imports b imports a)
        idx.index_file(Path::new("/"), "b.rs", b"use crate::a;", &known);
        idx.index_file(Path::new("/"), "c.rs", b"use crate::b;", &known);
        let impact = idx.impact("a.rs", 3);
        assert_eq!(
            impact,
            vec![("b.rs".to_string(), 1), ("c.rs".to_string(), 2)]
        );

        // Deleting b.rs prunes both its out-edge and edges into it (the
        // known set is the CURRENT corpus: b.rs is gone from it).
        idx.remove_file("b.rs");
        let mut after = known.clone();
        after.remove("b.rs");
        idx.prune(&after);
        assert_eq!(idx.impact("a.rs", 3), vec![]);
        assert!(idx.all().iter().all(|e| e.from != "c.rs"));
    }
}

#[cfg(test)]
mod probe {
    #[test]
    fn probe_rust_dep_query() {
        let lang = crate::symbols::language_for("rust").unwrap();
        let q = tree_sitter::Query::new(
            &lang,
            r#"(use_declaration argument: [(scoped_identifier) (identifier) (wildcard_import (scoped_identifier)) (use_as_clause)] @dep)"#,
        );
        eprintln!("rust dep query: {:?}", q.err());
        // Also show the AST of a use line.
        let mut p = tree_sitter::Parser::new();
        p.set_language(&lang).unwrap();
        let t = p.parse(b"use crate::retry;\nuse modbit_git::GitRepo;\n", None).unwrap();
        eprintln!("AST: {}", t.root_node().to_sexp());
    }
}
