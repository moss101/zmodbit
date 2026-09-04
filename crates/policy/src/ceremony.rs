//! Q&A → Plan → visual review admission (M2, REQ-EV-0265, ADAPT): the
//! workspace flow is ceremony-free BY DEFAULT. Clarification, planning,
//! and visual review are triggered by RISK or AMBIGUITY — never mandatory
//! ceremony for every task. Low-risk tasks proceed directly; high-risk
//! configured tasks must produce a plan and pass visual review.

use serde::{Deserialize, Serialize};

/// Which ceremonies a task must pass before execution begins.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ceremony {
    /// No gates: proceed directly (the default).
    None,
    /// Ambiguity detected: ask a clarifying question first.
    Clarify,
    /// High-risk task: a plan must be produced and approved.
    Plan,
    /// High-risk task: visual evidence review before merge/apply.
    VisualReview,
    /// High-risk with both configured.
    PlanAndVisualReview,
}

/// The operator's risk configuration (what counts as high-risk here).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CeremonyConfig {
    /// Description substrings that mark a task high-risk (e.g. "production",
    /// "irreversible", "secrets rotation").
    pub high_risk_patterns: Vec<String>,
    pub require_plan_for_high_risk: bool,
    pub require_visual_review_for_high_risk: bool,
}

impl CeremonyConfig {
    pub fn is_high_risk(&self, description: &str) -> bool {
        let lower = description.to_lowercase();
        self.high_risk_patterns
            .iter()
            .any(|p| lower.contains(&p.to_lowercase()))
    }
}

/// An ambiguity the runtime detected (missing required detail).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Ambiguity {
    pub missing: String,
    pub question: String,
    pub options: Vec<String>,
}

/// The admission decision for one task.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Admission {
    /// Proceed with no ceremony.
    Proceed,
    /// Ask the clarifying question before starting.
    NeedsClarification {
        missing: String,
        question: String,
    },
    /// Produce a plan for approval first.
    RequiresPlan {
        reason: String,
    },
    /// Apply only after visual review of the result.
    RequiresVisualReview {
        reason: String,
    },
    RequiresPlanAndVisualReview {
        reason: String,
    },
}

impl Admission {
    pub fn blocks_execution(&self) -> bool {
        !matches!(self, Admission::Proceed)
    }
}

/// Admits a task into execution (REQ-EV-0265). Deterministic and
/// explanation-carrying: low-risk skips ceremony; high-risk gets exactly
/// the ceremonies the operator configured; ambiguity outranks ceremony
/// (no point planning an unclear task).
pub fn admit(
    description: &str,
    ambiguity: Option<Ambiguity>,
    config: &CeremonyConfig,
) -> Admission {
    // Ambiguity first: clarification precedes any ceremony.
    if let Some(a) = ambiguity {
        return Admission::NeedsClarification {
            missing: a.missing,
            question: a.question,
        };
    }

    if !config.is_high_risk(description) {
        return Admission::Proceed;
    }

    // High-risk: apply exactly the configured ceremonies.
    let plan = config.require_plan_for_high_risk;
    let review = config.require_visual_review_for_high_risk;
    let reason = "high-risk task per operator configuration".to_string();
    match (plan, review) {
        (true, true) => Admission::RequiresPlanAndVisualReview { reason },
        (true, false) => Admission::RequiresPlan { reason },
        (false, true) => Admission::RequiresVisualReview { reason },
        (false, false) => Admission::Proceed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> CeremonyConfig {
        CeremonyConfig {
            high_risk_patterns: vec![
                "production".into(),
                "irreversible".into(),
                "drop table".into(),
            ],
            require_plan_for_high_risk: true,
            require_visual_review_for_high_risk: true,
        }
    }

    /// QUAL-EV-0265: low-risk task skips ceremony; high-risk configured
    /// task requires plan/review.
    #[test]
    fn low_risk_skips_ceremony_high_risk_requires_plan_and_review() {
        // Low-risk: proceeds with zero ceremony.
        let low = admit(
            "rename the `fetch_user` helper to `load_user`",
            None,
            &config(),
        );
        assert_eq!(low, Admission::Proceed);
        assert!(!low.blocks_execution());

        // High-risk by config pattern: plan + visual review required.
        let high = admit(
            "migrate the production database to the new schema",
            None,
            &config(),
        );
        assert!(matches!(
            high,
            Admission::RequiresPlanAndVisualReview { .. }
        ));
        assert!(high.blocks_execution());

        // High-risk with plan-only configuration.
        let plan_only = CeremonyConfig {
            require_plan_for_high_risk: true,
            require_visual_review_for_high_risk: false,
            ..config()
        };
        let mid = admit("rotate the secrets used in production", None, &plan_only);
        assert!(matches!(mid, Admission::RequiresPlan { .. }));

        // Ambiguity outranks ceremony: an unclear high-risk task asks first.
        let unclear = admit(
            "migrate the production database",
            Some(Ambiguity {
                missing: "target schema version".into(),
                question: "Which schema version should the migration target?".into(),
                options: vec!["v2".into(), "v3".into()],
            }),
            &config(),
        );
        assert!(matches!(unclear, Admission::NeedsClarification { .. }));
    }
}
