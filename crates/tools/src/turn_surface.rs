//! Configuration-dependent model tool surface (M2, REQ-EV-0096): the set
//! of tool schemas a model sees is COMPILED PER TASK/TURN from three
//! independent axes — host SUPPORT (adapters), kernel POLICY (grants), and
//! task RELEVANCE (tags). The snapshot of that compilation is the proof:
//! across modes, denied and irrelevant tools are ABSENT, not merely
//! de-prioritized.

use crate::schema::{ToolSchema, ToolSchemaExt};
use modbit_policy::EffectClass;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One tool as the compiler sees it (full description, not the projected
/// schema).
#[derive(Clone, Debug)]
pub struct SurfaceEntry {
    pub name: String,
    pub effect_class: EffectClass,
    pub schema: ToolSchema,
    /// Requires these consumer adapters to be enabled (empty = headless-safe).
    pub required_adapters: Vec<String>,
    /// Relevance tags intersected with the task profile.
    pub tags: Vec<String>,
}

/// The per-turn configuration that drives compilation.
#[derive(Clone, Debug, PartialEq)]
pub struct TurnConfig {
    pub enabled_adapters: Vec<String>,
    /// Tools the kernel has granted this session (canonical names).
    pub granted_tools: Vec<String>,
    /// Effect ceiling from the session profile.
    pub max_effect: EffectClass,
    /// Task tags selecting relevance (empty = all tools relevant).
    pub task_tags: Vec<String>,
}

/// Compiles the visible surface for one turn: support ∩ policy ∩ relevance
/// (with the effect ceiling applied). Denial at ANY axis removes the tool.
pub fn compile_surface(entries: &[SurfaceEntry], config: &TurnConfig) -> Vec<String> {
    entries
        .iter()
        .filter(|e| {
            // Support axis.
            if !e
                .required_adapters
                .iter()
                .all(|a| config.enabled_adapters.contains(a))
            {
                return false;
            }
            // Policy axis: kernel grant + effect ceiling.
            if !config.granted_tools.contains(&e.name) || e.effect_class > config.max_effect {
                return false;
            }
            // Relevance axis.
            config.task_tags.is_empty() || e.tags.iter().any(|t| config.task_tags.contains(t))
        })
        .map(|e| e.name.clone())
        .collect()
}

/// Snapshots the compiled surface schemas (what the model would receive).
pub fn snapshot_schemas(entries: &[SurfaceEntry], config: &TurnConfig) -> BTreeMap<String, String> {
    compile_surface(entries, config)
        .into_iter()
        .filter_map(|name| {
            entries
                .iter()
                .find(|e| e.name == name)
                .map(|e| (name.clone(), e.schema.schema_text_of(&name)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ParamSpec, ParamType};

    fn schema() -> ToolSchema {
        let mut parameters = BTreeMap::new();
        parameters.insert(
            "path".to_string(),
            ParamSpec {
                param_type: ParamType::Str,
                required: true,
                default: None,
                description: "target path".into(),
            },
        );
        ToolSchema {
            aliases: BTreeMap::new(),
            parameters,
        }
    }

    fn entries() -> Vec<SurfaceEntry> {
        vec![
            SurfaceEntry {
                name: "modbit.file.read".into(),
                effect_class: EffectClass::ReadOnly,
                schema: schema(),
                required_adapters: vec![],
                tags: vec!["files".into()],
            },
            SurfaceEntry {
                name: "modbit.file.write".into(),
                effect_class: EffectClass::Write,
                schema: schema(),
                required_adapters: vec![],
                tags: vec!["files".into()],
            },
            SurfaceEntry {
                name: "modbit.shell.run".into(),
                effect_class: EffectClass::External,
                schema: schema(),
                required_adapters: vec!["ui.terminal".into()],
                tags: vec!["shell".into()],
            },
            SurfaceEntry {
                name: "modbit.web.fetch".into(),
                effect_class: EffectClass::ReadOnly,
                schema: schema(),
                required_adapters: vec![],
                tags: vec!["web".into()],
            },
        ]
    }

    /// QUAL-EV-0096: snapshot tool schemas across modes and verify that
    /// denied and irrelevant tools are ABSENT.
    #[test]
    fn surface_snapshots_across_modes_omit_denied_and_irrelevant_tools() {
        let all = entries();

        // Mode A: read-only review session, no terminal adapter, web task.
        let mode_a = TurnConfig {
            enabled_adapters: vec![],
            granted_tools: vec![
                "modbit.file.read".into(),
                "modbit.file.write".into(), // granted but EXCEEDS ceiling
                "modbit.shell.run".into(),  // granted but adapter missing
                "modbit.web.fetch".into(),
            ],
            max_effect: EffectClass::ReadOnly,
            task_tags: vec!["web".into()],
        };
        let snapshot_a = snapshot_schemas(&all, &mode_a);
        // Relevant + allowed:
        assert_eq!(
            snapshot_a.keys().collect::<Vec<_>>(),
            vec!["modbit.web.fetch"]
        );
        // Denied by ceiling (write) — ABSENT even though granted:
        assert!(!snapshot_a.contains_key("modbit.file.write"));
        // Denied by support (no ui.terminal adapter) — ABSENT:
        assert!(!snapshot_a.contains_key("modbit.shell.run"));
        // Granted + allowed but IRRELEVANT to a web task — ABSENT:
        assert!(!snapshot_a.contains_key("modbit.file.read"));

        // Mode B: full-dev session, terminal enabled, ceiling External.
        let mode_b = TurnConfig {
            enabled_adapters: vec!["ui.terminal".into()],
            granted_tools: vec![
                "modbit.file.read".into(),
                "modbit.file.write".into(),
                "modbit.shell.run".into(),
                "modbit.web.fetch".into(),
            ],
            max_effect: EffectClass::External,
            task_tags: vec![],
        };
        let snapshot_b = snapshot_schemas(&all, &mode_b);
        assert_eq!(snapshot_b.len(), 4, "everything relevant in dev mode");
        for name in snapshot_b.values() {
            assert!(name.contains("tool modbit."), "schema text present");
        }
    }
}
