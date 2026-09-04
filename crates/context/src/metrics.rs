//! Context efficiency metrics (M3, REQ-EV-0173): verified outcome per
//! token, latency, and cost — reported TOGETHER on one benchmark
//! dashboard, so quality and economics are never evaluated apart.

use serde::{Deserialize, Serialize};

/// One measured run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EfficiencyRecord {
    pub task_id: String,
    /// Whether deterministic verification passed for the run.
    pub outcome_verified: bool,
    pub input_tokens: u64,
    pub latency_ms: u128,
    /// Normalized cost units (e.g. micro-USD-equivalent).
    pub cost_units: u64,
}

/// The aggregated dashboard (QUAL-EV-0173): quality AND economics side by
/// side.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EfficiencyDashboard {
    pub runs: usize,
    /// Quality: share of runs with verified outcomes (bps).
    pub verified_rate_bps: u64,
    /// Economics: totals over all runs.
    pub total_tokens: u64,
    pub total_latency_ms: u128,
    pub total_cost_units: u64,
    /// Tokens and cost PER VERIFIED OUTCOME (the efficiency metric).
    pub tokens_per_verified_outcome: Option<u64>,
    pub cost_per_verified_outcome: Option<u64>,
}

#[derive(Default)]
pub struct Dashboard {
    records: Vec<EfficiencyRecord>,
}

impl Dashboard {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn record(&mut self, record: EfficiencyRecord) {
        self.records.push(record);
    }

    /// Builds the dashboard: quality (verified rate) computed together
    /// with the economics (totals and per-verified-outcome costs).
    pub fn report(&self) -> EfficiencyDashboard {
        let runs = self.records.len();
        let verified = self.records.iter().filter(|r| r.outcome_verified).count();
        let total_tokens: u64 = self.records.iter().map(|r| r.input_tokens).sum();
        let total_latency_ms: u128 = self.records.iter().map(|r| r.latency_ms).sum();
        let total_cost_units: u64 = self.records.iter().map(|r| r.cost_units).sum();
        let tokens_per = if verified > 0 {
            Some(total_tokens / verified as u64)
        } else {
            None
        };
        let cost_per = if verified > 0 {
            Some(total_cost_units / verified as u64)
        } else {
            None
        };
        EfficiencyDashboard {
            runs,
            verified_rate_bps: if runs == 0 {
                0
            } else {
                (verified as u64 * 10_000) / runs as u64
            },
            total_tokens,
            total_latency_ms,
            total_cost_units,
            tokens_per_verified_outcome: tokens_per,
            cost_per_verified_outcome: cost_per,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0173: the benchmark dashboard reports quality and economics
    /// together.
    #[test]
    fn dashboard_reports_quality_and_economics_together() {
        let mut dashboard = Dashboard::new();
        dashboard.record(EfficiencyRecord {
            task_id: "t1".into(),
            outcome_verified: true,
            input_tokens: 4000,
            latency_ms: 1200,
            cost_units: 30,
        });
        dashboard.record(EfficiencyRecord {
            task_id: "t2".into(),
            outcome_verified: true,
            input_tokens: 6000,
            latency_ms: 800,
            cost_units: 50,
        });
        dashboard.record(EfficiencyRecord {
            task_id: "t3".into(),
            outcome_verified: false,
            input_tokens: 10_000,
            latency_ms: 2000,
            cost_units: 60,
        });

        let report = dashboard.report();
        // Quality: 2/3 verified.
        assert_eq!(report.runs, 3);
        assert_eq!(report.verified_rate_bps, 6_666);
        // Economics: totals.
        assert_eq!(report.total_tokens, 20_000);
        assert_eq!(report.total_cost_units, 140);
        // Efficiency: per verified outcome.
        assert_eq!(report.tokens_per_verified_outcome, Some(10_000));
        assert_eq!(report.cost_per_verified_outcome, Some(70));
    }
}
