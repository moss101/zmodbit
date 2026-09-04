//! Device/machine authority + policy generation + hot revalidation (M2,
//! REQ-EV-0040/0041, docs/23 § Approval policy). Device/MDM constraints sit
//! above project config — a project file cannot disable a device
//! requirement. Policy snapshots are immutable; hot revalidation creates a
//! new version without mutating in-flight state.

use serde::{Deserialize, Serialize};

/// Device/MDM-level constraints that sit above project config.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DevicePolicy {
    pub requires_device_trust: bool,
    pub proxy: Option<String>,
    pub update_channel: String,
    pub sandbox_required: bool,
    pub telemetry_opt_out: bool,
}

/// A project-level config that tries to set policy fields. Device
/// requirements cannot be disabled by lower layers.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub requires_device_trust: Option<bool>,
    pub sandbox_required: Option<bool>,
    pub telemetry_opt_out: Option<bool>,
}

/// Merges device policy with project config. Device constraints always win:
/// if the device requires trust or sandbox, project cannot disable them.
pub fn merge_device_policy(device: &DevicePolicy, project: &ProjectConfig) -> DevicePolicy {
    let mut merged = device.clone();
    // Project can only tighten (set to true), never loosen (set to false).
    if let Some(v) = project.requires_device_trust {
        merged.requires_device_trust = merged.requires_device_trust || v;
    }
    if let Some(v) = project.sandbox_required {
        merged.sandbox_required = merged.sandbox_required || v;
    }
    if let Some(v) = project.telemetry_opt_out {
        merged.telemetry_opt_out = merged.telemetry_opt_out || v;
    }
    merged
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PolicySnapshot {
    pub version: u64,
    pub allowed_tools: Vec<String>,
    pub forbidden_tools: Vec<String>,
    pub created_at_ms: u128,
}

/// Hot revalidation: creates a new immutable snapshot. The in-flight
/// snapshot is never mutated — the caller swaps atomically.
pub fn revalidate(
    current: &PolicySnapshot,
    new_allowed: Vec<String>,
    new_forbidden: Vec<String>,
    now_ms: u128,
) -> PolicySnapshot {
    PolicySnapshot {
        version: current.version + 1,
        allowed_tools: new_allowed,
        forbidden_tools: new_forbidden,
        created_at_ms: now_ms,
    }
}

/// Checks whether a tool is authorized under a given snapshot.
pub fn is_tool_allowed(snapshot: &PolicySnapshot, tool: &str) -> bool {
    snapshot.allowed_tools.iter().any(|t| t == tool)
        && !snapshot.forbidden_tools.iter().any(|t| t == tool)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0040: a project file attempting to disable a device
    /// requirement is rejected (the constraint survives the merge).
    #[test]
    fn device_requirement_survives_project_merge() {
        let device = DevicePolicy {
            requires_device_trust: true,
            proxy: None,
            update_channel: "stable".into(),
            sandbox_required: false,
            telemetry_opt_out: false,
        };
        let project = ProjectConfig {
            requires_device_trust: Some(false),
            ..Default::default()
        };
        let merged = merge_device_policy(&device, &project);
        assert!(
            merged.requires_device_trust,
            "device trust cannot be disabled"
        );
    }

    #[test]
    fn project_can_add_sandbox_requirement() {
        let device = DevicePolicy {
            requires_device_trust: true,
            proxy: None,
            update_channel: "stable".into(),
            sandbox_required: false,
            telemetry_opt_out: false,
        };
        let project = ProjectConfig {
            sandbox_required: Some(true),
            ..Default::default()
        };
        let merged = merge_device_policy(&device, &project);
        assert!(merged.sandbox_required, "project can tighten sandbox");
        assert!(merged.requires_device_trust, "device trust survives");
    }

    /// QUAL-EV-0041: policy hot revalidation — new snapshot, old unchanged.
    #[test]
    fn revalidation_creates_new_version() {
        let current = PolicySnapshot {
            version: 1,
            allowed_tools: vec!["fs.read".into()],
            forbidden_tools: vec![],
            created_at_ms: 1000,
        };
        let next = revalidate(
            &current,
            vec!["fs.read".into(), "fs.write".into()],
            vec!["shell.run".into()],
            2000,
        );
        assert_eq!(next.version, 2);
        assert_eq!(
            next.allowed_tools,
            vec!["fs.read".to_string(), "fs.write".to_string()]
        );
        assert_eq!(next.forbidden_tools, vec!["shell.run".to_string()]);
    }

    #[test]
    fn forbidden_tool_is_absent_after_revalidation() {
        let current = PolicySnapshot {
            version: 1,
            allowed_tools: vec!["fs.read".into(), "shell.run".into()],
            forbidden_tools: vec![],
            created_at_ms: 1000,
        };
        let next = revalidate(
            &current,
            vec!["fs.read".into()],
            vec!["shell.run".into()],
            2000,
        );
        assert!(
            !next.allowed_tools.contains(&"shell.run".to_string()),
            "shell.run must be absent after revocation"
        );
    }
}
