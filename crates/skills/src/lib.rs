//! modbit-skills — skill registry, execution capsules, instruction
//! compiler (M5, docs/16 § skills).
//!
//! This slice: filesystem-discovered skills (REQ-EV-0114), the skill
//! execution capsule with narrow-only authority (REQ-EV-0061),
//! path-scoped lazy rules (REQ-EV-0059), and the instruction manifest
//! with explicit selection provenance (REQ-EV-0105).
//!
//! Canonical owner subsystem: skills (docs/81). Layout: docs/12.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

pub mod eval_harness;
pub mod evolution;
pub mod impact_log;
pub mod layering;
pub mod wiki_index;

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// Filesystem-discovered skills (REQ-EV-0114)
// ---------------------------------------------------------------------------

/// A discovered, hash-pinned skill.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiscoveredSkill {
    pub name: String,
    pub version: String,
    pub description: String,
    /// Path of the SKILL.md manifest.
    pub manifest_path: String,
    /// sha256 over the manifest bytes (provenance pin).
    pub content_sha256: String,
}

#[derive(Debug)]
pub enum SkillDiscoveryError {
    InvalidMetadata { path: String, reason: String },
    Io(std::io::Error),
}

impl fmt::Display for SkillDiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SkillDiscoveryError::InvalidMetadata { path, reason } => {
                write!(f, "invalid skill metadata in {path}: {reason}")
            }
            SkillDiscoveryError::Io(e) => write!(f, "skill discovery io: {e}"),
        }
    }
}

impl std::error::Error for SkillDiscoveryError {}

/// Parses a SKILL.md manifest: `key: value` header lines until the first
/// empty line, then the body.
fn parse_skill_manifest(path: &str, bytes: &[u8]) -> Result<DiscoveredSkill, SkillDiscoveryError> {
    let text = String::from_utf8_lossy(bytes);
    let mut name = None;
    let mut version = None;
    let mut description = None;
    for line in text.lines() {
        if line.trim().is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("name: ") {
            name = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("version: ") {
            version = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("description: ") {
            description = Some(v.trim().to_string());
        }
    }
    let name = name.ok_or_else(|| SkillDiscoveryError::InvalidMetadata {
        path: path.to_string(),
        reason: "missing name".into(),
    })?;
    let version = version.ok_or_else(|| SkillDiscoveryError::InvalidMetadata {
        path: path.to_string(),
        reason: "missing version".into(),
    })?;
    let description = description.ok_or_else(|| SkillDiscoveryError::InvalidMetadata {
        path: path.to_string(),
        reason: "missing description".into(),
    })?;
    Ok(DiscoveredSkill {
        name,
        version,
        description,
        manifest_path: path.to_string(),
        content_sha256: sha256_hex(bytes),
    })
}

/// The skill registry, refreshed from disk.
#[derive(Default)]
pub struct SkillRegistry {
    pub skills: BTreeMap<String, DiscoveredSkill>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Default::default()
    }

    /// Scans `dir` recursively for SKILL.md manifests. Invalid metadata
    /// FAILS the whole refresh (fail-closed) — a broken skill cannot
    /// silently vanish into a partial registry.
    pub fn refresh(&mut self, dir: &Path) -> Result<usize, SkillDiscoveryError> {
        self.skills.clear();
        let mut count = 0usize;
        fn walk(dir: &Path, found: &mut Vec<(String, Vec<u8>)>) -> Result<(), SkillDiscoveryError> {
            for entry in std::fs::read_dir(dir).map_err(SkillDiscoveryError::Io)? {
                let entry = entry.map_err(SkillDiscoveryError::Io)?;
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, found)?;
                } else if path.file_name().map(|n| n == "SKILL.md").unwrap_or(false) {
                    let bytes = std::fs::read(&path).map_err(SkillDiscoveryError::Io)?;
                    found.push((path.to_string_lossy().to_string(), bytes));
                }
            }
            Ok(())
        }
        let mut found = Vec::new();
        if dir.exists() {
            walk(dir, &mut found)?;
        }
        for (path, bytes) in found {
            let skill = parse_skill_manifest(&path, &bytes)?;
            self.skills.insert(skill.name.clone(), skill);
            count += 1;
        }
        Ok(count)
    }
}

