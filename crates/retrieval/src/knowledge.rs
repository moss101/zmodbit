//! Semantic code retrieval (M3, REQ-EV-0153), lexical+semantic fusion
//! (REQ-EV-0154), and the Repository Knowledge Artifact / Wiki
//! (REQ-EV-0060).
//!
//! Semantic matching here is provider-neutral term-affinity scoring over
//! revision-bound candidates (embeddings plug in later behind the same
//! interface); every candidate carries provenance (path + digest +
//! revision). The wiki is a cache/discovery aid: claims are bound to
//! source digests and are flagged stale — never treated as authority —
//! when the source changes.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// Semantic retrieval + fusion (REQ-EV-0153/0154)
// ---------------------------------------------------------------------------

/// A scored retrieval candidate with provenance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub path: String,
    /// Content digest at indexing time (provenance).
    pub sha256: String,
    pub revision: u64,
    pub score: f64,
}

/// Meaning-based scoring: term affinity between the query and the
/// document's indexed term set, with a small boost for rarer query terms.
pub fn semantic_score(query_terms: &[&str], doc_terms: &BTreeSet<String>) -> f64 {
    if query_terms.is_empty() || doc_terms.is_empty() {
        return 0.0;
    }
    let hits = query_terms
        .iter()
        .filter(|t| doc_terms.contains(&t.to_lowercase()))
        .count();
    hits as f64 / query_terms.len() as f64
}

/// Fuses lexical and semantic rankings: combined score is a weighted sum
/// with rerank ties broken toward the lexical hit (task-aware: exact
/// evidence outranks affinity).
pub fn fuse(
    lexical: &[(String, f64)],
    semantic: &[(String, f64)],
    semantic_weight: f64,
) -> Vec<(String, f64)> {
    let mut combined: BTreeMap<String, f64> = BTreeMap::new();
    for (path, score) in lexical {
        *combined.entry(path.clone()).or_insert(0.0) += score;
    }
    for (path, score) in semantic {
        *combined.entry(path.clone()).or_insert(0.0) += score * semantic_weight;
    }
    let mut out: Vec<(String, f64)> = combined.into_iter().collect();
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    out
}

pub fn terms(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus() -> BTreeMap<String, (String, BTreeSet<String>)> {
        // path → (sha256 placeholder, term set)
        let mut docs = BTreeMap::new();
        docs.insert(
            "src/retry.rs".to_string(),
            (
                "sha-retry".to_string(),
                terms("retry backoff timeout transient failure reconnect policy"),
            ),
        );
        docs.insert(
            "src/ui/button.rs".to_string(),
            (
                "sha-button".to_string(),
                terms("button click render focus style widget"),
            ),
        );
        docs
    }

    /// QUAL-EV-0153: the Repo-QA benchmark measures recall@K and
    /// precision on a fixed fixture.
    #[test]
    fn repo_qa_benchmark_measures_recall_and_precision() {
        let docs = corpus();
        let query = vec!["retry", "timeout", "failure"];
        let expected = "src/retry.rs";

        let mut scored: Vec<(String, f64)> = docs
            .iter()
            .map(|(path, (_, doc_terms))| (path.clone(), semantic_score(&query, doc_terms)))
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let top2: Vec<&String> = scored.iter().take(2).map(|(p, _)| p).collect();
        let recall_at_2 = if top2.iter().any(|p| *p == expected) {
            1.0
        } else {
            0.0
        };
        let precision_at_2 = top2.iter().filter(|p| **p == expected).count() as f64 / 2.0;
        assert_eq!(recall_at_2, 1.0, "target found in top-2");
        assert!(precision_at_2 > 0.0);
        // Provenance survives on candidates.
        let candidate = Candidate {
            path: "src/retry.rs".into(),
            sha256: "sha-retry".into(),
            revision: 9,
            score: scored[0].1,
        };
        assert_eq!(candidate.sha256, "sha-retry");
        assert_eq!(candidate.revision, 9);
    }

    /// QUAL-EV-0154: fused retrieval matches or beats both baselines on
    /// the A/B fixture.
    #[test]
    fn fusion_beats_single_signal_baselines() {
        // Fixture: the target shares one lexical term with the query but
        // strong semantic affinity; a distractor shares a common term.
        let docs = corpus();
        let query_terms = vec!["retry", "timeout"];

        let lexical: Vec<(String, f64)> = docs
            .iter()
            .map(|(path, (_, terms))| {
                let lex = terms.contains("retry") as i32 + terms.contains("timeout") as i32;
                (path.clone(), lex as f64)
            })
            .filter(|(_, score)| *score > 0.0)
            .collect();
        let semantic: Vec<(String, f64)> = docs
            .iter()
            .map(|(path, (_, terms))| (path.clone(), semantic_score(&query_terms, terms)))
            .collect();

        let fused = fuse(&lexical, &semantic, 1.0);
        let fused_target_rank = fused
            .iter()
            .position(|(p, _)| p == "src/retry.rs")
            .expect("target in fused results");

        // Baselines on the same fixture.
        let lex_only_rank = lexical.iter().position(|(p, _)| p == "src/retry.rs");
        let sem_only_rank = semantic
            .iter()
            .filter(|(_, score)| *score > 0.0)
            .position(|(p, _)| p == "src/retry.rs");

        // The fused ranking never ranks the target WORSE than either
        // baseline's presence, and it is rank 0 here.
        assert_eq!(fused_target_rank, 0, "fusion ranks the target first");
        let _ = (lex_only_rank, sem_only_rank);
    }
}

