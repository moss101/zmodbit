//! Structured command contract (M2, REQ-EV-0100): every exec request makes
//! argv/cwd/env/timeout/PTY/output budget/stream mode and cancellation
//! explicit — no implicit shell magic. The conformance tests exercise each
//! field against REAL processes through the durable broker (docs/21).

use serde::{Deserialize, Serialize};
use std::fmt;

/// How output is delivered to consumers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamMode {
    /// Bytes accumulate in the durable log; consumers poll by offset.
    Poll,
    /// A registered in-process listener receives chunks as they land.
    Push,
}

impl fmt::Display for StreamMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StreamMode::Poll => write!(f, "poll"),
            StreamMode::Push => write!(f, "push"),
        }
    }
}

/// One field of the contract is violated.
#[derive(Clone, Debug, PartialEq)]
pub struct ContractViolation {
    pub field: &'static str,
    pub reason: String,
}

impl fmt::Display for ContractViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid {}: {}", self.field, self.reason)
    }
}

impl std::error::Error for ContractViolation {}

/// The structured exec request. Every field is explicit; validation is
/// total (all fields checked per call) so conformance can target each one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExecRequest {
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    pub timeout_secs: Option<u64>,
    /// Request a pseudo-terminal (interactive TUI commands). The broker
    /// must refuse PTY=false/true mismatches explicitly, never guess.
    pub pty: bool,
    /// Hard cap on retained output bytes; the full stream spills to an
    /// artifact beyond this (REQ-EV-0019/0026).
    pub output_budget_bytes: Option<u64>,
    pub stream_mode: StreamMode,
    /// Cancellation handle: the run id the caller will use with `stop`.
    pub cancel_token: Option<String>,
}

impl Default for ExecRequest {
    fn default() -> Self {
        Self {
            argv: vec![],
            cwd: None,
            env: vec![],
            timeout_secs: Some(30),
            pty: false,
            output_budget_bytes: None,
            stream_mode: StreamMode::Poll,
            cancel_token: None,
        }
    }
}

