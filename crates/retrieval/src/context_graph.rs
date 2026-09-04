//! Shared AST/symbol chunk representation (M3, REQ-EV-0005), code
//! structure mapping (REQ-EV-0155), and the dependency/call graph with
//! impact analysis (REQ-EV-0157).
//!
//! ONE structural representation — symbol chunks keyed by
//! language-qualified ids — feeds index, query, and impact paths, so
//! symbol identity is consistent across all of them. Extraction here is
//! intentionally a lightweight line-shape parser (no tree-sitter yet);
//! the REPRESENTATION is the contract.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Symbol kinds extractable across languages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Function,
    Struct,
    Trait,
    Class,
    Const,
}

impl SymbolKind {
    fn parse(lang: Language, line: &str) -> Option<(Self, String)> {
        let trimmed = line.trim_start();
        let (kind, rest) = match lang {
            Language::Rust => {
                let rest = trimmed
                    .strip_prefix("pub fn ")
                    .or_else(|| trimmed.strip_prefix("fn "))
                    .map(|r| (SymbolKind::Function, r))
                    .or_else(|| {
                        trimmed
                            .strip_prefix("pub struct ")
                            .or_else(|| trimmed.strip_prefix("struct "))
                            .map(|r| (SymbolKind::Struct, r))
                    })
                    .or_else(|| {
                        trimmed
                            .strip_prefix("pub trait ")
                            .or_else(|| trimmed.strip_prefix("trait "))
                            .map(|r| (SymbolKind::Trait, r))
                    })?;
                rest
            }
            Language::TypeScript => {
                let rest = trimmed
                    .strip_prefix("export function ")
                    .or_else(|| trimmed.strip_prefix("function "))
                    .map(|r| (SymbolKind::Function, r))
                    .or_else(|| {
                        trimmed
                            .strip_prefix("export class ")
                            .or_else(|| trimmed.strip_prefix("class "))
                            .map(|r| (SymbolKind::Class, r))
                    })
                    .or_else(|| {
                        trimmed
                            .strip_prefix("export const ")
                            .or_else(|| trimmed.strip_prefix("const "))
                            .map(|r| (SymbolKind::Const, r))
                    })?;
                rest
            }
        };
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() {
            None
        } else {
            Some((kind, name))
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    Rust,
    TypeScript,
}

/// A symbol chunk: the shared structural unit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SymbolChunk {
    /// Language-qualified identity: "rust:src/lib.rs:main" — the SAME id
    /// is used by index, query, and impact paths.
    pub symbol_id: String,
    pub kind: SymbolKind,
    pub path: String,
    pub line_no: usize,
    pub signature: String,
    /// Digest of the source file the chunk came from.
    pub source_sha256: String,
}

/// A dependency edge between files.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyEdge {
    pub from: String,
    pub to: String,
    /// How `to` was referenced (import/require shape).
    pub kind: &'static str,
}

/// The code structure map: files, symbols, and dependency edges
/// (REQ-EV-0155). Nodes are files and symbols; edges are dependencies.
#[derive(Default)]
pub struct ContextGraph {
    pub files: BTreeMap<String, String>, // path → source sha256
    pub symbols: BTreeMap<String, SymbolChunk>,
    pub deps: Vec<DependencyEdge>,
    /// test-file suffixes recognized for impact classification.
    pub test_markers: Vec<String>,
}

impl ContextGraph {
    pub fn new() -> Self {
        Self {
            test_markers: vec!["tests/".into(), "_test.".into(), ".test.".into()],
            ..Default::default()
        }
    }

    /// Indexes one file, extracting symbol chunks.
    pub fn index_file(&mut self, lang: Language, path: &str, bytes: &[u8]) {
        let sha256 = crate::sha256_hex(bytes);
        self.files.insert(path.to_string(), sha256.clone());
        let text = String::from_utf8_lossy(bytes);
        for (i, line) in text.lines().enumerate() {
            if let Some((kind, name)) = SymbolKind::parse(lang, line) {
                let symbol_id = format!("{}:{}:{}", lang_str(lang), path, name);
                self.symbols.insert(
                    symbol_id.clone(),
                    SymbolChunk {
                        symbol_id,
                        kind,
                        path: path.to_string(),
                        line_no: i + 1,
                        signature: line.trim().chars().take(120).collect(),
                        source_sha256: sha256.clone(),
                    },
                );
            }
        }
    }

    /// Records a dependency edge (extracted from imports/requires).
    pub fn add_dependency(&mut self, from: &str, to: &str, kind: &'static str) {
        self.deps.push(DependencyEdge {
            from: from.to_string(),
            to: to.to_string(),
            kind,
        });
    }

