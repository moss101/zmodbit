//! Eval harness for skill evolution (M5, REQ-EV-0205/0206/0207, all
//! EXPERIMENT): a cross-model transfer matrix (baseline vs skill deltas
//! per model family, hidden-regression rejection), the
//! evolution-complements-model-scaling hypothesis measured with A/B
//! confidence intervals on identical tasks/environment, and the
//! persistent-knowledge ablation justifying the wiki's complexity.

use serde::{Deserialize, Serialize};

/// One model family in the transfer matrix.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelFamily {
    pub name: String,
}

/// Per-model measured outcome with and without the evolved skill.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelOutcome {
    pub model: String,
    pub baseline_score: f64,
    pub with_skill_score: f64,
}

impl ModelOutcome {
    pub fn delta(&self) -> f64 {
        self.with_skill_score - self.baseline_score
    }
}

/// The nightly matrix report (QUAL-EV-0205).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransferMatrix {
    pub outcomes: Vec<ModelOutcome>,
    /// A hidden regression on ANY model family vetoes broad promotion.
    pub hidden_regression: Option<String>,
}

/// Builds the matrix and detects hidden regressions (delta < 0 on any
/// family vetoes promotion even if the mean delta is positive).
pub fn build_matrix(outcomes: Vec<ModelOutcome>) -> TransferMatrix {
    let hidden_regression = outcomes
        .iter()
        .find(|o| o.delta() < 0.0)
        .map(|o| o.model.clone());
    TransferMatrix {
        outcomes,
        hidden_regression,
    }
}

impl TransferMatrix {
    pub fn mean_delta(&self) -> f64 {
        let n = self.outcomes.len().max(1) as f64;
        self.outcomes.iter().map(|o| o.delta()).sum::<f64>() / n
    }

    pub fn promotion_vetoed(&self) -> bool {
        self.hidden_regression.is_some()
    }
}

/// An A/B trial under IDENTICAL tasks/environment (REQ-EV-0206): same
/// task ids and environment fingerprint; the scores differ.
#[derive(Clone, Debug, PartialEq)]
pub struct AbTrial {
    pub task_id: String,
    pub env_fingerprint: String,
    pub control_score: f64,
    pub treatment_score: f64,
}

/// The A/B report with 95% confidence intervals on the paired deltas
/// (normal approximation) — the hypothesis test for
/// evolution-complements-scaling (treated as a research hypothesis, NOT
/// a Modbit claim).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AbReport {
    pub trials: usize,
    pub mean_delta: f64,
    pub ci95_low: f64,
    pub ci95_high: f64,
    /// Statistically meaningful: the 95% CI excludes zero.
    pub significant: bool,
    /// Environment fairness: all trials shared task/env fingerprints.
    pub environment_fair: bool,
}

pub fn ab_report(trials: &[AbTrial]) -> AbReport {
    let diffs: Vec<f64> = trials
        .iter()
        .map(|t| t.treatment_score - t.control_score)
        .collect();
    let n = diffs.len().max(1) as f64;
    let mean = diffs.iter().sum::<f64>() / n;
    let var = if trials.len() > 1 {
        diffs.iter().map(|d| (d - mean).powi(2)).sum::<f64>() / (trials.len() - 1) as f64
    } else {
        0.0
    };
    let half = 1.96 * var.sqrt() / n.sqrt();
    let env_fair = trials.iter().all(|t| {
        t.env_fingerprint
            == trials
                .first()
                .map(|f| f.env_fingerprint.as_str())
                .unwrap_or("")
    });
    AbReport {
        trials: trials.len(),
        mean_delta: mean,
        ci95_low: mean - half,
        ci95_high: mean + half,
        significant: trials.len() > 1 && mean - half > 0.0,
        environment_fair: env_fair,
    }
}

/// The ablation (REQ-EV-0207): the evolution-lab mechanism (wiki +
/// proposer) versus simpler skill refinement WITHOUT persistent
/// knowledge. Promotion of the mechanism requires a PRACTICALLY
/// meaningful lift (≥ `min_lift_bps` basis points on mean score).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AblationReport {
    pub with_wiki_mean: f64,
    pub without_wiki_mean: f64,
    pub lift_bps: u64,
    pub promotion_justified: bool,
}

pub fn run_ablation(
    with_wiki_scores: &[f64],
    without_wiki_scores: &[f64],
    min_lift_bps: u64,
) -> AblationReport {
    let mean = |xs: &[f64]| xs.iter().sum::<f64>() / xs.len().max(1) as f64;
    let with = mean(with_wiki_scores);
    let without = mean(without_wiki_scores);
    let lift_bps = if without > 0.0 {
        (((with - without) / without) * 10_000.0).max(0.0) as u64
    } else {
        0
    };
    AblationReport {
        with_wiki_mean: with,
        without_wiki_mean: without,
        lift_bps,
        promotion_justified: lift_bps >= min_lift_bps,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0205: the nightly matrix reports baseline vs skill deltas
    /// per model and rejects a hidden regression.
    #[test]
    fn transfer_matrix_rejects_hidden_regression() {
        let outcomes = vec![
            ModelOutcome {
                model: "glm-5.3-flash".into(),
                baseline_score: 0.6,
                with_skill_score: 0.8,
            },
            ModelOutcome {
                model: "other-family-a".into(),
                baseline_score: 0.7,
                with_skill_score: 0.85,
            },
            ModelOutcome {
                model: "other-family-b".into(),
                baseline_score: 0.8,
                with_skill_score: 0.75, // HIDDEN REGRESSION
            },
        ];
        let matrix = build_matrix(outcomes);
        assert!(matrix.mean_delta() > 0.0, "mean looks good on average");
        assert_eq!(
            matrix.hidden_regression.as_deref(),
            Some("other-family-b"),
            "hidden regression detected per-model"
        );
        assert!(matrix.promotion_vetoed(), "broad promotion vetoed");
    }

    /// QUAL-EV-0206: the A/B benchmark uses the same tasks/environment
    /// and records confidence intervals.
    #[test]
    fn ab_benchmark_records_confidence_intervals() {
        let trials: Vec<AbTrial> = (0..8)
            .map(|i| AbTrial {
                task_id: format!("task-{i}"),
                env_fingerprint: "linux-x86-frozen".into(),
                control_score: 0.5,
                treatment_score: 0.5 + 0.1 + (i as f64 * 0.01),
            })
            .collect();
        let report = ab_report(&trials);
        assert_eq!(report.trials, 8);
        assert!(report.environment_fair, "same tasks/environment");
        assert!(report.significant, "CI excludes zero: {report:?}");
        assert!(report.ci95_low > 0.0 && report.ci95_high > report.ci95_low);
    }

    /// QUAL-EV-0207: promotion of the evolution mechanism requires a
    /// practically meaningful lift over simpler refinement (ablation).
    #[test]
    fn ablation_gates_mechanism_promotion() {
        // Small lift: mechanism promotion NOT justified.
        let small = run_ablation(&[0.6, 0.62], &[0.6, 0.61], 500);
        assert!(small.lift_bps < 500);
        assert!(!small.promotion_justified);

        // Meaningful lift: justified.
        let meaningful = run_ablation(&[0.75, 0.77], &[0.6, 0.61], 500);
        assert!(meaningful.lift_bps >= 500);
        assert!(meaningful.promotion_justified);
    }
}
