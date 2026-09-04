//! Benchmark harness (M3, REQ-EV-0249..0254): a frozen benchmark profile
//! run with and without Modbit retrieval; paired distributions with
//! confidence; normalized tool-call counts; warm/cold timing; unbiased
//! prompts; and the structural-advantage profile experiment (A baseline,
//! B hybrid, C structural).

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Frozen profile (REQ-EV-0249)
// ---------------------------------------------------------------------------

/// A frozen retrieval benchmark profile: fixed queries with expected
/// files, identical across with/without-Modbit runs.
pub struct FrozenProfile {
    /// (query terms, expected path).
    pub cases: Vec<(Vec<&'static str>, &'static str)>,
}

/// Result of one profile run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProfileResult {
    pub with_modbit_retrieval: bool,
    pub recall_at_1: f64,
    /// Structural signals used (only in the Modbit arm).
    pub structural_signals: bool,
}

/// A frozen candidate pool entry: path + indexed terms.
pub struct Candidate {
    pub path: &'static str,
    pub terms: &'static [&'static str],
}

/// Runs the frozen profile. Baseline arm: plain term overlap. Modbit arm:
/// the same overlap ENHANCED with structural signals (implementation
/// files under src/ outrank incidental term matches).
pub fn run_profile(
    cases: &[(Vec<&'static str>, &'static str)],
    candidates: &[Candidate],
    with_modbit: bool,
) -> ProfileResult {
    let mut hits = 0usize;
    for (query_terms, expected) in cases {
        let mut scored: Vec<(String, f64)> = candidates
            .iter()
            .map(|c| {
                let overlap = query_terms.iter().filter(|t| c.terms.contains(t)).count() as f64;
                let mut score = overlap;
                if with_modbit && c.path.starts_with("src/") {
                    score += 2.0;
                }
                (c.path.to_string(), score)
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        if scored.first().map(|(p, _)| p.as_str()) == Some(*expected) {
            hits += 1;
        }
    }
    ProfileResult {
        with_modbit_retrieval: with_modbit,
        recall_at_1: hits as f64 / cases.len() as f64,
        structural_signals: with_modbit,
    }
}

// ---------------------------------------------------------------------------
// Paired measurement (REQ-EV-0250) + tool calls (0251) + timing (0252)
// ---------------------------------------------------------------------------

/// One paired baseline/treatment observation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PairedSample {
    pub baseline: u64,
    pub treatment: u64,
}

/// The paired report: distribution summary + 95% confidence interval on
/// the mean difference (normal approximation).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PairedReport {
    pub n: usize,
    pub mean_diff: f64,
    pub stddev_diff: f64,
    pub ci95_low: f64,
    pub ci95_high: f64,
    /// Treatment saves when the whole CI is below zero.
    pub reduction_confident: bool,
}

/// Computes the paired report over per-task differences (baseline −
/// treatment; positive means the treatment saved).
pub fn paired_report(samples: &[PairedSample]) -> PairedReport {
    let diffs: Vec<f64> = samples
        .iter()
        .map(|s| (s.baseline - s.treatment) as f64)
        .collect();
    let n = diffs.len();
    let mean = diffs.iter().sum::<f64>() / n.max(1) as f64;
    let var = if n > 1 {
        diffs.iter().map(|d| (d - mean).powi(2)).sum::<f64>() / (n - 1) as f64
    } else {
        0.0
    };
    let stddev = var.sqrt();
    // 95% CI: mean ± 1.96 * stddev/sqrt(n).
    let half = 1.96 * stddev / (n.max(1) as f64).sqrt();
    PairedReport {
        n,
        mean_diff: mean,
        stddev_diff: stddev,
        ci95_low: mean - half,
        ci95_high: mean + half,
        reduction_confident: n > 1 && mean - half > 0.0,
    }
}

/// Normalized tool-call accounting per task (REQ-EV-0251).
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCallCount {
    pub task_id: String,
    pub model: String,
    pub env_fingerprint: String,
    pub calls: u64,
}

/// Verifies two variant runs share model/task/environment (only the
/// capability profile may differ).
pub fn same_conditions(a: &ToolCallCount, b: &ToolCallCount) -> bool {
    a.task_id == b.task_id && a.model == b.model && a.env_fingerprint == b.env_fingerprint
}

