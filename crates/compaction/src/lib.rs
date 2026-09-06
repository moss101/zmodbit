//! modbit-compaction — context history compaction epochs (M3, docs/19 §
//! compaction): ContextEpoch (REQ-EV-0056), CompactionManifest with
//! hot/warm/cold tiers (REQ-EV-0057), and the async stale-result guard
//! (REQ-EV-0058).
//!
//! Authority rule: compaction changes the MODEL-VISIBLE projection only —
//! the run's canonical event history is never touched.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;

pub mod hot_path;

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// ContextEpoch (REQ-EV-0056)
// ---------------------------------------------------------------------------

/// A versioned, model-visible epoch: the projection boundary a context
/// pack is built against. The run's canonical history is untouched.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContextEpoch {
    pub epoch_id: String,
    /// Canonical history revision the epoch was computed against.
    pub base_revision: u64,
    /// Epoch this one supersedes (lineage), if any.
    pub parent_epoch: Option<String>,
    /// Digest of the compacted projection content.
    pub projection_digest: String,
    /// Terminal for this lineage: an epoch superseded by a fork/revert
    /// with an incompatible base cannot be applied again.
    pub superseded_by: Option<String>,
}

#[derive(Debug)]
pub enum EpochError {
    /// The epoch was forked/reverted away — applying it would mix
    /// incompatible projections.
    Invalidated {
        epoch_id: String,
        reason: String,
    },
    UnknownEpoch(String),
}

impl fmt::Display for EpochError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EpochError::Invalidated { epoch_id, reason } => {
                write!(f, "epoch {epoch_id} invalidated: {reason}")
            }
            EpochError::UnknownEpoch(id) => write!(f, "unknown epoch {id:?}"),
        }
    }
}

impl std::error::Error for EpochError {}

/// Registry of epochs in creation order. Fork/revert MARKS epochs
/// invalidated (retaining canonical history) rather than deleting them.
#[derive(Default)]
pub struct EpochRegistry {
    epochs: Vec<ContextEpoch>,
}

impl EpochRegistry {
    pub fn new() -> Self {
        Default::default()
    }

    /// Creates the root epoch against a canonical revision.
    pub fn create(&mut self, base_revision: u64, projection: &[u8]) -> ContextEpoch {
        let epoch = ContextEpoch {
            epoch_id: format!("epoch-{}", &sha256_hex(projection)[..12]),
            base_revision,
            parent_epoch: None,
            projection_digest: sha256_hex(projection),
            superseded_by: None,
        };
        self.epochs.push(epoch.clone());
        epoch
    }

    /// Compacts again on top of an epoch (same lineage, newer revision).
    pub fn compact(
        &mut self,
        parent_id: &str,
        base_revision: u64,
        projection: &[u8],
    ) -> Result<ContextEpoch, EpochError> {
        self.require_live(parent_id)?;
        let epoch = ContextEpoch {
            epoch_id: format!("epoch-{}", &sha256_hex(projection)[..12]),
            base_revision,
            parent_epoch: Some(parent_id.to_string()),
            projection_digest: sha256_hex(projection),
            superseded_by: None,
        };
        self.epochs.push(epoch.clone());
        Ok(epoch)
    }

    /// FORK/REVERT: the canonical head moves to `new_revision` along a
    /// different path. Every live epoch whose base is ahead of the new
    /// head is invalidated — its compacted output must never be applied.
    /// History is RETAINED (superseded_by records the cause).
    pub fn fork_or_revert(&mut self, new_revision: u64, cause: &str) -> Vec<String> {
        let mut invalidated = Vec::new();
        for epoch in &mut self.epochs {
            if epoch.superseded_by.is_none() && epoch.base_revision > new_revision {
                epoch.superseded_by = Some(cause.to_string());
                invalidated.push(epoch.epoch_id.clone());
            }
        }
        invalidated
    }

    fn require_live(&self, epoch_id: &str) -> Result<(), EpochError> {
        let epoch = self
            .epochs
            .iter()
            .find(|e| e.epoch_id == epoch_id)
            .ok_or_else(|| EpochError::UnknownEpoch(epoch_id.to_string()))?;
        if let Some(by) = &epoch.superseded_by {
            return Err(EpochError::Invalidated {
                epoch_id: epoch_id.to_string(),
                reason: by.clone(),
            });
        }
        Ok(())
    }

    /// Applying an epoch requires it to be live.
    pub fn apply(&self, epoch_id: &str) -> Result<&ContextEpoch, EpochError> {
        self.require_live(epoch_id)?;
        Ok(self
            .epochs
            .iter()
            .find(|e| e.epoch_id == epoch_id)
            .expect("checked above"))
    }
}

// ---------------------------------------------------------------------------
// CompactionManifest + hot/warm/cold tiers (REQ-EV-0057)
// ---------------------------------------------------------------------------

/// Retention tier of a compacted item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Verbatim in the model-visible pack.
    Hot,
    /// Compressed summary line in the pack.
    Warm,
    /// Reference only (resolvable by id on demand).
    Cold,
}

/// A labeled item preserved through compaction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreservedItem {
    pub label: Label,
    pub text: String,
    pub tier: Tier,
}

/// Labels that MUST survive compaction verbatim (docs/19 fidelity rules).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Label {
    Instruction,
    Decision,
    Approval,
    Fact,
    ResourceRef,
}

/// The manifest of one compaction: what was preserved, what was
/// compressed, and where the source head was.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompactionManifest {
    /// Canonical history head the compaction covered (through this seq).
    pub source_head: u64,
    pub preserved: Vec<PreservedItem>,
    /// The compressed projection text (what the model actually sees).
    pub compressed_projection: String,
    /// cold-tier item ids resolvable on demand.
    pub cold_refs: Vec<String>,
}