// ---------------------------------------------------------------------------
// Skill execution capsule (REQ-EV-0061)
// ---------------------------------------------------------------------------

/// The authority a skill REQUESTS.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestedAuthority {
    ReadOnly,
    Write,
    External,
    Admin,
}

/// The task's capability CEILING.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorityCeiling {
    ReadOnly,
    Write,
    External,
}

/// The skill execution capsule: invocation contract, context, tool
/// requirements, model policy, verification — and the narrowed authority.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkillCapsule {
    pub skill_name: String,
    pub invocation_contract: String,
    pub context_requirements: Vec<String>,
    pub tool_requirements: Vec<String>,
    pub model_policy: String,
    pub verification: String,
    /// The authority the capsule actually runs with — set by narrowing.
    pub effective_authority: AuthorityCeiling,
}

#[derive(Debug)]
pub enum CapsuleError {
    /// The skill requested authority ABOVE the task ceiling. Capsules
    /// only NARROW; they never widen.
    AuthorityEscalation {
        requested: RequestedAuthority,
        ceiling: AuthorityCeiling,
    },
}

impl fmt::Display for CapsuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CapsuleError::AuthorityEscalation { requested, ceiling } => {
                write!(f, "skill requests {requested:?} but task ceiling is {ceiling:?} — capsules narrow, never widen")
            }
        }
    }
}

impl std::error::Error for CapsuleError {}

/// Builds a capsule whose effective authority is the MINIMUM of what the
/// skill requests and the task ceiling. Admin is NEVER grantable: a
/// skill requesting admin under any ceiling is refused outright.
pub fn build_capsule(
    skill_name: &str,
    requested: RequestedAuthority,
    ceiling: AuthorityCeiling,
) -> Result<SkillCapsule, CapsuleError> {
    if requested == RequestedAuthority::Admin {
        return Err(CapsuleError::AuthorityEscalation { requested, ceiling });
    }
    // Narrowing: the effective authority is the MINIMUM of what the
    // skill requests and the task ceiling (Admin is refused above).
    let effective = match (requested, ceiling) {
        (RequestedAuthority::ReadOnly, _) => AuthorityCeiling::ReadOnly,
        (RequestedAuthority::Write, AuthorityCeiling::ReadOnly) => AuthorityCeiling::ReadOnly,
        (RequestedAuthority::Write, _) => AuthorityCeiling::Write,
        (RequestedAuthority::External, ceiling) => ceiling,
        _ => AuthorityCeiling::ReadOnly,
    };
    Ok(SkillCapsule {
        skill_name: skill_name.to_string(),
        invocation_contract: format!("{skill_name}(context, tools) -> result"),
        context_requirements: vec!["task context pack".into()],
        tool_requirements: vec![],
        model_policy: "task-scoped".into(),
        verification: "deterministic gates".into(),
        effective_authority: effective,
    })
}

// ---------------------------------------------------------------------------
// Path-scoped lazy rules (REQ-EV-0059)
// ---------------------------------------------------------------------------

/// A workspace rule scoped to path prefixes, with explicit precedence.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScopedRule {
    pub id: String,
    /// Path prefix that ACTIVATES the rule.
    pub scope_prefix: String,
    pub text: String,
    /// Lower number = higher precedence; ties break by id.
    pub precedence: u32,
}

