//! Token-budget context packing (M3, REQ-EV-0166) and context
//! compression (REQ-EV-0167).
//!
//! PACK COMPILER: fragments are packed value-ordered under a hard byte
//! budget; REQUIRED critical facts are reserved BEFORE budget packing, so
//! they are retained even under pressure — the budget is never exceeded.
//! COMPRESSION: lower-priority fragments compress into recoverable
//! handles (digest-addressed) with full provenance; hydration restores
//! them verbatim.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// A fragment offered to the pack compiler.
#[derive(Clone, Debug)]
pub struct Fragment {
    pub path: String,
    pub text: String,
    pub value: f64,
    /// Critical fragments are MANDATORY: reserved before budget packing.
    pub critical: bool,
}

/// A packed fragment.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PackedFragment {
    pub path: String,
    pub text: String,
    pub critical: bool,
}

#[derive(Debug)]
pub enum PackError {
    /// Required critical facts do not fit even alone.
    CriticalOverflow { needed: usize, budget: usize },
}

impl std::fmt::Display for PackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PackError::CriticalOverflow { needed, budget } => {
                write!(f, "critical facts need {needed}B but budget is {budget}B")
            }
        }
    }
}

impl std::error::Error for PackError {}

/// The pack: fragments + compressed handles for the remainder.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContextPack {
    pub packed: Vec<PackedFragment>,
    pub handles: Vec<CompressedHandle>,
    pub used_bytes: usize,
}

/// A recoverable handle standing in for a compressed fragment.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompressedHandle {
    pub path: String,
    /// Digest of the full original text (hydration key).
    pub sha256: String,
    /// One-line projection retained in context.
    pub projection: String,
}

/// The compression store: digest → full text (recoverable).
#[derive(Default)]
pub struct CompressionStore {
    blobs: BTreeMap<String, String>,
}

impl CompressionStore {
    pub fn new() -> Self {
        Default::default()
    }

    /// Compresses a fragment into a handle + stores the recoverable blob.
    pub fn compress(&mut self, fragment: &Fragment) -> CompressedHandle {
        let sha256 = sha256_hex(fragment.text.as_bytes());
        self.blobs.insert(sha256.clone(), fragment.text.clone());
        CompressedHandle {
            path: fragment.path.clone(),
            projection: format!(
                "[{}: {} lines, hydrate by digest]",
                fragment.path,
                fragment.text.lines().count()
            ),
            sha256,
        }
    }

    /// Hydrates a handle back to the FULL original text (pass = verbatim).
    pub fn hydrate(&self, handle: &CompressedHandle) -> Option<&str> {
        self.blobs.get(&handle.sha256).map(|s| s.as_str())
    }
}

/// Packs fragments: critical facts reserved first (always retained), then
/// value-ordered non-critical fragments until the budget is exhausted;
/// the remainder compresses into handles. Budget is NEVER exceeded.
pub fn pack_with_budget(
    fragments: &[Fragment],
    budget: usize,
    store: &mut CompressionStore,
) -> Result<ContextPack, PackError> {
    let mut pack = ContextPack::default();

    // 1. Reserve mandatory critical facts.
    let mut critical_bytes = 0usize;
    for fragment in fragments.iter().filter(|f| f.critical) {
        critical_bytes += fragment.text.len();
    }
    if critical_bytes > budget {
        return Err(PackError::CriticalOverflow {
            needed: critical_bytes,
            budget,
        });
    }
    for fragment in fragments.iter().filter(|f| f.critical) {
        pack.used_bytes += fragment.text.len();
        pack.packed.push(PackedFragment {
            path: fragment.path.clone(),
            text: fragment.text.clone(),
            critical: true,
        });
    }

    // 2. Value-ordered non-critical fragments within the remaining budget.
    let mut rest: Vec<&Fragment> = fragments.iter().filter(|f| !f.critical).collect();
    rest.sort_by(|a, b| {
        b.value
            .partial_cmp(&a.value)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for fragment in rest {
        if pack.used_bytes + fragment.text.len() <= budget {
            pack.used_bytes += fragment.text.len();
            pack.packed.push(PackedFragment {
                path: fragment.path.clone(),
                text: fragment.text.clone(),
                critical: false,
            });
        } else {
            // Compress the overflow into a recoverable handle.
            pack.handles.push(store.compress(fragment));
        }
    }
    Ok(pack)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fragments() -> Vec<Fragment> {
        vec![
            Fragment {
                path: "docs/big-context.md".into(),
                text: "x".repeat(600),
                value: 0.9,
                critical: false,
            },
            Fragment {
                path: "facts/critical-constraint.md".into(),
                text: "NEVER migrate the production DB without approval".into(),
                value: 1.0,
                critical: true,
            },
            Fragment {
                path: "src/lib.rs".into(),
                text: "pub fn entry() {}".into(),
                value: 0.95,
                critical: false,
            },
        ]
    }

    /// QUAL-EV-0166: budget never exceeded; required critical facts
    /// retained.
    #[test]
    fn budget_never_exceeded_and_critical_facts_retained() {
        let mut store = CompressionStore::new();
        let budget = 200usize;
        let pack = pack_with_budget(&fragments(), budget, &mut store).unwrap();

        assert!(pack.used_bytes <= budget, "budget must hold");
        // The critical fact is retained VERBATIM even though a
        // higher-value fragment wanted the space.
        assert!(pack
            .packed
            .iter()
            .any(|p| p.critical && p.text.contains("NEVER migrate the production DB")));
        // The big fragment compressed into a handle.
        assert!(pack.handles.iter().any(|h| h.path == "docs/big-context.md"));
    }

    /// QUAL-EV-0167: compression fidelity + handle hydration pass.
    #[test]
    fn compression_handles_hydrate_verbatim() {
        let mut store = CompressionStore::new();
        let fragment = Fragment {
            path: "src/large.rs".into(),
            text: "pub fn large() { /* 300 lines of real code */ }".into(),
            value: 0.3,
            critical: false,
        };
        let handle = store.compress(&fragment);
        assert!(handle.projection.contains("src/large.rs"));
        // Hydration restores the FULL original text verbatim.
        assert_eq!(store.hydrate(&handle).unwrap(), fragment.text);
        // Digest is a faithful address for the content.
        assert_eq!(handle.sha256, sha256_hex(fragment.text.as_bytes()));
    }

    /// Critical facts alone exceeding the budget fail LOUDLY.
    #[test]
    fn critical_overflow_fails_loudly() {
        let mut store = CompressionStore::new();
        let oversized = vec![Fragment {
            path: "facts/huge.md".into(),
            text: "y".repeat(500),
            value: 1.0,
            critical: true,
        }];
        let err = pack_with_budget(&oversized, 100, &mut store).unwrap_err();
        assert!(matches!(
            err,
            PackError::CriticalOverflow {
                needed: 500,
                budget: 100
            }
        ));
    }
}
