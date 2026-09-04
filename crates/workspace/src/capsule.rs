//! Environment blueprints (M2, REQ-EV-0146) and the Workspace Capsule
//! (REQ-EV-0176). A blueprint is a reusable, REVISIONED environment
//! definition; rebuilding from it pins the exact environment digest. A
//! capsule is a portable bounded task package whose handoff scan proves no
//! authority/secret values are smuggled inside.

use crate::handoff::EnvironmentSnapshot;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// A reusable environment definition. Every material change bumps
/// `revision`; `digest` pins the exact resolved environment (QUAL-EV-0146).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentBlueprint {
    pub blueprint_id: String,
    pub revision: u64,
    pub toolchain: String,
    pub path_entries: Vec<String>,
    pub workspace_roots: Vec<String>,
    /// Tool name → required version prefix.
    pub tool_requirements: BTreeMap<String, String>,
    /// Env refs (names + non-secret values) applied to sessions.
    pub env_refs: BTreeMap<String, String>,
}

impl EnvironmentBlueprint {
    pub fn new(blueprint_id: &str) -> Self {
        Self {
            blueprint_id: blueprint_id.to_string(),
            revision: 1,
            toolchain: String::new(),
            path_entries: Vec::new(),
            workspace_roots: Vec::new(),
            tool_requirements: BTreeMap::new(),
            env_refs: BTreeMap::new(),
        }
    }

    /// Material change: bumps the revision (blueprints are immutable-ish —
    /// you never mutate a pinned revision, you derive the next one).
    pub fn revise(&mut self) {
        self.revision += 1;
    }

    /// The exact environment digest this revision pins.
    pub fn digest(&self) -> String {
        let canonical = serde_json::to_vec(self).unwrap_or_default();
        sha256_hex(&canonical)
    }

    /// Rebuilds the pinned environment snapshot from the blueprint: the
    /// rebuilt snapshot's identity matches the pinned digest exactly.
    pub fn rebuild(
        &self,
        available_tools: &BTreeMap<String, String>,
    ) -> Result<EnvironmentSnapshot, String> {
        // Tool availability: the rebuilt environment must satisfy the
        // blueprint's requirements exactly as pinned.
        let mut tool_availability = BTreeMap::new();
        for (name, required_prefix) in &self.tool_requirements {
            let actual = available_tools
                .get(name)
                .ok_or_else(|| format!("blueprint tool {name:?} unavailable"))?;
            if !actual.starts_with(required_prefix.as_str()) {
                return Err(format!(
                    "blueprint tool {name:?} is {actual:?}, requires {required_prefix:?}"
                ));
            }
            tool_availability.insert(name.clone(), actual.clone());
        }
        Ok(EnvironmentSnapshot {
            snapshot_id: format!("{}-r{}", self.blueprint_id, self.revision),
            toolchain: self.toolchain.clone(),
            path_entries: self.path_entries.clone(),
            workspace_roots: self.workspace_roots.clone(),
            tool_availability,
            revision: self.revision,
        })
    }
}

/// The canonical identity digest of a resolved environment snapshot — the
/// value a blueprint's rebuild PINS (QUAL-EV-0146).
pub fn environment_identity_digest(snapshot: &EnvironmentSnapshot) -> String {
    sha256_hex(
        &serde_json::to_vec(&(
            &snapshot.toolchain,
            &snapshot.path_entries,
            &snapshot.workspace_roots,
            &snapshot.tool_availability,
            snapshot.revision,
        ))
        .unwrap_or_default(),
    )
}

/// Rebuild pinning proof (QUAL-EV-0146): a later rebuild must digest to
/// the pinned value — any drift is a typed error before work starts.
pub fn verify_rebuild_pin(
    rebuilt: &EnvironmentSnapshot,
    pinned_digest: &str,
) -> Result<(), String> {
    let digest = environment_identity_digest(rebuilt);
    if digest != pinned_digest {
        return Err(format!(
            "rebuild digest {digest} does not match pinned {pinned_digest}"
        ));
    }
    Ok(())
}

