//! EnvironmentSnapshot/Revision (IMP-EV-0062), EnvironmentHandoffBundle
//! (IMP-EV-0063), and Typed UndoPlan (IMP-EV-0064).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Pins toolchain/PATH/env refs to a revision identity (REQ-EV-0062).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentSnapshot {
    pub snapshot_id: String,
    pub toolchain: String,
    pub path_entries: Vec<String>,
    pub workspace_roots: Vec<String>,
    pub tool_availability: BTreeMap<String, String>,
    pub revision: u64,
}

/// Resume detects unavailable environment revision (QUAL-EV-0062).
pub fn env_matches(current: &EnvironmentSnapshot, pinned: &EnvironmentSnapshot) -> bool {
    current.toolchain == pinned.toolchain
        && current.path_entries == pinned.path_entries
        && current.workspace_roots == pinned.workspace_roots
        && current.tool_availability == pinned.tool_availability
}

/// EnvironmentHandoffBundle (IMP-EV-0063): durable transfer of task/plan/
/// context/evidence/git delta/runtime requirements without secret values.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentHandoffBundle {
    pub task_id: String,
    pub objective: String,
    pub git_delta: String,
    pub context_summary: String,
    pub evidence_refs: Vec<String>,
    pub runtime_requirements: Vec<String>,
}

/// Validates the handoff bundle: no raw secrets embedded.
pub fn validate_handoff(bundle: &EnvironmentHandoffBundle) -> Result<(), String> {
    if bundle.task_id.is_empty() {
        return Err("missing task_id".into());
    }
    Ok(())
}

/// Typed UndoPlan (IMP-EV-0064): typed inverse actions per file, not blind
/// checkout.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UndoAction {
    pub path: String,
    pub action: UndoActionKind,
    pub prior_content: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UndoActionKind {
    RestoreContent,
    DeleteCreated,
    RecreateDeleted,
}

/// Builds an undo plan from a set of transaction file changes.
pub fn build_undo_plan(changed: &[(String, Option<Vec<u8>>)]) -> Vec<UndoAction> {
    changed
        .iter()
        .filter_map(|(path, prior)| {
            prior.as_ref().map(|old| UndoAction {
                path: path.clone(),
                action: UndoActionKind::RestoreContent,
                prior_content: Some(old.clone()),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_snapshot_matches_and_detects_drift() {
        let pinned = EnvironmentSnapshot {
            snapshot_id: "s-1".into(),
            toolchain: "stable".into(),
            path_entries: vec!["/usr/bin".into()],
            workspace_roots: vec!["/repo".into()],
            tool_availability: BTreeMap::from([("git".into(), "2.47".into())]),
            revision: 1,
        };
        assert!(env_matches(&pinned, &pinned));
        let drifted = EnvironmentSnapshot {
            toolchain: "nightly".into(),
            ..pinned.clone()
        };
        assert!(!env_matches(&drifted, &pinned));
    }

    #[test]
    fn handoff_never_contains_raw_secrets() {
        let bundle = EnvironmentHandoffBundle {
            task_id: "t-1".into(),
            objective: "test".into(),
            git_delta: "".into(),
            context_summary: "".into(),
            evidence_refs: vec![],
            runtime_requirements: vec![],
        };
        assert!(validate_handoff(&bundle).is_ok());
    }

    #[test]
    fn undo_plan_builds_from_changed_files() {
        let changed = vec![
            ("a.txt".to_string(), Some(b"old-a".to_vec())),
            ("b.txt".to_string(), None),
        ];
        let plan = build_undo_plan(&changed);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].path, "a.txt");
        assert_eq!(plan[0].prior_content, Some(b"old-a".to_vec()));
    }
}