    /// Query path: symbol lookup by name across languages.
    pub fn query_symbol(&self, name: &str) -> Vec<&SymbolChunk> {
        self.symbols
            .values()
            .filter(|s| s.symbol_id.ends_with(&format!(":{name}")))
            .collect()
    }

    /// Structural path (REQ-EV-0155): resolves a cross-file route from
    /// symbol A to symbol B through dependency edges (BFS on files).
    pub fn structural_path(&self, from_path: &str, to_path: &str) -> Option<Vec<String>> {
        if from_path == to_path {
            return Some(vec![from_path.to_string()]);
        }
        let mut queue = std::collections::VecDeque::new();
        let mut parent: BTreeMap<String, String> = BTreeMap::new();
        queue.push_back(from_path.to_string());
        while let Some(current) = queue.pop_front() {
            for edge in &self.deps {
                if edge.from == current && !parent.contains_key(&edge.to) {
                    parent.insert(edge.to.clone(), current.clone());
                    if edge.to == to_path {
                        // Reconstruct.
                        let mut route = vec![to_path.to_string()];
                        let mut node = to_path.to_string();
                        while let Some(prev) = parent.get(&node) {
                            route.push(prev.clone());
                            node = prev.clone();
                        }
                        route.reverse();
                        return Some(route);
                    }
                    queue.push_back(edge.to.clone());
                }
            }
        }
        None
    }

    fn is_test_file(&self, path: &str) -> bool {
        self.test_markers.iter().any(|m| path.contains(m.as_str()))
    }

    /// IMPACT (REQ-EV-0157): the reverse-dependency closure of a changed
    /// file — everything that transitively depends on it, split into
    /// source and test files.
    pub fn impact_of(&self, changed_path: &str) -> (Vec<String>, Vec<String>) {
        let mut affected: BTreeSet<String> = BTreeSet::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(changed_path.to_string());
        while let Some(current) = queue.pop_front() {
            for edge in &self.deps {
                if edge.to == current && affected.insert(edge.from.clone()) {
                    queue.push_back(edge.from.clone());
                }
            }
        }
        affected.remove(changed_path);
        let mut source = Vec::new();
        let mut tests = Vec::new();
        for path in affected {
            if self.is_test_file(&path) {
                tests.push(path);
            } else {
                source.push(path);
            }
        }
        (source, tests)
    }
}

fn lang_str(lang: Language) -> &'static str {
    match lang {
        Language::Rust => "rust",
        Language::TypeScript => "ts",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUST_LIB: &[u8] = b"pub fn parse_config() -> u32 { 1 }\npub struct Config;\n";
    const TS_APP: &[u8] = b"import { parse_config } from './lib';\nexport function start() { return parse_config(); }\n";
    const RUST_TEST: &[u8] =
        b"#[test]\nfn parse_config_works() { assert_eq!(parse_config(), 1); }\n";

    fn graph() -> ContextGraph {
        let mut g = ContextGraph::new();
        g.index_file(Language::Rust, "src/lib.rs", RUST_LIB);
        g.index_file(Language::TypeScript, "app/start.ts", TS_APP);
        g.index_file(Language::Rust, "tests/lib_test.rs", RUST_TEST);
        // app/start.ts imports src/lib.rs; the test depends on src/lib.rs.
        g.add_dependency("app/start.ts", "src/lib.rs", "import");
        g.add_dependency("tests/lib_test.rs", "src/lib.rs", "test");
        g
    }

    /// QUAL-EV-0005: symbol identity is consistent across index, query,
    /// and impact paths (cross-language fixture).
    #[test]
    fn symbol_identity_consistent_across_paths() {
        let g = graph();

        // Index path: the chunk exists with a canonical id.
        let id = "rust:src/lib.rs:parse_config";
        assert!(g.symbols.contains_key(id));

        // Query path: same identity comes back.
        let hits = g.query_symbol("parse_config");
        assert_eq!(
            hits.len(),
            1,
            "exact symbol id match, cross-language names don't collide"
        );
        assert_eq!(hits[0].symbol_id, id);
        assert_eq!(hits[0].kind, SymbolKind::Function);

        // Impact path: the SAME file identity drives impact.
        let (source, tests) = g.impact_of("src/lib.rs");
        assert!(source.contains(&"app/start.ts".to_string()));
        assert_eq!(tests, vec!["tests/lib_test.rs".to_string()]);
    }

    /// QUAL-EV-0155: a cross-file query resolves the structural path.
    #[test]
    fn cross_file_query_resolves_structural_path() {
        let g = graph();
        let route = g
            .structural_path("app/start.ts", "src/lib.rs")
            .expect("dependency edge resolves");
        assert_eq!(route, vec!["app/start.ts", "src/lib.rs"]);
        // Disconnected files have no route.
        assert!(g.structural_path("src/lib.rs", "app/start.ts").is_none());
    }

    /// QUAL-EV-0157: the impact benchmark checks affected file/test
    /// recall — everything that depends on the changed file is found.
    #[test]
    fn impact_benchmark_finds_affected_files_and_tests() {
        let g = graph();
        let (source, tests) = g.impact_of("src/lib.rs");

        // Expected universe for this fixture.
        let expected_source: BTreeSet<&str> = ["app/start.ts"].into_iter().collect();
        let expected_tests: BTreeSet<&str> = ["tests/lib_test.rs"].into_iter().collect();

        let got_source: BTreeSet<&str> = source.iter().map(|s| s.as_str()).collect();
        let got_tests: BTreeSet<&str> = tests.iter().map(|s| s.as_str()).collect();
        // Recall = |found ∩ expected| / |expected| — both 1.0 here.
        let source_recall =
            got_source.intersection(&expected_source).count() as f64 / expected_source.len() as f64;
        let test_recall =
            got_tests.intersection(&expected_tests).count() as f64 / expected_tests.len() as f64;
        assert_eq!(source_recall, 1.0);
        assert_eq!(test_recall, 1.0);
    }
}

