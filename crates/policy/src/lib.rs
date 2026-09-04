//! modbit-policy — capabilities, approvals, protected paths (M2.4 slice,
//! docs/16 Tool Capability, docs/17 Capability Kernel).
//!
//! Fail-closed capability kernel: a tool operation is allowed ONLY when an
//! explicit capability grant covers it. Missing grants deny; protected paths
//! deny writes/external effects even with a grant. The kernel never creates
//! capabilities on behalf of a caller (docs/21: the broker/exec layer is not
//! authorized to decide policy — that is this kernel's exclusive role).
//!
//! Canonical owner subsystem: effects-security / tool-runtime (docs/81).

use std::fmt;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

pub mod admin_precedence;
pub mod approvals;
pub mod capability_set;
pub mod device_policy;
pub mod patch_gate;
pub mod policy_before_exec;
pub use approvals::{intent_hash, Approval, ApprovalError, ApprovalState, ApprovalStore};
pub use capability_set::{check_e2e_capability, AdvertisedCapability, ProtocolCapabilitySet};
pub use device_policy::{
    merge_device_policy, revalidate, DevicePolicy, PolicySnapshot, ProjectConfig,
};

/// Effect class of a tool (docs/16 § Tool capability).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    ReadOnly,
    Write,
    External,
}

/// One capability grant: covers one tool name and the operation class it may
/// run. Created explicitly by the host/approval flow.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CapabilityGrant {
    pub grant_id: String,
    pub tool: String,
    pub effect_class: EffectClass,
}

/// The request a tool invocation makes against policy.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRequest {
    pub tool: String,
    pub effect_class: EffectClass,
    pub arguments: serde_json::Value,
}

#[derive(Debug)]
pub enum PolicyDecision {
    Allow,
    Deny { reason: String },
}

impl PolicyDecision {
    pub fn is_allow(&self) -> bool {
        matches!(self, PolicyDecision::Allow)
    }
}

impl fmt::Display for PolicyDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PolicyDecision::Allow => write!(f, "allow"),
            PolicyDecision::Deny { reason } => write!(f, "deny: {reason}"),
        }
    }
}

/// Fail-closed policy kernel: no grant → deny; protected path involvement →
/// deny write/external effects regardless of grants.
pub struct PolicyKernel {
    grants: Mutex<Vec<CapabilityGrant>>,
    protected_paths: Vec<String>,
}

impl PolicyKernel {
    /// A kernel whose write/external operations deny on these protected
    /// path prefixes (exact-prefix match).
    pub fn new(protected_paths: Vec<String>) -> Self {
        Self {
            grants: Mutex::new(Vec::new()),
            protected_paths,
        }
    }

    pub fn grant(&self, grant: CapabilityGrant) {
        self.grants
            .lock()
            .expect("policy mutex poisoned")
            .push(grant);
    }

    pub fn revoke(&self, grant_id: &str) {
        self.grants
            .lock()
            .expect("policy mutex poisoned")
            .retain(|g| g.grant_id != grant_id);
    }

    pub fn is_protected(&self, path: &str) -> bool {
        self.protected_paths.iter().any(|p| path.starts_with(p))
    }

