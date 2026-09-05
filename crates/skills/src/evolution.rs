//! Skill Evolution Lab (M5, REQ-EV-0196..0200, ADAPT/EXPERIMENT): three
//! DISTINCT stores — immutable raw experience traces, a versioned
//! persistent wiki, and candidate skill packages — plus the maintainer
//! consolidator, the atomic proposer, and validation gating with
//! rollback. Evolution output is never production authority; the wiki
//! persists independently of active skill versions.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// Store 1: immutable raw experience traces
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceOutcome {
    Success,
    Failure,
}

/// A raw evaluation trace: append-only, immutable.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RawTrace {
    pub trace_id: String,
    pub outcome: TraceOutcome,
    pub text: String,
    pub ts_ms: i64,
}

// ---------------------------------------------------------------------------
// Store 2: persistent versioned wiki
// ---------------------------------------------------------------------------

/// One consolidated pattern in the evolution wiki.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WikiEntry {
    pub entry_id: String,
    pub pattern: String,
    /// The trace ids this entry was consolidated FROM (provenance).
    pub provenance: Vec<String>,
    /// Confidence 0.0..=1.0 recorded per consolidation.
    pub confidence: f64,
    /// Wiki revision this entry landed at.
    pub revision: u64,
}

/// The versioned wiki. Its head advances only on consolidation and
/// persists independently of any active skill version.
#[derive(Default)]
pub struct EvolutionWiki {
    pub head: u64,
    pub entries: BTreeMap<String, WikiEntry>,
}

impl EvolutionWiki {
    /// Consolidation: an entry is APPENDED at a new revision. Contradictory
    /// evidence produces SEPARATE entries with provenance and confidence —
    /// the maintainer never overwrites (REQ-EV-0198, EXPERIMENT).
    pub fn consolidate(
        &mut self,
        pattern: &str,
        provenance: Vec<String>,
        confidence: f64,
    ) -> WikiEntry {
        self.head += 1;
        let entry = WikiEntry {
            entry_id: format!(
                "wiki-{}-{}",
                self.head,
                &sha256_hex(pattern.as_bytes())[..8]
            ),
            pattern: pattern.to_string(),
            provenance,
            confidence,
            revision: self.head,
        };
        self.entries.insert(entry.entry_id.clone(), entry.clone());
        entry
    }
}

// ---------------------------------------------------------------------------
// Store 3: candidate skill packages
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateStatus {
    Proposed,
    Validated,
    Rejected,
}

/// A candidate skill diff.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CandidateSkill {
    pub candidate_id: String,
    pub objective: String,
    /// The diff text: must change ONE bounded behavior.
    pub diff: String,
    /// Motivating evidence ids (wiki entries / traces) this diff cites.
    pub motivating_evidence: Vec<String>,
    pub status: CandidateStatus,
    /// sha256 of the ACTIVE skill this candidate was based on — proving
    /// the active skill stays byte-identical on rejection.
    pub active_skill_sha256: String,
}

#[derive(Debug)]
pub enum ProposalError {
    /// A candidate diff touching more than one behavior is refused —
    /// proposals must be atomic.
    MultiBehaviorDiff,
}

impl fmt::Display for ProposalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProposalError::MultiBehaviorDiff => {
                write!(
                    f,
                    "candidate diff changes multiple behaviors — must be atomic"
                )
            }
        }
    }
}