/// A bounded expansion from a symbol to its connected engineering
/// context (REQ-EV-0164): callers, tests, config, evidence — but NEVER
/// more than `budget` nodes.
#[derive(Clone, Debug, PartialEq)]
pub struct Expansion {
    pub root_symbol: String,
    pub nodes: Vec<String>,
    pub truncated: bool,
}

impl ContextGraph {
    /// Expands from a symbol through its file's dependency neighborhood,
    /// stopping at the budget. The cap is hard: runaway expansion is
    /// impossible by construction.
    pub fn expand_from_symbol(&self, symbol_id: &str, budget: usize) -> Result<Expansion, String> {
        let symbol = self
            .symbols
            .get(symbol_id)
            .ok_or_else(|| format!("unknown symbol {symbol_id:?}"))?;
        if budget == 0 {
            return Ok(Expansion {
                root_symbol: symbol_id.to_string(),
                nodes: Vec::new(),
                truncated: true,
            });
        }
        let root_file = &symbol.path;
        let mut nodes = vec![root_file.clone()];
        // Direct dependents (callers) and dependencies, breadth-first.
        let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        for edge in &self.deps {
            if edge.to == *root_file {
                queue.push_back(edge.from.clone());
            } else if edge.from == *root_file {
                queue.push_back(edge.to.clone());
            }
        }
        let mut visited: BTreeSet<String> = [root_file.clone()].into_iter().collect();
        while let Some(next) = queue.pop_front() {
            if nodes.len() >= budget {
                return Ok(Expansion {
                    root_symbol: symbol_id.to_string(),
                    nodes,
                    truncated: true,
                });
            }
            if visited.insert(next.clone()) {
                nodes.push(next.clone());
            }
        }
        let truncated = nodes.len() >= budget;
        Ok(Expansion {
            root_symbol: symbol_id.to_string(),
            nodes,
            truncated,
        })
    }
}

#[cfg(test)]
mod expansion_tests {
    use super::*;

    /// QUAL-EV-0164: the budget cap prevents runaway expansion.
    #[test]
    fn expansion_budget_cap_prevents_runaway() {
        let mut g = ContextGraph::new();
        g.index_file(Language::Rust, "src/core.rs", b"pub fn core() {}\n");
        g.index_file(Language::Rust, "src/mid.rs", b"pub fn mid() {}\n");
        g.index_file(Language::Rust, "src/edge.rs", b"pub fn edge() {}\n");
        g.index_file(
            Language::Rust,
            "tests/core_test.rs",
            b"#[test]\nfn core_ok() {}\n",
        );
        for (from, to) in [
            ("src/mid.rs", "src/core.rs"),
            ("src/edge.rs", "src/mid.rs"),
            ("tests/core_test.rs", "src/core.rs"),
        ] {
            g.add_dependency(from, to, "call");
        }

        // Budget 2: root + one neighbor, truncated.
        let id = "rust:src/core.rs:core";
        let expansion = g.expand_from_symbol(id, 2).unwrap();
        assert_eq!(expansion.nodes.len(), 2);
        assert!(expansion.truncated);

        // Budget 10: everything reachable, not truncated.
        let expansion = g.expand_from_symbol(id, 10).unwrap();
        assert!(expansion.nodes.len() >= 3);
        assert!(!expansion.truncated);
        assert!(expansion.nodes.contains(&"tests/core_test.rs".to_string()));

        // Zero budget: nothing expanded.
        assert!(g.expand_from_symbol(id, 0).unwrap().nodes.is_empty());
        // Unknown symbol: typed error.
        assert!(g.expand_from_symbol("rust:x:y", 5).is_err());
    }
}
