//! Context provenance (M3, REQ-EV-0169) and temporal validity
//! (REQ-EV-0170). Every non-ephemeral fragment in a prompt envelope
//! carries source/repo/revision/hash/retrieval-reason provenance and the
//! envelope VALIDATES it. Temporal validity evaluates staleness: a stale
//! cache entry is never labeled current.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Full provenance attached to a context fragment.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    /// Where the fragment came from (file, tool output, connector...).
    pub source: String,
    pub repo: String,
    pub revision: u64,
    pub sha256: String,
    /// Why the fragment was retrieved into context.
    pub retrieval_reason: String,
}

/// A fragment in the prompt envelope.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EnvelopeFragment {
    pub text: String,
    /// Ephemeral fragments (scratchpad echoes) are exempt from
    /// provenance requirements.
    pub ephemeral: bool,
    pub provenance: Option<Provenance>,
}

#[derive(Debug)]
pub enum ProvenanceError {
    MissingProvenance { index: usize },
    Incomplete { index: usize, field: &'static str },
}

impl fmt::Display for ProvenanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProvenanceError::MissingProvenance { index } => {
                write!(f, "fragment {index} is non-ephemeral but has no provenance")
            }
            ProvenanceError::Incomplete { index, field } => {
                write!(f, "fragment {index} provenance incomplete: missing {field}")
            }
        }
    }
}

impl std::error::Error for ProvenanceError {}

impl Provenance {
    fn is_complete(&self) -> Option<&'static str> {
        if self.source.trim().is_empty() {
            Some("source")
        } else if self.repo.trim().is_empty() {
            Some("repo")
        } else if self.sha256.is_empty() {
            Some("sha256")
        } else if self.retrieval_reason.trim().is_empty() {
            Some("retrieval_reason")
        } else {
            None
        }
    }
}

/// Validates the prompt envelope: EVERY non-ephemeral fragment must carry
/// complete provenance (QUAL-EV-0169).
pub fn validate_envelope(fragments: &[EnvelopeFragment]) -> Result<(), ProvenanceError> {
    for (index, fragment) in fragments.iter().enumerate() {
        if fragment.ephemeral {
            continue;
        }
        let provenance = fragment
            .provenance
            .as_ref()
            .ok_or(ProvenanceError::MissingProvenance { index })?;
        if let Some(field) = provenance.is_complete() {
            return Err(ProvenanceError::Incomplete { index, field });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Temporal validity (REQ-EV-0170)
// ---------------------------------------------------------------------------

/// Validity of a cached/indexed item against the current revision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Validity {
    /// Matches the current worktree revision.
    Current,
    /// Cached at an older revision — NEVER labeled current.
    Stale { cached_at: u64, current: u64 },
}

/// Evaluates temporal validity of an item cached at `cached_revision`
/// against the current revision.
pub fn evaluate_validity(cached_revision: u64, current_revision: u64) -> Validity {
    if cached_revision == current_revision {
        Validity::Current
    } else {
        Validity::Stale {
            cached_at: cached_revision,
            current: current_revision,
        }
    }
}

impl Validity {
    /// The label shown to consumers — a stale item can NEVER say current.
    pub fn label(&self) -> &'static str {
        match self {
            Validity::Current => "current",
            Validity::Stale { .. } => "stale",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provenance() -> Provenance {
        Provenance {
            source: "file:src/lib.rs".into(),
            repo: "zmodbit".into(),
            revision: 41,
            sha256: "abc".into(),
            retrieval_reason: "query: parse_config".into(),
        }
    }

    /// QUAL-EV-0169: the envelope validates provenance on every
    /// non-ephemeral fragment.
    #[test]
    fn envelope_validates_provenance_on_non_ephemeral_fragments() {
        let fragments = vec![
            EnvelopeFragment {
                text: "policy".into(),
                ephemeral: false,
                provenance: Some(provenance()),
            },
            EnvelopeFragment {
                text: "scratch echo".into(),
                ephemeral: true, // exempt
                provenance: None,
            },
        ];
        assert!(validate_envelope(&fragments).is_ok());

        // A non-ephemeral fragment WITHOUT provenance is rejected.
        let bad = vec![EnvelopeFragment {
            text: "mystery bytes".into(),
            ephemeral: false,
            provenance: None,
        }];
        assert!(matches!(
            validate_envelope(&bad),
            Err(ProvenanceError::MissingProvenance { index: 0 })
        ));

        // Incomplete provenance names the missing field.
        let mut incomplete = provenance();
        incomplete.sha256 = String::new();
        let bad2 = vec![EnvelopeFragment {
            text: "x".into(),
            ephemeral: false,
            provenance: Some(incomplete),
        }];
        assert!(matches!(
            validate_envelope(&bad2),
            Err(ProvenanceError::Incomplete {
                index: 0,
                field: "sha256"
            })
        ));
    }

    /// QUAL-EV-0170: a stale cache entry is NEVER labeled current.
    #[test]
    fn stale_cache_never_labeled_current() {
        assert_eq!(evaluate_validity(41, 41), Validity::Current);
        assert_eq!(evaluate_validity(41, 41).label(), "current");

        let stale = evaluate_validity(39, 41);
        assert_eq!(
            stale,
            Validity::Stale {
                cached_at: 39,
                current: 41
            }
        );
        assert_eq!(stale.label(), "stale", "stale must never read current");
    }
}