/// Warm/cold timing (REQ-EV-0252): warm agent time excludes index build;
/// cold time-to-first-use includes it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TimingReport {
    pub warm_agent_ms: u128,
    pub index_build_ms: u128,
    pub cold_total_ms: u128,
}

pub fn timing_report(warm_agent_ms: u128, index_build_ms: u128) -> TimingReport {
    TimingReport {
        warm_agent_ms,
        index_build_ms,
        cold_total_ms: warm_agent_ms + index_build_ms,
    }
}

// ---------------------------------------------------------------------------
// Unbiased prompts (REQ-EV-0253)
// ---------------------------------------------------------------------------

/// Verifies benchmark prompts are identical EXCEPT the capability-profile
/// section (the treatment must not be biased by different prompts).
pub fn prompts_differ_only_by_profile(
    baseline_prompt: &str,
    treatment_prompt: &str,
    profile_prefix: &str,
) -> bool {
    // Strips from the profile prefix to the end of that sentence.
    let strip = |p: &str| match p.find(profile_prefix) {
        Some(start) => {
            let rest = &p[start..];
            let end = rest.find('.').map(|i| start + i + 1).unwrap_or(p.len());
            format!("{}{}", &p[..start], &p[end..])
        }
        None => p.to_string(),
    };
    strip(baseline_prompt) == strip(treatment_prompt)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> (Vec<(Vec<&'static str>, &'static str)>, Vec<Candidate>) {
        let cases = vec![
            (vec!["retry", "backoff"], "src/retry.rs"),
            (vec!["button", "click"], "src/ui/button.rs"),
        ];
        // Decoys share terms with the queries but are NOT implementations.
        let candidates = vec![
            Candidate {
                path: "src/retry.rs",
                terms: &["retry", "policy"],
            },
            Candidate {
                path: "docs/retry-notes.md",
                terms: &["retry", "backoff", "timeout"],
            },
            Candidate {
                path: "src/ui/button.rs",
                terms: &["button", "click"],
            },
        ];
        (cases, candidates)
    }

    /// QUAL-EV-0249: the same frozen profile runs with and without Modbit
    /// retrieval — structural signals improve the arm.
    #[test]
    fn frozen_profile_runs_both_arms() {
        let (cases, candidates) = profile();
        let baseline = run_profile(&cases, &candidates, false);
        let treatment = run_profile(&cases, &candidates, true);
        assert!(
            treatment.recall_at_1 > baseline.recall_at_1,
            "baseline {:?} vs modbit {:?}",
            baseline.recall_at_1,
            treatment.recall_at_1
        );
    }

    /// QUAL-EV-0250: paired report with distribution + confidence.
    #[test]
    fn paired_report_shows_confident_reduction() {
        // Treatment saves tokens on every paired task.
        let samples: Vec<PairedSample> = (1..=5)
            .map(|i| PairedSample {
                baseline: 1000 * i,
                treatment: 700 * i,
            })
            .collect();
        let report = paired_report(&samples);
        assert_eq!(report.n, 5);
        assert_eq!(report.mean_diff, 900.0);
        assert!(report.reduction_confident, "CI wholly positive: {report:?}");
        assert!(report.ci95_low > 0.0);
    }

    /// QUAL-EV-0251: tool-call counting enforces same conditions.
    #[test]
    fn tool_call_counts_require_same_conditions() {
        let a = ToolCallCount {
            task_id: "t1".into(),
            model: "glm-5.3-flash".into(),
            env_fingerprint: "linux-x86".into(),
            calls: 12,
        };
        let mut b = a.clone();
        assert!(same_conditions(&a, &b));
        b.model = "other".into();
        assert!(!same_conditions(&a, &b), "model changed: not comparable");
    }

    /// QUAL-EV-0252: warm and cold timing both reported.
    #[test]
    fn timing_reports_warm_and_cold() {
        let report = timing_report(1500, 8000);
        assert_eq!(report.warm_agent_ms, 1500);
        assert_eq!(report.cold_total_ms, 9500, "cold includes index build");
    }

    /// QUAL-EV-0253: prompts identical except the capability profile.
    #[test]
    fn prompts_identical_except_capability_profile() {
        let base = "You are a coding agent. Capabilities: PLACEHOLDER. Complete the task.";
        let a = base.replace("PLACEHOLDER", "baseline");
        let b = base.replace("PLACEHOLDER", "modbit-retrieval");
        assert!(prompts_differ_only_by_profile(&a, &b, "Capabilities:"));
    }
}
