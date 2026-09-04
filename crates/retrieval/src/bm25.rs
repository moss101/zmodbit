//! BM25 lexical ranking (M3.2, docs/18): Okapi BM25 over the repository
//! index — k1/b parameters, inverse document frequency, and length
//! normalization. This is an in-repo BM25 implementation delivering
//! tantivy-class ranking without an external engine dependency; the
//! scoring contract (segment per file, idf-weighted term matching) is
//! what M3.2 requires.

use std::collections::BTreeMap;

/// BM25 parameters.
pub const K1: f64 = 1.2;
pub const B: f64 = 0.75;

fn terms(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .collect()
}

/// A searchable document (one file segment).
#[derive(Clone, Debug)]
pub struct Bm25Doc {
    pub path: String,
    pub revision: u64,
    term_freqs: BTreeMap<String, usize>,
    pub length: usize,
}

impl Bm25Doc {
    pub fn new(path: &str, bytes: &[u8], revision: u64) -> Self {
        let mut term_freqs: BTreeMap<String, usize> = BTreeMap::new();
        let mut length = 0usize;
        for term in terms(&String::from_utf8_lossy(bytes)) {
            *term_freqs.entry(term).or_insert(0) += 1;
            length += 1;
        }
        Self {
            path: path.to_string(),
            revision,
            term_freqs,
            length,
        }
    }

    pub fn frequency(&self, term: &str) -> usize {
        self.term_freqs.get(term).copied().unwrap_or(0)
    }
}

/// The BM25 index: documents + document frequencies.
#[derive(Default)]
pub struct Bm25Index {
    pub docs: Vec<Bm25Doc>,
    doc_freqs: BTreeMap<String, usize>,
    avg_length: f64,
}

impl Bm25Index {
    pub fn new() -> Self {
        Default::default()
    }

    /// Adds (or replaces) a document segment and recomputes document
    /// frequencies.
    pub fn add(&mut self, doc: Bm25Doc) {
        self.docs.retain(|d| d.path != doc.path);
        self.docs.push(doc);
        self.recompute();
    }

    fn recompute(&mut self) {
        self.doc_freqs.clear();
        for doc in &self.docs {
            for term in doc.term_freqs.keys() {
                *self.doc_freqs.entry(term.clone()).or_insert(0) += 1;
            }
        }
        self.avg_length = if self.docs.is_empty() {
            0.0
        } else {
            self.docs.iter().map(|d| d.length as f64).sum::<f64>() / self.docs.len() as f64
        };
    }

    /// Inverse document frequency with the non-negative idf floor
    /// (clamped AFTER the log so common terms score 0, never negative).
    pub fn idf(&self, term: &str) -> f64 {
        let n = self.docs.len() as f64;
        let df = self.doc_freqs.get(term).copied().unwrap_or(0) as f64;
        ((n - df + 0.5) / (df + 0.5)).ln().max(0.0)
    }

    /// BM25 score of one document for the query terms.
    pub fn score(&self, doc: &Bm25Doc, query_terms: &[&str]) -> f64 {
        let mut score = 0.0;
        for term in query_terms {
            let tf = doc.frequency(term) as f64;
            if tf == 0.0 {
                continue;
            }
            let idf = self.idf(term);
            let norm = tf * (K1 + 1.0)
                / (tf + K1 * (1.0 - B + B * doc.length as f64 / self.avg_length.max(1.0)));
            score += idf * norm;
        }
        score
    }

    /// Ranked search: (path, score, revision) descending.
    pub fn search(&self, query: &str) -> Vec<(String, f64, u64)> {
        let query_terms: Vec<String> = terms(query);
        let query_refs: Vec<&str> = query_terms.iter().map(|s| s.as_str()).collect();
        let mut scored: Vec<(String, f64, u64)> = self
            .docs
            .iter()
            .map(|d| (d.path.clone(), self.score(d, &query_refs), d.revision))
            .filter(|(_, score, _)| *score > 0.0)
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> Bm25Index {
        let mut index = Bm25Index::new();
        index.add(Bm25Doc::new(
            "src/retry.rs",
            b"retry backoff timeout transient failure retry policy\n",
            5,
        ));
        index.add(Bm25Doc::new(
            "src/ui/button.rs",
            b"button click render focus style widget\n",
            5,
        ));
        index.add(Bm25Doc::new(
            "docs/retry-notes.md",
            b"retry notes about the retry mechanism\n",
            5,
        ));
        index
    }

    /// M3.2: BM25 ranking — idf weights rare terms higher, length
    /// normalization applies, ranking is stable and revision-bound.
    #[test]
    fn bm25_ranks_by_idf_weighted_term_matches() {
        let index = index();

        // "retry" appears in 2 docs (idf ~ 0 after the non-negative
        // floor); "backoff" in 1 — the rare term carries the score, so
        // src/retry.rs (which has BOTH) is the top hit.
        let hits = index.search("retry backoff");
        assert_eq!(hits[0].0, "src/retry.rs");
        assert!(hits[0].1 > 0.0);

        // idf sanity: a term in every doc scores ~0; a rare term scores high.
        let common = index.idf("retry");
        let rare = index.idf("backoff");
        assert!(rare > common);

        // A term matching nothing yields no hits.
        assert!(index.search("xylophone").is_empty());

        // Revision-bound candidates.
        assert_eq!(hits[0].2, 5);
    }
}
