//! Verification plane (M2, IMP-EV-0068): coordinates gate execution across
//! a workspace — pre-edit baseline, post-edit verification, and
//! regression-only attribution in a single call.
use crate::diagnostics::{compare, Diagnostic};
use crate::{Gate, GateResult, VerificationReport};
use std::path::Path;

/// Runs a set of gates against a directory and collects the report.
pub fn run_gates_in_dir(
    gates: &[Gate],
    dir: &Path,
) -> Result<VerificationReport, std::io::Error> {
    let mut results = Vec::new();
    for gate in gates {
        let started = std::time::Instant::now();
        let mut command = std::process::Command::new(&gate.argv[0]);
        command.args(&gate.argv[1..]);
        command.current_dir(dir);
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        let output = command.output()?;
        let passed = output.status.success();
        let duration_ms = started.elapsed().as_millis();
        results.push(GateResult {
            name: gate.name.clone(),
            passed,
            exit_code: output.status.code(),
            timed_out: false,
            duration_ms,
            output_tail: String::from_utf8_lossy(&output.stdout).to_string(),
        });
    }
    let passed = results.iter().all(|r| r.passed);
    Ok(VerificationReport { passed, gates: results })
}

/// Compares pre/post diagnostics using the shared `compare` function.
pub fn compare_diagnostics(
    pre: &[Diagnostic],
    post: &[Diagnostic],
) -> crate::diagnostics::DiagnosticDiff {
    crate::diagnostics::compare(pre, post)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Gate;

    #[test]
    fn gate_runs_in_dir_and_reports() {
        let dir = std::env::temp_dir().join(format!("modbit-vp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let gates = vec![Gate::new("check", &["true"], 10)];
        let report = run_gates_in_dir(&gates, &dir).unwrap();
        assert!(report.passed);
    }
}