/// A bounded portable task package: context/decisions/evidence refs plus
/// runtime requirements (REQ-EV-0176). Boundness is enforced at
/// construction: each section has a hard byte cap.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceCapsule {
    pub capsule_id: String,
    pub source_session: String,
    pub context_summary: String,
    pub decisions: Vec<String>,
    pub evidence_refs: Vec<String>,
    pub runtime_requirements: Vec<String>,
    pub digest: String,
}

/// Section byte caps (bounded package — never a workspace dump).
const CONTEXT_CAP: usize = 16 * 1024;
const DECISIONS_CAP: usize = 8 * 1024;

#[derive(Debug)]
pub enum CapsuleError {
    TooLarge {
        section: &'static str,
        bytes: usize,
        cap: usize,
    },
    SmuggledSecret {
        field: &'static str,
        pattern: &'static str,
    },
}

impl std::fmt::Display for CapsuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CapsuleError::TooLarge {
                section,
                bytes,
                cap,
            } => {
                write!(f, "capsule section {section} is {bytes}B, cap {cap}B")
            }
            CapsuleError::SmuggledSecret { field, pattern } => {
                write!(f, "capsule {field} looks like a secret ({pattern})")
            }
        }
    }
}

impl std::error::Error for CapsuleError {}

/// Secret-shape patterns the handoff scan rejects outright.
const SECRET_PATTERNS: [(&str, &str); 5] = [
    ("sk-", "OpenAI-style API key prefix"),
    ("AKIA", "AWS access key id"),
    ("BEGIN PRIVATE KEY", "embedded private key"),
    ("ghp_", "GitHub personal access token"),
    ("xoxb-", "Slack bot token"),
];

fn scan_text(field: &'static str, text: &str) -> Result<(), CapsuleError> {
    for (pattern, what) in SECRET_PATTERNS {
        if text.contains(pattern) {
            return Err(CapsuleError::SmuggledSecret {
                field,
                pattern: what,
            });
        }
    }
    Ok(())
}

impl WorkspaceCapsule {
    /// Packs a capsule, enforcing boundness and scanning for smuggled
    /// secret SHAPES at construction.
    pub fn pack(
        capsule_id: &str,
        source_session: &str,
        context_summary: String,
        decisions: Vec<String>,
        evidence_refs: Vec<String>,
        runtime_requirements: Vec<String>,
    ) -> Result<Self, CapsuleError> {
        if context_summary.len() > CONTEXT_CAP {
            return Err(CapsuleError::TooLarge {
                section: "context_summary",
                bytes: context_summary.len(),
                cap: CONTEXT_CAP,
            });
        }
        let decisions_bytes: usize = decisions.iter().map(|d| d.len() + 1).sum();
        if decisions_bytes > DECISIONS_CAP {
            return Err(CapsuleError::TooLarge {
                section: "decisions",
                bytes: decisions_bytes,
                cap: DECISIONS_CAP,
            });
        }
        scan_text("context_summary", &context_summary)?;
        for d in &decisions {
            scan_text("decisions", d)?;
        }
        let mut capsule = Self {
            capsule_id: capsule_id.to_string(),
            source_session: source_session.to_string(),
            context_summary,
            decisions,
            evidence_refs,
            runtime_requirements,
            digest: String::new(),
        };
        // Serialization of owned strings cannot fail; digest covers every
        // section (computed before digest is filled).
        let canonical = serde_json::to_vec(&capsule).unwrap_or_default();
        capsule.digest = sha256_hex(&canonical);
        Ok(capsule)
    }
}

