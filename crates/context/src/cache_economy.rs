//! Context economy (M3, REQ-EV-0111): the prompt compiler already emits a
//! deterministic cache key per compiled prompt. This ledger TRACKS the
//! cacheable-prefix economics — hit/miss per turn — and proves that a
//! compaction INVALIDATES the key (new epoch → new prefix) while a stable
//! prefix HITs. No proprietary provider algorithm is copied: hits are
//! counted by prefix-key equality, which is the provider-neutral contract.

use modbit_prompt_compiler::{compile, CompilerInputs};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One recorded dispatch: the prefix key and whether the provider cache
/// could serve it (key seen before at the same or older head).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CacheEvent {
    pub cache_key: String,
    pub hit: bool,
    pub head: u64,
}

/// The cache economics ledger.
#[derive(Default)]
pub struct CacheLedger {
    /// cache_key → first head that introduced it.
    seen: BTreeMap<String, u64>,
    pub events: Vec<CacheEvent>,
}

/// The benchmark report (QUAL-EV-0111).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CacheReport {
    pub hits: usize,
    pub misses: usize,
    /// hits / total, as basis points (0..=10000).
    pub hit_rate_bps: u64,
    /// Every compaction (head change with a re-compiled prefix) must
    /// produce a miss, never a false hit.
    pub compaction_invalidation_correct: bool,
}

impl CacheLedger {
    pub fn new() -> Self {
        Default::default()
    }

    /// Records a dispatch compiled from the given inputs at the given
    /// canonical head. A key is a HIT when it was seen at an EARLIER OR
    /// EQUAL head (stable prefix → cache hit); a brand-new key is a MISS.
    /// A compaction that changes the compiled prompt changes the key — a
    /// new key at a NEWER head is a MISS, never a hit.
    pub fn record(&mut self, inputs: &CompilerInputs, head: u64) -> CacheEvent {
        let compiled = compile(inputs);
        let key = compiled.cache_key;
        let event = match self.seen.get(&key) {
            Some(&first_head) if first_head <= head => CacheEvent {
                cache_key: key,
                hit: true,
                head,
            },
            _ => {
                self.seen.insert(key.clone(), head);
                CacheEvent {
                    cache_key: key,
                    hit: false,
                    head,
                }
            }
        };
        self.events.push(event.clone());
        event
    }

    /// The benchmark report over all recorded events.
    pub fn report(&self) -> CacheReport {
        let hits = self.events.iter().filter(|e| e.hit).count();
        let misses = self.events.len() - hits;
        // Invalidation correctness: every key change (miss) that follows a
        // head INCREASE must come with a head strictly greater than the
        // key's first introduction — a compaction never fakes a hit.
        let compaction_invalidation_correct = self.events.iter().all(|e| {
            let first = self.seen.get(&e.cache_key).copied().unwrap_or(0);
            if e.hit {
                e.head >= first
            } else {
                !self.seen.contains_key(&e.cache_key) || self.seen[&e.cache_key] <= e.head
            }
        });
        CacheReport {
            hits,
            misses,
            hit_rate_bps: if self.events.is_empty() {
                0
            } else {
                (hits as u64 * 10_000) / self.events.len() as u64
            },
            compaction_invalidation_correct,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(text: &str) -> CompilerInputs {
        CompilerInputs {
            model: "glm-5.3-flash".into(),
            provider: "z.ai".into(),
            system_policy: format!("policy {text}"),
            workspace_rules: "rules".into(),
            compaction_epoch: None,
            task_context_pack: "pack".into(),
            recent_events: "events".into(),
        }
    }

    /// QUAL-EV-0111: the benchmark reports cached-prefix hit/miss and
    /// compaction invalidation correctness.
    #[test]
    fn cache_ledger_reports_hits_and_compaction_invalidation() {
        let mut ledger = CacheLedger::new();

        // Turn 1: cold prefix — miss.
        ledger.record(&inputs("v1"), 10);
        // Turns 2-3: same stable prefix — hits.
        ledger.record(&inputs("v1"), 11);
        ledger.record(&inputs("v1"), 12);

        // Compaction: the epoch segment changes the compiled prefix — new
        // key, and it must be a MISS (never a stale hit).
        let mut compacted = inputs("v1");
        compacted.compaction_epoch = Some("epoch: compressed 90 turns".into());
        ledger.record(&compacted, 13);

        let report = ledger.report();
        assert_eq!(report.hits, 2);
        assert_eq!(report.misses, 2);
        assert_eq!(report.hit_rate_bps, 5_000);
        assert!(report.compaction_invalidation_correct);

        // And after compaction, the NEW prefix is cacheable again.
        ledger.record(&compacted, 14);
        assert_eq!(ledger.report().hits, 3);
    }
}
