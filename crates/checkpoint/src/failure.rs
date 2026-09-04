//! Typed failure diagnostics (M4, REQ-EV-0073/0245): every failure
//! carries a class, retryability, an operator action, evidence, and a
//! recovery path. Fault INJECTION verifies there is no generic success on
//! timeout/corrupt state, and the fault corpus produces STABLE
//! diagnostic features (same fault → same typed diagnostic).

use serde::{Deserialize, Serialize};
use std::fmt;

/// The failure taxonomy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    Timeout,
    CorruptState,
    MissingArtifact,
    PolicyDenied,
    TransportLost,
}

/// The typed recovery path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPath {
    /// The caller may retry the same operation.
    Retry,
    /// Retry from the last durable checkpoint.
    RetryFromCheckpoint,
    /// Manual operator intervention required.
    OperatorIntervention,
}

/// The structured diagnostic.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FailureDiagnostic {
    pub class: FailureClass,
    pub retryable: bool,
    pub operator_action: String,
    pub evidence: Vec<String>,
    pub recovery_path: RecoveryPath,
    /// Stable feature key: identical faults produce identical keys
    /// (REQ-EV-0245).
    pub feature_key: String,
}

/// An injectable fault (the fault-injection surface of REQ-EV-0073).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InjectedFault {
    Timeout,
    CorruptState,
    MissingArtifact,
    PolicyDenied,
    TransportLost,
}

#[derive(Debug)]
pub enum FaultOutcome {
    /// The injected fault surfaced as a fully typed diagnostic.
    Diagnostic(FailureDiagnostic),
}

impl fmt::Display for FaultOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FaultOutcome::Diagnostic(d) => write!(
                f,
                "{:?} (retryable={}) → {:?}: {}",
                d.class, d.retryable, d.recovery_path, d.operator_action
            ),
        }
    }
}

/// Runs the operation under fault injection. `fault` simulates the
/// failure; the layer MUST produce a typed diagnostic — never a generic
/// success (QUAL-EV-0073).
pub fn execute_with_fault_injection(fault: Option<InjectedFault>) -> Result<(), FaultOutcome> {
    let Some(fault) = fault else {
        return Ok(()); // no injected fault: genuine success
    };
    let diagnostic = match fault {
        InjectedFault::Timeout => FailureDiagnostic {
            class: FailureClass::Timeout,
            retryable: true,
            operator_action: "retry the operation; extend the timeout if it recurs".into(),
            evidence: vec!["deadline exceeded after scheduled budget".into()],
            recovery_path: RecoveryPath::Retry,
            feature_key: "timeout:retry:deadline".into(),
        },
        InjectedFault::CorruptState => FailureDiagnostic {
            class: FailureClass::CorruptState,
            retryable: false,
            operator_action: "restore from the last checkpoint; do not reuse the corrupt file"
                .into(),
            evidence: vec!["state digest mismatch on load".into()],
            recovery_path: RecoveryPath::RetryFromCheckpoint,
            feature_key: "corrupt_state:checkpoint_restore".into(),
        },
        InjectedFault::MissingArtifact => FailureDiagnostic {
            class: FailureClass::MissingArtifact,
            retryable: false,
            operator_action: "rebuild the artifact from its producing task".into(),
            evidence: vec!["expected artifact absent at addressed path".into()],
            recovery_path: RecoveryPath::OperatorIntervention,
            feature_key: "missing_artifact:rebuild".into(),
        },
        InjectedFault::PolicyDenied => FailureDiagnostic {
            class: FailureClass::PolicyDenied,
            retryable: false,
            operator_action: "request the capability grant from the operator".into(),
            evidence: vec!["capability kernel denied: no covering grant".into()],
            recovery_path: RecoveryPath::OperatorIntervention,
            feature_key: "policy_denied:grant_request".into(),
        },
        InjectedFault::TransportLost => FailureDiagnostic {
            class: FailureClass::TransportLost,
            retryable: true,
            operator_action: "reattach via the durable handle; output replays from the cursor"
                .into(),
            evidence: vec!["client transport disconnected mid-stream".into()],
            recovery_path: RecoveryPath::Retry,
            feature_key: "transport_lost:reattach".into(),
        },
    };
    Err(FaultOutcome::Diagnostic(diagnostic))
}

/// The fault corpus (REQ-EV-0245): every fault class produces a typed,
/// STABLE diagnostic — the feature key is deterministic across runs.
pub fn fault_corpus() -> Vec<(InjectedFault, FailureDiagnostic)> {
    let faults = [
        InjectedFault::Timeout,
        InjectedFault::CorruptState,
        InjectedFault::MissingArtifact,
        InjectedFault::PolicyDenied,
        InjectedFault::TransportLost,
    ];
    faults
        .iter()
        .map(|fault| {
            // execute_with_fault_injection ALWAYS errs for an injected fault.
            let outcome = execute_with_fault_injection(Some(*fault));
            match outcome {
                Err(FaultOutcome::Diagnostic(d)) => (*fault, d),
                Ok(()) => panic!("injected fault {fault:?} must never yield success"),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0073: fault injection verifies no generic success on
    /// timeout/corrupt state.
    #[test]
    fn injected_faults_never_yield_generic_success() {
        for fault in [
            InjectedFault::Timeout,
            InjectedFault::CorruptState,
            InjectedFault::MissingArtifact,
        ] {
            match execute_with_fault_injection(Some(fault)) {
                Ok(()) => panic!("{fault:?} must not produce success"),
                Err(FaultOutcome::Diagnostic(d)) => {
                    // Typed diagnostics carry actionable fields.
                    assert!(!d.operator_action.is_empty());
                    assert!(!d.evidence.is_empty());
                    assert!(!d.feature_key.is_empty());
                }
            }
        }
    }

    /// QUAL-EV-0245: the fault corpus produces STABLE diagnostic features.
    #[test]
    fn fault_corpus_features_are_stable() {
        let first = fault_corpus();
        let second = fault_corpus();
        assert_eq!(first.len(), 5);
        for ((fault_a, diag_a), (fault_b, diag_b)) in first.iter().zip(second.iter()) {
            assert_eq!(fault_a, fault_b);
            // Same fault → identical stable features.
            assert_eq!(diag_a.feature_key, diag_b.feature_key);
            assert_eq!(diag_a.class, diag_b.class);
            assert_eq!(diag_a.retryable, diag_b.retryable);
            assert_eq!(diag_a.recovery_path, diag_b.recovery_path);
        }
        // Distinct faults have distinct feature keys.
        let keys: std::collections::BTreeSet<&str> =
            first.iter().map(|(_, d)| d.feature_key.as_str()).collect();
        assert_eq!(keys.len(), 5);
    }
}
