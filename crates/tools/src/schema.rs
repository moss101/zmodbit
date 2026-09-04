//! Tool schemas (M2, REQ-EV-0079/0116): a schema normalizes caller intent
//! BEFORE policy and execution — aliases are repaired to canonical
//! parameter names, defaults are filled, scalar types are coerced — and
//! anything unrepairable is REJECTED before any effector runs. Schemas also
//! project the minimal per-agent tool surface (REQ-EV-0116): only
//! supported + authorized + relevant tools travel to the model, measured in
//! schema bytes against the eager all-tools baseline.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The scalar types a tool parameter accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamType {
    Str,
    Int,
    Bool,
}

/// One tool parameter.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ParamSpec {
    pub param_type: ParamType,
    #[serde(default)]
    pub required: bool,
    /// Canonical default filled when the parameter is absent and optional.
    #[serde(default)]
    pub default: Option<serde_json::Value>,
    /// One-line description (travels in the token-counted schema text).
    pub description: String,
}

/// A typed tool schema: the contract between caller and effector.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSchema {
    /// Alternate parameter names repaired to canonical ones
    /// (e.g. "dir"/"folder" → "path").
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
    pub parameters: BTreeMap<String, ParamSpec>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SchemaError {
    /// Unrepairable input: rejected BEFORE any effect.
    Rejected { param: String, reason: String },
}

impl std::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchemaError::Rejected { param, reason } => {
                write!(f, "schema rejected {param:?}: {reason}")
            }
        }
    }
}

impl std::error::Error for SchemaError {}

fn coerce(
    param: &str,
    spec: &ParamSpec,
    raw: serde_json::Value,
) -> Result<serde_json::Value, SchemaError> {
    let wrong = |reason: &str| SchemaError::Rejected {
        param: param.to_string(),
        reason: reason.to_string(),
    };
    match spec.param_type {
        ParamType::Str => match raw {
            v @ serde_json::Value::String(_) => Ok(v),
            other => Err(wrong(&format!("expected string, got {other}"))),
        },
        ParamType::Int => match raw {
            v @ serde_json::Value::Number(_) => Ok(v),
            // Repair: stringified integers ("42") are common model output.
            serde_json::Value::String(s) => s
                .trim()
                .parse::<i64>()
                .map(|n| serde_json::json!(n))
                .map_err(|_| wrong(&format!("expected integer, got {s:?}"))),
            other => Err(wrong(&format!("expected integer, got {other}"))),
        },
        ParamType::Bool => match raw {
            v @ serde_json::Value::Bool(_) => Ok(v),
            serde_json::Value::String(s) => match s.trim() {
                "true" | "1" | "yes" => Ok(serde_json::json!(true)),
                "false" | "0" | "no" => Ok(serde_json::json!(false)),
                _ => Err(wrong(&format!("expected boolean, got {s:?}"))),
            },
            other => Err(wrong(&format!("expected boolean, got {other}"))),
        },
    }
}

impl ToolSchema {
    /// Normalizes raw caller arguments into the canonical form: aliases
    /// repaired, types coerced, defaults filled, unknown keys rejected.
    /// Runs BEFORE policy and execution (REQ-EV-0079).
    pub fn normalize(&self, raw: &serde_json::Value) -> Result<serde_json::Value, SchemaError> {
        let mut out = serde_json::Map::new();
        let obj = match raw {
            serde_json::Value::Null => serde_json::Map::new(),
            serde_json::Value::Object(map) => map.clone(),
            other => {
                return Err(SchemaError::Rejected {
                    param: "<arguments>".into(),
                    reason: format!("arguments must be an object, got {other}"),
                });
            }
        };
        for (key, value) in obj {
            // Repair alias → canonical name.
            let canonical = self
                .aliases
                .get(&key)
                .cloned()
                .unwrap_or_else(|| key.clone());
            let spec = self
                .parameters
                .get(&canonical)
                .ok_or_else(|| SchemaError::Rejected {
                    param: key.clone(),
                    reason: format!("unknown parameter (no canonical form for {key:?})"),
                })?;
            let coerced = coerce(&canonical, spec, value)?;
            out.insert(canonical, coerced);
        }
        // Fill defaults, then enforce required.
        for (name, spec) in &self.parameters {
            if !out.contains_key(name) {
                if let Some(default) = &spec.default {
                    out.insert(name.clone(), default.clone());
                } else if spec.required {
                    return Err(SchemaError::Rejected {
                        param: name.clone(),
                        reason: "required parameter missing".into(),
                    });
                }
            }
        }
        Ok(serde_json::Value::Object(out))
    }