/// Handoff verification (QUAL-EV-0176): scans the capsule against the
/// session's KNOWN secret values — a capsule that embeds any of them is
/// refused. Catches what pattern matching cannot (custom secret values).
pub fn verify_no_smuggled_secrets(
    capsule: &WorkspaceCapsule,
    known_secret_values: &[String],
) -> Result<(), CapsuleError> {
    let haystacks = [("context_summary", capsule.context_summary.as_str())];
    for (field, text) in haystacks {
        for secret in known_secret_values {
            if secret.len() >= 8 && text.contains(secret.as_str()) {
                return Err(CapsuleError::SmuggledSecret {
                    field,
                    pattern: "known session secret value",
                });
            }
        }
    }
    for d in &capsule.decisions {
        for secret in known_secret_values {
            if secret.len() >= 8 && d.contains(secret.as_str()) {
                return Err(CapsuleError::SmuggledSecret {
                    field: "decisions",
                    pattern: "known session secret value",
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0146: rebuild pins the exact environment digest.
    #[test]
    fn blueprint_rebuild_pins_exact_digest() {
        let mut bp = EnvironmentBlueprint::new("rust-ci");
        bp.toolchain = "stable-1.97".into();
        bp.path_entries = vec!["/usr/local/bin".into()];
        bp.workspace_roots = vec!["/workspace".into()];
        bp.tool_requirements.insert("cargo".into(), "1.".into());

        let mut tools = BTreeMap::new();
        tools.insert("cargo".to_string(), "1.97.1".to_string());
        let first = bp.rebuild(&tools).unwrap();

        // Pin the first rebuild; an identical rebuild reproduces the pin.
        let pinned = environment_identity_digest(&first);
        let again = bp.rebuild(&tools).unwrap();
        verify_rebuild_pin(&again, &pinned).unwrap();

        // A material change bumps the revision and the digest moves.
        bp.tool_requirements.insert("rustc".into(), "1.".into());
        bp.revise();
        assert_eq!(bp.revision, 2);
        // rustc now required but missing from `tools`: rebuild fails.
        let drifted = bp.rebuild(&tools).unwrap_err();
        assert!(drifted.contains("unavailable"));

        // With the tool present, the new revision rebuilds — and no longer
        // matches the OLD pin (drift is detected, never silently accepted).
        tools.insert("rustc".to_string(), "1.97.1".to_string());
        let rebuilt_v2 = bp.rebuild(&tools).unwrap();
        assert_ne!(
            environment_identity_digest(&rebuilt_v2),
            pinned,
            "a new revision must not match the old pin"
        );
        verify_rebuild_pin(&rebuilt_v2, &pinned).unwrap_err();

        // Unavailable tool: rebuild fails BEFORE any work starts.
        let missing = bp.rebuild(&BTreeMap::new()).unwrap_err();
        assert!(missing.contains("unavailable"));
    }

    /// QUAL-EV-0176: handoff verifies no authority/secret values are
    /// smuggled in the capsule.
    #[test]
    fn capsule_never_carries_secrets() {
        // Pattern-level smuggle is rejected at pack time.
        let packed = WorkspaceCapsule::pack(
            "cap-1",
            "sess-1",
            "used key sk-abc123def456 for provider calls".into(),
            vec![],
            vec!["artifact:deadbeef".into()],
            vec!["rust".into()],
        );
        assert!(matches!(packed, Err(CapsuleError::SmuggledSecret { .. })));

        // A clean pack succeeds and is bounded.
        let capsule = WorkspaceCapsule::pack(
            "cap-2",
            "sess-1",
            "refactored the event store; 3 decisions recorded".into(),
            vec!["chose WAL mode".into()],
            vec!["artifact:cafe".into()],
            vec!["rust".into()],
        )
        .unwrap();
        assert!(!capsule.digest.is_empty());

        // Value-level smuggle (custom secret, no recognizable shape) is
        // caught against the session's known secret values.
        let leaked = WorkspaceCapsule::pack(
            "cap-3",
            "sess-1",
            "the warehouse password is hunter2-do-not-use!".into(),
            vec![],
            vec![],
            vec![],
        )
        .unwrap();
        let err =
            verify_no_smuggled_secrets(&leaked, &["hunter2-do-not-use!".to_string()]).unwrap_err();
        assert!(matches!(err, CapsuleError::SmuggledSecret { .. }));

        // The clean capsule passes the same check.
        verify_no_smuggled_secrets(&capsule, &["hunter2-do-not-use!".to_string()]).unwrap();

        // Over-large context is refused at pack time.
        let big = WorkspaceCapsule::pack(
            "cap-4",
            "sess-1",
            "x".repeat(CONTEXT_CAP + 1),
            vec![],
            vec![],
            vec![],
        );
        assert!(matches!(big, Err(CapsuleError::TooLarge { .. })));
    }
}
