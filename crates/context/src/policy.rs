//! Context policy (M3, REQ-EV-0203): the production inference agent
//! receives approved skills — NEVER the raw evolution wiki or optimization
//! traces. A prompt audit confirms the evolution store is absent during a
//! normal run; authorized retrieval of evolution evidence requires an
//! explicit, separately-authorized task flag.

use serde::{Deserialize, Serialize};

/// Sources a context fragment may come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextSource {
    ApprovedSkill,
    SystemPolicy,
    Workspace,
    /// The optimization wiki / self-improvement traces. FORBIDDEN in
    /// normal-run prompts.
    EvolutionWiki,
    EvolutionTrace,
}

/// One fragment of the assembled prompt with its source.
#[derive(Clone, Debug, PartialEq)]
pub struct PromptFragment {
    pub source: ContextSource,
    pub text: String,
}

#[derive(Debug)]
pub enum AuditError {
    ForbiddenSource { index: usize, source: ContextSource },
}

impl std::fmt::Display for AuditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuditError::ForbiddenSource { index, source } => {
                write!(
                    f,
                    "prompt fragment {index} carries forbidden source {source:?}"
                )
            }
        }
    }
}

impl std::error::Error for AuditError {}

/// Is this source allowed in a NORMAL production run?
pub fn allowed_in_normal_run(source: ContextSource) -> bool {
    !matches!(
        source,
        ContextSource::EvolutionWiki | ContextSource::EvolutionTrace
    )
}

/// The prompt audit (QUAL-EV-0203): verifies the evolution store is
/// ABSENT from the assembled prompt during a normal run.
pub fn audit_normal_run(fragments: &[PromptFragment]) -> Result<(), AuditError> {
    for (index, fragment) in fragments.iter().enumerate() {
        if !allowed_in_normal_run(fragment.source) {
            return Err(AuditError::ForbiddenSource {
                index,
                source: fragment.source,
            });
        }
    }
    Ok(())
}

/// An evolution-evidence retrieval task: only this EXPLICIT,
/// separately-flagged request may read the evolution store.
#[derive(Clone, Debug, PartialEq)]
pub struct EvolutionRetrievalTask {
    pub authorized: bool,
}

pub fn may_retrieve_evolution(task: &EvolutionRetrievalTask) -> bool {
    task.authorized
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0203: the prompt audit confirms the evolution store is
    /// absent during a normal run — and flags it if it ever appears.
    #[test]
    fn evolution_store_absent_in_normal_run() {
        // Normal run: approved skill + policy + workspace — passes.
        let normal = vec![
            PromptFragment {
                source: ContextSource::ApprovedSkill,
                text: "skill: run verification gates".into(),
            },
            PromptFragment {
                source: ContextSource::SystemPolicy,
                text: "policy text".into(),
            },
            PromptFragment {
                source: ContextSource::Workspace,
                text: "src/lib.rs contents".into(),
            },
        ];
        assert!(audit_normal_run(&normal).is_ok());

        // A leaked evolution wiki fragment FAILS the audit.
        let leaked = vec![
            PromptFragment {
                source: ContextSource::ApprovedSkill,
                text: "skill".into(),
            },
            PromptFragment {
                source: ContextSource::EvolutionWiki,
                text: "mutation history: tried prompt X, gained 3%".into(),
            },
        ];
        assert!(matches!(
            audit_normal_run(&leaked),
            Err(AuditError::ForbiddenSource {
                index: 1,
                source: ContextSource::EvolutionWiki
            })
        ));

        // Evolution traces are equally forbidden.
        let traces = vec![PromptFragment {
            source: ContextSource::EvolutionTrace,
            text: "trace".into(),
        }];
        assert!(audit_normal_run(&traces).is_err());

        // Only an explicitly AUTHORIZED retrieval task may read it.
        assert!(!may_retrieve_evolution(&EvolutionRetrievalTask {
            authorized: false
        }));
        assert!(may_retrieve_evolution(&EvolutionRetrievalTask {
            authorized: true
        }));
    }
}
