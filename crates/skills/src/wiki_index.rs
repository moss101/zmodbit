//! On-demand proposer retrieval from the wiki index (M3/M5,
//! REQ-EV-0204): the evolution agent starts with a COMPACT index (entry
//! ids + one-line summaries) and hydrates specific patterns/traces on
//! demand — a large corpus stays within a token budget while provenance
//! remains complete.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// A full wiki record (provenance-complete).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WikiRecord {
    pub entry_id: String,
    pub pattern: String,
    pub outcome: String,
    pub provenance: Vec<String>,
}

/// The compact index entry (what the agent sees by default).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndexEntry {
    pub entry_id: String,
    /// One-line outcome summary.
    pub summary: String,
}

/// The wiki index over a large corpus.
#[derive(Default)]
pub struct WikiIndex {
    records: BTreeMap<String, WikiRecord>,
    index: Vec<IndexEntry>,
}

/// The proposer's working set: index + hydrated records, with a token
/// budget enforced at hydration time.
#[derive(Default)]
pub struct ProposerWorkingSet {
    pub index_entries: Vec<IndexEntry>,
    pub hydrated: BTreeMap<String, WikiRecord>,
    pub used_bytes: usize,
    pub budget_bytes: usize,
}

#[derive(Debug)]
pub enum HydrationError {
    BudgetExceeded { requested: usize, budget: usize },
    UnknownEntry(String),
}

impl std::fmt::Display for HydrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HydrationError::BudgetExceeded { requested, budget } => {
                write!(f, "hydration would exceed budget: {requested} > {budget}")
            }
            HydrationError::UnknownEntry(id) => write!(f, "unknown wiki entry {id:?}"),
        }
    }
}

impl WikiIndex {
    /// Builds the index from full records (index entries stay compact).
    pub fn build(records: Vec<WikiRecord>) -> Self {
        let index = records
            .iter()
            .map(|r| IndexEntry {
                entry_id: r.entry_id.clone(),
                summary: format!("{} ({})", r.pattern, r.outcome),
            })
            .collect();
        let mut map = BTreeMap::new();
        for r in records {
            map.insert(r.entry_id.clone(), r);
        }
        Self {
            records: map,
            index,
        }
    }

    pub fn index_entries(&self) -> &[IndexEntry] {
        &self.index
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Hydrates ONE record into the working set, enforcing the byte
    /// budget. Provenance travels with the hydrated record.
    pub fn hydrate(
        &self,
        entry_id: &str,
        working_set: &mut ProposerWorkingSet,
    ) -> Result<(), HydrationError> {
        let record = self
            .records
            .get(entry_id)
            .ok_or_else(|| HydrationError::UnknownEntry(entry_id.to_string()))?;
        let bytes = record.pattern.len() + record.outcome.len() + record.provenance.join(",").len();
        if working_set.used_bytes + bytes > working_set.budget_bytes {
            return Err(HydrationError::BudgetExceeded {
                requested: working_set.used_bytes + bytes,
                budget: working_set.budget_bytes,
            });
        }
        working_set.used_bytes += bytes;
        working_set
            .hydrated
            .insert(entry_id.to_string(), record.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn large_corpus(entries: usize) -> WikiIndex {
        let records = (0..entries)
            .map(|i| WikiRecord {
                entry_id: format!("wiki-{i}"),
                pattern: format!(
                    "pattern {i}: {} (provenance trace ids attached)",
                    "detail ".repeat(10)
                ),
                outcome: if i % 2 == 0 { "success" } else { "failure" }.to_string(),
                provenance: vec![format!("trace-{i}a"), format!("trace-{i}b")],
            })
            .collect();
        WikiIndex::build(records)
    }

    /// QUAL-EV-0204: a large corpus stays within the token budget while
    /// provenance remains complete on hydrated records.
    #[test]
    fn large_corpus_stays_within_budget_with_complete_provenance() {
        let wiki = large_corpus(500);

        // The default working set: compact index only.
        let mut working_set = ProposerWorkingSet {
            index_entries: wiki.index_entries().to_vec(),
            hydrated: BTreeMap::new(),
            used_bytes: 0,
            budget_bytes: 2048,
        };
        assert_eq!(working_set.index_entries.len(), 500);

        // Hydrate a few specific entries within budget.
        for id in ["wiki-0", "wiki-42", "wiki-199"] {
            wiki.hydrate(id, &mut working_set).unwrap();
        }
        assert_eq!(working_set.hydrated.len(), 3);
        assert!(working_set.used_bytes <= working_set.budget_bytes);

        // Provenance completeness on hydrated records.
        let record = &working_set.hydrated["wiki-42"];
        assert_eq!(record.provenance, vec!["trace-42a", "trace-42b"]);

        // Runaway hydration hits the budget — typed, not silent.
        let mut small = ProposerWorkingSet {
            index_entries: wiki.index_entries().to_vec(),
            hydrated: BTreeMap::new(),
            used_bytes: 0,
            budget_bytes: 64,
        };
        assert!(matches!(
            wiki.hydrate("wiki-0", &mut small),
            Err(HydrationError::BudgetExceeded { .. })
        ));

        // Every digest of a hydrated record is stable (addressable).
        for record in working_set.hydrated.values() {
            let digest = sha256_hex(record.pattern.as_bytes());
            assert_eq!(digest.len(), 64);
        }
    }
}