    /// The token-counted schema text (what a model would actually see).
    pub fn schema_text(name: &str, schema: &ToolSchema) -> String {
        let mut text = format!("tool {name}(");
        let mut first = true;
        for (param, spec) in &schema.parameters {
            if !first {
                text.push_str(", ");
            }
            first = false;
            text.push_str(&format!(
                "{param}: {:?}{} — {}",
                spec.param_type,
                if spec.required { "" } else { " (optional)" },
                spec.description
            ));
        }
        text.push(')');
        text
    }
}

/// The minimal per-agent tool surface (REQ-EV-0116): the projection of the
/// full registry down to tools that are (a) authorized for the agent's
/// effect ceiling, (b) on the agent's allowlist, and (c) relevant to the
/// task tags. Fewer tools = fewer schema tokens at prompt time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentToolProfile {
    pub agent_id: String,
    /// Effect ceiling: tools above it are not projected.
    pub max_effect: modbit_policy::EffectClass,
    /// Tool name prefixes the agent may see (empty = all).
    pub allowed_prefixes: Vec<String>,
    /// Task tags selecting relevance (tool tags ∩ task tags ≠ ∅).
    pub task_tags: Vec<String>,
}

/// A registry entry with its schema and relevance tags, as supplied by the
/// host when projecting a surface.
#[derive(Clone, Debug)]
pub struct SurfaceCandidate {
    pub name: String,
    pub effect_class: modbit_policy::EffectClass,
    pub schema: ToolSchema,
    pub tags: Vec<String>,
}

/// Projects the minimal tool surface for an agent.
pub fn minimal_surface(
    profile: &AgentToolProfile,
    candidates: &[SurfaceCandidate],
) -> Vec<SurfaceCandidate> {
    candidates
        .iter()
        .filter(|c| {
            if c.effect_class > profile.max_effect {
                return false;
            }
            if !profile.allowed_prefixes.is_empty()
                && !profile
                    .allowed_prefixes
                    .iter()
                    .any(|p| c.name.starts_with(p.as_str()))
            {
                return false;
            }
            if !profile.task_tags.is_empty()
                && !c.tags.iter().any(|t| profile.task_tags.contains(t))
            {
                return false;
            }
            true
        })
        .cloned()
        .collect()
}

/// Total schema bytes a model prompt would pay for a surface (the token
/// benchmark unit; bytes ≈ chars/4 tokens).
pub fn surface_schema_bytes(surface: &[SurfaceCandidate]) -> usize {
    surface
        .iter()
        .map(|c| ToolSchema::schema_text(&c.name, &c.schema).len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use modbit_policy::EffectClass;

    fn fs_read_schema() -> ToolSchema {
        let mut parameters = BTreeMap::new();
        parameters.insert(
            "path".to_string(),
            ParamSpec {
                param_type: ParamType::Str,
                required: true,
                default: None,
                description: "file path to read".into(),
            },
        );
        parameters.insert(
            "max_bytes".to_string(),
            ParamSpec {
                param_type: ParamType::Int,
                required: false,
                default: Some(serde_json::json!(65536)),
                description: "read cap".into(),
            },
        );
        ToolSchema {
            aliases: BTreeMap::from([("dir".to_string(), "path".to_string())]),
            parameters,
        }
    }

    /// QUAL-EV-0079: an aliased/repairable invocation is normalized before
    /// the effector; an unrepairable one is rejected before ANY effect.
    #[test]
    fn schema_repairs_alias_and_rejects_invalid_before_effector() {
        let schema = fs_read_schema();

        // Alias repaired + stringified int coerced + default filled.
        let raw = serde_json::json!({"dir": "src/main.rs", "max_bytes": "1024"});
        let normalized = schema.normalize(&raw).unwrap();
        assert_eq!(normalized["path"], "src/main.rs");
        assert_eq!(normalized["max_bytes"], 1024);

        // Unrepairable: wrong type that cannot coerce.
        let bad = serde_json::json!({"path": 17});
        let err = schema.normalize(&bad).unwrap_err();
        assert!(matches!(err, SchemaError::Rejected { .. }));

        // Missing required parameter: rejected.
        let missing = schema.normalize(&serde_json::json!({})).unwrap_err();
        assert!(missing.to_string().contains("required"));
    }

    /// QUAL-EV-0116: the minimal surface costs strictly fewer schema bytes
    /// than the eager all-tools baseline.
    #[test]
    fn minimal_surface_beats_eager_baseline_on_schema_tokens() {
        let candidates: Vec<SurfaceCandidate> = (0..20)
            .map(|i| SurfaceCandidate {
                name: format!("tool{i}.run"),
                effect_class: if i % 3 == 0 {
                    EffectClass::External
                } else if i % 3 == 1 {
                    EffectClass::Write
                } else {
                    EffectClass::ReadOnly
                },
                schema: fs_read_schema(),
                tags: vec![format!("area{i}")],
            })
            .collect();

        let profile = AgentToolProfile {
            agent_id: "reviewer".into(),
            max_effect: EffectClass::ReadOnly,
            allowed_prefixes: vec![],
            task_tags: vec!["area2".into()],
        };
        let minimal = minimal_surface(&profile, &candidates);
        assert_eq!(minimal.len(), 1, "only the relevant read-only tool");

        let minimal_bytes = surface_schema_bytes(&minimal);
        let eager_bytes = surface_schema_bytes(&candidates);
        assert!(
            minimal_bytes < eager_bytes / 4,
            "minimal surface ({minimal_bytes}B) must be far below eager baseline ({eager_bytes}B)"
        );
    }
}

/// A deferred tool: NOT in the model's always-on surface. Its metadata is
/// searchable; the full schema is revealed only on activation, and
/// activation still goes through policy (REQ-EV-0134).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeferredToolEntry {
    pub name: String,
    /// One-line summary indexed for search.
    pub summary: String,
    pub tags: Vec<String>,
    pub effect_class: modbit_policy::EffectClass,
}