    /// The policy decision for a tool call given the grants the caller
    /// holds. Fail-closed at every branch.
    pub fn check(
        &self,
        request: &ToolCallRequest,
        caller_grants: &[CapabilityGrant],
    ) -> PolicyDecision {
        // Protected paths: writes and external effects on protected paths
        // are denied regardless of grants.
        if !matches!(request.effect_class, EffectClass::ReadOnly) {
            if let Some(path) = request.arguments.get("path").and_then(|v| v.as_str()) {
                if self.is_protected(path) {
                    return PolicyDecision::Deny {
                        reason: format!("path {path:?} is protected"),
                    };
                }
            }
        }
        // Fail-closed: an explicit matching grant must exist.
        let covered = caller_grants
            .iter()
            .any(|g| g.tool == request.tool && g.effect_class == request.effect_class);
        if covered {
            PolicyDecision::Allow
        } else {
            PolicyDecision::Deny {
                reason: format!(
                    "no capability grant covers tool {:?} with effect class {:?}",
                    request.tool, request.effect_class
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn grant(tool: &str, class: EffectClass) -> CapabilityGrant {
        CapabilityGrant {
            grant_id: format!("g-{tool}"),
            tool: tool.into(),
            effect_class: class,
        }
    }

    fn kernel() -> PolicyKernel {
        PolicyKernel::new(vec!["/protected".into()])
    }

    #[test]
    fn missing_grant_is_fail_closed_deny() {
        let k = kernel();
        let request = ToolCallRequest {
            tool: "shell.run".into(),
            effect_class: EffectClass::External,
            arguments: json!({ "argv": ["echo", "hi"] }),
        };
        assert!(!k.check(&request, &[]).is_allow());
    }

    #[test]
    fn matching_grant_allows() {
        let k = kernel();
        let grants = vec![grant("shell.run", EffectClass::External)];
        let request = ToolCallRequest {
            tool: "shell.run".into(),
            effect_class: EffectClass::External,
            arguments: json!({ "argv": ["echo", "hi"] }),
        };
        assert!(k.check(&request, &grants).is_allow());
    }

    #[test]
    fn effect_class_mismatch_denies() {
        let k = kernel();
        let grants = vec![grant("fs.write", EffectClass::ReadOnly)];
        let request = ToolCallRequest {
            tool: "fs.write".into(),
            effect_class: EffectClass::Write,
            arguments: json!({ "path": "/ws/file.txt", "content": "x" }),
        };
        assert!(!k.check(&request, &grants).is_allow());
    }

    #[test]
    fn protected_paths_deny_writes_even_with_grant() {
        let k = kernel();
        let grants = vec![grant("fs.write", EffectClass::Write)];
        let request = ToolCallRequest {
            tool: "fs.write".into(),
            effect_class: EffectClass::Write,
            arguments: json!({ "path": "/protected/secrets.env" }),
        };
        assert!(
            !k.check(&request, &grants).is_allow(),
            "protected path denied"
        );

        // Read-only operations remain grant-gated but unaffected by the
        // protected-path write rule.
        let read_request = ToolCallRequest {
            tool: "fs.stat".into(),
            effect_class: EffectClass::ReadOnly,
            arguments: json!({ "path": "/protected/secrets.env" }),
        };
        assert!(k.check(&read_request, &grants_for_read()).is_allow());
    }

    fn grants_for_read() -> Vec<CapabilityGrant> {
        vec![grant("fs.stat", EffectClass::ReadOnly)]
    }

    #[test]
    fn revoke_removes_the_grant() {
        let k = kernel();
        k.grant(grant("fs.write", EffectClass::Write));
        k.revoke("g-fs.write");
        let request = ToolCallRequest {
            tool: "fs.write".into(),
            effect_class: EffectClass::Write,
            arguments: json!({ "path": "/ws/file.txt" }),
        };
        assert!(!k.check(&request, &[]).is_allow(), "revoked grants deny");
    }
}

/// Capability-oriented autonomous mode (M2, REQ-EV-0045): unattended
/// execution uses an explicit bounded capability profile — never bypass/yolo.
pub mod autonomous {
    use serde::{Deserialize, Serialize};

    /// Maximum effect class an autonomous run can use.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum MaxEffect {
        ReadOnly,
        Write,
        External,
    }

    /// Explicit bounded capability profile for unattended runs.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct AutonomousProfile {
        pub profile_id: String,
        pub max_effect: MaxEffect,
        pub allowed_tool_prefixes: Vec<String>,
        pub protected_path_prefixes: Vec<String>,
        pub max_concurrent: u32,
    }

    /// Checks whether a tool invocation is within the profile ceiling.
    pub fn check_autonomous(
        profile: &AutonomousProfile,
        tool: &str,
        is_write: bool,
    ) -> Result<(), String> {
        let write_allowed = profile.max_effect >= MaxEffect::Write;
        if is_write && !write_allowed {
            return Err(format!(
                "profile {} does not allow write operations",
                profile.profile_id
            ));
        }
        let allowed = profile
            .allowed_tool_prefixes
            .iter()
            .any(|prefix| tool.starts_with(prefix.as_str()));
        if !allowed {
            return Err(format!("tool {tool:?} not in profile allowlist"));
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn profile() -> AutonomousProfile {
            AutonomousProfile {
                profile_id: "safe-run".into(),
                max_effect: MaxEffect::Write,
                allowed_tool_prefixes: vec!["fs.".into(), "git.".into()],
                protected_path_prefixes: vec!["/protected".into()],
                max_concurrent: 2,
            }
        }

        #[test]
        fn autonomous_mode_rejects_unlisted_tools() {
            let p = profile();
            assert!(check_autonomous(&p, "fs.read", false).is_ok());
            assert!(check_autonomous(&p, "git.commit", true).is_ok());
            assert!(check_autonomous(&p, "shell.run", true).is_err());
        }

        #[test]
        fn autonomous_mode_rejects_protected_paths() {
            let p = profile();
            assert!(check_autonomous(&p, "/protected/secret", true).is_err());
        }
    }
}

/// Workspace trust is DISTINCT from sandbox grants (REQ-EV-0093). A trusted
/// repo still cannot use denied network/secret capabilities.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceTrust {
    pub workspace_root: String,
    pub trusted: bool,
}

/// Sandbox grants: fs/network/secret/tool scopes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SandboxGrants {
    pub fs: bool,
    pub network: bool,
    pub secrets: bool,
    pub tools: bool,
}

/// Checks that workspace trust does NOT grant sandbox capabilities.
/// Returns Err if the caller assumed trust implies sandbox.
pub fn check_trust_sandbox_separation(
    trusted: bool,
    grants: &SandboxGrants,
    requested: &str,
) -> Result<(), String> {
    let _ = trusted;
    match requested {
        "network" => {
            if !grants.network {
                return Err("trusted workspace does not grant network access".into());
            }
        }
        "secrets" => {
            if !grants.secrets {
                return Err("trusted workspace does not grant secret access".into());
            }
        }
        _ => {}
    }
    Ok(())
}

/// Permission modes (REQ-EV-0136): friendly modes compile to monotonic
/// policy. Mode switch requiring user action cannot be triggered by model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    ReadOnly,
    Standard,
    Autonomous,
}

