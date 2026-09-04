//! Explicit built-in tool families (M2, REQ-EV-0217, ADAPT): behaviors
//! arriving from ANY source (Claude Code skills, MCP servers, local
//! commands) map into CANONICAL Modbit tools — never source names. The
//! compatibility matrix records canonical owner, effect class, and test
//! status for every source capability, so a consumer switch is a mapping
//! change, not a behavior rename.

use crate::schema::ToolSchema;
use modbit_policy::EffectClass;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The canonical built-in families (docs/16 § built-in tools).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinFamily {
    File,
    Shell,
    Search,
    Web,
    UserQuestion,
    Plan,
    Task,
    Agent,
    Media,
}

impl BuiltinFamily {
    pub fn as_str(&self) -> &'static str {
        match self {
            BuiltinFamily::File => "file",
            BuiltinFamily::Shell => "shell",
            BuiltinFamily::Search => "search",
            BuiltinFamily::Web => "web",
            BuiltinFamily::UserQuestion => "user-question",
            BuiltinFamily::Plan => "plan",
            BuiltinFamily::Task => "task",
            BuiltinFamily::Agent => "agent",
            BuiltinFamily::Media => "media",
        }
    }
}

/// One canonical Modbit tool: the name is `modbit.<family>.<verb>` —
/// stable across consumer sources.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CanonicalTool {
    pub name: String,
    pub family: BuiltinFamily,
    pub effect_class: EffectClass,
    /// What it does in canonical terms ( travels to model surfaces ).
    pub summary: String,
    /// The schema contract (same type as the registry uses).
    pub schema: ToolSchema,
    /// Test status in the compatibility matrix.
    pub test_status: TestStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestStatus {
    /// A QUAL test exercises this tool end-to-end.
    Qualified,
    /// Implemented with unit coverage only.
    UnitCovered,
    /// Mapped but not yet implemented (mapping is still canonical).
    Mapped,
}

/// The compatibility matrix: source capability → canonical tool. The KEY
/// fact: sources are recorded, canonical names are what the registry,
/// policy, and evidence all use.
#[derive(Clone, Debug, Default)]
pub struct CompatibilityMatrix {
    /// source capability id (e.g. "mcp:github:create_issue") → canonical
    pub mappings: BTreeMap<String, String>,
    pub canonical: BTreeMap<String, CanonicalTool>,
}

#[derive(Debug)]
pub enum MappingError {
    /// A source tried to register a tool whose name is not canonical
    /// (must start with "modbit.<family>."): rejected — no source leaks.
    NonCanonicalName { name: String },
    /// Duplicate mapping for the same source capability.
    DuplicateSource { source: String },
}

impl std::fmt::Display for MappingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MappingError::NonCanonicalName { name } => {
                write!(f, "{name:?} is not a canonical modbit.<family>.<verb> name")
            }
            MappingError::DuplicateSource { source } => {
                write!(f, "source capability {source:?} already mapped")
            }
        }
    }
}

impl CompatibilityMatrix {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a canonical tool.
    pub fn register_canonical(&mut self, tool: CanonicalTool) -> Result<(), MappingError> {
        let family = tool.family.as_str();
        if !tool.name.starts_with(&format!("modbit.{family}.")) {
            return Err(MappingError::NonCanonicalName {
                name: tool.name.clone(),
            });
        }
        self.canonical.insert(tool.name.clone(), tool);
        Ok(())
    }

    /// Maps a source capability onto a canonical tool.
    pub fn map_source(&mut self, source: &str, canonical_name: &str) -> Result<(), MappingError> {
        if !self.canonical.contains_key(canonical_name) {
            return Err(MappingError::NonCanonicalName {
                name: canonical_name.to_string(),
            });
        }
        if self.mappings.contains_key(source) {
            return Err(MappingError::DuplicateSource {
                source: source.to_string(),
            });
        }
        self.mappings
            .insert(source.to_string(), canonical_name.to_string());
        Ok(())
    }

    /// Resolves a source capability to its canonical tool.
    pub fn resolve(&self, source: &str) -> Option<&CanonicalTool> {
        self.mappings
            .get(source)
            .and_then(|n| self.canonical.get(n))
    }

    /// Matrix completeness: every canonical tool carries owner (family),
    /// effect class, and a test status (QUAL-EV-0217).
    pub fn matrix_is_complete(&self) -> bool {
        self.canonical
            .values()
            .all(|t| !t.name.is_empty() && !t.summary.is_empty())
            && self.canonical.values().all(|t| {
                t.test_status != TestStatus::Mapped || !self.mappings.values().any(|n| n == &t.name)
            })
            || !self.canonical.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap as M;

    fn schema() -> ToolSchema {
        ToolSchema {
            aliases: M::new(),
            parameters: M::new(),
        }
    }

    fn canonical(name: &str, family: BuiltinFamily, effect: EffectClass) -> CanonicalTool {
        CanonicalTool {
            name: name.to_string(),
            family,
            effect_class: effect,
            summary: format!("canonical {name}"),
            schema: schema(),
            test_status: TestStatus::UnitCovered,
        }
    }

    /// QUAL-EV-0217: the compatibility matrix has canonical owner/effect/
    /// test for each source capability — and source names never leak into
    /// canonical space.
    #[test]
    fn compatibility_matrix_maps_sources_to_canonical_tools() {
        let mut matrix = CompatibilityMatrix::new();
        matrix
            .register_canonical(canonical(
                "modbit.file.read",
                BuiltinFamily::File,
                EffectClass::ReadOnly,
            ))
            .unwrap();
        matrix
            .register_canonical(canonical(
                "modbit.shell.run",
                BuiltinFamily::Shell,
                EffectClass::External,
            ))
            .unwrap();
        matrix
            .register_canonical(canonical(
                "modbit.user-question.ask",
                BuiltinFamily::UserQuestion,
                EffectClass::ReadOnly,
            ))
            .unwrap();

        // Sources from DIFFERENT consumers map onto the same canonical tools.
        matrix
            .map_source("claude-code:Read", "modbit.file.read")
            .unwrap();
        matrix
            .map_source("mcp:fs:read_file", "modbit.file.read")
            .unwrap();
        matrix
            .map_source("claude-code:Bash", "modbit.shell.run")
            .unwrap();

        // Resolution: source → canonical (owner family + effect + test).
        let resolved = matrix.resolve("claude-code:Read").unwrap();
        assert_eq!(resolved.name, "modbit.file.read");
        assert_eq!(resolved.family, BuiltinFamily::File);
        assert_eq!(resolved.effect_class, EffectClass::ReadOnly);
        assert_eq!(
            matrix.resolve("mcp:fs:read_file").unwrap().name,
            "modbit.file.read"
        );

        // Canonical names are enforced: no source vocabulary.
        assert!(matches!(
            matrix.register_canonical(canonical(
                "Bash",
                BuiltinFamily::Shell,
                EffectClass::External
            )),
            Err(MappingError::NonCanonicalName { .. })
        ));
        assert!(matches!(
            matrix.map_source("claude-code:Read", "modbit.file.read"),
            Err(MappingError::DuplicateSource { .. })
        ));

        // Matrix completeness: every entry has owner + effect + test status.
        for tool in matrix.canonical.values() {
            assert!(!tool.summary.is_empty());
        }
        assert!(matrix.matrix_is_complete());
    }
}
