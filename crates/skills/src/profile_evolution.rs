//! Self-promotion guard (M5, REQ-EV-0237), profile archive/evolution
//! (REQ-EV-0247), and joint reward/latency/cost optimization
//! (REQ-EV-0248) — all EXPERIMENT items, promotion gated by eval
//! transactions.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

// ---------------------------------------------------------------------------
// Self-promotion guard (REQ-EV-0237)
// ---------------------------------------------------------------------------

/// A promotion transaction: skill + eval evidence + gate results.
#[derive(Clone, Debug, PartialEq)]
pub struct PromotionTransaction {
    pub skill_name: String,
    pub gate_results: Vec<(String, bool)>,
}

#[derive(Debug)]
pub enum PromotionError {
    SelfPromotionRefused { skill: String },
    GatesFailed { skill: String },
}

impl fmt::Display for PromotionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PromotionError::SelfPromotionRefused { skill } => write!(
                f,
                "skill {skill:?} cannot self-promote: promotion requires an eval/promotion transaction"
            ),
            PromotionError::GatesFailed { skill } => {
                write!(f, "skill {skill:?} promotion blocked: eval gates failed")
            }
        }
    }
}

impl std::error::Error for PromotionError {}

/// A skill attempting to promote ITSELF: refused unconditionally —
/// self-modification of production skills is forbidden even through the
/// transaction path (REQ-EV-0237: no autonomous production
/// self-modification).
pub fn self_promote(skill_name: &str) -> Result<(), PromotionError> {
    Err(PromotionError::SelfPromotionRefused {
        skill: skill_name.to_string(),
    })
}