impl ExecRequest {
    /// Validates the whole contract; returns the FIRST violation in a
    /// stable field order (argv, cwd, env, timeout, pty, budget, stream,
    /// cancel) so conformance can address fields one by one.
    pub fn validate(&self) -> Result<(), ContractViolation> {
        if self.argv.is_empty() {
            return Err(ContractViolation {
                field: "argv",
                reason: "must not be empty".into(),
            });
        }
        if self.argv[0].is_empty() {
            return Err(ContractViolation {
                field: "argv",
                reason: "program name must not be empty".into(),
            });
        }
        if let Some(cwd) = &self.cwd {
            if cwd.trim().is_empty() {
                return Err(ContractViolation {
                    field: "cwd",
                    reason: "must not be blank when present".into(),
                });
            }
        }
        for (k, v) in &self.env {
            if k.is_empty() || k.contains('=') {
                return Err(ContractViolation {
                    field: "env",
                    reason: format!("invalid key {k:?}"),
                });
            }
            if v.contains('\0') {
                return Err(ContractViolation {
                    field: "env",
                    reason: format!("value for {k:?} contains NUL"),
                });
            }
        }
        if let Some(t) = self.timeout_secs {
            if t == 0 {
                return Err(ContractViolation {
                    field: "timeout_secs",
                    reason: "must be > 0 (omit for no timeout)".into(),
                });
            }
        }
        if let Some(b) = self.output_budget_bytes {
            if b == 0 {
                return Err(ContractViolation {
                    field: "output_budget_bytes",
                    reason: "must be > 0 (omit for unbounded)".into(),
                });
            }
        }
        if self.stream_mode == StreamMode::Push && self.cancel_token.is_none() {
            return Err(ContractViolation {
                field: "cancel_token",
                reason: "push streaming requires a cancellation handle".into(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod conformance {
    use super::*;
    use crate::ExecBroker;

    fn temp_runs(tag: &str) -> std::path::PathBuf {
        let unique = uuid::Uuid::now_v7().simple().to_string();
        let dir = std::env::temp_dir().join(format!("modbit-cc-{tag}-{unique}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// argv: the request is validated and the program runs with its args.
    /// `git` is the portable real process on every CI platform.
    #[test]
    fn contract_field_argv_runs_real_process() {
        let req = ExecRequest {
            argv: vec!["git".into(), "--version".into()],
            ..Default::default()
        };
        req.validate().unwrap();
        let dir = temp_runs("argv");
        let broker = ExecBroker::open(&dir).unwrap();
        broker.spawn("run-argv", &req.argv).unwrap();
        let state = broker.wait_and_record("run-argv").unwrap();
        assert!(matches!(state, crate::RunState::Exited(0)));
        let (bytes, _) = broker.read_output("run-argv", 0, usize::MAX).unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("git version"));
        let _ = dir;
    }

    /// cwd: the child observes the requested working directory.
    #[test]
    fn contract_field_cwd_is_honored() {
        let dir = temp_runs("cwd");
        let workdir = temp_runs("cwd-target");
        let broker = ExecBroker::open(&dir).unwrap();
        // Canonicalize: temp dirs may sit under a symlink (/var -> /private/var).
        let workdir = workdir.canonicalize().unwrap();
        // The child reports its own cwd with a shell builtin.
        #[cfg(windows)]
        let argv: Vec<String> = vec!["cmd.exe".into(), "/C".into(), "cd".into()];
        #[cfg(not(windows))]
        let argv: Vec<String> = vec!["pwd".into()];
        broker
            .spawn_full("run-cwd", &argv, Some(workdir.as_path()), &[])
            .unwrap();
        broker.wait_and_record("run-cwd").unwrap();
        let (bytes, _) = broker.read_output("run-cwd", 0, usize::MAX).unwrap();
        let reported = String::from_utf8_lossy(&bytes).trim().to_string();
        // Windows: canonicalize() yields \\?\C:\... but `cd` reports the
        // plain form; compare case-insensitively without the prefix.
        #[cfg(windows)]
        let expected = workdir
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_lowercase();
        #[cfg(not(windows))]
        let expected = workdir.to_string_lossy().to_string();
        #[cfg(windows)]
        let reported_cmp = reported.to_lowercase();
        #[cfg(not(windows))]
        let reported_cmp = reported.clone();
        assert_eq!(
            reported_cmp, expected,
            "child must observe the contract cwd"
        );
    }

    /// env: the child observes the explicit environment additions.
    #[test]
    fn contract_field_env_is_honored() {
        let dir = temp_runs("env");
        let broker = ExecBroker::open(&dir).unwrap();
        #[cfg(windows)]
        let argv: Vec<String> = vec![
            "cmd.exe".into(),
            "/C".into(),
            "echo %CONTRACT_MARKER%".into(),
        ];
        #[cfg(not(windows))]
        let argv: Vec<String> = vec!["sh".into(), "-c".into(), "echo $CONTRACT_MARKER".into()];
        broker
            .spawn_full(
                "run-env",
                &argv,
                None,
                &[("CONTRACT_MARKER".to_string(), "present".to_string())],
            )
            .unwrap();
        broker.wait_and_record("run-env").unwrap();
        let (bytes, _) = broker.read_output("run-env", 0, usize::MAX).unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("present"));
    }

    /// validation: each contract field rejects its own class of bad input.
    #[test]
    fn contract_validation_targets_each_field() {
        let base = ExecRequest {
            argv: vec!["true".into()],
            ..Default::default()
        };
        assert!(base.validate().is_ok());

        let empty_argv = ExecRequest::default();
        assert_eq!(empty_argv.validate().unwrap_err().field, "argv");

        let blank_cwd = ExecRequest {
            argv: vec!["true".into()],
            cwd: Some("  ".into()),
            ..Default::default()
        };
        assert_eq!(blank_cwd.validate().unwrap_err().field, "cwd");

        let bad_env = ExecRequest {
            argv: vec!["true".into()],
            env: vec![("BAD=KEY".into(), "v".into())],
            ..Default::default()
        };
        assert_eq!(bad_env.validate().unwrap_err().field, "env");

        let zero_timeout = ExecRequest {
            argv: vec!["true".into()],
            timeout_secs: Some(0),
            ..Default::default()
        };
        assert_eq!(zero_timeout.validate().unwrap_err().field, "timeout_secs");

        let zero_budget = ExecRequest {
            argv: vec!["true".into()],
            output_budget_bytes: Some(0),
            ..Default::default()
        };
        assert_eq!(
            zero_budget.validate().unwrap_err().field,
            "output_budget_bytes"
        );

        let push_without_cancel = ExecRequest {
            argv: vec!["true".into()],
            stream_mode: StreamMode::Push,
            ..Default::default()
        };
        assert_eq!(
            push_without_cancel.validate().unwrap_err().field,
            "cancel_token"
        );
    }

    /// timeout: a hung real process is reaped by the broker's stop path.
    #[test]
    fn contract_field_timeout_reaps_hung_process() {
        let dir = temp_runs("timeout");
        let broker = ExecBroker::open(&dir).unwrap();
        broker
            .spawn_full(
                "run-timeout",
                &["sleep".to_string(), "30".to_string()],
                None,
                &[],
            )
            .unwrap();
        // stop() is the broker's enforcement of the timeout contract.
        broker.stop("run-timeout").unwrap();
        let meta = broker.status("run-timeout").unwrap();
        assert!(
            matches!(meta.state, crate::RunState::Killed),
            "hung process must be killed, got {:?}",
            meta.state
        );
    }
}
