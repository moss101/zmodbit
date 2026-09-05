//! Memory/project instruction layering (M5, REQ-EV-0129) and
//! extension-provided skills (REQ-EV-0181).
//!
//! LAYERING: instructions and memory arrive from scoped sources (user /
//! project / runtime memory), each provenance-bound, with precedence,
//! optional TTL, and CONFLICT DIAGNOSTICS — a conflicting rule resolves
//! to an explicit winner whose source is shown.
//! EXTENSIONS: portable SKILL.md packages install with hash validation
//! and activate WITHOUT capability escalation (capsules narrow only).

use crate::build_capsule;
use crate::AuthorityCeiling;
use crate::RequestedAuthority;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// Instruction/memory layering (REQ-EV-0129)
// ---------------------------------------------------------------------------

/// The scope a layer comes from, with its precedence rank (lower wins).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstructionScope {
    /// The operator's personal instructions (highest rank).
    User,
    /// The project's committed instructions.
    Project,
    /// Runtime memory accumulated during sessions (lowest rank).
    Memory,
}

/// One provenance-bound instruction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayeredInstruction {
    pub scope: InstructionScope,
    /// The subject this instruction governs — conflicting rules share a
    /// subject and resolve to one winner.
    pub subject: String,
    pub text: String,
    /// Optional expiry (unix ms): expired instructions drop out.
    pub ttl_expires_ms: Option<i64>,
    pub sha256: String,
}

impl LayeredInstruction {
    pub fn new(
        scope: InstructionScope,
        subject: &str,
        text: &str,
        ttl_expires_ms: Option<i64>,
    ) -> Self {
        Self {
            scope,
            subject: subject.to_string(),
            text: text.to_string(),
            ttl_expires_ms,
            sha256: sha256_hex(text.as_bytes()),
        }
    }
}

/// The resolved winner for a conflicting rule position, with the losers
/// recorded as conflict diagnostics.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResolvedInstruction {
    pub winner: LayeredInstruction,
    /// The conflicting instructions that LOST, with their sources.
    pub conflicts: Vec<(InstructionScope, String)>,
}

#[derive(Debug)]
pub enum LayerError {
    Serialization(serde_json::Error),
}

impl fmt::Display for LayerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LayerError::Serialization(e) => write!(f, "layer serialization: {e}"),
        }
    }
}

impl std::error::Error for LayerError {}

/// Resolves instruction layers at `now_ms`: expired items drop out;
/// items with identical normalized text CONFLICT — the highest-precedence
/// scope wins and the losers are recorded as diagnostics with sources.
pub fn resolve_layers(layers: &[LayeredInstruction], now_ms: i64) -> Vec<ResolvedInstruction> {
    let mut by_subject: BTreeMap<String, Vec<&LayeredInstruction>> = BTreeMap::new();
    for layer in layers {
        // TTL: expired instructions are gone.
        if let Some(expires) = layer.ttl_expires_ms {
            if expires <= now_ms {
                continue;
            }
        }
        by_subject
            .entry(layer.subject.to_lowercase())
            .or_default()
            .push(layer);
    }
    let mut out = Vec::new();
    for (_subject, mut group) in by_subject {
        group.sort_by(|a, b| a.scope.cmp(&b.scope));
        let winner = group[0].clone();
        let conflicts = group[1..]
            .iter()
            .map(|l| (l.scope, l.text.clone()))
            .collect();
        out.push(ResolvedInstruction { winner, conflicts });
    }
    out
}

// ---------------------------------------------------------------------------
// Extension-provided skills (REQ-EV-0181)
// ---------------------------------------------------------------------------

/// An installed extension skill: hash-validated at install time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InstalledExtension {
    pub name: String,
    pub version: String,
    pub sha256: String,
    pub origin: String,
}

#[derive(Debug)]
pub enum ExtensionError {
    HashMismatch { expected: String, actual: String },
    InvalidManifest(String),
    EscalationRefused { skill: String },
}

impl fmt::Display for ExtensionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExtensionError::HashMismatch { expected, actual } => {
                write!(
                    f,
                    "extension hash mismatch: expected {expected}, got {actual}"
                )
            }
            ExtensionError::InvalidManifest(why) => write!(f, "invalid extension manifest: {why}"),
            ExtensionError::EscalationRefused { skill } => {
                write!(f, "extension skill {skill:?} cannot escalate authority")
            }
        }
    }
}

impl std::error::Error for ExtensionError {}

