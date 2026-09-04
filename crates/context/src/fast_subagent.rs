//! Fast Context subagent (M3, REQ-EV-0174): a bounded, READ-ONLY
//! retrieval specialist. It is structurally incapable of mutation — its
//! tool surface has no write tools — and every pack it produces is
//! provenance-complete (validated by the envelope rules of REQ-EV-0169).

use crate::provenance::{validate_envelope, EnvelopeFragment, Provenance};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The read-only fetcher the subagent is allowed to call.
pub trait ReadOnlyFetch {
    /// Returns (bytes, revision) for a path — reads only.
    fn fetch(&self, path: &str) -> Option<(Vec<u8>, u64)>;
}

#[derive(Debug)]
pub enum SubagentError {
    MutationRefused { tool: String },
    UnknownPath(String),
    BudgetExceeded { requested: usize, budget: usize },
}

impl std::fmt::Display for SubagentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubagentError::MutationRefused { tool } => {
                write!(
                    f,
                    "fast-context subagent has no {tool:?} tool: read-only specialist"
                )
            }
            SubagentError::UnknownPath(p) => write!(f, "unknown path {p:?}"),
            SubagentError::BudgetExceeded { requested, budget } => {
                write!(f, "fetch budget exceeded: {requested} > {budget}")
            }
        }
    }
}

impl std::error::Error for SubagentError {}

/// A provenance-complete pack produced by the specialist.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProvenancePack {
    pub fragments: Vec<EnvelopeFragment>,
    pub total_bytes: usize,
}

/// The specialist. There is NO mutation tool on this struct — refusal is
/// structural, not a runtime convention.
pub struct FastContextSubagent<'a> {
    pub fetcher: &'a dyn ReadOnlyFetch,
    /// Hard byte budget for the total pack.
    pub budget: usize,
}

impl<'a> FastContextSubagent<'a> {
    /// Any mutation request is REFUSED (typed), never attempted.
    pub fn apply_mutation(&self, tool: &str) -> Result<(), SubagentError> {
        Err(SubagentError::MutationRefused {
            tool: tool.to_string(),
        })
    }

    /// Builds a provenance-complete pack for the requested paths, within
    /// budget.
    pub fn build_pack(&self, paths: &[&str]) -> Result<ProvenancePack, SubagentError> {
        let mut fragments = Vec::new();
        let mut total = 0usize;
        for path in paths {
            let (bytes, revision) = self
                .fetcher
                .fetch(path)
                .ok_or_else(|| SubagentError::UnknownPath(path.to_string()))?;
            total += bytes.len();
            if total > self.budget {
                return Err(SubagentError::BudgetExceeded {
                    requested: total,
                    budget: self.budget,
                });
            }
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            fragments.push(EnvelopeFragment {
                text: String::from_utf8_lossy(&bytes).to_string(),
                ephemeral: false,
                provenance: Some(Provenance {
                    source: format!("file:{path}"),
                    repo: "workspace".into(),
                    revision,
                    sha256: format!("{:x}", hasher.finalize()),
                    retrieval_reason: "fast-context subagent fetch".into(),
                }),
            });
        }
        let pack = ProvenancePack {
            fragments,
            total_bytes: total,
        };
        // The pack MUST validate: provenance-complete by construction.
        validate_envelope(&pack.fragments)
            .map_err(|e| SubagentError::UnknownPath(format!("provenance invalid: {e}")))?;
        Ok(pack)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    struct MemFetch {
        files: BTreeMap<String, (Vec<u8>, u64)>,
    }

    impl ReadOnlyFetch for MemFetch {
        fn fetch(&self, path: &str) -> Option<(Vec<u8>, u64)> {
            self.files.get(path).cloned()
        }
    }

    /// QUAL-EV-0174: the specialist has no mutation tools and produces a
    /// provenance-complete pack.
    #[test]
    fn specialist_is_read_only_and_produces_provenance_complete_pack() {
        let mut files = BTreeMap::new();
        files.insert(
            "src/lib.rs".to_string(),
            (b"pub fn entry() {}".to_vec(), 41u64),
        );
        files.insert(
            "src/retry.rs".to_string(),
            (b"pub fn retry() {}".to_vec(), 41u64),
        );
        let fetcher = MemFetch { files };

        let subagent = FastContextSubagent {
            fetcher: &fetcher,
            budget: 4096,
        };

        // Mutation attempts are refused structurally.
        assert!(matches!(
            subagent.apply_mutation("fs.write"),
            Err(SubagentError::MutationRefused { .. })
        ));
        assert!(matches!(
            subagent.apply_mutation("change_engine.apply"),
            Err(SubagentError::MutationRefused { .. })
        ));

        // The pack is built and VALIDATES provenance end-to-end.
        let pack = subagent
            .build_pack(&["src/lib.rs", "src/retry.rs"])
            .unwrap();
        assert_eq!(pack.fragments.len(), 2);
        assert!(validate_envelope(&pack.fragments).is_ok());
        assert!(pack
            .fragments
            .iter()
            .all(|f| f.provenance.as_ref().unwrap().revision == 41));

        // Budget enforcement.
        let tiny = FastContextSubagent {
            fetcher: &fetcher,
            budget: 8,
        };
        assert!(matches!(
            tiny.build_pack(&["src/lib.rs", "src/retry.rs"]),
            Err(SubagentError::BudgetExceeded { .. })
        ));
    }
}