/// Activates rules only for matching active paths — an unrelated module's
/// rule stays absent until its path is touched (QUAL-EV-0059).
/// Deterministic ordering: precedence ascending, then id ascending.
pub fn active_rules<'a>(rules: &'a [ScopedRule], active_paths: &[&str]) -> Vec<&'a ScopedRule> {
    let mut active: Vec<&ScopedRule> = rules
        .iter()
        .filter(|rule| {
            active_paths
                .iter()
                .any(|p| p.starts_with(rule.scope_prefix.as_str()))
        })
        .collect();
    active.sort_by(|a, b| a.precedence.cmp(&b.precedence).then(a.id.cmp(&b.id)));
    active
}

// ---------------------------------------------------------------------------
// Instruction manifest (REQ-EV-0105)
// ---------------------------------------------------------------------------

/// One explicitly selected instruction source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InstructionEntry {
    pub kind: String,
    pub name: String,
    pub version: String,
    pub source: String,
    /// WHY it was selected.
    pub reason: String,
    pub sha256: String,
}

/// The manifest that records every selected skill/rule for the prompt —
/// the audit trail that survives compaction (it is part of the stable
/// prompt segment).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct InstructionManifest {
    pub entries: Vec<InstructionEntry>,
}

impl InstructionManifest {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn add_skill(&mut self, skill: &DiscoveredSkill, reason: &str) {
        self.entries.push(InstructionEntry {
            kind: "skill".to_string(),
            name: skill.name.clone(),
            version: skill.version.clone(),
            source: skill.manifest_path.clone(),
            reason: reason.to_string(),
            sha256: skill.content_sha256.clone(),
        });
    }

    pub fn add_rule(&mut self, rule: &ScopedRule, reason: &str) {
        self.entries.push(InstructionEntry {
            kind: "rule".to_string(),
            name: rule.id.clone(),
            version: "1".into(),
            source: format!("rule:{}", rule.scope_prefix),
            reason: reason.to_string(),
            sha256: sha256_hex(rule.text.as_bytes()),
        });
    }

