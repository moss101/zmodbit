//! Skill impact log (M3→M5, REQ-EV-0201) and PURPOSE/motivating-knowledge
//! linkage (REQ-EV-0202). The impact log records every proposal's diff,
//! source patterns, benchmark scores, disposition, model, and environment
//! — an audit can reconstruct WHY each skill version was accepted or
//! rejected. PURPOSE metadata links a skill to its motivating knowledge;
//! the runtime loads only the purpose SUMMARY — the detailed evolution
//! wiki stays inaccessible by default.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One impact-log entry: the full decision record for a candidate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ImpactEntry {
    pub candidate_id: String,
    pub diff: String,
    /// Wiki/trace ids the proposal cited.
    pub source_patterns: Vec<String>,
    pub benchmark_scores: BTreeMap<String, f64>,
    /// accepted | rejected.
    pub disposition: &'static str,
    pub model: String,
    pub environment: String,
}

/// The append-only impact log.
#[derive(Default)]
pub struct ImpactLog {
    entries: Vec<ImpactEntry>,
}

impl ImpactLog {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn record(&mut self, entry: ImpactEntry) {
        self.entries.push(entry);
    }

    /// Audit: reconstruct WHY a candidate was accepted/rejected.
    pub fn audit(&self, candidate_id: &str) -> Vec<&ImpactEntry> {
        self.entries
            .iter()
            .filter(|e| e.candidate_id == candidate_id)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// PURPOSE linkage (REQ-EV-0202)
// ---------------------------------------------------------------------------

/// The PURPOSE metadata bound to a skill.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkillPurpose {
    /// The compact summary the RUNTIME sees.
    pub purpose_summary: String,
    pub assumptions: Vec<String>,
    /// Evidence ids into the evolution wiki (NOT inlined into prompts).
    pub evidence_refs: Vec<String>,
}

impl SkillPurpose {
    /// The runtime view: ONLY the purpose summary. The detailed wiki
    /// remains inaccessible by default (QUAL-EV-0202).
    pub fn runtime_view(&self) -> String {
        self.purpose_summary.clone()
    }

    /// The evidence ids stay addressable, not inlined.
    pub fn evidence_ids(&self) -> &[String] {
        &self.evidence_refs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0201: the audit reconstructs why each skill version was
    /// accepted/rejected.
    #[test]
    fn audit_reconstructs_accept_reject_reasons() {
        let mut log = ImpactLog::new();
        log.record(ImpactEntry {
            candidate_id: "cand-1".into(),
            diff: "- <<CHANGE>> guard retry edge case".into(),
            source_patterns: vec!["wiki-1-retry".into()],
            benchmark_scores: BTreeMap::from([("recall".into(), 0.9), ("safety".into(), 1.0)]),
            disposition: "accepted",
            model: "glm-5.3-flash".into(),
            environment: "linux-x86".into(),
        });
        log.record(ImpactEntry {
            candidate_id: "cand-1".into(),
            diff: "- <<CHANGE>> aggressive parallel retries".into(),
            source_patterns: vec![],
            benchmark_scores: BTreeMap::from([("safety".into(), 0.4)]),
            disposition: "rejected",
            model: "glm-5.3-flash".into(),
            environment: "linux-x86".into(),
        });

        let audit = log.audit("cand-1");
        assert_eq!(audit.len(), 2);
        assert_eq!(audit[0].disposition, "accepted");
        assert_eq!(audit[1].disposition, "rejected");
        // WHY: the rejected one had a failing safety score.
        assert!(audit[1].benchmark_scores["safety"] < 0.5);
        // The accepted one cited a real source pattern.
        assert_eq!(audit[0].source_patterns, vec!["wiki-1-retry"]);
    }

    /// QUAL-EV-0202: runtime loads the purpose SUMMARY; the detailed wiki
    /// stays inaccessible by default.
    #[test]
    fn runtime_gets_purpose_summary_only() {
        let purpose = SkillPurpose {
            purpose_summary: "Retries with bounded backoff; never parallel.".into(),
            assumptions: vec!["transient failures dominate".into()],
            evidence_refs: vec!["wiki-3".to_string(), "wiki-7".to_string()],
        };
        let view = purpose.runtime_view();
        assert!(view.contains("bounded backoff"));
        // The wiki content itself is NOT in the runtime view.
        assert!(!view.contains("wiki"));
        // Evidence remains addressable by id, on demand.
        assert_eq!(purpose.evidence_ids(), &["wiki-3", "wiki-7"]);
    }
}
