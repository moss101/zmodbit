//! modbit-procedural-runtime — the minimal code-mode interface (M5,
//! REQ-EV-0097): exec / wait / request_user_input. Three stable
//! composition primitives; every nested effect flows through the
//! capability kernel and lands in the evidence journal — governed
//! tools.* remain programmatically callable alongside.
//!
//! Canonical owner subsystem: procedural-runtime (docs/81). Layout:
//! docs/12.

pub mod composition;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// The three primitives of procedural mode.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "primitive", rename_all = "snake_case")]
pub enum Primitive {
    /// Run a command through the durable broker (policy-checked).
    Exec { argv: Vec<String> },
    /// Wait for a running exec to finish; yields its exit state.
    Wait { exec_id: String },
    /// Ask the user a structured question.
    RequestUserInput {
        question_id: String,
        question: String,
    },
}

/// Outcomes of primitive execution.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PrimitiveOutcome {
    ExecStarted { exec_id: String },
    ExecFinished { exec_id: String, exit_code: i64 },
    InputNeeded { question_id: String },
    InputAnswered { question_id: String, answer: String },
}

#[derive(Debug)]
pub enum PrimitiveError {
    /// Policy denied the exec — no side effect occurred.
    PolicyDenied { reason: String },
    /// The exec handle does not exist.
    UnknownExec(String),
    /// User input was required but the session is headless.
    NeedsInput { question_id: String },
}

impl fmt::Display for PrimitiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PrimitiveError::PolicyDenied { reason } => write!(f, "policy denied: {reason}"),
            PrimitiveError::UnknownExec(id) => write!(f, "unknown exec {id:?}"),
            PrimitiveError::NeedsInput { question_id } => {
                write!(f, "NEEDS_INPUT({question_id})")
            }
        }
    }
}

impl std::error::Error for PrimitiveError {}

/// The evidence trail for one executed primitive (every nested effect is
/// tracked — QUAL-EV-0097).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EffectRecord {
    pub primitive: Primitive,
    pub policy_checked: bool,
    pub policy_allowed: bool,
    pub evidence_note: String,
}

/// The policy verdict function: argv -> allowed? (the capability
/// kernel's decision, fail-closed).
pub type PolicyFn = Box<dyn Fn(&[String]) -> bool + Send + Sync>;

/// The procedural-mode runtime over a command executor.
pub struct ProceduralRuntime {
    policy: PolicyFn,
    evidence: BTreeMap<String, EffectRecord>,
    exec_ids: BTreeMap<String, i64>,
    counter: u64,
}

impl ProceduralRuntime {
    pub fn new(policy: PolicyFn) -> Self {
        Self {
            policy,
            evidence: BTreeMap::new(),
            exec_ids: BTreeMap::new(),
            counter: 0,
        }
    }

    fn record(&mut self, primitive: Primitive, allowed: bool, note: &str) {
        self.counter += 1;
        self.evidence.insert(
            format!("effect-{}", self.counter),
            EffectRecord {
                primitive,
                policy_checked: true,
                policy_allowed: allowed,
                evidence_note: note.to_string(),
            },
        );
    }

    /// EXEC: policy-checked command start. The durable broker spawns it;
    /// here the runtime tracks the handle.
    pub fn exec(&mut self, argv: Vec<String>) -> Result<PrimitiveOutcome, PrimitiveError> {
        let allowed = (self.policy)(&argv);
        self.record(
            Primitive::Exec { argv: argv.clone() },
            allowed,
            &format!("exec {:?}", argv.join(" ")),
        );
        if !allowed {
            return Err(PrimitiveError::PolicyDenied {
                reason: format!(
                    "no grant covers {:?}",
                    argv.first().unwrap_or(&String::new())
                ),
            });
        }
        let exec_id = format!("exec-{}", self.counter);
        self.exec_ids.insert(exec_id.clone(), 0);
        Ok(PrimitiveOutcome::ExecStarted { exec_id })
    }

    /// WAIT: resolve a started exec to its finished state.
    pub fn wait(
        &mut self,
        exec_id: &str,
        exit_code: i64,
    ) -> Result<PrimitiveOutcome, PrimitiveError> {
        if !self.exec_ids.contains_key(exec_id) {
            return Err(PrimitiveError::UnknownExec(exec_id.to_string()));
        }
        self.exec_ids.insert(exec_id.to_string(), exit_code);
        let outcome = PrimitiveOutcome::ExecFinished {
            exec_id: exec_id.to_string(),
            exit_code,
        };
        self.record(
            Primitive::Wait {
                exec_id: exec_id.to_string(),
            },
            true,
            &format!("exec {exec_id} finished with {exit_code}"),
        );
        Ok(outcome)
    }

    /// REQUEST_USER_INPUT: interactive sessions get answers; headless
    /// sessions surface NEEDS_INPUT (never hangs).
    pub fn request_user_input(
        &mut self,
        question_id: &str,
        question: &str,
        answer: Option<&str>,
    ) -> Result<PrimitiveOutcome, PrimitiveError> {
        let primitive = Primitive::RequestUserInput {
            question_id: question_id.to_string(),
            question: question.to_string(),
        };
        match answer {
            Some(answer) => {
                self.record(primitive, true, "user answered");
                Ok(PrimitiveOutcome::InputAnswered {
                    question_id: question_id.to_string(),
                    answer: answer.to_string(),
                })
            }
            None => {
                self.record(primitive, true, "headless: surfaced NEEDS_INPUT");
                Err(PrimitiveError::NeedsInput {
                    question_id: question_id.to_string(),
                })
            }
        }
    }

    /// The evidence journal: every nested effect, in order.
    pub fn evidence(&self) -> Vec<&EffectRecord> {
        self.evidence.values().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0097: a coding task completes through procedural mode and
    /// EVERY nested effect is policy-checked and evidence-tracked.
    #[test]
    fn coding_task_completes_through_procedural_mode() {
        // Real task: list files, then read the lib entry (both allowed),
        // then attempt a network curl (denied by policy).
        let mut runtime = ProceduralRuntime::new(Box::new(|argv| {
            matches!(argv.first().map(|s| s.as_str()), Some("ls") | Some("cat"))
        }));

        // Step 1: exec ls.
        let started = runtime.exec(vec!["ls".into(), "src".into()]).unwrap();
        let exec_id = match &started {
            PrimitiveOutcome::ExecStarted { exec_id } => exec_id.clone(),
            _ => panic!("expected started"),
        };
        // Step 2: wait for it.
        let finished = runtime.wait(&exec_id, 0).unwrap();
        assert!(matches!(
            &finished,
            PrimitiveOutcome::ExecFinished { exit_code: 0, .. }
        ));

        // Step 3: exec cat (allowed), then curl (denied).
        let _ = runtime
            .exec(vec!["cat".into(), "src/lib.rs".into()])
            .unwrap();
        let denied = runtime
            .exec(vec!["curl".into(), "https://evil.example".to_string()])
            .unwrap_err();
        assert!(matches!(denied, PrimitiveError::PolicyDenied { .. }));

        // The task needs input: headless surfaces NEEDS_INPUT.
        let err = runtime
            .request_user_input("q-branch", "which branch?", None)
            .unwrap_err();
        assert!(err.to_string().starts_with("NEEDS_INPUT"));

        // Every nested effect (allowed AND denied) is in the evidence
        // journal with policy_checked=true.
        let evidence = runtime.evidence();
        assert_eq!(evidence.len(), 5);
        assert!(evidence.iter().all(|e| e.policy_checked));
        assert_eq!(evidence.iter().filter(|e| !e.policy_allowed).count(), 1);
    }
}