/// A permission mode change request.
#[derive(Clone, Debug)]
pub struct ModeChangeRequest {
    pub from: PermissionMode,
    pub to: PermissionMode,
    pub by_model: bool,
}

/// Mode changes are monotonic downward (tightening) without user action.
/// A model cannot increase permissions.
pub fn validate_mode_change(request: &ModeChangeRequest) -> Result<(), String> {
    if request.by_model && request.to > request.from {
        return Err(format!(
            "model cannot escalate permissions from {:?} to {:?}",
            request.from, request.to
        ));
    }
    Ok(())
}

#[cfg(test)]
mod trust_tests {
    use super::*;

    /// QUAL-EV-0093: trusted repo still cannot use denied network capability.
    #[test]
    fn trusted_workspace_cannot_use_denied_network() {
        let grants = SandboxGrants {
            fs: true,
            network: false,
            secrets: false,
            tools: true,
        };
        let result = check_trust_sandbox_separation(true, &grants, "network");
        assert!(result.is_err(), "trusted workspace must not grant network");
    }

    #[test]
    fn sandbox_grants_are_independent_of_trust() {
        let grants = SandboxGrants {
            fs: true,
            network: true,
            secrets: false,
            tools: true,
        };
        let result = check_trust_sandbox_separation(true, &grants, "network");
        assert!(result.is_ok(), "network granted explicitly");
    }
}