    /// The manifest is content-addressed: identical selections hash
    /// identically, so compaction preserving this segment preserves the
    /// key.
    pub fn manifest_digest(&self) -> String {
        sha256_hex(&serde_json::to_vec(self).unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0114: add/remove skill on disk; registry refreshes with
    /// hash/provenance and invalid metadata fails.
    #[test]
    fn filesystem_discovery_refresh_add_remove_and_invalid_fails() {
        let dir = std::env::temp_dir().join(format!("skills-{}", uuid::Uuid::now_v7().simple()));
        let skill_a = dir.join("alpha").join("SKILL.md");
        let skill_b = dir.join("beta").join("SKILL.md");
        std::fs::create_dir_all(skill_a.parent().unwrap()).unwrap();
        std::fs::create_dir_all(skill_b.parent().unwrap()).unwrap();
        std::fs::write(
            &skill_a,
            "name: alpha\nversion: 1.0.0\ndescription: Alpha skill\n\nbody\n",
        )
        .unwrap();
        std::fs::write(
            &skill_b,
            "name: beta\nversion: 0.2.0\ndescription: Beta skill\n",
        )
        .unwrap();

        let mut registry = SkillRegistry::new();
        assert_eq!(registry.refresh(&dir).unwrap(), 2);
        let alpha = registry.skills.get("alpha").unwrap();
        assert_eq!(alpha.version, "1.0.0");
        assert_eq!(
            alpha.content_sha256,
            sha256_hex(std::fs::read(&skill_a).unwrap().as_slice())
        );
        assert!(alpha.manifest_path.ends_with("SKILL.md"));

        // REMOVE beta on disk -> refresh drops it.
        std::fs::remove_file(&skill_b).unwrap();
        registry.refresh(&dir).unwrap();
        assert!(!registry.skills.contains_key("beta"));

        // INVALID metadata: refresh fails closed.
        std::fs::write(&skill_a, "version: 2.0.0\n").unwrap();
        assert!(matches!(
            registry.refresh(&dir),
            Err(SkillDiscoveryError::InvalidMetadata { .. })
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// QUAL-EV-0061: a malicious skill requesting admin capability cannot
    /// widen task authority.
    #[test]
    fn malicious_skill_cannot_widen_authority() {
        // Admin is refused under ANY ceiling.
        assert!(matches!(
            build_capsule(
                "evil",
                RequestedAuthority::Admin,
                AuthorityCeiling::External
            ),
            Err(CapsuleError::AuthorityEscalation { .. })
        ));
        // Write requested under a ReadOnly ceiling: NARROWED to ReadOnly
        // — the skill simply cannot write.
        let sneaky = build_capsule(
            "sneaky",
            RequestedAuthority::Write,
            AuthorityCeiling::ReadOnly,
        )
        .unwrap();
        assert_eq!(sneaky.effective_authority, AuthorityCeiling::ReadOnly);
        // A legitimate skill narrows to the ceiling.
        let capsule = build_capsule(
            "deploy",
            RequestedAuthority::External,
            AuthorityCeiling::Write,
        )
        .unwrap();
        assert_eq!(capsule.effective_authority, AuthorityCeiling::Write);
        assert!(capsule.invocation_contract.starts_with("deploy("));
        assert!(!capsule.verification.is_empty());
    }

    /// QUAL-EV-0059: an unrelated module's rule stays absent until its
    /// path is touched.
    #[test]
    fn lazy_rules_activate_only_on_matching_paths() {
        let rules = vec![
            ScopedRule {
                id: "retry-rules".into(),
                scope_prefix: "src/retry".into(),
                text: "always preserve the backoff contract".into(),
                precedence: 2,
            },
            ScopedRule {
                id: "ui-rules".into(),
                scope_prefix: "src/ui".into(),
                text: "focus management is mandatory".into(),
                precedence: 1,
            },
        ];

        // Touching src/lib.rs: NO rules apply (both scopes unrelated).
        assert!(active_rules(&rules, &["src/lib.rs"]).is_empty());

        // Touching src/retry.rs: only the retry rule activates.
        let active = active_rules(&rules, &["src/lib.rs", "src/retry.rs"]);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, "retry-rules");

        // Touching both scopes: deterministic precedence order.
        let active = active_rules(&rules, &["src/retry.rs", "src/ui/button.rs"]);
        assert_eq!(active.len(), 2);
        assert_eq!(active[0].id, "ui-rules");
        assert_eq!(active[1].id, "retry-rules");
    }

    /// QUAL-EV-0105: the manifest records selected skills/rules with
    /// source/version/reason, and its digest survives compaction.
    #[test]
    fn instruction_manifest_records_selection_and_survives_compaction() {
        let skill = DiscoveredSkill {
            name: "alpha".into(),
            version: "1.0.0".into(),
            description: "Alpha skill".into(),
            manifest_path: "skills/alpha/SKILL.md".into(),
            content_sha256: "sha-alpha".into(),
        };
        let rule = ScopedRule {
            id: "retry-rules".into(),
            scope_prefix: "src/retry".into(),
            text: "preserve backoff".into(),
            precedence: 2,
        };

        let mut manifest = InstructionManifest::new();
        manifest.add_skill(&skill, "task touches retry logic");
        manifest.add_rule(&rule, "scoped to src/retry");
        let digest_before = manifest.manifest_digest();

        // "Compaction" preserves the stable manifest segment: re-serialize
        // and rebuild — identical content, identical digest.
        let bytes = serde_json::to_vec(&manifest).unwrap();
        let restored: InstructionManifest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(restored.manifest_digest(), digest_before);

        // Selection provenance is explicit.
        assert_eq!(manifest.entries[0].kind, "skill");
        assert_eq!(manifest.entries[0].version, "1.0.0");
        assert!(manifest.entries[0].reason.contains("retry"));
        assert_eq!(manifest.entries[1].kind, "rule");

        // A different selection changes the digest.
        let mut other = manifest.clone();
        other.add_rule(&rule, "different reason");
        assert_ne!(other.manifest_digest(), digest_before);
    }
}