/// Installs an extension package: validates the provided hash, parses the
/// manifest, and returns the installed skill. Hash mismatch refuses the
/// install outright.
pub fn install_extension(
    manifest_bytes: &[u8],
    expected_sha256: &str,
    origin: &str,
) -> Result<InstalledExtension, ExtensionError> {
    let actual = sha256_hex(manifest_bytes);
    if actual != expected_sha256 {
        return Err(ExtensionError::HashMismatch {
            expected: expected_sha256.to_string(),
            actual,
        });
    }
    let text = String::from_utf8_lossy(manifest_bytes);
    let mut name = None;
    let mut version = None;
    for line in text.lines() {
        if line.trim().is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("name: ") {
            name = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("version: ") {
            version = Some(v.trim().to_string());
        }
    }
    let name = name.ok_or_else(|| ExtensionError::InvalidManifest("missing name".into()))?;
    let version =
        version.ok_or_else(|| ExtensionError::InvalidManifest("missing version".into()))?;
    Ok(InstalledExtension {
        name,
        version,
        sha256: actual,
        origin: origin.to_string(),
    })
}

/// Activates an installed extension under the task's authority ceiling —
/// WITHOUT escalation (delegates to the capsule's narrow-only contract).
pub fn activate_extension(
    installed: &InstalledExtension,
    requested: RequestedAuthority,
    ceiling: AuthorityCeiling,
) -> Result<crate::SkillCapsule, ExtensionError> {
    build_capsule(&installed.name, requested, ceiling).map_err(|_| {
        ExtensionError::EscalationRefused {
            skill: installed.name.clone(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0129: conflicting rules show an explicit winner and source.
    #[test]
    fn conflicting_rules_resolve_to_winner_with_source() {
        let now = 1_000_000;
        let layers = vec![
            LayeredInstruction::new(
                InstructionScope::Memory,
                "commit-policy",
                "always run clippy before commit",
                None,
            ),
            LayeredInstruction::new(
                InstructionScope::User,
                "commit-policy",
                "ALWAYS run clippy before commit, with -D warnings",
                None,
            ),
            LayeredInstruction::new(
                InstructionScope::Project,
                "commit-policy",
                "run clippy before commit using the CI profile",
                Some(now + 60_000), // still valid
            ),
        ];
        let resolved = resolve_layers(&layers, now);
        assert_eq!(resolved.len(), 1, "one conflicting rule position");
        let position = &resolved[0];
        // User scope wins (highest precedence).
        assert_eq!(position.winner.scope, InstructionScope::User);
        assert!(position.winner.text.contains("-D warnings"));
        // The losers are recorded as conflicts with their sources.
        assert_eq!(position.conflicts.len(), 2);
        assert!(position
            .conflicts
            .iter()
            .any(|(scope, _)| *scope == InstructionScope::Project));
        assert!(position
            .conflicts
            .iter()
            .any(|(scope, _)| *scope == InstructionScope::Memory));

        // A TTL-expired instruction drops out entirely.
        let expired = vec![LayeredInstruction::new(
            InstructionScope::User,
            "api-choice",
            "use the old API",
            Some(now - 1),
        )];
        assert!(resolve_layers(&expired, now).is_empty());
    }

    /// QUAL-EV-0181: install an extension skill with hash validation and
    /// activate WITHOUT capability escalation.
    #[test]
    fn extension_install_validates_hash_and_activates_without_escalation() {
        let manifest = b"name: helper\nversion: 1.1.0\n";
        let sha256 = sha256_hex(manifest);

        // Valid hash: installs with provenance.
        let installed = install_extension(manifest, &sha256, "extension:acme-tools").unwrap();
        assert_eq!(installed.name, "helper");
        assert_eq!(installed.sha256, sha256);

        // Hash mismatch: install refused outright.
        assert!(matches!(
            install_extension(manifest, "deadbeef", "extension:acme-tools"),
            Err(ExtensionError::HashMismatch { .. })
        ));

        // Activation WITHOUT escalation: read-only request under a
        // read-only ceiling works; an escalation attempt is refused.
        let capsule = activate_extension(
            &installed,
            RequestedAuthority::ReadOnly,
            AuthorityCeiling::ReadOnly,
        )
        .unwrap();
        assert_eq!(capsule.effective_authority, AuthorityCeiling::ReadOnly);
        assert!(matches!(
            activate_extension(
                &installed,
                RequestedAuthority::Admin,
                AuthorityCeiling::External
            ),
            Err(ExtensionError::EscalationRefused { .. })
        ));
        // The capsule narrowed an External request to the Write ceiling.
        let narrowed = activate_extension(
            &installed,
            RequestedAuthority::External,
            AuthorityCeiling::Write,
        )
        .unwrap();
        assert_eq!(narrowed.effective_authority, AuthorityCeiling::Write);
    }
}
