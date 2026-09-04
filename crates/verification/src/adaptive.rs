//! Optional adaptive LLM verification (M2, REQ-EV-0069, EXPERIMENT): a
//! semantic verifier is strictly OPTIONAL — off by default — and always
//! subordinate to the deterministic gates. A disabled configuration emits
//! ZERO verifier model calls (QUAL-EV-0069): the verifier closure is never
//! invoked, and the run verdict is the deterministic one.

use crate::VerificationReport;
use serde::{Deserialize, Serialize};

/// Verifier configuration. `enabled` defaults to FALSE everywhere it can
/// be constructed implicitly; there is no path that flips it on by
/// accident (it must be explicitly opted into).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AdaptiveVerifierConfig {
    /// Opt-in only. Default construction is always disabled.
    pub enabled: bool,
    /// Deterministic gates that MUST pass before the verifier is even
    /// consulted (subordination to gates).
    pub required_gate: String,
    /// The model the verifier would call when enabled.
    pub verifier_model: String,
}

impl Default for AdaptiveVerifierConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            required_gate: "build".to_string(),
            verifier_model: String::new(),
        }
    }
}

/// The verifier's semantic opinion.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SemanticVerdict {
    pub model: String,
    pub agrees: bool,
    pub note: String,
}

#[derive(Debug)]
pub enum AdaptiveVerifierError {
    /// The deterministic gate failed: the verifier never runs, no matter
    /// what — gates are authoritative.
    GateFailed { gate: String },
    /// Verifier invoked while the configuration is disabled (bug guard).
    Disabled,
}

impl std::fmt::Display for AdaptiveVerifierError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdaptiveVerifierError::GateFailed { gate } => {
                write!(
                    f,
                    "deterministic gate {gate:?} failed; verifier not consulted"
                )
            }
            AdaptiveVerifierError::Disabled => {
                write!(f, "adaptive verifier is disabled")
            }
        }
    }
}

impl std::error::Error for AdaptiveVerifierError {}

/// Outcome of an adaptive verification pass.
#[derive(Clone, Debug, PartialEq)]
pub enum AdaptiveOutcome {
    /// Disabled configuration: zero verifier calls were made.
    SkippedZeroCalls,
    /// Verifier ran (gates passed first) and returned a semantic verdict.
    Consulted(SemanticVerdict),
}

/// Runs the optional adaptive verification. `verifier_call` is the closure
/// that would invoke a model; it is passed in so the zero-call property is
/// structural — when disabled (or the gate failed) it is NEVER called.
pub fn run_adaptive_verification<V>(
    config: &AdaptiveVerifierConfig,
    report: &VerificationReport,
    verifier_call: V,
) -> Result<AdaptiveOutcome, AdaptiveVerifierError>
where
    V: FnOnce(&VerificationReport) -> Result<SemanticVerdict, String>,
{
    // Subordination: the named deterministic gate must have passed first.
    let gate_ok = report
        .gates
        .iter()
        .find(|g| g.name == config.required_gate)
        .map(|g| g.passed)
        .unwrap_or(false);
    if !gate_ok {
        return Err(AdaptiveVerifierError::GateFailed {
            gate: config.required_gate.clone(),
        });
    }

    // Opt-in gate: disabled means ZERO model calls — the closure is never
    // invoked, no token is spent, nothing is emitted.
    if !config.enabled {
        return Ok(AdaptiveOutcome::SkippedZeroCalls);
    }

    let verdict = verifier_call(report).map_err(|_| AdaptiveVerifierError::Disabled)?;
    Ok(AdaptiveOutcome::Consulted(verdict))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GateResult;

    fn report_with(gate: &str, passed: bool) -> VerificationReport {
        VerificationReport {
            gates: vec![GateResult {
                name: gate.to_string(),
                passed,
                exit_code: Some(0),
                timed_out: false,
                duration_ms: 10,
            }],
            passed,
        }
    }

    /// QUAL-EV-0069: a disabled configuration emits ZERO verifier model
    /// calls — the call closure is structurally never invoked.
    #[test]
    fn disabled_config_emits_zero_verifier_model_calls() {
        let config = AdaptiveVerifierConfig {
            verifier_model: "gpt-5-mini".into(),
            ..Default::default()
        };
        assert!(!config.enabled, "off by default");

        let mut calls = 0;
        let report = report_with("build", true);
        let outcome = run_adaptive_verification(&config, &report, |_r| {
            calls += 1;
            Ok(SemanticVerdict {
                model: "gpt-5-mini".into(),
                agrees: true,
                note: "would have called the model".into(),
            })
        })
        .unwrap();
        assert_eq!(calls, 0, "disabled config must emit zero model calls");
        assert_eq!(outcome, AdaptiveOutcome::SkippedZeroCalls);
    }

    /// Even when enabled, the verifier is subordinate: a failed
    /// deterministic gate means it is never consulted.
    #[test]
    fn verifier_is_subordinate_to_deterministic_gates() {
        let config = AdaptiveVerifierConfig {
            enabled: true,
            required_gate: "build".into(),
            verifier_model: "gpt-5-mini".into(),
        };
        let mut calls = 0;
        let failing = report_with("build", false);
        let err = run_adaptive_verification(&config, &failing, |_r| {
            calls += 1;
            Ok(SemanticVerdict {
                model: String::new(),
                agrees: true,
                note: String::new(),
            })
        })
        .unwrap_err();
        assert!(matches!(err, AdaptiveVerifierError::GateFailed { .. }));
        assert_eq!(calls, 0, "failed gates mean zero verifier calls");

        // With the gate green, the enabled verifier IS consulted.
        let passing = report_with("build", true);
        let outcome = run_adaptive_verification(&config, &passing, |_r| {
            calls += 1;
            Ok(SemanticVerdict {
                model: "gpt-5-mini".into(),
                agrees: false,
                note: "looks unfinished".into(),
            })
        })
        .unwrap();
        assert_eq!(calls, 1);
        assert!(matches!(outcome, AdaptiveOutcome::Consulted(_)));
    }
}