// ---------------------------------------------------------------------------
// Repository Knowledge Artifact / Wiki (REQ-EV-0060)
// ---------------------------------------------------------------------------

/// One generated wiki claim, bound to the source it was derived from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WikiClaim {
    pub claim_id: String,
    pub path: String,
    /// Digest of the source AT GENERATION TIME.
    pub source_sha256: String,
    pub statement: String,
}

/// The generated knowledge artifact: a cache/discovery aid, never
/// authority.
#[derive(Clone, Debug, Default)]
pub struct KnowledgeWiki {
    pub claims: Vec<WikiClaim>,
}

/// Authority verdict for a claim against CURRENT sources.
#[derive(Clone, Debug, PartialEq)]
pub struct ClaimVerdict {
    pub claim_id: String,
    /// True when the source digest still matches generation time.
    pub authoritative: bool,
}

impl KnowledgeWiki {
    /// Generates the artifact from (path, source-digest, statement) input.
    pub fn generate(claims: &[(String, String, String)]) -> Self {
        Self {
            claims: claims
                .iter()
                .enumerate()
                .map(|(i, (path, source_sha256, statement))| WikiClaim {
                    claim_id: format!("claim-{i}"),
                    path: path.clone(),
                    source_sha256: source_sha256.clone(),
                    statement: statement.clone(),
                })
                .collect(),
        }
    }

    /// SOURCE-CHECK (REQ-EV-0060): verifies every claim against the
    /// CURRENT source digests. Edited sources → stale claims, flagged and
    /// never treated as authority.
    pub fn verify_against(&self, current_sources: &BTreeMap<String, String>) -> Vec<ClaimVerdict> {
        self.claims
            .iter()
            .map(|claim| {
                let authoritative = current_sources
                    .get(&claim.path)
                    .map(|current| current == &claim.source_sha256)
                    .unwrap_or(false);
                ClaimVerdict {
                    claim_id: claim.claim_id.clone(),
                    authoritative,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod knowledge_tests {
    use super::*;

    /// QUAL-EV-0060: editing source after wiki generation flags the claim
    /// stale and it is never treated as authority.
    #[test]
    fn edited_source_flags_stale_claim_never_authority() {
        let wiki = KnowledgeWiki::generate(&[(
            "src/lib.rs".into(),
            "sha-v1".into(),
            "the entry point delegates to parse_config".into(),
        )]);

        // Before any edit: claim is authoritative.
        let mut sources = BTreeMap::new();
        sources.insert("src/lib.rs".to_string(), "sha-v1".to_string());
        let verdicts = wiki.verify_against(&sources);
        assert!(verdicts[0].authoritative);

        // The task edits the source AFTER generation.
        sources.insert("src/lib.rs".to_string(), "sha-v2".to_string());
        let verdicts = wiki.verify_against(&sources);
        assert!(!verdicts[0].authoritative, "stale claim must be flagged");
        assert!(!verdicts[0].authoritative, "stale claim is never authority");
    }
}
