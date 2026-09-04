//! Context window breakdown (M3, REQ-EV-0131, Context Inspector) and
//! conversation compaction fidelity (REQ-EV-0130).
//!
//! The inspector exposes composition, token cost, source, freshness and
//! reasons for every byte in a compiled request envelope — and its totals
//! must MATCH the actual provider request exactly (no hidden bytes).
//! Compaction fidelity: a critical-fact corpus must survive at or above
//! the fidelity threshold through CompactionManifest.build.

use modbit_compaction::{CompactionManifest, Label};
use serde::{Deserialize, Serialize};

/// One accounted slice of the context window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowSlice {
    pub source: String,
    pub content: String,
    /// Reason this slice is present (why it was included).
    pub reason: String,
    /// True when the slice came from a fresh read (REQ-EV-0002).
    pub fresh: bool,
}

impl WindowSlice {
    pub fn new(source: &str, content: impl Into<String>, reason: &str, fresh: bool) -> Self {
        Self {
            source: source.to_string(),
            content: content.into(),
            reason: reason.to_string(),
            fresh,
        }
    }
}

/// The full breakdown of one provider request envelope.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WindowBreakdown {
    pub slices: Vec<WindowSlice>,
}

impl WindowBreakdown {
    pub fn new() -> Self {
        Self { slices: Vec::new() }
    }

    pub fn push(&mut self, slice: WindowSlice) {
        self.slices.push(slice);
    }

    /// Total bytes across all slices.
    pub fn total_bytes(&self) -> usize {
        self.slices.iter().map(|s| s.content.len()).sum()
    }

    /// Bytes per source (the composition view).
    pub fn composition(&self) -> Vec<(String, usize)> {
        let mut out: Vec<(String, usize)> = Vec::new();
        for slice in &self.slices {
            match out.iter_mut().find(|(src, _)| *src == slice.source) {
                Some((_, n)) => *n += slice.content.len(),
                None => out.push((slice.source.clone(), slice.content.len())),
            }
        }
        out
    }

    /// Freshness: the share of bytes read fresh at pack time (basis pts).
    pub fn fresh_bps(&self) -> u64 {
        let total = self.total_bytes();
        if total == 0 {
            return 10_000;
        }
        let fresh = self
            .slices
            .iter()
            .filter(|s| s.fresh)
            .map(|s| s.content.len())
            .sum::<usize>();
        (fresh as u64 * 10_000) / total as u64
    }
}

/// Verifies the inspector's totals MATCH the actual provider request
/// envelope (QUAL-EV-0131): the sum of slice bytes must equal the size of
/// the serialized request's content payload.
pub fn verify_totals_match_envelope(
    breakdown: &WindowBreakdown,
    envelope_content_bytes: usize,
) -> bool {
    breakdown.total_bytes() == envelope_content_bytes
}

/// The fidelity threshold for critical-fact corpora (docs/19).
pub const FIDELITY_THRESHOLD_BPS: u64 = 8_000;

/// Runs a critical-fact corpus through compaction and measures fidelity:
/// the share of corpus items that survive verbatim in the manifest's
/// preserved set (Hot tier) or compressed projection.
pub fn compaction_fidelity_bps(
    corpus: &[(Option<Label>, String)],
    manifest: &CompactionManifest,
) -> u64 {
    if corpus.is_empty() {
        return 10_000;
    }
    let survived = corpus
        .iter()
        .filter(|(_, text)| {
            // Verbatim survival: Hot-preserved, or referenced in the
            // projection (compressed form keeps a truncated marker).
            manifest.preserved.iter().any(|p| &p.text == text)
                || manifest
                    .compressed_projection
                    .contains(&format!("[{}]", text.chars().take(40).collect::<String>()))
        })
        .count();
    (survived as u64 * 10_000) / corpus.len() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0131: inspector totals match the actual request envelope.
    #[test]
    fn inspector_totals_match_envelope() {
        let mut breakdown = WindowBreakdown::new();
        breakdown.push(WindowSlice::new(
            "system-policy",
            "be terse",
            "canonical segment 1",
            true,
        ));
        breakdown.push(WindowSlice::new(
            "task-context",
            "objective: summarize workspace",
            "task scope",
            true,
        ));
        breakdown.push(WindowSlice::new(
            "recent-events",
            "3 tool calls since last turn",
            "continuity",
            false,
        ));

        // Composition sums per source; freshness reflects the stale slice.
        assert_eq!(
            breakdown.composition(),
            vec![
                ("system-policy".to_string(), 8),
                ("task-context".to_string(), 30),
                ("recent-events".to_string(), 28)
            ]
        );
        assert!(breakdown.fresh_bps() < 10_000);

        // Totals match a real envelope: build the "request" from the same
        // slices and compare.
        let envelope: String = breakdown
            .slices
            .iter()
            .map(|s| s.content.as_str())
            .collect::<Vec<_>>()
            .join("");
        assert!(verify_totals_match_envelope(&breakdown, envelope.len()));
        // And a mismatching envelope is detected.
        assert!(!verify_totals_match_envelope(
            &breakdown,
            envelope.len() + 1
        ));
    }

    /// QUAL-EV-0130: a critical-fact corpus meets the fidelity threshold
    /// through compaction.
    #[test]
    fn critical_fact_corpus_meets_fidelity_threshold() {
        let corpus: Vec<(Option<Label>, String)> = vec![
            (
                Some(Label::Instruction),
                "always run cargo clippy before commit".into(),
            ),
            (Some(Label::Decision), "use WAL mode".into()),
            (Some(Label::Approval), "migration approved".into()),
            (None, "explored the retrieval module".into()),
            (None, "benchmark harness drafted".into()),
        ];
        let manifest = CompactionManifest::build(7, &corpus);
        let fidelity = compaction_fidelity_bps(&corpus, &manifest);
        assert!(
            fidelity >= FIDELITY_THRESHOLD_BPS,
            "fidelity {fidelity} bps below threshold"
        );
        // Mandatory labels survive verbatim regardless of threshold.
        for text in [
            "always run cargo clippy before commit",
            "use WAL mode",
            "migration approved",
        ] {
            assert!(manifest.preserved.iter().any(|p| p.text == text));
        }
    }
}