impl CompactionManifest {
    /// Builds the manifest: labeled instructions/decisions/approvals are
    /// forced to Hot (verbatim survival); everything else is compressed
    /// into the projection.
    pub fn build(source_head: u64, items: &[(Option<Label>, String)]) -> Self {
        let mut preserved = Vec::new();
        let mut warm_lines = Vec::new();
        let mut cold_refs = Vec::new();
        for (index, (label, text)) in items.iter().enumerate() {
            match label {
                Some(l @ (Label::Instruction | Label::Decision | Label::Approval)) => {
                    preserved.push(PreservedItem {
                        label: *l,
                        text: text.clone(),
                        tier: Tier::Hot,
                    });
                }
                Some(Label::Fact) => preserved.push(PreservedItem {
                    label: Label::Fact,
                    text: text.clone(),
                    tier: Tier::Warm,
                }),
                Some(Label::ResourceRef) | None => {
                    let id = format!("cold-{index}");
                    cold_refs.push(id.clone());
                    if label.is_some() {
                        preserved.push(PreservedItem {
                            label: Label::ResourceRef,
                            text: id,
                            tier: Tier::Cold,
                        });
                    }
                    warm_lines.push(format!("[{}]", text.chars().take(40).collect::<String>()));
                }
            }
        }
        let compressed_projection = format!(
            "compact head={source_head} items={}\n{}",
            items.len(),
            warm_lines.join("\n")
        );
        Self {
            source_head,
            preserved,
            compressed_projection,
            cold_refs,
        }
    }
}

// ---------------------------------------------------------------------------
// Async stale guard (REQ-EV-0058)
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ApplyError {
    /// The compaction was computed against an older head; history moved
    /// while the job ran. The result is discarded, never applied.
    Stale {
        computed_at_head: u64,
        current_head: u64,
    },
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApplyError::Stale {
                computed_at_head,
                current_head,
            } => write!(
                f,
                "stale compaction: computed at head {computed_at_head}, current head {current_head}"
            ),
        }
    }
}

impl std::error::Error for ApplyError {}

/// Applies an async compaction result to the live context. The guard
/// compares the head the job computed against with the CURRENT head —
/// history moved while the job ran → typed Stale, result discarded.
pub fn apply_compaction(
    manifest: &CompactionManifest,
    computed_at_head: u64,
    current_head: u64,
) -> Result<CompactionManifest, ApplyError> {
    if computed_at_head < current_head {
        return Err(ApplyError::Stale {
            computed_at_head,
            current_head,
        });
    }
    Ok(manifest.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0056: fork/revert invalidates incompatible compaction
    /// output and retains canonical history.
    #[test]
    fn fork_revert_invalidates_epochs_and_retains_history() {
        let mut registry = EpochRegistry::new();
        let e1 = registry.create(1, b"projection v1");

        // Compact again on the same lineage.
        let e2 = registry.compact(&e1.epoch_id, 2, b"projection v2").unwrap();
        assert_eq!(e2.parent_epoch.as_deref(), Some(e1.epoch_id.as_str()));

        // Fork/revert: canonical head moves BACKWARD to revision 1 — e2
        // (base 2) is incompatible and must be invalidated.
        let invalidated = registry.fork_or_revert(1, "revert to rev 1");
        assert_eq!(invalidated, vec![e2.epoch_id.clone()]);
        assert!(matches!(
            registry.apply(&e2.epoch_id),
            Err(EpochError::Invalidated { .. })
        ));
        // Canonical history retained: e1 (base 1) still applies.
        assert!(registry.apply(&e1.epoch_id).is_ok());
    }

    /// QUAL-EV-0057: labeled instructions/decisions/approvals survive
    /// compaction verbatim; the rest compresses.
    #[test]
    fn compaction_fidelity_labeled_items_survive() {
        let manifest = CompactionManifest::build(
            41,
            &[
                (
                    Some(Label::Instruction),
                    "always run cargo clippy before commit".into(),
                ),
                (
                    Some(Label::Decision),
                    "chose WAL mode for the event store".into(),
                ),
                (None, "read 3 files about indexing".into()),
                (
                    Some(Label::Approval),
                    "user approved the migration plan".into(),
                ),
            ],
        );
        // All three mandatory labels survive verbatim at Hot tier.
        for expected in [
            "always run cargo clippy before commit",
            "chose WAL mode for the event store",
            "user approved the migration plan",
        ] {
            assert!(
                manifest
                    .preserved
                    .iter()
                    .any(|p| p.text == expected && p.tier == Tier::Hot),
                "{expected:?} must survive verbatim"
            );
        }
        // Source head tracked.
        assert_eq!(manifest.source_head, 41);
        // The projection compressed the unlabeled item.
        assert!(manifest
            .compressed_projection
            .contains("read 3 files about"));
        assert_eq!(manifest.cold_refs.len(), 1);
    }

    /// QUAL-EV-0058: a compaction computed against a stale head is
    /// rejected — never applied.
    #[test]
    fn async_compaction_stale_result_is_rejected() {
        let manifest = CompactionManifest::build(10, &[(Some(Label::Fact), "fact".into())]);
        // History moved 10 → 12 while the async job ran.
        let err = apply_compaction(&manifest, 10, 12).unwrap_err();
        assert!(matches!(
            err,
            ApplyError::Stale {
                computed_at_head: 10,
                current_head: 12
            }
        ));
        // Same-head applies cleanly.
        assert!(apply_compaction(&manifest, 12, 12).is_ok());
    }
}
