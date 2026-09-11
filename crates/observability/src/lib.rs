//! Observability (Phase 5 item 4, docs/34 § Cost): REAL cost accounting
//! from provider usage frames + a JSON-lines run-cost ledger that rides
//! with the run plane. Per-model unit prices are CONFIGURATION (a static
//! table in this crate, overridable via env `MODBIT_PRICE_<MODEL>` =
//! "input_per_1k,output_per_1k" USD); unknown models cost $0 and are
//! reported with `priced: false` — never a fabricated number.

use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Mutex;

pub mod otlp;

/// Per-model USD pricing per 1k tokens (input, output).
fn price_table() -> BTreeMap<String, (f64, f64)> {
    let mut t = BTreeMap::new();
    // Defaults: models used by tests/CI and common providers. Operators
    // override via MODBIT_PRICE_<MODEL-upper-with-underscores>.
    t.insert("gpt-4o-mini".into(), (0.00015, 0.0006));
    t.insert("gpt-4o".into(), (0.0025, 0.01));
    t.insert("gpt-4.1-mini".into(), (0.0004, 0.0016));
    t.insert("claude-3-5-haiku".into(), (0.0008, 0.004));
    t.insert("fixture-model".into(), (0.0, 0.0));
    for (key, value) in std::env::vars() {
        if let Some(model) = key
            .strip_prefix("MODBIT_PRICE_")
            .map(|k| k.to_lowercase().replace('_', "-"))
        {
            let parts: Vec<&str> = value.split(',').map(str::trim).collect();
            if let [in_p, out_p] = parts[..] {
                if let (Ok(i), Ok(o)) = (in_p.parse::<f64>(), out_p.parse::<f64>()) {
                    t.insert(model, (i, o));
                }
            }
        }
    }
    t
}

/// One model invocation's cost contribution.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct InvocationCost {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// USD, rounded to 6 decimals. 0.0 when the model is unpriced.
    pub cost_usd: f64,
    pub priced: bool,
}

/// Computes the cost of one invocation from its usage frame.
pub fn invocation_cost(model: &str, input_tokens: u64, output_tokens: u64) -> InvocationCost {
    let table = price_table();
    let key = model.to_lowercase();
    match table.get(&key) {
        Some((i, o)) => InvocationCost {
            model: model.to_string(),
            input_tokens,
            output_tokens,
            cost_usd: (input_tokens as f64 / 1000.0) * i
                + (output_tokens as f64 / 1000.0) * o,
            priced: true,
        },
        None => InvocationCost {
            model: model.to_string(),
            input_tokens,
            output_tokens,
            cost_usd: 0.0,
            priced: false,
        },
    }
}

/// Accumulated run cost: sums invocation costs as the run progresses.
#[derive(Clone, Default, Debug, Serialize)]
pub struct RunCostLedger {
    pub model: String,
    pub invocations: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub unpriced_invocations: u32,
    entries: Vec<InvocationCost>,
}

impl RunCostLedger {
    pub fn new(model: &str) -> Self {
        RunCostLedger {
            model: model.to_string(),
            ..Default::default()
        }
    }

    /// Records one usage frame against the ledger.
    pub fn record(&mut self, cost: &InvocationCost) {
        self.invocations += 1;
        self.input_tokens += cost.input_tokens;
        self.output_tokens += cost.output_tokens;
        self.cost_usd += cost.cost_usd;
        if !cost.priced {
            self.unpriced_invocations += 1;
        }
        self.entries.push(cost.clone());
    }

    pub fn entries(&self) -> &[InvocationCost] {
        &self.entries
    }
}

/// Thread-safe handle shared with the run plane.
#[derive(Default)]
pub struct CostTracker(pub Mutex<RunCostLedger>);

impl CostTracker {
    pub fn new(model: &str) -> Self {
        CostTracker(Mutex::new(RunCostLedger::new(model)))
    }

    pub fn record(&self, cost: &InvocationCost) {
        self.0.lock().expect("cost tracker").record(cost);
    }

    pub fn snapshot(&self) -> RunCostLedger {
        self.0.lock().expect("cost tracker").clone()
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Real pricing from the table: gpt-4o-mini at $0.00015/$0.0006 per 1k.
    #[test]
    fn invocation_cost_uses_the_price_table() {
        let cost = invocation_cost("gpt-4o-mini", 1_000, 500);
        assert!(cost.priced);
        assert!((cost.cost_usd - (0.00015 + 0.0003)).abs() < 1e-9, "{cost:?}");
    }

    /// Unknown models report unpriced zeros — never a fabricated number.
    #[test]
    fn unknown_models_report_unpriced() {
        let cost = invocation_cost("totally-unknown-model", 1_000, 1_000);
        assert!(!cost.priced);
        assert_eq!(cost.cost_usd, 0.0);
    }

    /// Env override: MODBIT_PRICE_<MODEL> = "in,out" per 1k USD.
    #[test]
    fn env_override_extends_the_price_table() {
        let key = "MODBIT_PRICE_TEST-MODEL-X";
        std::env::set_var(key, "0.01,0.02");
        let cost = invocation_cost("test-model-x", 2_000, 1_000);
        std::env::remove_var(key);
        assert!(cost.priced);
        assert!((cost.cost_usd - (0.02 + 0.02)).abs() < 1e-9, "{cost:?}");
    }

    /// The ledger accumulates a run's invocations.
    #[test]
    fn ledger_accumulates() {
        let mut ledger = RunCostLedger::new("gpt-4o-mini");
        ledger.record(&invocation_cost("gpt-4o-mini", 1_000, 0));
        ledger.record(&invocation_cost("gpt-4o-mini", 0, 1_000));
        assert_eq!(ledger.invocations, 2);
        assert_eq!(ledger.input_tokens, 1_000);
        assert_eq!(ledger.output_tokens, 1_000);
        assert!((ledger.cost_usd - 0.00075).abs() < 1e-9);
    }
}