/// The searchable catalog of deferred tool metadata.
#[derive(Clone, Debug, Default)]
pub struct DeferredCatalog {
    pub entries: Vec<DeferredToolEntry>,
}

/// A search hit: the deferred tool plus its (still unauthorized) activation
/// descriptor.
#[derive(Clone, Debug)]
pub struct SearchHit {
    pub entry: DeferredToolEntry,
    /// The schema text the model receives AFTER activation — discovery
    /// returns only name + summary, never the parameter contract.
    pub schema_revealed_on_activation: bool,
}

impl DeferredCatalog {
    pub fn new(entries: Vec<DeferredToolEntry>) -> Self {
        Self { entries }
    }

    /// Searches the catalog by substring across name, summary, and tags.
    /// Search is DISCOVERY ONLY: it grants nothing.
    pub fn search(&self, query: &str) -> Vec<SearchHit> {
        let q = query.to_lowercase();
        self.entries
            .iter()
            .filter(|e| {
                e.name.to_lowercase().contains(&q)
                    || e.summary.to_lowercase().contains(&q)
                    || e.tags.iter().any(|t| t.to_lowercase().contains(&q))
            })
            .map(|e| SearchHit {
                entry: e.clone(),
                schema_revealed_on_activation: true,
            })
            .collect()
    }
}

#[cfg(test)]
mod deferred_tests {
    use super::*;
    use crate::ToolRegistry;
    use modbit_policy::{EffectClass, PolicyDecision};
    use std::sync::Arc;

    fn catalog() -> DeferredCatalog {
        DeferredCatalog::new(vec![
            DeferredToolEntry {
                name: "db.query".into(),
                summary: "run SQL against the analytics warehouse".into(),
                tags: vec!["database".into(), "sql".into()],
                effect_class: EffectClass::ReadOnly,
            },
            DeferredToolEntry {
                name: "image.render".into(),
                summary: "render a PNG from a chart spec".into(),
                tags: vec!["graphics".into()],
                effect_class: EffectClass::Write,
            },
        ])
    }

    /// QUAL-EV-0134: search finds the tool, activation still requires
    /// permission — discovery does not authorize.
    #[test]
    fn search_finds_tool_but_activation_still_enforces_policy() {
        let hits = catalog().search("sql");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry.name, "db.query");
        assert!(hits[0].schema_revealed_on_activation);

        // Activation path: the registry executes ONLY on a policy Allow.
        let registry = ToolRegistry::new();
        registry
            .register(
                "db.query",
                "1.0.0",
                EffectClass::ReadOnly,
                Arc::new(|_args| Ok(serde_json::json!({"rows": []}))),
            )
            .unwrap();

        // The kernel's DENY decision (discovery did NOT authorize).
        let deny = PolicyDecision::Deny {
            reason: "tool not granted for this session".into(),
        };
        let err = registry
            .execute("db.query", &serde_json::json!({"sql": "SELECT 1"}), &deny)
            .unwrap_err();
        assert!(
            matches!(err, crate::ToolError::PolicyDenied { .. }),
            "found-by-search tool must still be denied without a grant"
        );
    }
}
