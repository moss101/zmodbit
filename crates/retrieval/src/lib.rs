//! modbit-retrieval — exact/BM25/vector/AST/graph search (M3.1, docs/18).
//!
//! This slice: the exact/regex/path index (M3.1), incremental Merkle
//! repository indexing (REQ-EV-0004), and read-through freshness hydration
//! (REQ-EV-0002): indexes rank candidates, but source bytes are ALWAYS
//! re-read from the active revision before prompt inclusion.
//!
//! Canonical owner subsystem: context-engine (docs/81). Layout: docs/12.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;

pub mod context_graph;
pub mod engineering_context;
pub mod history_context;
pub mod hydration;
pub mod knowledge;
pub mod merkle;

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// One indexed file: content identity + the revision it was seen at.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileEntry {
    pub sha256: String,
    pub byte_length: u64,
    pub line_count: usize,
    /// Workspace revision at last index/update of this file.
    pub indexed_at_revision: u64,
}

/// A search hit with provenance (path + line + snippet).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    pub path: String,
    /// 1-based line number.
    pub line_no: usize,
    pub snippet: String,
}

#[derive(Debug)]
pub enum IndexError {
    InvalidRegex(String),
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IndexError::InvalidRegex(e) => write!(f, "invalid regex: {e}"),
        }
    }
}

impl std::error::Error for IndexError {}

/// The exact/regex/path repository index (M3.1). Bound to a workspace
/// revision; queries run over in-memory line data for candidate ranking,
/// while fresh BYTES always come from hydration.
#[derive(Default)]
pub struct RepositoryIndex {
    pub workspace_revision: u64,
    pub files: BTreeMap<String, FileEntry>,
    lines: BTreeMap<String, Vec<String>>,
}

impl RepositoryIndex {
    pub fn new(workspace_revision: u64) -> Self {
        Self {
            workspace_revision,
            ..Default::default()
        }
    }

    /// Indexes (or re-indexes) one file at the given revision.
    pub fn index_file(&mut self, path: &str, bytes: &[u8], revision: u64) {
        let text = String::from_utf8_lossy(bytes);
        let lines: Vec<String> = text.lines().map(String::from).collect();
        self.files.insert(
            path.to_string(),
            FileEntry {
                sha256: sha256_hex(bytes),
                byte_length: bytes.len() as u64,
                line_count: lines.len(),
                indexed_at_revision: revision,
            },
        );
        self.lines.insert(path.to_string(), lines);
        self.workspace_revision = revision;
    }

    /// Removes a deleted file from the index.
    pub fn remove_file(&mut self, path: &str) {
        self.files.remove(path);
        self.lines.remove(path);
    }

    /// Exact term query: lines containing the term verbatim.
    pub fn exact(&self, term: &str) -> Vec<SearchHit> {
        let mut hits = Vec::new();
        for (path, lines) in &self.lines {
            for (i, line) in lines.iter().enumerate() {
                if line.contains(term) {
                    hits.push(SearchHit {
                        path: path.clone(),
                        line_no: i + 1,
                        snippet: line.trim().chars().take(160).collect(),
                    });
                }
            }
        }
        hits
    }

    /// Regex query over file contents.
    pub fn regex(&self, pattern: &str) -> Result<Vec<SearchHit>, IndexError> {
        let re = regex::Regex::new(pattern).map_err(|e| IndexError::InvalidRegex(e.to_string()))?;
        let mut hits = Vec::new();
        for (path, lines) in &self.lines {
            for (i, line) in lines.iter().enumerate() {
                if re.is_match(line) {
                    hits.push(SearchHit {
                        path: path.clone(),
                        line_no: i + 1,
                        snippet: line.trim().chars().take(160).collect(),
                    });
                }
            }
        }
        Ok(hits)
    }

    /// Path query: case-insensitive substring over indexed paths.
    pub fn path(&self, needle: &str) -> Vec<String> {
        let needle = needle.to_lowercase();
        self.files
            .keys()
            .filter(|p| p.to_lowercase().contains(&needle))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// M3.1: exact, regex, and path queries over the indexed tree.
    #[test]
    fn exact_regex_and_path_queries_rank_hits() {
        let mut index = RepositoryIndex::new(7);
        index.index_file("src/lib.rs", b"fn main() {}\nfn helper() { main(); }\n", 7);
        index.index_file(
            "tests/main_loop.rs",
            b"#[test]\nfn main_loop_completes() { main(); }\n",
            7,
        );

        // Exact: hits every line containing the term, with line numbers.
        let hits = index.exact("main()");
        assert_eq!(hits.len(), 3);
        assert!(hits.iter().all(|h| h.line_no >= 1));

        // Regex: anchored pattern narrows to the definition.
        let hits = index.regex(r"fn main\(\) \{\}").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "src/lib.rs");

        // Invalid regex: typed error, not a panic.
        assert!(matches!(
            index.regex("fn ([unclosed"),
            Err(IndexError::InvalidRegex(_))
        ));

        // Path: substring over PATHS (content is irrelevant here).
        assert_eq!(index.path("main"), vec!["tests/main_loop.rs"]);
        assert_eq!(index.path("lib"), vec!["src/lib.rs"]);
        assert!(index.path("nope").is_empty());

        // Deletion removes candidates.
        index.remove_file("tests/main_loop.rs");
        assert!(index.path("tests").is_empty());
        assert_eq!(index.exact("main_loop_completes").len(), 0);
    }
}
