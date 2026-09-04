//! Deterministic verification gates (M2.8, docs/50 § Test Strategy —
//! Real-System Completion Gates). A gate is a real process run (build,
//! test, lint) with a timeout, bounded evidence and a typed pass/fail
//! result. A verification plan passes only when EVERY gate passes —
//! completion requires real-system evidence, never a model claim.

pub mod adaptive;
pub mod diagnostics;
pub mod evidence_index;

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Output tail kept as evidence per gate (bounded, docs/33).
pub const OUTPUT_TAIL_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug)]
pub struct Gate {
    pub name: String,
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub timeout: Duration,
}

impl Gate {
    pub fn new(name: &str, argv: &[&str], timeout_secs: u64) -> Self {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        Self {
            name: name.to_string(),
            argv: argv.to_vec(),
            cwd: None,
            timeout: Duration::from_secs(timeout_secs),
        }
    }

    pub fn with_cwd(mut self, cwd: PathBuf) -> Self {
        self.cwd = Some(cwd);
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct GateResult {
    pub name: String,
    pub passed: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u128,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]

pub struct VerificationReport {
    pub passed: bool,
    pub gates: Vec<GateResult>,
}

#[derive(Debug)]
pub enum VerificationError {
    Io(std::io::Error),
    EmptyArgv,
}

impl fmt::Display for VerificationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VerificationError::Io(e) => write!(f, "io: {e}"),
            VerificationError::EmptyArgv => write!(f, "gate with empty argv"),
        }
    }
}

impl std::error::Error for VerificationError {}

/// Runs gates sequentially, stopping at the first failure (deterministic
/// order, evidence retained). A hung gate is killed at its timeout and
/// reported as failed-with-timeout.
pub fn run_plan(gates: &[Gate]) -> Result<VerificationReport, VerificationError> {
    let mut results = Vec::new();
    for gate in gates {
        if gate.argv.is_empty() {
            return Err(VerificationError::EmptyArgv);
        }
        let started = Instant::now();
        let mut child = Command::new(&gate.argv[0])
            .args(&gate.argv[1..])
            .current_dir(gate.cwd.as_deref().unwrap_or(Path::new(".")))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(VerificationError::Io)?;

        let timed_out;
        let exit_code;
        loop {
            if Instant::now() >= started + gate.timeout {
                let _ = child.kill();
                let _ = child.wait();
                timed_out = true;
                exit_code = None;
                break;
            }
            match child.try_wait().map_err(VerificationError::Io)? {
                Some(status) => {
                    timed_out = false;
                    exit_code = status.code();
                    break;
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        let duration_ms = started.elapsed().as_millis();
        let passed = !timed_out && exit_code == Some(0);
        results.push(GateResult {
            name: gate.name.clone(),
            passed,
            exit_code,
            timed_out,
            duration_ms,
        });
        if !passed {
            break;
        }
    }
    let passed = !results.is_empty() && results.iter().all(|r| r.passed);
    Ok(VerificationReport {
        passed,
        gates: results,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passing_gates_report_success() {
        let report = run_plan(&[Gate::new("g1", &["git", "--version"], 30)]).unwrap();
        assert!(report.passed, "{report:?}");
    }

    #[test]
    fn failing_gate_stops_the_plan_with_evidence() {
        let report = run_plan(&[
            Gate::new("failing", &["git", "definitely-not-a-command"], 30),
            Gate::new("never-reached", &["git", "--version"], 30),
        ])
        .unwrap();
        assert!(!report.passed);
        assert_eq!(report.gates.len(), 1, "plan stops at first failure");
        assert!(!report.gates[0].passed);
    }

    #[test]
    fn hung_gate_times_out_and_is_killed() {
        let argv: Vec<&str> = if cfg!(windows) {
            vec!["ping", "-n", "30", "127.0.0.1"]
        } else {
            vec!["sleep", "30"]
        };
        let gate = Gate::new("timeout-gate", &argv, 3);
        let report = run_plan(&[gate]).unwrap();
        assert!(!report.passed);
        assert!(report.gates.iter().any(|g| g.timed_out));
    }
}
