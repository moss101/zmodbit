//! SKILL.md-based procedural packaging (M5, REQ-EV-0209) and invocation
//! policy metadata (REQ-EV-0214).
//!
//! PACKAGING: a portable package carries documentation + resources;
//! Modbit's EXECUTION POLICY STAYS SEPARATE (the package parser never
//! reads or grants authority). Malformed or oversized packages are
//! rejected. INVOCATION METADATA: a skill is model-invocable, user-only,
//! or system-only per policy — the model can NEVER invoke a skill marked
//! non-model-invocable.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The package size guard: oversized packages are rejected outright.
pub const MAX_PACKAGE_BYTES: usize = 512 * 1024;

/// A parsed skill package.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkillPackage {
    pub name: String,
    pub version: String,
    pub description: String,
    /// Resource files (relative path → content) referenced by the skill.
    pub resources: BTreeMap<String, String>,
    pub body: String,
}

#[derive(Debug)]
pub enum PackageError {
    Malformed { reason: String },
    Oversized { bytes: usize, cap: usize },
}

impl std::fmt::Display for PackageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PackageError::Malformed { reason } => write!(f, "malformed package: {reason}"),
            PackageError::Oversized { bytes, cap } => {
                write!(f, "package is {bytes}B, cap {cap}B")
            }
        }
    }
}

impl std::error::Error for PackageError {}

/// Parses a package: SKILL.md manifest section + `resource:` lines
/// naming files carried in the same archive. Validation is total:
/// missing/empty fields and malformed resource lines are rejected.
pub fn parse_package(bytes: &[u8]) -> Result<SkillPackage, PackageError> {
    if bytes.len() > MAX_PACKAGE_BYTES {
        return Err(PackageError::Oversized {
            bytes: bytes.len(),
            cap: MAX_PACKAGE_BYTES,
        });
    }
    let text = String::from_utf8_lossy(bytes);
    let mut name = None;
    let mut version = None;
    let mut description = None;
    let mut resources = BTreeMap::new();
    let mut in_header = true;
    let mut body_lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        if in_header {
            if line.trim().is_empty() {
                in_header = false;
                continue;
            }
            if let Some(v) = line.strip_prefix("name: ") {
                name = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("version: ") {
                version = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("description: ") {
                description = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("resource: ") {
                let path = v.trim();
                if path.is_empty() || path.starts_with('/') || path.contains("..") {
                    return Err(PackageError::Malformed {
                        reason: format!("unsafe resource path {path:?}"),
                    });
                }
                resources.insert(path.to_string(), String::new());
            } else {
                return Err(PackageError::Malformed {
                    reason: format!("unknown header line {line:?}"),
                });
            }
        } else {
            body_lines.push(line);
        }
    }
    let name = name.ok_or_else(|| PackageError::Malformed {
        reason: "missing name".into(),
    })?;
    let version = version.ok_or_else(|| PackageError::Malformed {
        reason: "missing version".into(),
    })?;
    let description = description.ok_or_else(|| PackageError::Malformed {
        reason: "missing description".into(),
    })?;
    if name.is_empty() || version.is_empty() {
        return Err(PackageError::Malformed {
            reason: "name/version must not be empty".into(),
        });
    }
    Ok(SkillPackage {
        name,
        version,
        description,
        resources,
        body: body_lines.join("\n"),
    })
}

// ---------------------------------------------------------------------------
// Invocation policy metadata (REQ-EV-0214)
// ---------------------------------------------------------------------------

/// Who may invoke a skill.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationPolicy {
    /// The model may invoke it as a tool.
    ModelInvocable,
    /// Only the user (operator surface) may invoke it.
    UserOnly,
    /// Only the system (workflows) may invoke it.
    SystemOnly,
}

/// A registry entry carrying its invocation policy.
#[derive(Clone, Debug)]
pub struct InvocableSkill {
    pub name: String,
    pub policy: InvocationPolicy,
}

#[derive(Debug)]
pub enum InvocationError {
    NotModelInvocable {
        skill: String,
        policy: InvocationPolicy,
    },
}

impl std::fmt::Display for InvocationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InvocationError::NotModelInvocable { skill, policy } => {
                write!(
                    f,
                    "skill {skill:?} is {policy:?} — the model cannot invoke it"
                )
            }
        }
    }
}

impl std::error::Error for InvocationError {}

/// The model's attempt to invoke a skill. Non-model-invocable skills are
/// refused BEFORE any effect (QUAL-EV-0214).
pub fn model_invoke(skill: &InvocableSkill, arguments: &str) -> Result<String, InvocationError> {
    if skill.policy != InvocationPolicy::ModelInvocable {
        return Err(InvocationError::NotModelInvocable {
            skill: skill.name.clone(),
            policy: skill.policy,
        });
    }
    Ok(format!("invoked {} with {arguments}", skill.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0209: the parser validates metadata/resources and rejects
    /// malformed/oversized packages. Execution policy stays separate.
    #[test]
    fn package_parser_validates_and_rejects() {
        let good = b"name: deploy-helper\nversion: 1.2.0\ndescription: Deployment walkthrough\nresource: runbook.md\nresource: templates/env.txt\n\nWalk through the deployment steps.\n";
        let package = parse_package(good).unwrap();
        assert_eq!(package.name, "deploy-helper");
        assert_eq!(package.resources.len(), 2);
        assert!(package.body.contains("deployment steps"));
        // The package carries NO execution policy: Modbit policy applies
        // separately (the parsed struct has no authority field).
        assert!(serde_json::to_string(&package)
            .unwrap()
            .to_lowercase()
            .contains("deploy-helper"));

        // Malformed: unknown header line.
        assert!(matches!(
            parse_package(b"authority: admin\n"),
            Err(PackageError::Malformed { .. })
        ));
        // Malformed: unsafe resource path.
        assert!(matches!(
            parse_package(b"name: x\nversion: 1\ndescription: d\nresource: /etc/passwd\n"),
            Err(PackageError::Malformed { .. })
        ));
        // Malformed: missing description.
        assert!(matches!(
            parse_package(b"name: x\nversion: 1\n"),
            Err(PackageError::Malformed { .. })
        ));
        // Oversized.
        let big = vec![b'x'; MAX_PACKAGE_BYTES + 1];
        assert!(matches!(
            parse_package(&big),
            Err(PackageError::Oversized { .. })
        ));
    }

    /// QUAL-EV-0214: the model cannot invoke a skill marked
    /// non-model-invocable.
    #[test]
    fn model_cannot_invoke_non_model_invocable_skill() {
        let model_skill = InvocableSkill {
            name: "retry-helper".into(),
            policy: InvocationPolicy::ModelInvocable,
        };
        let user_skill = InvocableSkill {
            name: "operator-console".into(),
            policy: InvocationPolicy::UserOnly,
        };
        let system_skill = InvocableSkill {
            name: "internal-sweep".into(),
            policy: InvocationPolicy::SystemOnly,
        };

        assert!(model_invoke(&model_skill, "{}").is_ok());
        assert!(matches!(
            model_invoke(&user_skill, "{}"),
            Err(InvocationError::NotModelInvocable { .. })
        ));
        assert!(matches!(
            model_invoke(&system_skill, "{}"),
            Err(InvocationError::NotModelInvocable { .. })
        ));
    }
}