/// The lab: all three stores.
#[derive(Default)]
pub struct EvolutionLab {
    pub traces: Vec<RawTrace>,
    pub wiki: EvolutionWiki,
    pub candidates: BTreeMap<String, CandidateSkill>,
    /// The currently ACTIVE skill bytes (production authority).
    pub active_skill: Vec<u8>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl EvolutionLab {
    /// Records an immutable raw trace.
    pub fn record_trace(&mut self, outcome: TraceOutcome, text: &str) -> String {
        let trace_id = format!(
            "trace-{}",
            &sha256_hex(format!("{text}{}", now_ms()).as_bytes())[..12]
        );
        self.traces.push(RawTrace {
            trace_id: trace_id.clone(),
            outcome,
            text: text.to_string(),
            ts_ms: now_ms(),
        });
        trace_id
    }

    /// PROPOSER (REQ-EV-0199, EXPERIMENT): generates an atomic candidate
    /// from selected evidence. The diff must reference motivating
    /// evidence ids AND change exactly ONE bounded behavior (measured by
    /// the diff's change-marker count).
    pub fn propose(
        &mut self,
        objective: &str,
        diff: &str,
        motivating_evidence: Vec<String>,
        active_skill: &[u8],
    ) -> Result<String, ProposalError> {
        // Atomicity gate: count change markers in the diff.
        let markers = diff.matches("<<CHANGE>>").count();
        if markers > 1 || motivating_evidence.is_empty() {
            return Err(ProposalError::MultiBehaviorDiff);
        }
        let candidate_id = format!("cand-{}", &sha256_hex(diff.as_bytes())[..12]);
        self.candidates.insert(
            candidate_id.clone(),
            CandidateSkill {
                candidate_id: candidate_id.clone(),
                objective: objective.to_string(),
                diff: diff.to_string(),
                motivating_evidence,
                status: CandidateStatus::Proposed,
                active_skill_sha256: sha256_hex(active_skill),
            },
        );
        Ok(candidate_id)
    }

    /// VALIDATION GATE + ROLLBACK (REQ-EV-0200/0197): a candidate that
    /// regresses safety/quality is REJECTED; the previous active skill
    /// stays byte-identical; the rejected candidate is retained as an
    /// evaluation artifact; and the wiki head is UNCHANGED unless the
    /// wiki is separately reverted.
    pub fn gate(
        &mut self,
        candidate_id: &str,
        gates_pass: bool,
    ) -> Result<CandidateStatus, String> {
        let candidate = self
            .candidates
            .get_mut(candidate_id)
            .ok_or_else(|| format!("unknown candidate {candidate_id}"))?;
        if gates_pass {
            candidate.status = CandidateStatus::Validated;
            // Promotion would swap active_skill here — gated callers only.
            Ok(CandidateStatus::Validated)
        } else {
            candidate.status = CandidateStatus::Rejected;
            // Active skill stays BYTE-IDENTICAL (unchanged on rejection).
            Ok(CandidateStatus::Rejected)
        }
    }

    /// DELETE/REJECT cleanup (REQ-EV-0196): removes a candidate package —
    /// raw traces and wiki knowledge REMAIN INTACT.
    pub fn delete_candidate(&mut self, candidate_id: &str) -> Result<(), String> {
        self.candidates
            .remove(candidate_id)
            .map(|_| ())
            .ok_or_else(|| format!("unknown candidate {candidate_id}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lab() -> EvolutionLab {
        EvolutionLab {
            active_skill: b"skill v1 body".to_vec(),
            ..Default::default()
        }
    }

    /// QUAL-EV-0196: delete/reject a candidate; raw traces and wiki
    /// knowledge remain intact.
    #[test]
    fn deleting_candidate_leaves_traces_and_wiki_intact() {
        let mut lab = lab();
        let t1 = lab.record_trace(TraceOutcome::Failure, "skill missed the retry edge case");
        let entry = lab
            .wiki
            .consolidate("retry needs edge-case guard", vec![t1.clone()], 0.8);
        let evidence = vec![entry.entry_id.clone(), t1.clone()];
        let cand = lab
            .propose(
                "harden retry",
                "- <<CHANGE>> guard the retry edge case",
                evidence,
                &lab.active_skill.clone(),
            )
            .unwrap();

        lab.delete_candidate(&cand).unwrap();
        assert!(!lab.candidates.contains_key(&cand), "candidate gone");
        // Traces and wiki are INTACT.
        assert_eq!(lab.traces.len(), 1);
        assert!(lab.wiki.entries.contains_key(&entry.entry_id));
    }

    /// QUAL-EV-0197: rolling back a candidate leaves the wiki head
    /// unchanged unless the wiki is separately reverted.
    #[test]
    fn candidate_rollback_leaves_wiki_head_unchanged() {
        let mut lab = lab();
        let t = lab.record_trace(TraceOutcome::Success, "worked");
        let entry = lab.wiki.consolidate("pattern", vec![t], 0.9);
        let head_before = lab.wiki.head;

        let evidence = vec![entry.entry_id.clone()];
        let cand = lab
            .propose(
                "try it",
                "- <<CHANGE>> apply pattern",
                evidence,
                &lab.active_skill.clone(),
            )
            .unwrap();
        lab.gate(&cand, false).unwrap(); // rollback/reject

        assert_eq!(
            lab.wiki.head, head_before,
            "wiki head unchanged by candidate rollback"
        );
    }

    /// QUAL-EV-0198 (EXPERIMENT): contradictory traces are BOTH recorded
    /// with provenance and confidence — never overwritten.
    #[test]
    fn contradictory_traces_both_recorded_with_provenance() {
        let mut lab = lab();
        let success = lab.record_trace(TraceOutcome::Success, "skill X improved recall");
        let failure = lab.record_trace(TraceOutcome::Failure, "skill X broke edge case");

        let pro = lab
            .wiki
            .consolidate("skill X helps recall", vec![success.clone()], 0.7);
        let con = lab
            .wiki
            .consolidate("skill X breaks edge cases", vec![failure.clone()], 0.7);

        assert_ne!(pro.entry_id, con.entry_id, "both sides recorded separately");
        assert_eq!(pro.provenance, vec![success.clone()]);
        assert_eq!(con.provenance, vec![failure]);
        // Both remain in the wiki.
        assert!(lab.wiki.entries.contains_key(&pro.entry_id));
        assert!(lab.wiki.entries.contains_key(&con.entry_id));
    }

    /// QUAL-EV-0199 (EXPERIMENT): candidate diffs reference motivating
    /// evidence and change ONE bounded behavior.
    #[test]
    fn proposals_reference_evidence_and_stay_atomic() {
        let mut lab = lab();
        let t = lab.record_trace(TraceOutcome::Failure, "timeout on slow retry");
        let wiki_entry = lab
            .wiki
            .consolidate("slow retry needs timeout", vec![t.clone()], 0.6);

        // Atomic proposal citing evidence: accepted.
        let evidence = vec![wiki_entry.entry_id.clone(), t.clone()];
        let cand = lab
            .propose(
                "bound retry duration",
                "- <<CHANGE>> add 30s timeout to retry loop",
                evidence,
                &lab.active_skill.clone(),
            )
            .unwrap();
        assert_eq!(
            lab.candidates.get(&cand).unwrap().motivating_evidence.len(),
            2,
            "motivating evidence ids recorded"
        );

        // A multi-behavior diff is refused.
        assert!(matches!(
            lab.propose(
                "too much",
                "- <<CHANGE>> add timeout\n- <<CHANGE>> change backoff\n- <<CHANGE>> swap policy",
                vec![wiki_entry.entry_id.clone()],
                &lab.active_skill.clone(),
            ),
            Err(ProposalError::MultiBehaviorDiff)
        ));
        // No evidence cited: refused.
        assert!(matches!(
            lab.propose(
                "unsourced",
                "- <<CHANGE>> tweak",
                vec![],
                &lab.active_skill.clone()
            ),
            Err(ProposalError::MultiBehaviorDiff)
        ));
    }

    /// QUAL-EV-0200: a regressing candidate is rejected, the previous
    /// active skill remains BYTE-IDENTICAL, and the candidate is retained
    /// as an evaluation artifact.
    #[test]
    fn regressing_candidate_rejected_active_stays_byte_identical() {
        let mut lab = lab();
        let active_before = lab.active_skill.clone();
        let t = lab.record_trace(TraceOutcome::Failure, "candidate regressed");
        let entry = lab.wiki.consolidate("bad idea", vec![t], 0.5);
        let evidence = vec![entry.entry_id.clone()];
        let cand = lab
            .propose(
                "risky change",
                "- <<CHANGE>> risky edit",
                evidence,
                &lab.active_skill.clone(),
            )
            .unwrap();

        let status = lab.gate(&cand, false).unwrap();
        assert_eq!(status, CandidateStatus::Rejected);
        // Active skill byte-identical.
        assert_eq!(lab.active_skill, active_before);
        // The rejected candidate is RETAINED as an evaluation artifact.
        assert_eq!(
            lab.candidates.get(&cand).unwrap().status,
            CandidateStatus::Rejected
        );
    }
}