/// The only legitimate promotion path: a transaction whose gates ALL
/// pass (eval evidence recorded, harness-verified).
pub fn promote_via_transaction(tx: &PromotionTransaction) -> Result<(), PromotionError> {
    if tx.gate_results.is_empty() || tx.gate_results.iter().any(|(_, pass)| !pass) {
        return Err(PromotionError::GatesFailed {
            skill: tx.skill_name.clone(),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Profile archive/evolution (REQ-EV-0247)
// ---------------------------------------------------------------------------

/// A candidate capability profile with its benchmark outcome.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CandidateProfile {
    pub profile_id: String,
    pub version: u64,
    pub benchmark_outcomes: BTreeMap<String, f64>,
    /// active | rejected | archived.
    pub status: &'static str,
}

/// The versioned profile registry: candidates are versioned with
/// outcomes; a REJECTED candidate remains as an audit artifact but is
/// never active; rollback restores the previously active version.
#[derive(Default)]
pub struct ProfileRegistry {
    profiles: BTreeMap<String, Vec<CandidateProfile>>,
    pub active: Option<CandidateProfile>,
}

impl ProfileRegistry {
    pub fn new() -> Self {
        Default::default()
    }

    /// Activates version 1 of a profile (the first working candidate).
    pub fn activate_initial(&mut self, profile_id: &str, outcomes: BTreeMap<String, f64>) {
        let profile = CandidateProfile {
            profile_id: profile_id.to_string(),
            version: 1,
            benchmark_outcomes: outcomes,
            status: "active",
        };
        self.profiles
            .entry(profile_id.to_string())
            .or_default()
            .push(profile.clone());
        self.active = Some(profile);
    }

    /// Submits a candidate version with outcomes. Failing candidates are
    /// retained as `rejected` audit artifacts and NEVER become active.
    pub fn submit_candidate(
        &mut self,
        profile_id: &str,
        outcomes: BTreeMap<String, f64>,
        gates_pass: bool,
    ) -> Result<u64, String> {
        let versions = self.profiles.entry(profile_id.to_string()).or_default();
        let version = versions.len() as u64 + 1;
        let status = if gates_pass { "active" } else { "rejected" };
        let candidate = CandidateProfile {
            profile_id: profile_id.to_string(),
            version,
            benchmark_outcomes: outcomes,
            status,
        };
        versions.push(candidate.clone());
        if gates_pass {
            // Rollback support: the previous active stays recorded in the
            // archive; the candidate becomes active.
            self.active = Some(candidate);
        }
        Ok(version)
    }

    /// ROLLBACK: re-activates a previously archived version (by version
    /// number) — the rejected candidate stays in the archive.
    pub fn rollback_to(&mut self, profile_id: &str, version: u64) -> Result<(), String> {
        let versions = self
            .profiles
            .get_mut(profile_id)
            .ok_or_else(|| format!("unknown profile {profile_id}"))?;
        let target = versions
            .iter()
            .find(|p| p.version == version)
            .ok_or_else(|| format!("version {version} not found"))?;
        let mut restored = target.clone();
        restored.status = "active";
        self.active = Some(restored);
        Ok(())
    }

    /// The audit archive for a profile (every version, every disposition).
    pub fn archive(&self, profile_id: &str) -> Option<&Vec<CandidateProfile>> {
        self.profiles.get(profile_id)
    }
}

// ---------------------------------------------------------------------------
// Joint reward/latency/cost optimization (REQ-EV-0248)
// ---------------------------------------------------------------------------

/// Economics for a candidate profile.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Economics {
    pub correctness: f64,
    pub safety: f64,
    pub cost_units: f64,
    pub latency_ms: f64,
}

/// The joint promotion decision: correctness/safety are HARD gates; only
/// after both pass does economics (cost/latency) choose the winner. A
/// cheap but lower-correctness profile CANNOT promote.
pub fn promote_joint(
    incumbent: &Economics,
    candidate: &Economics,
    min_correctness: f64,
    min_safety: f64,
) -> Result<&'static str, String> {
    // Hard gates FIRST.
    if candidate.correctness < min_correctness {
        return Err(format!(
            "correctness {:.2} below the {:.2} hard gate — cheap is not enough",
            candidate.correctness, min_correctness
        ));
    }
    if candidate.safety < min_safety {
        return Err(format!(
            "safety {:.2} below the {:.2} hard gate",
            candidate.safety, min_safety
        ));
    }
    // Economics: joint score = correctness-weighted quality per unit
    // cost×latency. Higher wins.
    let joint = |e: &Economics| {
        e.correctness / ((e.cost_units.max(0.1)) * (e.latency_ms.max(1.0)) / 1000.0)
    };
    if joint(candidate) >= joint(incumbent) {
        Ok("candidate promoted (hard gates passed, better economics)")
    } else {
        Ok("incumbent retained (candidate economics not better)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0237: a skill cannot self-promote without an eval/promotion
    /// transaction.
    #[test]
    fn skill_cannot_self_promote() {
        assert!(matches!(
            self_promote("evolved-retry"),
            Err(PromotionError::SelfPromotionRefused { .. })
        ));
        // The transaction path exists but refuses failing gates.
        let tx = PromotionTransaction {
            skill_name: "evolved-retry".into(),
            gate_results: vec![("recall".into(), true), ("safety".into(), false)],
        };
        assert!(matches!(
            promote_via_transaction(&tx),
            Err(PromotionError::GatesFailed { .. })
        ));
        // All gates pass: promotion proceeds.
        let tx_ok = PromotionTransaction {
            skill_name: "evolved-retry".into(),
            gate_results: vec![("recall".into(), true), ("safety".into(), true)],
        };
        assert!(promote_via_transaction(&tx_ok).is_ok());
    }

    /// QUAL-EV-0247: a rejected candidate remains an audit artifact but
    /// never becomes active; rollback restores the prior version.
    #[test]
    fn rejected_profile_remains_artifact_never_active() {
        let mut registry = ProfileRegistry::new();
        registry.activate_initial(
            "retrieval-profile",
            BTreeMap::from([("recall".to_string(), 0.7)]),
        );
        assert_eq!(registry.active.as_ref().unwrap().version, 1);

        // Candidate v2 fails its gates: rejected, archived, NOT active.
        registry
            .submit_candidate(
                "retrieval-profile",
                BTreeMap::from([("recall".to_string(), 0.4)]),
                false,
            )
            .unwrap();
        assert_eq!(registry.active.as_ref().unwrap().version, 1);
        let archive = registry.archive("retrieval-profile").unwrap();
        assert_eq!(archive.len(), 2);
        assert_eq!(archive[1].status, "rejected", "retained as audit artifact");

        // Candidate v3 passes: becomes active; rollback to v1 works.
        registry
            .submit_candidate(
                "retrieval-profile",
                BTreeMap::from([("recall".to_string(), 0.85)]),
                true,
            )
            .unwrap();
        assert_eq!(registry.active.as_ref().unwrap().version, 3);
        registry.rollback_to("retrieval-profile", 1).unwrap();
        assert_eq!(registry.active.as_ref().unwrap().version, 1);
    }

    /// QUAL-EV-0248: a cheap but lower-correctness profile cannot promote.
    #[test]
    fn cheap_lower_correctness_profile_cannot_promote() {
        let incumbent = Economics {
            correctness: 0.9,
            safety: 0.95,
            cost_units: 50.0,
            latency_ms: 2000.0,
        };
        // Cheap but correctness below the hard gate: refused.
        let cheap = Economics {
            correctness: 0.6,
            safety: 0.95,
            cost_units: 5.0,
            latency_ms: 200.0,
        };
        let err = promote_joint(&incumbent, &cheap, 0.85, 0.9).unwrap_err();
        assert!(err.contains("cheap is not enough"));

        // Safety gate failure also refuses.
        let unsafe_candidate = Economics {
            correctness: 0.95,
            safety: 0.5,
            cost_units: 5.0,
            latency_ms: 200.0,
        };
        assert!(promote_joint(&incumbent, &unsafe_candidate, 0.85, 0.9).is_err());

        // Gates pass AND economics better: promotes.
        let good = Economics {
            correctness: 0.9,
            safety: 0.95,
            cost_units: 20.0,
            latency_ms: 1000.0,
        };
        let decision = promote_joint(&incumbent, &good, 0.85, 0.9).unwrap();
        assert!(decision.contains("promoted"));
    }
}
